#!/usr/bin/env python3
"""Read-only bounded Linux checkpoint/queue/process/disk correlation.

Run on the Linux Docker host. No SDK access, sysctl changes, cache dropping,
container mutation, or signals to capture processes. Optional perf signals only
its own new process group. Output is private and may contain diagnostic symbols.
"""
import argparse
import json
import math
import os
from pathlib import Path
import resource
import shutil
import signal
import subprocess
import sys
import time

LIMIT = 4 * 1024 * 1024


def bounded_json(path):
    with Path(path).open('rb') as f:
        raw = f.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ValueError('STATUS_TOO_LARGE')
    value = json.loads(raw)
    if not isinstance(value, dict) or value.get('version') != 1 or value.get('kind') != 'capture-service-status':
        raise ValueError('STATUS_SCHEMA')
    return value


def counters(path):
    return {p[0]: int(p[1]) for p in (s.split() for s in Path(path).read_text().splitlines()) if len(p) == 2}


def proc_stat(text):
    _, rest = text.rsplit(') ', 1)
    f = rest.split()
    return {'cpu_ticks': int(f[11]) + int(f[12]), 'user_ticks': int(f[11]), 'system_ticks': int(f[12]),
            'start_ticks': int(f[19]), 'threads': int(f[17]), 'minor_faults': int(f[7]), 'major_faults': int(f[9]),
            'block_delay_ticks': int(f[39]) if len(f) > 39 else None, 'state': f[0]}


def ns_pids(path):
    for line in Path(path).read_text().splitlines():
        if line.startswith('NSpid:'):
            return list(map(int, line.split()[1:]))
    return []


def read_process(pid, proc=Path('/proc')):
    base = proc / str(pid)
    value = proc_stat((base / 'stat').read_text())
    value['host_pid'] = pid
    value['namespace_pids'] = ns_pids(base / 'status')
    fields = {}
    for line in (base / 'status').read_text().splitlines():
        key, _, text = line.partition(':')
        if key in ('VmRSS', 'VmHWM'):
            fields[key] = int(text.split()[0]) * 1024
    value['memory'] = fields
    for name in ('io', 'schedstat'):
        try:
            value[name] = counters(base / name) if name == 'io' else list(map(int, (base / name).read_text().split()))
        except (OSError, ValueError):
            value[name] = None
    return value


def resolve_worker(init, namespace_pid, proc=Path('/proc')):
    wanted = os.readlink(proc / str(init) / 'ns/pid')
    queue, seen = [init], set()
    while queue and len(seen) < 128:
        pid = queue.pop(0)
        if pid in seen:
            continue
        seen.add(pid)
        try:
            if os.readlink(proc / str(pid) / 'ns/pid') == wanted:
                ids = ns_pids(proc / str(pid) / 'status')
                if ids and ids[-1] == namespace_pid:
                    return pid
            children = (proc / str(pid) / 'task' / str(pid) / 'children').read_text().split()
            queue.extend(int(c) for c in children[:128-len(seen)])
        except (OSError, ValueError):
            continue
    raise ValueError('WORKER_NOT_FOUND_IN_CONTAINER')


def thread_cpu(pid, limit=256):
    entries = sorted(Path(f'/proc/{pid}/task').iterdir(), key=lambda p: int(p.name))
    rows = []
    for entry in entries[:limit]:
        try:
            stat = (entry / 'stat').read_text()
            row = proc_stat(stat)
            ids = ns_pids(entry / 'status')
            rows.append({'host_tid': int(entry.name), 'namespace_tid': ids[-1] if ids else None,
                         'comm': stat.split('(', 1)[1].rsplit(') ', 1)[0][:64],
                         'cpu_ticks': row['cpu_ticks'], 'start_ticks': row['start_ticks']})
        except (OSError, ValueError):
            continue
    return {'threads': rows, 'truncated': len(entries) > limit}


def read_disks(names, proc=Path('/proc')):
    result = {}
    for line in (proc / 'diskstats').read_text().splitlines():
        p = line.split()
        if len(p) >= 14 and p[2] in names:
            result[p[2]] = dict(zip(('read_ops', 'read_sectors', 'read_ms', 'write_ops', 'write_sectors', 'write_ms', 'inflight', 'io_ms', 'weighted_ms'),
                                   map(int, (p[3], p[5], p[6], p[7], p[9], p[10], p[11], p[12], p[13]))))
    return result


def disk_delta(old, new, elapsed):
    delta = {k: new[k]-old[k] for k in old if k != 'inflight'}
    if elapsed <= 0 or any(v < 0 for v in delta.values()):
        return {'error': 'DISK_COUNTER_RESET'}
    ops = delta['read_ops'] + delta['write_ops']
    return {'read_bytes_per_second': delta['read_sectors']*512/elapsed,
            'write_bytes_per_second': delta['write_sectors']*512/elapsed,
            'average_request_ms': (delta['read_ms']+delta['write_ms'])/ops if ops else None,
            'busy_percent': delta['io_ms']/(elapsed*10), 'inflight': new['inflight'],
            'scope': 'whole selected block layer; not recorder-only or per-fsync latency'}


def group_path(pid):
    for line in Path(f'/proc/{pid}/cgroup').read_text().splitlines():
        if line.startswith('0::'):
            relative = Path(line[3:])
            if '..' in relative.parts:
                raise ValueError('CGROUP_PATH')
            return Path('/sys/fs/cgroup') / str(relative).lstrip('/')
    return None


def read_group(root):
    if root is None:
        return None
    result = {}
    for name in ('cpu.stat', 'memory.stat', 'memory.events', 'io.stat'):
        try:
            if name == 'io.stat':
                result[name] = (root/name).read_text()[:16384]
            else:
                result[name] = counters(root/name)
        except (OSError, ValueError):
            result[name] = None
    for name in ('memory.current', 'memory.peak'):
        try:
            result[name] = int((root/name).read_text())
        except (OSError, ValueError):
            result[name] = None
    return result


class Trace:
    def __init__(self):
        self.identity = None
        self.epoch = -1
        self.cursor = 0
        self.lost = 0
        self.before_start = 0
        self.supported = False
        self.enabled = None

    def extract(self, value, process):
        raw = value.get('raw_recording') or {}
        if not isinstance(raw, dict):raise ValueError('RAW_STATUS_SCHEMA')
        identity = (value.get('session_id'), raw.get('session_id'), process.get('host_pid'), process.get('start_ticks'))
        if identity != self.identity:
            self.identity = identity
            self.epoch += 1
            self.cursor = 0
        disk = raw.get('disk_stages') or {}
        if not isinstance(disk, dict):raise ValueError('DISK_STATUS_SCHEMA')
        obs = disk.get('checkpoint_observation') or {}
        if not isinstance(obs, dict):raise ValueError('OBSERVATION_SCHEMA')
        self.supported = obs.get('version') == 1
        self.enabled = obs.get('enabled')
        if type(obs.get('latest_event_id')) is int and obs['latest_event_id'] < self.cursor:raise ValueError('TRACE_COUNTER_RESET')
        events = sorted((e for e in obs.get('events', []) if isinstance(e,dict) and type(e.get('event_id')) is int), key=lambda e: e['event_id'])
        fresh = [e for e in events if e['event_id'] > self.cursor]
        if fresh:
            missing = max(0, fresh[0]['event_id']-self.cursor-1)
            if self.cursor:
                self.lost += missing
            else:
                self.before_start += missing
            self.cursor = fresh[-1]['event_id']
        # Do not copy status paths, configuration, device IDs or image payloads.
        status = {k: value.get(k) for k in ('state','ready','updated_unix_ms','pairs_published','pairs_rejected','publish_failures')}
        status['raw'] = {k: raw.get(k) for k in ('state','reason','submitted_pairs','written_pairs','imu_samples_submitted')}
        status['writer_queue'] = raw.get('writer_queue')
        status['observation'] = {k: obs.get(k) for k in ('enabled','clock','anchor_monotonic_ns','anchor_unix_ns','oldest_event_id','latest_event_id','overwritten_events','active_limit_hits','active')}
        return status, fresh


class Target:
    def __init__(self, spec=None, status=None, pid=None):
        self.init = None
        self.init_start = None
        self.pid = pid
        self.start = None
        self.trace = Trace()
        self.group = None
        self.last_process = None
        self.last_time = None
        if spec:
            container, sep, path = spec.partition(':')
            if not sep or not container or not path.startswith('/') or '..' in Path(path).parts:
                raise ValueError('TARGET_EXPECTED_CONTAINER_COLON_ABSOLUTE_STATUS')
            found = subprocess.run(['docker','inspect','--format','{{.State.Pid}}',container],capture_output=True,text=True,check=True,timeout=10)
            self.init = int(found.stdout.strip())
            if self.init <= 0:
                raise ValueError('CONTAINER_NOT_RUNNING')
            self.init_start = read_process(self.init)['start_ticks']
            self.status = Path(f'/proc/{self.init}/root') / path.lstrip('/')
        else:
            self.status = Path(status)
            if not self.status.is_absolute() or not pid or pid <= 0:
                raise ValueError('HOST_STATUS_AND_PID_REQUIRED')

    def sample(self, now):
        if self.init and read_process(self.init)['start_ticks'] != self.init_start:
            raise ValueError('CONTAINER_PID_CHANGED')
        value = bounded_json(self.status)
        ns_pid = value.get('pid')
        if type(ns_pid) is not int or ns_pid <= 0:
            raise ValueError('WORKER_NOT_READY')
        if self.init and (self.pid is None or ns_pid != getattr(self, 'ns_pid', None)):
            self.pid = resolve_worker(self.init, ns_pid)
            self.start = None
            self.last_process = self.last_time = None
        self.ns_pid = ns_pid
        process = read_process(self.pid)
        if process['namespace_pids'] and process['namespace_pids'][-1] != ns_pid:
            raise ValueError('WORKER_NAMESPACE_PID_DIFFERS')
        if self.start is not None and process['start_ticks'] != self.start:
            raise ValueError('WORKER_PID_REUSED')
        self.start = process['start_ticks']
        self.group = group_path(self.pid)
        rates = None
        if self.last_process and self.last_time is not None:
            elapsed = (now-self.last_time)/1e9
            delta = process['cpu_ticks']-self.last_process['cpu_ticks']
            if delta < 0:
                raise ValueError('PROCESS_COUNTER_RESET')
            rates = {'cpu_cores': delta/os.sysconf('SC_CLK_TCK')/elapsed}
        status, events = self.trace.extract(value, process)
        self.last_process, self.last_time = process, now
        return {'process':process,'rates':rates,'status':status,'cgroup':read_group(self.group),'epoch':self.trace.epoch}, events


class Output:
    def __init__(self, root, max_bytes):
        root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.root, self.max_bytes, self.used = root, max_bytes, 0
        self.file = (root/'samples.jsonl').open('x', buffering=1)

    def emit(self, value):
        text = json.dumps(value,separators=(',',':'))+'\n'
        size = len(text.encode())
        if self.used+size > self.max_bytes:
            raise ValueError('OUTPUT_LIMIT_REACHED')
        self.file.write(text)
        self.used += size


class Profile:
    def __init__(self, args, root):
        self.args, self.root, self.process, self.error_file = args, root, None, None
        self.result = {'requested':args.perf,'state':'not_started' if args.perf else 'disabled'}

    def start(self, pids):
        binary = self.args.perf_bin or shutil.which('perf')
        if not binary:
            self.result['state'] = 'unavailable'; return
        limit = self.args.perf_max_mib*1024*1024
        def limits():
            resource.setrlimit(resource.RLIMIT_FSIZE,(limit,limit))
        self.error_file = (self.root/'perf.stderr').open('xb')
        command = [binary,'record','-e','cpu-clock','-F',str(self.args.perf_frequency),'--clockid','mono','--call-graph','dwarf,4096','-o',str(self.root/'perf.data'),'-p',','.join(map(str,pids)),'--','sleep',str(self.args.perf_seconds)]
        try:
            self.process = subprocess.Popen(command,stdout=subprocess.DEVNULL,stderr=self.error_file,start_new_session=True,preexec_fn=limits)
            self.result.update(state='running',start_monotonic_ns=time.monotonic_ns(),requested_seconds=self.args.perf_seconds,host_pids=pids,frequency=self.args.perf_frequency,clock='mono',stack_bytes=4096,max_file_bytes=limit)
        except OSError:
            self.result['state']='unavailable';self.error_file.close()

    def finish(self):
        if self.process:
            if self.process.poll() is None:
                os.killpg(self.process.pid,signal.SIGINT)  # only our new perf/sleep group
            try:self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid,signal.SIGKILL);self.process.wait()
            self.error_file.close()
            path=self.root/'perf.data';size=path.stat().st_size if path.exists() else 0
            self.result.update(finish_monotonic_ns=time.monotonic_ns(),state='recorded_not_validated' if self.process.returncode==0 and size>0 else 'failed_or_unavailable',returncode=self.process.returncode,data_bytes=size)
        return self.result


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--target',action='append',default=[],metavar='CONTAINER:/ABS/STATUS')
    ap.add_argument('--host-status',type=Path);ap.add_argument('--pid',type=int)
    ap.add_argument('--disk',action='append',default=[])
    ap.add_argument('--seconds',type=float,default=180);ap.add_argument('--interval',type=float,default=.2)
    ap.add_argument('--thread-interval',type=float,default=1.)
    ap.add_argument('--output',type=Path,required=True);ap.add_argument('--max-output-mib',type=int,default=256)
    ap.add_argument('--perf',action='store_true');ap.add_argument('--perf-bin');ap.add_argument('--perf-after',type=float,default=0)
    ap.add_argument('--perf-seconds',type=float,default=30);ap.add_argument('--perf-frequency',type=int,default=49);ap.add_argument('--perf-max-mib',type=int,default=64)
    a=ap.parse_args()
    if not (math.isfinite(a.seconds) and 0<a.seconds<=3600 and math.isfinite(a.interval) and .05<=a.interval<=10 and a.seconds/a.interval<=100000 and math.isfinite(a.thread_interval) and a.thread_interval>=a.interval and a.thread_interval<=60 and 1<=a.max_output_mib<=1024 and 1<=a.perf_frequency<=99 and math.isfinite(a.perf_seconds) and 0<a.perf_seconds<=300 and math.isfinite(a.perf_after) and 0<=a.perf_after<a.seconds and 1<=a.perf_max_mib<=256 and len(a.target)<=8 and len(a.disk)<=8):
        ap.error('invalid bounded sampling/profile window')
    if bool(a.target)==bool(a.host_status) or (a.host_status and not a.pid):ap.error('choose --target, or --host-status with --pid')
    os.umask(0o077)
    out=Output(a.output,a.max_output_mib*1024*1024)
    profile=Profile(a,a.output);errors={};count=0;interrupted=False
    started=time.monotonic_ns();cpu0=time.process_time_ns();child0=resource.getrusage(resource.RUSAGE_CHILDREN);next_threads=0;previous_disks=None;previous_disk_time=None
    stop=[False]
    def stopped(_sig,_frame):stop[0]=True
    old={s:signal.signal(s,stopped) for s in (signal.SIGINT,signal.SIGTERM)}
    targets=[]
    try:
        if sys.platform!='linux':raise ValueError('LINUX_HOST_REQUIRED')
        targets=[Target(spec=s) for s in a.target] if a.target else [Target(status=a.host_status,pid=a.pid)]
        while True:
            now=time.monotonic_ns();elapsed=(now-started)/1e9
            row={'kind':'sample','monotonic_ns':now,'unix_ns':time.time_ns(),'elapsed_seconds':elapsed,'targets':[]}
            for index,target in enumerate(targets):
                try:
                    result,events=target.sample(now)
                    if elapsed>=next_threads:
                        result['thread_cpu']=thread_cpu(target.pid)
                    row['targets'].append({'index':index,**result})
                    for event in events:out.emit({'kind':'checkpoint_event','target_index':index,'epoch':target.trace.epoch,'observed_monotonic_ns':now,**event})
                except (OSError,ValueError,KeyError,TypeError,json.JSONDecodeError) as error:
                    code=str(error) if isinstance(error,ValueError) else type(error).__name__
                    errors[code]=errors.get(code,0)+1;row['targets'].append({'index':index,'error':code})
            if elapsed>=next_threads:next_threads=elapsed+a.thread_interval
            disks=read_disks(set(a.disk));row['disks']=disks
            if previous_disks is not None:
                row['disk_rates']={k:disk_delta(previous_disks[k],v,(now-previous_disk_time)/1e9) for k,v in disks.items() if k in previous_disks}
            row['missing_disks']=[d for d in a.disk if d not in disks]
            if row['missing_disks']:errors['MISSING_REQUESTED_DISK']=errors.get('MISSING_REQUESTED_DISK',0)+1
            previous_disks,previous_disk_time=disks,now
            out.emit(row);count+=1
            if a.perf and profile.result['state']=='not_started' and elapsed>=a.perf_after:
                pids=[t['process']['host_pid'] for t in row['targets'] if 'process' in t]
                if pids:profile.start(pids)
            if elapsed>=a.seconds or stop[0]:interrupted=stop[0];break
            time.sleep(min(a.interval,max(0,a.seconds-elapsed)))
    except (OSError,ValueError,subprocess.SubprocessError) as error:
        code=str(error) if isinstance(error,ValueError) else type(error).__name__;errors[code]=errors.get(code,0)+1
    finally:
        perf=profile.finish();out.file.close()
        elapsed=(time.monotonic_ns()-started)/1e9
        children=resource.getrusage(resource.RUSAGE_CHILDREN)
        summary={'version':1,'samples':count,'elapsed_seconds':elapsed,'interrupted':interrupted,'errors':errors,'observer_cpu_seconds':(time.process_time_ns()-cpu0)/1e9,'observer_cpu_cores':(time.process_time_ns()-cpu0)/1e9/elapsed,'observer_total_process_cpu_seconds':resource.getrusage(resource.RUSAGE_SELF).ru_utime+resource.getrusage(resource.RUSAGE_SELF).ru_stime,'observer_child_cpu_seconds':children.ru_utime+children.ru_stime-child0.ru_utime-child0.ru_stime,'observer_peak_rss_kib':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,'jsonl_bytes':out.used,'perf':perf,'targets':[{'index':i,'lost_events_after_start':t.trace.lost,'history_events_before_start':t.trace.before_start,'epochs':t.trace.epoch+1,'observation_supported':t.trace.supported,'observation_enabled':t.trace.enabled} for i,t in enumerate(targets)],'scope':'read-only observations, not data/durability certification; perf may contain stack memory and paths; no automatic upload'}
        (a.output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
        for s,handler in old.items():signal.signal(s,handler)
    print(json.dumps(summary))
    return 1 if errors or any(t.trace.lost for t in targets) or (a.perf and perf['state']!='recorded_not_validated') else 0

if __name__=='__main__':raise SystemExit(main())
