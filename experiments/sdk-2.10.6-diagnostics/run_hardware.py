#!/usr/bin/env python3
"""One-command, rootful Linux three-camera experiment; never controls production.

Every phase runs under the same UID/GID. No resume, deletion or automatic retry.
Failed runs retain logs, snapshots, native partial data and a rollback command.
"""
import argparse
import json
import math
import os
from pathlib import Path
import platform
import signal
import stat
import subprocess
import sys
import time
import uuid

import experiment_runtime as rt
import hardware as hw

HERE = Path(__file__).resolve().parent
LOG_LIMIT = 64 * 1024**2


class Cancelled(Exception):
    pass


def atomic(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    with temporary.open('w') as out:
        json.dump(value, out, sort_keys=True, indent=2)
        out.write('\n');out.flush();os.fsync(out.fileno())
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:os.fsync(fd)
    finally:os.close(fd)


def process_identity(pid):
    try:
        root = Path('/proc')/str(pid)
        fields = (root/'stat').read_text().rsplit(') ', 1)[1].split()
        return {'pid':pid, 'uid':root.stat().st_uid, 'pgid':int(fields[2]),
                'session':int(fields[3]), 'start_ticks':int(fields[19])}
    except FileNotFoundError:return None


def stop_saved_children(root, token):
    path = root/'children.json'
    if not path.exists():return []
    saved = rt.read_json(path)
    rt.require(saved['uid'] == os.getuid() and saved['token'] == token, 'CHILD_OWNER_MISMATCH')
    errors = []
    for expected in saved['processes']:
        try:
            pid = expected['pid'];rt.require(type(pid) is int and pid > 1, 'CHILD_PID')
            current = process_identity(pid)
            if current is None:continue
            rt.require(current == {k:expected[k] for k in ('pid','uid','pgid','session','start_ticks')}
                       and current['uid'] == os.getuid() and current['pgid'] == current['session'] == pid, 'CHILD_PID_REUSED_OR_CHANGED')
            environment = (Path('/proc')/str(pid)/'environ').read_bytes().split(b'\0')
            rt.require(('GDP_SDK_RUN_TOKEN='+token).encode() in environment, 'CHILD_RUN_TOKEN_CHANGED')
            os.killpg(pid, signal.SIGTERM)
            deadline = time.monotonic()+20
            while time.monotonic() < deadline:
                now = process_identity(pid)
                if now is None or now != current:break
                # Zombies have exited and have no executable children to signal.
                if (Path('/proc')/str(pid)/'stat').read_text().rsplit(') ',1)[1].startswith('Z '):break
                time.sleep(.1)
            else:
                rt.require(process_identity(pid) == current, 'CHILD_CHANGED_BEFORE_KILL')
                os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:continue
        except Exception as error:errors.append(str(error))
    return errors


def local_rootful(require_compose=True):
    rt.require(sys.platform.startswith('linux'), 'LINUX_HOST_REQUIRED')
    rt.require(os.getuid() == os.geteuid() == 0 and os.getgid() == os.getegid(), 'RUN_ALL_PHASES_WITH_SUDO_ROOT_UID')
    context = json.loads(hw.command(['docker', 'context', 'inspect']))[0]
    endpoint = context['Endpoints']['docker']['Host'] if os.environ.get('DOCKER_CONTEXT') else os.environ.get('DOCKER_HOST') or context['Endpoints']['docker']['Host']
    rt.require(endpoint.startswith('unix://') and not os.environ.get('DOCKER_CONTEXT', '').startswith('ssh:'), 'LOCAL_DOCKER_REQUIRED')
    options = json.loads(hw.command(['docker', 'info', '--format', '{{json .SecurityOptions}}'])) or []
    rt.require(not any('rootless' in str(v) for v in options), 'ROOTFUL_DOCKER_REQUIRED')
    if require_compose:hw.command(['docker', 'compose', 'version'])


def settings(a):
    rt.require(1 <= a.seconds <= 86400 and 30 <= a.healthy_timeout <= 3600
               and 1 <= a.poll_seconds <= 10 and 0 <= a.trim_seconds <= 60
               and a.seconds > 2*a.trim_seconds, 'TIMING_LIMITS')
    rt.require(1 <= a.ros_domain_id <= 232 and a.max_bytes > 0, 'RECORDING_LIMITS')
    plan = rt.read_json(a.plan)
    rt.require(len(plan['cameras']) == 3, 'THREE_CAMERAS_REQUIRED')
    rates = []
    for camera in plan['cameras']:
        config = rt.read_json(camera['capture_config'])
        rates += [config[s]['fps'] for s in ('color', 'depth')]
    rt.require(all(type(v) in (int, float) and math.isfinite(v) and 0 < v <= 1000 for v in rates), 'SOURCE_FPS')
    # Native capture begins before Docker health becomes healthy. Budget that
    # entire wait plus sampling and a tail, with 10% source-rate headroom.
    minimum = math.ceil(max(rates) * 1.1 * (a.healthy_timeout + a.seconds + 60))
    if a.pairs is None:a.pairs = minimum
    rt.require(a.pairs >= minimum, 'PAIR_TARGET_TOO_SHORT_MINIMUM:' + str(minimum))
    if a.completion_timeout is None:a.completion_timeout = math.ceil(2*a.pairs/min(rates)+120)
    rt.require(1 <= a.completion_timeout <= 86400 and 1 <= a.audit_timeout <= 86400
               and 1 <= a.build_timeout <= 86400, 'DEADLINE_LIMITS')
    return a


def check_rows(a, run, rows, sampling=False):
    rt.require(len(rows['cameras']) == 3, 'THREE_STATUS_ROWS_REQUIRED')
    ready = complete = 0
    for c, row in zip(run['cameras'], rows['cameras']):
        rt.require(row['name'] == c['name'] and row['running'], 'CAPTURE_EXITED:' + c['name'])
        s = row.get('capture')
        if s is None:
            rt.require(not sampling, 'MISSING_CAPTURE_DURING_SAMPLE')
            continue
        hw.recording_path(a, c, s, False)  # generation, instance, config hash and path binding
        rt.require(s['state'] not in ('failed', 'stopped') and s.get('restart_attempt', 0) == 0, 'CAPTURE_FAILED:' + c['name'])
        raw = s['raw_recording']
        rt.require(raw['state'] != 'failed' and raw['whole_pair_queue_overflows'] == 0, 'RECORDING_FAILED:' + c['name'])
        for key in ('stamp_failures', 'publish_failures', 'monotonic_rejections'):
            rt.require(s.get(key, 0) == 0, key + ':' + c['name'])
        rt.require((s.get('status_persistence') or {}).get('errors', 0) == 0, 'STATUS_PERSISTENCE_ERROR')
        imu = s.get('imu') or {}
        rt.require(imu.get('error') is None, 'IMU_FAILED:' + c['name'])
        rt.require(0 <= time.time_ns()//10**6-s['updated_unix_ms'] <= 10000, 'STALE_CAPTURE_STATUS:' + c['name'])
        if s.get('ready'):
            rt.require(s['message_timestamp'] == 'global' and imu.get('shared_ros_context') is True
                       and imu.get('ready') is True, 'GLOBAL_IMU_CONTEXT_REQUIRED')
        if row['health'] == 'unhealthy':raise ValueError('CONTAINER_UNHEALTHY:' + c['name'])
        if row['health'] == 'healthy' and s.get('ready') and raw['state'] == 'recording':ready += 1
        if raw['state'] == 'complete':
            rt.require(row['health'] == 'healthy' and s.get('ready'), 'UNHEALTHY_AT_RECORDING_COMPLETION')
            rt.require(raw['written_pairs'] == raw['max_pairs'] == a.pairs, 'PAIR_TARGET_MISMATCH')
            complete += 1
        if sampling:rt.require(raw['state'] == 'recording' and row['health'] == 'healthy' and s.get('ready'), 'RECORDING_ENDED_OR_UNHEALTHY_DURING_SAMPLE')
    return ready == 3, complete == 3


class Runner:
    def __init__(self, a):
        self.a = a;self.root = a.root;self.run = None;self.children = [];self.logs = []
        self.observer = None;self.stage = 'preflight';self.seq = 0;self.child_bindings = []
        self.token = uuid.uuid4().hex
        self.result = {'version':1, 'experiment':rt.EXPERIMENT, 'ok':False, 'uid':os.getuid(), 'gid':os.getgid(),
                       'started_unix_ns':time.time_ns(), 'stage':self.stage, 'run_token':self.token,
                       'rollback_argv':[sys.executable, str(HERE/'run_hardware.py'), 'rollback', '--root', str(a.root)],
                       'production_changed':False, 'hardware_acceptance_verified':False}

    def save(self, **values):
        self.result.update(values);atomic(self.root/'result.json', self.result)

    def phase(self, name):
        self.stage = name;self.save(stage=name)
        with (self.root/'events.jsonl').open('a') as out:
            out.write(json.dumps({'stage':name, 'unix_ns':time.time_ns()})+'\n');out.flush();os.fsync(out.fileno())
        print(json.dumps({'stage':name, 'root':str(self.root)}), flush=True)

    def spawn(self, args, name):
        path = self.root/(name+'.log');log = path.open('x');self.logs.append(log)
        child = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
                                 env=dict(os.environ, GDP_SDK_RUN_TOKEN=self.token))
        self.children.append((child, path))
        if sys.platform.startswith('linux'):
            identity = process_identity(child.pid)
            rt.require(identity is not None and identity['pgid'] == identity['session'] == child.pid
                       and identity['uid'] == os.getuid(), 'CHILD_PROCESS_IDENTITY')
            self.child_bindings.append(dict(identity, name=name))
            atomic(self.root/'children.json', {'version':1,'uid':os.getuid(),'token':self.token,'processes':self.child_bindings})
        return child

    def bounded(self, child, path):
        rt.require(path.stat().st_size <= LOG_LIMIT, 'LOG_LIMIT:'+path.name)
        return child.poll()

    def terminate(self, child):
        if child.poll() is not None:return
        try:
            os.killpg(child.pid, signal.SIGTERM)
            try:child.wait(timeout=20)
            except subprocess.TimeoutExpired:os.killpg(child.pid, signal.SIGKILL);child.wait(timeout=5)
        except ProcessLookupError:child.wait(timeout=5)

    def command(self, args, name, timeout):
        child = self.spawn(args, name);path = self.children[-1][1];log = self.logs[-1]
        deadline = time.monotonic()+timeout
        try:
            while self.bounded(child, path) is None:
                rt.require(time.monotonic() < deadline, 'COMMAND_TIMEOUT:'+name)
                time.sleep(min(1, self.a.poll_seconds))
            rt.require(child.returncode == 0, 'COMMAND_FAILED:'+name+':'+str(child.returncode))
        finally:
            self.terminate(child);log.close();self.logs.remove(log)

    def snapshot(self, label):
        value = hw.status(self.run)
        atomic(self.root/(label+'.json'), value)
        return value

    def wait_for(self, completion=False):
        deadline = time.monotonic() + (self.a.completion_timeout if completion else self.a.healthy_timeout)
        while True:
            rows = self.snapshot('latest-status')
            ready, complete = check_rows(self.a, self.run, rows)
            if complete and not completion:raise ValueError('RECORDING_COMPLETED_BEFORE_SAMPLE')
            if (complete if completion else ready):return rows
            rt.require(time.monotonic() < deadline, 'RECORDING_TIMEOUT' if completion else 'HEALTH_TIMEOUT')
            time.sleep(self.a.poll_seconds)

    def sample(self):
        argv = hw.sample_commands(self.a, self.run)
        if self.run['mode'] == 'off':argv.pop('sdk_argv')
        if self.a.perf:argv['checkpoint_argv'] += ['--perf', '--perf-seconds', '30', '--perf-frequency', '49']
        for disk in self.a.disk:argv['checkpoint_argv'] += ['--disk', disk]
        self.observer = 'gdp-sdkdiag-'+self.run['run_id'][:12]+'-observer'
        argv['ros_observer_argv'][2:2] = ['--init', '--name', self.observer,
                                         '--label', hw.LABEL+'='+rt.EXPERIMENT, '--label', hw.RUN+'='+self.run['run_id']]
        atomic(self.root/'sampling-commands.json', argv)
        jobs = [(name, self.spawn(args, name)) for name, args in argv.items()]
        deadline = time.monotonic()+self.a.seconds+120
        while True:
            check_rows(self.a, self.run, self.snapshot('latest-status'), sampling=True)
            for name, child in jobs:
                rc = self.bounded(child, self.root/(name+'.log'))
                rt.require(rc is None or rc == 0, 'SAMPLER_FAILED:'+name+':'+str(rc))
            if all(child.poll() is not None for _, child in jobs):break
            rt.require(time.monotonic() < deadline, 'SAMPLING_TIMEOUT')
            time.sleep(self.a.poll_seconds)
        # Exit 0 alone is insufficient: require complete, error-free evidence.
        checkpoint = rt.read_json(self.a.run_dir/'checkpoint-samples/summary.json')
        rt.require(not checkpoint['errors'] and not checkpoint.get('interrupted')
                   and checkpoint['elapsed_seconds'] >= self.a.seconds, 'CHECKPOINT_SAMPLE_INCOMPLETE')
        targets = checkpoint.get('targets', [])
        rt.require(len(targets) == 3 and all(t.get('observation_supported') is True
                   and t.get('observation_enabled') is True and t.get('lost_events_after_start') == 0
                   for t in targets), 'CHECKPOINT_OBSERVATION_INCOMPLETE')
        if self.a.perf:rt.require(checkpoint['perf'].get('state') == 'recorded_not_validated' and checkpoint['perf'].get('data_bytes', 0) > 0, 'PERF_PROFILE_INCOMPLETE')
        if self.run['mode'] == 'on':
            sdk = rt.read_json(self.a.run_dir/'sdk-samples/summary.json')
            rt.require(sdk.get('ok') is True and sdk['elapsed_seconds'] >= self.a.seconds, 'SDK_SAMPLE_INCOMPLETE')
        wire = rt.read_json(self.a.run_dir/'wire.sqlite3.summary.json')
        rt.require(wire.get('ok') is True and wire['elapsed_seconds'] >= self.a.seconds, 'DDS_SAMPLE_INCOMPLETE')
        atomic(self.root/'sampling.json', {'ok':True, 'commands':argv, 'seconds':self.a.seconds})

    def acceptance(self):
        report = rt.read_json(self.a.run_dir/'dds-native-comparison.json')
        cameras = report.get('cameras', [])
        rt.require(report.get('ok') is True and len(cameras) == 3
                   and {c['camera'] for c in cameras} == {c['name'] for c in self.run['cameras']}, 'DDS_ACCEPTANCE_REPORT_REQUIRED')
        for camera in cameras:
            summary = camera['summary']
            rt.require(summary.get('ok') is True and summary.get('audit_version') == 2
                       and summary.get('native_hashes_verified') is True
                       and summary.get('independent_audit_passes') == 2 and summary.get('recovered') is False
                       and summary.get('pairs') == self.a.pairs, 'NORMAL_TWO_READ_ACCEPTANCE_REQUIRED')
        self.result.update(source_gap_free=all(c['zero_source_gaps'] for c in cameras),
                           source_sequence_gaps={c['camera']:c['sequence_gaps'] for c in cameras},
                           acceptance_scope='normal recording, two native readers, hashes, Global/IMU and DDS matching; source gaps reported separately',
                           crash_recovery_verified=False)

    def evidence(self):
        if self.run is None:return
        try:self.snapshot('final-status')
        except Exception as error:self.result.setdefault('evidence_errors', []).append('status:'+str(error))
        for camera in self.run['cameras']:
            try:
                info = hw.own_container(self.run, camera)
                self.command(['docker', 'logs', '--tail', '2000', info['Id']], 'camera-'+camera['name']+'-'+str(self.seq), 20)
            except Exception as error:self.result.setdefault('evidence_errors', []).append(camera['name']+':'+str(error))
            destination = self.root/('sdk-final-'+camera['name']+'-'+str(self.seq));destination.mkdir(mode=0o700)
            try:
                paths = list(Path(camera['diagnostics']).glob('sdk-diag-*.json'))
                rt.require(len(paths) <= 256, 'SDK_EVIDENCE_FILE_LIMIT')
                for path in paths:
                    fd = os.open(path, os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
                    try:
                        info = os.fstat(fd)
                        rt.require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                                   and not info.st_mode & 0o077 and info.st_size <= 65536, 'PRIVATE_SDK_EVIDENCE')
                        data = os.read(fd, 65537);rt.require(len(data) <= 65536, 'SDK_EVIDENCE_LIMIT')
                        (destination/path.name).write_bytes(data)
                    finally:os.close(fd)
            except Exception as error:self.result.setdefault('evidence_errors', []).append('sdk:'+str(error))
        self.seq += 1

    def cleanup(self):
        errors = []
        # Address the owned Docker observer separately: killing its client alone
        # does not guarantee that a container has stopped.
        if self.observer and self.run:
            try:
                names = hw.command(['docker', 'ps', '-a', '--format', '{{.Names}}']).splitlines()
                if self.observer in names:
                    info = hw.own_container(self.run, {'container':self.observer})
                    hw.command(['docker', 'stop', '--time', '10', info['Id']], timeout=30)
            except Exception as error:errors.append('observer:'+str(error))
        for child, _ in self.children:
            try:self.terminate(child)
            except Exception as error:errors.append('child:'+str(error))
        for log in self.logs:log.close()
        self.logs = []
        if self.run:
            try:self.result['rollback'] = hw.stop(self.run)
            except Exception as error:errors.append('capture:'+str(error))
        self.result['cleanup_errors'] = errors
        return not errors

    def execute(self):
        try:
            if self.a.build:
                self.phase('build')
                self.command(['bash', str(HERE/'build_runtime.sh'), self.a.image, self.a.platform], 'build', self.a.build_timeout)
            self.phase('prepare');hw.prepare(self.a)
            self.run = hw.load_run(self.a.run_dir)
            self.save(run_id=self.run['run_id'], image_id=self.run['image_id'], runtime_sha256=self.run['runtime_sha256'])
            self.phase('start');hw.start(self.a, self.run)
            self.phase('wait-healthy');self.wait_for()
            self.phase('sample');self.sample()
            self.phase('wait-complete');self.wait_for(completion=True)
            self.phase('stop');self.evidence();hw.stop(self.run)
            self.phase('audit')
            self.command([sys.executable, str(HERE/'hardware.py'), 'audit', '--run-dir', str(self.a.run_dir)], 'audit', self.a.audit_timeout)
            self.phase('verify-dds')
            self.command([sys.executable, str(HERE/'hardware.py'), 'verify-dds', '--run-dir', str(self.a.run_dir),
                          '--trim-seconds', str(self.a.trim_seconds)], 'verify-dds', self.a.audit_timeout)
            self.acceptance()
            self.result.update(ok=True, hardware_acceptance_verified=True)
        except Exception as error:
            self.save(ok=False, failed_stage=self.stage, error=type(error).__name__+':'+str(error))
        finally:
            # A second Ctrl-C must not interrupt identity-checked rollback.
            for sig in (signal.SIGINT, signal.SIGTERM):signal.signal(sig, signal.SIG_IGN)
            if self.run is None and (self.a.run_dir/'run.json').is_file():
                try:self.run = hw.load_run(self.a.run_dir, verify=False)
                except Exception as error:self.result.setdefault('evidence_errors', []).append(str(error))
            cleaned = self.cleanup()
            try:self.evidence()
            except Exception as error:self.result.setdefault('evidence_errors', []).append(str(error))
            if not cleaned:self.result.update(ok=False, hardware_acceptance_verified=False)
            self.save(finished_unix_ns=time.time_ns(), stage='complete' if self.result['ok'] else 'failed')
        return 0 if self.result['ok'] else 1


def rollback(a):
    result = rt.read_json(a.root/'result.json')
    rt.require(result['uid'] == os.getuid() and result['gid'] == os.getgid(), 'RUN_UID_GID_MISMATCH')
    if not (a.root/'capture/run.json').is_file():
        rt.require(result.get('failed_stage', result.get('stage')) in ('preflight','build','prepare'), 'RUN_IDENTITY_MISSING_REVIEW_REQUIRED')
        errors = stop_saved_children(a.root, result.get('run_token',''))
        stopped = {'ok':not errors, 'stopped':[], 'reason':'capture-not-started', 'recordings_retained':True, 'child_cleanup_errors':errors}
        atomic(a.root/('rollback-'+str(time.time_ns())+'.json'), stopped)
        return stopped
    run = hw.load_run(a.root/'capture', verify=False)
    rt.require(result.get('run_id', run['run_id']) == run['run_id'], 'ROLLBACK_RUN_MISMATCH')
    observer = 'gdp-sdkdiag-'+run['run_id'][:12]+'-observer'
    names = hw.command(['docker', 'ps', '-a', '--format', '{{.Names}}']).splitlines()
    if observer in names:
        info = hw.own_container(run, {'container':observer})
        hw.command(['docker', 'stop', '--time', '10', info['Id']], timeout=30)
    errors = stop_saved_children(a.root, result.get('run_token',''))
    stopped = hw.stop(run);stopped['child_cleanup_errors'] = errors
    stopped['ok'] = stopped.get('ok', True) and not errors
    atomic(a.root/('rollback-'+str(time.time_ns())+'.json'), stopped)
    return stopped


def parser():
    p = argparse.ArgumentParser(description=__doc__);sub = p.add_subparsers(dest='action', required=True)
    s = sub.add_parser('run');s.add_argument('--plan', type=Path, required=True);s.add_argument('--image', required=True)
    s.add_argument('--mode', choices=('on', 'off'), default='on');s.add_argument('--build', action='store_true')
    s.add_argument('--platform', choices=('linux/amd64', 'linux/arm64'), default='linux/arm64' if platform.machine() in ('arm64','aarch64') else 'linux/amd64')
    s.add_argument('--seconds', type=int, default=600);s.add_argument('--pairs', type=int)
    s.add_argument('--max-bytes', type=int, required=True);s.add_argument('--ros-domain-id', type=int, required=True)
    s.add_argument('--diagnostics-root', type=Path);s.add_argument('--healthy-timeout', type=int, default=300)
    s.add_argument('--completion-timeout', type=int);s.add_argument('--audit-timeout', type=int, default=3600)
    s.add_argument('--build-timeout', type=int, default=3600);s.add_argument('--poll-seconds', type=float, default=5)
    s.add_argument('--trim-seconds', type=float, default=5);s.add_argument('--disk', action='append', default=[])
    s.add_argument('--perf', action='store_true');s.add_argument('--execute', action='store_true')
    sub.add_parser('rollback')
    for s in sub.choices.values():s.add_argument('--root', type=Path, required=True)
    return p


def main():
    os.umask(0o077);a = parser().parse_args();a.root = a.root.absolute()
    try:
        local_rootful(require_compose=a.action != 'rollback')
        if a.action == 'rollback':
            result = rollback(a);print(json.dumps(result, indent=2));return 0 if result['ok'] else 1
        rt.require(a.execute, 'EXPLICIT_EXECUTE_REQUIRED')
        a = settings(a)
        rt.require(not a.root.exists(), 'FRESH_ROOT_REQUIRED')
        a.root.mkdir(mode=0o700, parents=True);a.run_dir = a.root/'capture'
        a.diagnostics_root = (a.diagnostics_root or Path('/dev/shm')/('gdp-sdk-'+uuid.uuid4().hex)).absolute()
        atomic(a.root/'settings.json', {k:str(v) if isinstance(v,Path) else v for k,v in vars(a).items()})
        runner = Runner(a);runner.save()
        def cancelled(sig, _frame):raise Cancelled('SIGNAL:'+str(sig))
        previous = {sig:signal.signal(sig, cancelled) for sig in (signal.SIGINT,signal.SIGTERM)}
        try:code = runner.execute()
        finally:
            for sig, handler in previous.items():signal.signal(sig, handler)
        print(json.dumps(runner.result, indent=2));return code
    except Exception as error:
        print(json.dumps({'ok':False, 'code':'SDK_ONE_CLICK_FAILED', 'detail':str(error)}), file=sys.stderr);return 1

if __name__ == '__main__':raise SystemExit(main())
