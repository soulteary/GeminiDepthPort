#!/usr/bin/env python3
"""Bounded read-only writer/SDK/status-thread observations on the Linux host.

Attach only to an already pinned experiment identity. No USB access, installs,
cache dropping, sysctl changes or signals to capture processes. A selected
observation target does not make an experiment a single-camera capture run.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import stat
import subprocess
import sys
import time

import experiment_runtime as rt
import hardware as hw

MAX_JSON = 4 * 1024**2
THREAD_PREFIXES = ('native-depth', 'recording-check', 'status-persiste')


def require(ok, code):
    if not ok:raise ValueError(code)


def read_bytes(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        value = bytearray()
        while len(value) <= limit:
            block = os.read(fd, min(65536, limit+1-len(value)))
            if not block:break
            value.extend(block)
        require(len(value) <= limit, 'READ_LIMIT')
        return bytes(value)
    finally:os.close(fd)


def read_json(path, private=False):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode), 'REGULAR_JSON_REQUIRED')
    if private:require(info.st_uid == os.geteuid() and not info.st_mode & 0o077, 'PRIVATE_IDENTITY_REQUIRED')
    return json.loads(read_bytes(path, MAX_JSON), object_pairs_hook=rt.pairs)


def sha(path):
    return hashlib.sha256(read_bytes(path, MAX_JSON)).hexdigest()


def proc_identity(pid, proc):
    text = read_bytes(proc/str(pid)/'stat', 16384).decode()
    fields = text.rsplit(') ', 1)[1].split()
    require(len(fields) >= 20, 'PROC_STAT_SCHEMA')
    return {'pid': pid, 'state': fields[0], 'start_ticks': int(fields[19]),
            'comm': text.split('(', 1)[1].rsplit(') ', 1)[0][:64],
            'cpu_ticks': int(fields[11])+int(fields[12]),
            'pid_namespace': os.readlink(proc/str(pid)/'ns/pid')}


def status_matches(status, pin):
    expected = {'generation': pin['run_id'], 'config_sha256': pin['config_sha256'],
                'instance_id': pin['instance_id'], 'session_id': pin['session_id'], 'pid': pin['worker_pid']}
    require(all(status.get(key) == value for key, value in expected.items())
            and status.get('restart_attempt') == 0, 'STATUS_IDENTITY_CHANGED')


class Attachment:
    def __init__(self, root, run, camera, proc=Path('/proc'), inspect=None):
        self.root = root; self.run = run; self.camera = camera; self.proc = proc
        self.name = camera['name']; self.status_path = root/self.name/'state/status.json'
        require(Path(camera['status']).resolve() == self.status_path, 'STATUS_PATH_BINDING')
        self.pin_path = root/self.name/'runtime-identity.json'
        self.pin = read_json(self.pin_path, private=True)
        binding = read_json(root/self.name/'config/experiment.json')
        status = read_json(self.status_path); status_matches(status, self.pin)
        info = (inspect or hw.own_container)(run, camera)
        expected = {'container_id': info['Id'], 'image_id': run['image_id'],
            'container_started_at': info['State']['StartedAt'], 'run_id': run['run_id'],
            'config_sha256': binding['capture_sha256'], 'launch_instance_id': binding['instance_id']}
        require(sha(root/self.name/'config/capture.json')==binding['capture_sha256'],'CAPTURE_CONFIG_CHANGED')
        require(self.pin.get('version') == 1 and all(self.pin.get(k) == v for k, v in expected.items()), 'PIN_RUN_BINDING')
        require(binding['run_id'] == run['run_id'] and binding['image_id'] == run['image_id'], 'EXPERIMENT_RUN_BINDING')
        require(info['State']['Running'], 'ATTACH_REQUIRES_LIVE_WORKER')
        observed = hw.observe_runtime_process(info, status, binding, previous=self.pin['process'], proc=proc)
        require(observed == self.pin['process'], 'PIN_PROCESS_BINDING')
        self.pid = observed['worker_host_pid']; self.started = observed['worker_start_ticks']
        self.namespace = observed['pid_namespace']; self.exe = (proc/str(self.pid)/'exe').stat()
        self.terminal = False; self.identity_failed = False;self.stack_cache={};self.sample_index=0

    def check(self):
        identity = proc_identity(self.pid, self.proc)
        require(identity['start_ticks'] == self.started and identity['pid_namespace'] == self.namespace, 'WORKER_PID_REUSED')
        executable = (self.proc/str(self.pid)/'exe').stat()
        require((executable.st_dev, executable.st_ino) == (self.exe.st_dev, self.exe.st_ino), 'WORKER_EXEC_CHANGED')
        if identity['state'] == 'Z':raise ProcessLookupError('WORKER_ZOMBIE')
        return identity

    def sample(self, all_threads=False, max_threads=64, stack_bytes=8192):
        try:
            before = self.check()
            status = read_json(self.status_path); status_matches(status, self.pin)
            tasks = self.proc/str(self.pid)/'task'; candidates = [tasks/str(self.pid)]; scanned = 0; scan_truncated = False
            with os.scandir(tasks) as entries:
                for entry in entries:
                    if not entry.name.isdigit() or int(entry.name) == self.pid:continue
                    scanned += 1
                    if scanned > 1024:scan_truncated = True;break
                    if all_threads or read_field(Path(entry.path)/'comm', 256).get('value', '').startswith(THREAD_PREFIXES):
                        candidates.append(Path(entry.path))
                    if len(candidates) > max_threads:break
            truncated = len(candidates) > max_threads
            rows = [thread_sample(path, stack_bytes) for path in candidates[:max_threads]]
            after = self.check()
            require(before['start_ticks'] == after['start_ticks'], 'WORKER_PID_REUSED')
            self.sample_index+=1
            for row in rows:
                stack=row.get('kernel_stack') or {}
                if stack.get('state')=='available':
                    digest=hashlib.sha256(stack['value'].encode()).hexdigest();key=(row['tid'],row['start_ticks'])
                    prior=self.stack_cache.get(key)
                    if prior and prior[0]==digest:
                        row['kernel_stack']={'state':'same-as-earlier-observation','sha256':digest,'reference_worker_sample':prior[1]}
                    else:
                        stack['sha256']=digest
                        if len(self.stack_cache)>=1024:self.stack_cache.pop(next(iter(self.stack_cache)))
                        self.stack_cache[key]=(digest,self.sample_index)
            return {'name': self.name, 'state': 'observed', 'pid': self.pid, 'start_ticks': self.started,'worker_sample':self.sample_index,
                'status': compact_status(status), 'threads': rows,
                'thread_selection': 'all-bounded' if all_threads else 'main-and-native-checkpoint-status',
                'threads_truncated': truncated, 'thread_scan_truncated': scan_truncated}
        except (FileNotFoundError, ProcessLookupError) as error:
            try:self.check()
            except (FileNotFoundError,ProcessLookupError):pass
            except ValueError as changed:
                self.terminal=True;self.identity_failed=True
                return {'name':self.name,'pid':self.pid,'state':'identity-rejected','detail':str(changed),'threads':None}
            else:
                self.terminal=True;self.identity_failed=True
                return {'name':self.name,'pid':self.pid,'state':'binding-evidence-unavailable','detail':str(error),'threads':None}
            self.terminal = True
            return {'name': self.name, 'pid': self.pid, 'state': 'exited', 'detail': type(error).__name__+':'+str(error),
                    'threads': None, 'scope': 'worker/thread batch discarded after process exit; not zero activity'}
        except ValueError as error:
            self.terminal = True; self.identity_failed = True
            return {'name': self.name, 'pid': self.pid, 'state': 'identity-rejected', 'detail': str(error), 'threads': None}
        except PermissionError as error:
            return {'name': self.name, 'pid': self.pid, 'state': 'permission-denied', 'detail': str(error), 'threads': None}


def compact_status(status):
    raw=status.get('raw_recording') or {};queue=raw.get('writer_queue') or {}
    disk=raw.get('disk_stages') or {};observation=disk.get('checkpoint_observation') or {}
    persistence=status.get('status_persistence') or {};imu=status.get('imu') or {};cleanup=status.get('cleanup') or {}
    def maximum(value):
        value=value or {}
        return {key:value.get(key) for key in ('samples','maximum_ns')}
    updated=status.get('updated_unix_ms')
    return {'capture_state':status.get('state'),'ready':status.get('ready'),'reason':status.get('reason'),
        'status_updated_unix_ms':updated,'status_age_ms':time.time_ns()//1000000-updated if type(updated) is int else None,
        'raw':{key:raw.get(key) for key in ('state','reason','submitted_pairs','written_pairs','whole_pair_queue_overflows')},
        'writer_queue':{key:queue.get(key) for key in ('capacity_pairs','queued_pairs','inflight_pairs','oldest_queued_age_ns','high_water_pairs','discarded_pairs','closed')},
        'writer_wait':maximum(queue.get('wait')),'append':maximum(disk.get('append')),
        'checkpoint':{key:observation.get(key) for key in ('enabled','latest_event_id','active_limit_hits','active')},
        'persistence':{**{key:persistence.get(key) for key in ('active_write','submitted_sequence','completed_sequence','pending_snapshots','errors','submission_errors')},
            'recent_critical_events':persistence.get('critical_events',[])[-2:]},
        'imu':{**{key:imu.get(key) for key in ('error','queue_high_water','failure_observation')},'drain_interval':maximum(imu.get('drain_interval'))},
        'sdk_wait':maximum((status.get('pipeline_timing') or {}).get('sdk_wait_call')),
        'cleanup':{'active':cleanup.get('active'),'recent_completed':cleanup.get('completed',[])[-2:]}}


def read_field(path, limit):
    try:
        text = read_bytes(path, limit).decode('utf-8', 'replace').strip()
        return {'state': 'available' if text else 'empty-or-running', 'value': text}
    except PermissionError as error:return {'state': 'permission-denied', 'errno': error.errno}
    except FileNotFoundError:return {'state': 'exited-or-unavailable'}
    except OSError as error:return {'state': 'unavailable', 'errno': error.errno}
    except ValueError as error:return {'state': 'limit-or-schema', 'detail': str(error)}


def thread_sample(path, stack_bytes):
    try:
        before = read_bytes(path/'stat', 16384).decode(); fields = before.rsplit(') ', 1)[1].split()
        start = int(fields[19]); comm = before.split('(', 1)[1].rsplit(') ', 1)[0][:64]
        row = {'tid': int(path.name), 'start_ticks': start, 'comm': comm, 'task_state': fields[0],
            'cpu_ticks': int(fields[11])+int(fields[12]), 'wchan': read_field(path/'wchan', 1024),
            'kernel_stack': read_field(path/'stack', stack_bytes), 'schedstat': read_field(path/'schedstat', 1024)}
        if row['schedstat']['state'] == 'available':
            values = row['schedstat']['value'].split()
            if len(values) == 3 and all(v.isdigit() for v in values):
                row['schedstat']['counters'] = dict(zip(('runtime_ns','runqueue_wait_ns','timeslices'), map(int, values)))
            else:row['schedstat']['state'] = 'schema-unavailable'
        after = read_bytes(path/'stat', 16384).decode().rsplit(') ',1)[1].split()
        require(int(after[19]) == start, 'THREAD_TID_REUSED')
        return row
    except FileNotFoundError:return {'tid': int(path.name), 'state': 'exited', 'kernel_stack': {'state': 'unavailable'}}
    except (ValueError, IndexError) as error:return {'tid': int(path.name), 'state': 'identity-or-schema-rejected', 'detail': str(error)}


def disk_sample(names, proc, previous=None):
    current = {}; now = time.monotonic_ns()
    if not names:return {'state':'not-requested','monotonic_ns':now},None
    raw = read_field(proc/'diskstats', 1024**2)
    if raw['state'] != 'available':return {'state': raw['state'], 'monotonic_ns': now}, None
    for line in raw['value'].splitlines():
        values = line.split()
        if len(values) < 14 or values[2] not in names:continue
        counters = dict(zip(('read_ops','read_sectors','read_ms','write_ops','write_sectors','write_ms','inflight','io_ms','weighted_ms'),
            map(int,(values[3],values[5],values[6],values[7],values[9],values[10],values[11],values[12],values[13]))))
        current[values[2]] = counters
    result = {'state': 'available' if set(current) == set(names) else 'requested-device-missing',
              'monotonic_ns': now, 'devices': current}
    if previous:
        old_at, old = previous; elapsed = (now-old_at)/1e9; deltas = {}
        for name in set(current)&set(old):
            delta = {key: current[name][key]-old[name][key] for key in current[name] if key != 'inflight'}
            if any(v < 0 for v in delta.values()):deltas[name] = {'state': 'counter-regressed'};continue
            delta.update(write_bytes=delta['write_sectors']*512,read_bytes=delta['read_sectors']*512)
            deltas[name] = {'state': 'available', 'counters': delta, 'sampled_seconds': elapsed,
                'mean_read_request_ms': delta['read_ms']/delta['read_ops'] if delta['read_ops'] else None,
                'mean_write_request_ms': delta['write_ms']/delta['write_ops'] if delta['write_ops'] else None}
        result['interval_deltas'] = deltas
    return result, (now,current)


def memory_sample(proc):
    raw=read_field(proc/'meminfo',65536)
    if raw['state']!='available':return {'state':raw['state'],'fields_bytes':None}
    fields={};invalid=[]
    selected=('Dirty','Writeback','Cached','MemAvailable','MemFree','WritebackTmp')
    for line in raw['value'].splitlines():
        key,separator,text=line.partition(':')
        if not separator or key not in selected:continue
        values=text.split()
        if len(values)!=2 or not values[0].isdigit() or values[1]!='kB':invalid.append(key);continue
        fields[key]=int(values[0])*1024
    missing=[key for key in ('Dirty','Writeback') if key not in fields]
    return {'state':'schema-unavailable' if invalid else 'requested-fields-missing' if missing else 'available',
        'fields_bytes':fields,'missing_required':missing,'invalid_fields':invalid,
        'scope':'whole-host page cache/writeback snapshot; not per-camera ownership or unique IO cause'}


def load_attachments(run_dir, selected, proc=Path('/proc'), inspect=None):
    root = Path(run_dir).resolve(); run = read_json(root/'run.json', private=True)
    require(run.get('experiment') == rt.EXPERIMENT and re.fullmatch('[a-f0-9]{32}',run.get('run_id','')) is not None
            and re.fullmatch('sha256:[a-f0-9]{64}',run.get('image_id','')) is not None,'RUN_IDENTITY')
    cameras = run.get('cameras',[])
    require(1 <= len(cameras) <= 3 and len({c['name'] for c in cameras}) == len(cameras), 'CAMERA_SELECTION')
    require(not selected or set(selected) <= {c['name'] for c in cameras}, 'UNKNOWN_CAMERA')
    files = {'run.json': sha(root/'run.json')}
    require(type(run.get('files')) is dict and len(run['files']) <= 128, 'PREPARED_FILE_MANIFEST')
    for relative, expected in run['files'].items():
        require(type(relative) is str and len(relative)<=512 and not Path(relative).is_absolute() and '..' not in Path(relative).parts, 'RELATIVE_PREPARED_FILE')
        files[relative] = sha(root/relative); require(files[relative] == expected, 'PREPARED_FILE_CHANGED')
    chosen=[camera for camera in cameras if not selected or camera['name'] in selected]
    for camera in chosen:read_json(root/camera['name']/'runtime-identity.json',private=True)
    targets = []
    for camera in cameras:
        require(re.fullmatch('[a-z][a-z0-9-]{0,23}',camera['name']) is not None,'CAMERA_NAME')
        if selected and camera['name'] not in selected:continue
        for file in ('capture.json','experiment.json','runtime.json'):
            require(str(Path(camera['name'])/'config'/file) in files,'MISSING_PREPARED_BINDING')
        runtime_path = root/camera['name']/'config/runtime.json'
        require(hw.digest(read_json(runtime_path)) == run['runtime_sha256'], 'RUNTIME_MANIFEST_CHANGED')
        target = Attachment(root,run,camera,proc,inspect); targets.append(target)
        files[str(target.pin_path.relative_to(root))] = sha(target.pin_path)
    return run, targets, files


class Output:
    def __init__(self, root, max_bytes):
        self.root = Path(root); self.root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.max_bytes = max_bytes; self.used = 0
        self.path = self.root/'samples.jsonl'; self.file = self.path.open('x'); os.fchmod(self.file.fileno(),0o600)
    def emit(self, value):
        text = json.dumps(value,separators=(',',':'))+'\n'; size = len(text.encode())
        require(self.used+size <= self.max_bytes,'OBSERVER_OUTPUT_LIMIT')
        self.file.write(text); self.file.flush(); self.used += size
    def close(self):
        self.file.flush(); os.fsync(self.file.fileno()); self.file.close()


class Perf:
    def __init__(self, root, seconds=0, frequency=49, max_mib=64):
        self.root = root; self.seconds = seconds; self.frequency = frequency; self.max_bytes = max_mib*1024**2
        self.child = None; self.log = None; self.result = {'requested':bool(seconds),'state':'disabled'}
    def start(self, targets):
        if not self.seconds:return
        binary = shutil.which('perf')
        if not binary:self.result.update(state='unavailable',reason='perf not installed; no installation attempted');return
        for target in targets:target.check()
        self.log = (self.root/'perf.log').open('xb');os.fchmod(self.log.fileno(),0o600)
        def limits():resource.setrlimit(resource.RLIMIT_FSIZE,(self.max_bytes,self.max_bytes))
        argv = [binary,'record','-e','cpu-clock','-F',str(self.frequency),'--clockid','mono','--call-graph','dwarf,4096',
                '-o',str(self.root/'perf.data'),'-p',','.join(str(t.pid) for t in targets),'--','sleep',str(self.seconds)]
        self.child = subprocess.Popen(argv,stdout=self.log,stderr=self.log,start_new_session=True,preexec_fn=limits)
        self.result.update(state='running',argv=argv,scope='optional userspace stacks; perf tool overhead is separate; no kernel settings changed')
        try:
            for target in targets:target.check()
        except Exception as error:
            self.result['stopped_for_identity_rejection']=True
            self.finish()
            raise ValueError('PERF_TARGET_CHANGED_AFTER_START') from error
    def finish(self):
        if self.child:
            try:
                if self.child.poll() is None:
                    try:os.killpg(self.child.pid,signal.SIGINT)
                    except ProcessLookupError:pass
                    try:self.child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        try:os.killpg(self.child.pid,signal.SIGKILL)
                        except ProcessLookupError:pass
                        self.child.wait(timeout=5)
                size=(self.root/'perf.data').stat().st_size if (self.root/'perf.data').exists() else 0
                self.result.update(state='recorded-not-validated' if self.child.returncode==0 and size>0 else 'failed-or-interrupted',exit_code=self.child.returncode,data_bytes=size)
            except Exception as error:self.result.update(state='cleanup-error',detail=type(error).__name__+':'+str(error))
        if self.log:self.log.close()
        return self.result


def run(args, proc=Path('/proc'), inspect=None, sleep=time.sleep):
    require(math.isfinite(args.seconds) and 0 < args.seconds <= 86400 and .05 <= args.interval <= 10
            and 0 <= args.wait_for_bindings <= 3600 and 0 <= args.tail_seconds <= 30
            and 1 <= args.max_output_mib <= 1024 and 1 <= args.max_threads <= 256
            and 128 <= args.stack_bytes <= 16384 and 0 <= args.perf_seconds <= 300
            and 1 <= args.perf_frequency <= 99 and 1 <= args.perf_max_mib <= 256
            and len(args.camera)<=3 and len(args.disk)<=8 and len(set(args.disk))==len(args.disk)
            and all(re.fullmatch('[a-zA-Z0-9_.-]{1,64}',name) for name in args.disk),'OBSERVER_LIMITS')
    root = Path(args.run_dir).resolve(); output = Path(args.output).absolute()
    require(not output.resolve().is_relative_to(root),'OUTPUT_OUTSIDE_CAPTURE_REQUIRED')
    total_budget=args.max_output_mib*1024**2;metadata_reserve=256*1024
    data_budget=total_budget-metadata_reserve
    perf_file_limit=min(args.perf_max_mib*1024**2,data_budget//4) if args.perf_seconds else 0
    out = Output(output,data_budget-2*perf_file_limit);started = time.monotonic_ns();cpu0=time.process_time_ns()
    perf = Perf(output,args.perf_seconds,args.perf_frequency,args.perf_max_mib)
    perf.max_bytes=perf_file_limit or perf.max_bytes
    targets = []; inputs = {}; run_info = None; samples = 0; last = None; max_gap = 0; overruns = 0; gap_count=0; terminal_at = None
    stop_reason = 'duration-limit'; errors = []; unavailable = {}; gaps={};previous_disk = None; interrupted=[False]
    tool_hash=sha(Path(__file__))
    old = {}
    if __import__('threading').current_thread() is __import__('threading').main_thread():
        old = {sig:signal.signal(sig,lambda _s,_f:interrupted.__setitem__(0,True)) for sig in (signal.SIGINT,signal.SIGTERM)}
    try:
        while True:
            at = time.monotonic_ns();elapsed = (at-started)/1e9
            if not targets:
                try:
                    run_info, targets, inputs = load_attachments(root,args.camera,proc,inspect)
                    perf.start(targets)
                except (FileNotFoundError,ProcessLookupError) as error:
                    if elapsed > args.wait_for_bindings:raise ValueError('BINDING_WAIT_TIMEOUT:'+str(error)) from error
                    out.emit({'kind':'awaiting-verified-bindings','monotonic_ns':at,'unix_ns':time.time_ns(),'detail':str(error)})
            rows = []
            for target in targets:
                row = {'name':target.name,'state':'already-terminal','pid':target.pid} if target.terminal else target.sample(args.all_threads,args.max_threads,args.stack_bytes)
                rows.append(row)
                if row.get('state')=='permission-denied':gaps['target-permission-denied']=gaps.get('target-permission-denied',0)+1
                if row.get('threads_truncated') or row.get('thread_scan_truncated'):gaps['thread-selection-truncated']=gaps.get('thread-selection-truncated',0)+1
                for thread in row.get('threads') or []:
                    for field in ('kernel_stack','wchan','schedstat'):
                        state = (thread.get(field) or {}).get('state','unavailable')
                        if state not in ('available','same-as-earlier-observation'):unavailable[field+':'+state] = unavailable.get(field+':'+state,0)+1
            if any(target.identity_failed for target in targets) and perf.child is not None and perf.child.poll() is None:
                perf.result['stopped_for_identity_rejection']=True
                perf.finish()
            disks,previous_disk = disk_sample(args.disk,proc,previous_disk)
            memory=memory_sample(proc)
            if memory['state']!='available':gaps['meminfo:'+memory['state']]=gaps.get('meminfo:'+memory['state'],0)+1
            if args.disk and disks['state']!='available':gaps['disk:'+disks['state']]=gaps.get('disk:'+disks['state'],0)+1
            for device,value in disks.get('interval_deltas',{}).items():
                if value.get('state')!='available':gaps['disk:'+device+':'+value['state']]=gaps.get('disk:'+device+':'+value['state'],0)+1
            gap_ns=at-last if last is not None else None
            if gap_ns is not None:
                max_gap=max(max_gap,gap_ns)
                if gap_ns>args.interval*1.5e9:gap_count+=1
            last = at;duration = time.monotonic_ns()-at
            if duration > args.interval*1e9:overruns += 1
            out.emit({'kind':'writer-tail-sample','monotonic_ns':at,'unix_ns':time.time_ns(),'targets':rows,'disks':disks,'memory':memory,
                'iteration_wall_ns':duration,'overrun':duration>args.interval*1e9,
                'sample_gap_ns':gap_ns,'gap_over_requested_interval':gap_ns>args.interval*1e9 if gap_ns is not None else False})
            samples += 1
            if targets and all(t.terminal for t in targets):
                terminal_at = terminal_at or time.monotonic()
                if time.monotonic()-terminal_at >= args.tail_seconds:stop_reason='workers-terminal';break
            if args.stop_file and Path(args.stop_file).exists():stop_reason='stop-file';break
            if interrupted[0]:stop_reason='interrupted';break
            if elapsed >= args.seconds:break
            if samples >= 100000:raise ValueError('OBSERVER_SAMPLE_LIMIT')
            total = out.used+sum(path.stat().st_size for path in output.glob('perf.*') if path.is_file())
            require(total <= args.max_output_mib*1024**2,'OBSERVER_TOTAL_OUTPUT_LIMIT')
            sleep(max(0,args.interval-(time.monotonic_ns()-at)/1e9))
    except Exception as error:errors.append(type(error).__name__+':'+str(error));stop_reason='error'
    finally:
        perf_result=perf.finish();out.close()
        for sig,handler in old.items():signal.signal(sig,handler)
    after = {}
    for relative in inputs:
        try:after[relative]=sha(root/relative)
        except (OSError,ValueError) as error:after[relative]='unavailable:'+type(error).__name__
    if after != inputs:errors.append('INPUT_BINDING_CHANGED')
    final_tool_hash=sha(Path(__file__))
    if final_tool_hash!=tool_hash:errors.append('TOOL_SOURCE_CHANGED')
    if perf_result.get('requested') and perf_result.get('state')!='recorded-not-validated':gaps['perf:'+perf_result['state']]=1
    requested_disks_complete=not any(key.startswith('disk:') for key in gaps)
    full = bool(targets) and all(t.terminal for t in targets) and not any(t.identity_failed for t in targets)
    summary = {'version':1,'observation_only':True,'hardware_acceptance_verified':False,
        'ok':not errors and full and requested_disks_complete,'workflow_completed':not errors and full,
        'diagnostic_data_complete':not errors and full and not unavailable and not gaps,
        'diagnostic_gaps':gaps,'requested_disks_complete':requested_disks_complete,'stop_reason':stop_reason,'full_selected_worker_lifecycle_end_observed':full,
        'started_monotonic_ns':started,'finished_monotonic_ns':time.monotonic_ns(),'samples':samples,'bytes':out.used,
        'maximum_sample_gap_ns':max_gap,'sampling_overruns':overruns,'sampling_gap_count':gap_count,'sampling_gap_threshold_ns':round(args.interval*1.5e9),'unavailable_thread_fields':unavailable,
        'observer_cpu_ns':time.process_time_ns()-cpu0,'observer_peak_rss_kib':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
        'errors':errors,'input_sha256':inputs,'input_sha256_after':after,'tool_sha256':tool_hash,'tool_sha256_after':final_tool_hash,
        'runtime_sha256':run_info.get('runtime_sha256') if run_info else None,
        'run_id':run_info.get('run_id') if run_info else None,'image_id':run_info.get('image_id') if run_info else None,
        'identity_rejected_targets':[t.name for t in targets if t.identity_failed],
        'selected_cameras':[t.name for t in targets],'perf':perf_result,
        'limits':{'seconds':args.seconds,'max_output_mib':args.max_output_mib,'max_threads':args.max_threads,'stack_bytes':args.stack_bytes,'sample_bytes_limit':out.max_bytes,'metadata_reserve_bytes':metadata_reserve,'perf_file_limit_bytes':perf_file_limit},
        'scope':'only selected already-pinned workers; single observed target does not establish single-camera capture; kernel stacks and disk request means do not identify a unique device cause'}
    require(len(json.dumps(summary).encode())<metadata_reserve-4096,'SUMMARY_OUTPUT_LIMIT')
    with (output/'summary.json').open('x') as file:
        os.fchmod(file.fileno(),0o600);json.dump(summary,file,indent=2);file.write('\n');file.flush();os.fsync(file.fileno())
    files=sorted(path for path in output.iterdir() if path.is_file())
    with (output/'SHA256SUMS').open('x') as file:
        os.fchmod(file.fileno(),0o600)
        for path in files:
            h=hashlib.sha256()
            with path.open('rb') as source:
                for block in iter(lambda:source.read(1024**2),b''):h.update(block)
            file.write(h.hexdigest()+'  '+path.name+'\n')
        file.flush();os.fsync(file.fileno())
    fd=os.open(output,os.O_RDONLY|os.O_DIRECTORY)
    try:os.fsync(fd)
    finally:os.close(fd)
    return summary


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-dir',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--camera',action='append',default=[]);parser.add_argument('--disk',action='append',default=[])
    parser.add_argument('--seconds',type=float,default=3600);parser.add_argument('--interval',type=float,default=.2)
    parser.add_argument('--wait-for-bindings',type=float,default=300);parser.add_argument('--tail-seconds',type=float,default=5)
    parser.add_argument('--stop-file',type=Path);parser.add_argument('--max-output-mib',type=int,default=256)
    parser.add_argument('--all-threads',action='store_true');parser.add_argument('--max-threads',type=int,default=64)
    parser.add_argument('--stack-bytes',type=int,default=8192);parser.add_argument('--perf-seconds',type=float,default=0)
    parser.add_argument('--perf-frequency',type=int,default=49);parser.add_argument('--perf-max-mib',type=int,default=64)
    args=parser.parse_args()
    try:
        require(sys.platform.startswith('linux'),'LINUX_HOST_REQUIRED');os.umask(0o077)
        result=run(args);print(json.dumps({'ok':result['ok'],'output':str(args.output),'stop_reason':result['stop_reason']}))
        return 0 if result['ok'] else 1
    except Exception as error:print(json.dumps({'ok':False,'error':str(error)}),file=sys.stderr);return 1


if __name__=='__main__':raise SystemExit(main())
