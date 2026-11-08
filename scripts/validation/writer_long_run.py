#!/usr/bin/env python3
"""Long three-writer same-filesystem status-off/status-on isolation experiment.

The capture-probe libtest adapter reuses the common offline-depth driver and
runs production ServiceQuality/Persistence code in each writer process. This
is a storage experiment, not camera/SDK/USB/ROS or hardware acceptance.
"""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import resource
import signal
import subprocess
import sys
import time
import threading


def sibling(name):
    spec = importlib.util.spec_from_file_location('gdp_long_' + name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ab = sibling('writer_contention_ab')
TEST_NAME = 'service::writer_contention::writer_contention_with_status_child'
WRITERS = 3
MIB = 1024 ** 2
GIB = 1024 ** 3
FREE_SPACE_RESERVE = 2 * GIB
LIMITS = (
    'Three modeled nonblocking FIFO writers, real production checkpoint/readback and status persistence; '
    'no SDK/USB/ROS or physical camera acceptance. Status-off disables only the test adapter background. '
    'No queue, heartbeat, uncommitted credit or durability policy tuning. '
    'CPU includes synthetic input generation, hashing and independent readers; simultaneous RSS is sampled. '
    'Tracing is bounded and scoped to owned worker identities; unavailable/lost tracing is not complete evidence. '
    'SIGKILL recovery is a controlled process crash after a confirmed checkpoint, not physical power loss. '
    'A single paired repetition establishes no repeatability or statistical significance. '
    'Aggregate capacity watchdog is sampled, not a filesystem quota; any measured excess is reported. '
    'Keep original failure directories immutable and use a fresh output root.'
)


def source_hashes():
    repo = Path(__file__).resolve().parents[2]
    return {
        'common_driver_sha256': ab.sha256(repo / 'crates/offline-depth/tests/writer_contention_benchmark.rs'),
        'status_driver_sha256': ab.sha256(repo / 'crates/capture-probe/tests/support/writer_contention.rs'),
        'fixture_sha256': ab.sha256(repo / 'crates/offline-depth/tests/support/fixture.rs'),
    }


def storage_guard(args, deadline):
    ab.guard_budget(args.output, args.max_output_bytes, deadline)
    for path in (args.output, getattr(args,'backing_path',None)):
        if path is None: continue
        fs = os.statvfs(path)
        available = fs.f_bavail * fs.f_frsize
        if available < FREE_SPACE_RESERVE:
            raise RuntimeError('FREE_SPACE_RESERVE:' + str(path) + ':' + str(available))


def plan(args):
    if sys.platform != 'linux':
        raise ValueError('LINUX_REQUIRED')
    if type(args.seconds) is not int or not 1 <= args.seconds <= 3600:
        raise ValueError('SECONDS_REQUIRED_1_TO_3600')
    if args.seconds < 600 and not args.smoke:
        raise ValueError('LONG_RUN_REQUIRES_AT_LEAST_600_SECONDS')
    if type(args.repeats) is not int or not 1 <= args.repeats <= 10:
        raise ValueError('REPEATS_REQUIRED_1_TO_10')
    if not 1 <= args.max_wall_seconds <= 86400:
        raise ValueError('BOUNDED_WALL_TIME_REQUIRED')
    if not 128 * MIB <= args.max_output_bytes <= 1024 * GIB:
        raise ValueError('BOUNDED_OUTPUT_CAPACITY_REQUIRED')
    if not MIB <= args.trace_max_bytes <= 8 * GIB or not 4 <= args.trace_buffer_kib <= 16384:
        raise ValueError('BOUNDED_TRACE_CAPACITY_REQUIRED')
    if args.disk is not None and not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}', args.disk):
        raise ValueError('WHOLE_DEVICE_NAME_REQUIRED')
    if getattr(args,'storage_trace',False):
        if not args.disk:raise ValueError('STORAGE_WHOLE_DISK_REQUIRED')
        if args.allow_unavailable_trace:raise ValueError('STORAGE_TRACE_MUST_BE_REQUIRED')
        if not MIB<=args.storage_trace_max_bytes<=8*GIB:raise ValueError('STORAGE_TRACE_OUTPUT_BOUND')
    pairs = args.seconds * 30
    per_writer = pairs * (ab.PAIR_BYTES + 32768) + 16 * MIB
    # Replaced status files are bounded individually by the driver; reserve
    # live metadata, resource logs, per-group trace and trace summary/shas.
    group_overhead = max(8 * MIB, args.seconds * 65536) + 3 * 96 * MIB + args.trace_max_bytes + MIB
    if getattr(args,'storage_trace',False):group_overhead += args.storage_trace_max_bytes + MIB
    recovery = 32 * (ab.PAIR_BYTES + 32768) + 16 * MIB + 16 * MIB
    estimate = args.repeats * 2 * (WRITERS * per_writer + group_overhead) + 2 * recovery + 64 * MIB
    if estimate > args.max_output_bytes:
        raise ValueError('PLANNED_CAPACITY_EXCEEDS_LIMIT:' + str(estimate))
    # Full-capture wall time is explicit; readback/cleanup time must also fit.
    if args.repeats * 2 * args.seconds >= args.max_wall_seconds:
        raise ValueError('WALL_BUDGET_LEAVES_NO_READBACK_OR_CLEANUP_TIME')
    return {'pairs_per_writer': pairs, 'planned_groups': args.repeats * 2,
            'writers_per_group': WRITERS, 'planned_upper_bound_bytes': estimate,
            'nominal_capture_seconds_all_groups': args.repeats * 2 * args.seconds,
            'trace_max_bytes_per_group': args.trace_max_bytes,
            'storage_trace_max_bytes_per_group':args.storage_trace_max_bytes if getattr(args,'storage_trace',False) else 0,
            'resource_and_status_reserve_per_group': group_overhead - args.trace_max_bytes - (args.storage_trace_max_bytes if getattr(args,'storage_trace',False) else 0)}


def launch(binary, root, pairs, mode, status_on, barrier=None, checkpoint_batching='full-credit'):
    env = ab.child_environment(root, pairs, mode, barrier)
    env['GDP_CONTENTION_STATUS_LOAD'] = '1' if status_on else '0'
    env['GDP_CONTENTION_BATCH_POLICY'] = checkpoint_batching
    file_limit = pairs * 32768 + 16 * MIB
    def limits():
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, file_limit))
    with (root / (mode + '.log')).open('xb') as log:
        return subprocess.Popen([str(binary), '--exact', TEST_NAME, '--ignored', '--nocapture', '--test-threads=1'],
            env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, preexec_fn=limits)


def status_contract(row, status_on, minimum_periodic=1):
    if row.get('status_background_load') is not status_on:
        raise ValueError('STATUS_LOAD_MODE_CONTRACT')
    status = row.get('status_persistence')
    if not status_on:
        if status is not None:
            raise ValueError('STATUS_OFF_MUST_HAVE_NO_PERSISTENCE')
        return
    if type(status) is not dict:
        raise ValueError('REAL_STATUS_PERSISTENCE_REQUIRED')
    for key in ('errors', 'submission_errors', 'pump_errors'):
        if type(status.get(key)) is not int or status[key] != 0:
            raise ValueError('STATUS_PERSISTENCE_ERROR:' + key)
    for key in ('start_critical_ack', 'final_critical_ack'):
        if status.get(key) is not True:
            raise ValueError('STATUS_CRITICAL_ACK_REQUIRED:' + key)
    if (type(status.get('submitted_sequence')) is not int or status['submitted_sequence'] <= 0
            or type(status.get('completed_sequence')) is not int
            or status['completed_sequence'] != status['submitted_sequence']
            or type(status.get('pending_snapshots')) is not int or status['pending_snapshots'] != 0
            or status.get('protected_sequence') is not None):
        raise ValueError('STATUS_DURABLE_FINAL_FENCE_REQUIRED')
    if (type(status.get('periodic_submissions')) is not int
            or status['periodic_submissions'] < minimum_periodic
            or type(status.get('heartbeat_count')) is not int
            or status['heartbeat_count'] < status['periodic_submissions']):
        raise ValueError('STATUS_PERIODIC_BACKGROUND_REQUIRED')
    gap = status.get('heartbeat_max_gap_ns')
    if type(gap) is not int or gap < 0:
        raise ValueError('STATUS_HEARTBEAT_GAP_REQUIRED')
    if gap>10_000_000_000:raise ValueError('STATUS_HEARTBEAT_DEADLINE')


def validate_result(row, pairs, status_on, hashes, seconds):
    # Reuse the original real-Rust disk/readback/hash contract without changing
    # its writer-only workload assertion or mutating the actual child report.
    writer_only = dict(row)
    writer_only['status_background_load'] = False
    fingerprint = ab.validate_result(writer_only, pairs)
    status_contract(row, status_on, minimum_periodic=max(1, seconds))
    for key, expected in hashes.items():
        if row.get(key) != expected:
            raise ValueError('COMMON_DRIVER_SOURCE_IDENTITY:' + key)
    capture_ns = row.get('capture_elapsed_ns')
    if type(capture_ns) is not int or capture_ns < seconds * 1_000_000_000:
        raise ValueError('CAPTURE_DURATION_SHORTER_THAN_REQUESTED')
    for key in ('capture_cpu_seconds','capture_cpu_cores'):
        number = row.get(key)
        if type(number) not in (int,float) or not math.isfinite(number) or number<0:
            raise ValueError('CAPTURE_CPU_CONTRACT:' + key)
    if row.get('checkpoint_pipeline') is not True or row.get('credit_pairs') != 32 or row.get('credit_bytes') != 16 * MIB:
        raise ValueError('CHECKPOINT_POLICY_CONTRACT')
    return fingerprint


def validate_child_files(root, row, status_on, expected_dev):
    sample = row.get('background_sampling')
    if (type(sample) is not dict or sample.get('ok') is not True
            or sample.get('relative_path') != 'status-load-samples.jsonl'):
        raise ValueError('LIVE_BACKGROUND_SAMPLING_REQUIRED')
    for key in ('rows', 'bytes', 'max_rows', 'max_bytes'):
        if type(sample.get(key)) is not int or sample[key] < 0:
            raise ValueError('BACKGROUND_SAMPLE_LIMIT_SCHEMA:' + key)
    if not 0 < sample['rows'] <= sample['max_rows'] <= 20000 or not 0 < sample['bytes'] <= sample['max_bytes'] <= 64 * MIB:
        raise ValueError('BACKGROUND_SAMPLE_LIMIT_CONTRACT')
    path = root / sample['relative_path']
    if path.is_symlink() or not path.is_file() or path.stat().st_size != sample['bytes']:
        raise ValueError('BACKGROUND_SAMPLE_FILE_CONTRACT')
    with path.open('rb') as source:
        rows = sum(1 for _ in source)
    if rows != sample['rows']:
        raise ValueError('BACKGROUND_SAMPLE_ROW_COUNT')
    validate_event_archive(root, sample.get('checkpoint_events'))
    state = root / 'state'
    if status_on:
        reported = (row.get('status_persistence') or {}).get('state_directory')
        if not isinstance(reported, str) or Path(reported) != state or not state.is_dir() or state.is_symlink():
            raise ValueError('STATUS_DIRECTORY_CONTRACT')
        if state.stat().st_dev != expected_dev:
            raise ValueError('STATUS_AND_RECORDING_FILESYSTEM_DIFFER')
    elif state.exists() or state.is_symlink():
        raise ValueError('STATUS_OFF_CREATED_PRODUCTION_STATE')
    for path in (root, root/'capture', root/sample['relative_path']):
        if path.stat().st_dev != expected_dev:
            raise ValueError('STATUS_AND_RECORDING_FILESYSTEM_DIFFER')


def validate_event_archive(root, proof):
    if (type(proof) is not dict or proof.get('ok') is not True
            or proof.get('relative_path') != 'checkpoint-events.jsonl'
            or type(proof.get('rows')) is not int or proof['rows'] <= 0
            or type(proof.get('bytes')) is not int or type(proof.get('max_bytes')) is not int
            or not 0 < proof['bytes'] <= proof['max_bytes'] <= 16 * MIB):
        raise ValueError('CHECKPOINT_EVENT_ARCHIVE_CONTRACT')
    path = root / proof['relative_path']
    if path.is_symlink() or not path.is_file() or path.stat().st_size != proof['bytes']:
        raise ValueError('CHECKPOINT_EVENT_ARCHIVE_FILE')
    seen = 0
    with path.open('rb') as source:
        while raw := source.readline(65537):
            if len(raw) > 65536 or not raw.endswith(b'\n'):
                raise ValueError('CHECKPOINT_EVENT_ARCHIVE_LINE_LIMIT')
            event = json.loads(raw)
            if type(event) is not dict or type(event.get('event_id')) is not int or event['event_id'] != seen + 1:
                raise ValueError('CHECKPOINT_EVENT_ARCHIVE_GAP')
            seen += 1
    if seen != proof['rows']:
        raise ValueError('CHECKPOINT_EVENT_ARCHIVE_TAIL')


class StatusMonitor:
    """Bounded private status reads; ten-second production freshness unchanged."""
    def __init__(self,root,pid):
        self.path=root/'state/status.json';self.pid=pid;self.previous=None
    def sample(self,now_ms=None,require_fresh=True):
        now_ms=time.time_ns()//1_000_000 if now_ms is None else now_ms
        with self.path.open('rb') as source:raw=source.read(2*MIB+1)
        if len(raw)>2*MIB:raise ValueError('STATUS_FILE_BYTE_LIMIT')
        value=json.loads(raw)
        if (type(value) is not dict or type(value.get('version')) is not int or value['version']!=1
                or value.get('kind')!='capture-service-status' or type(value.get('pid')) is not int
                or value['pid']!=self.pid):raise ValueError('STATUS_IDENTITY_SCHEMA')
        session=value.get('session_id');sequence=value.get('heartbeat_sequence');updated=value.get('updated_unix_ms')
        if not isinstance(session,str) or re.fullmatch('[a-f0-9]{32}',session) is None:
            raise ValueError('STATUS_SESSION_IDENTITY')
        if type(sequence) is not int or sequence<=0 or type(updated) is not int or updated<=0:
            raise ValueError('STATUS_HEARTBEAT_SCHEMA')
        age=now_ms-updated
        if age<0:raise ValueError('STATUS_CLOCK_IN_FUTURE')
        if require_fresh and age>10000:raise ValueError('STATUS_HEARTBEAT_STALE')
        if self.previous is not None:
            old=self.previous
            if session!=old['session']:raise ValueError('STATUS_SESSION_CHANGED')
            if now_ms<old['observed_ms'] or updated<old['updated']:
                raise ValueError('STATUS_CLOCK_REGRESSION')
            if sequence<old['sequence']:raise ValueError('STATUS_HEARTBEAT_REGRESSION')
            if sequence==old['sequence'] and updated!=old['updated']:
                raise ValueError('STATUS_VERSION_CHANGED_WITHOUT_HEARTBEAT')
        self.previous={'session':session,'sequence':sequence,'updated':updated,'observed_ms':now_ms}
        return {'version':1,'heartbeat_sequence':sequence,'updated_unix_ms':updated,'status_age_ms':age,
                'identity_sha256':__import__('hashlib').sha256((str(self.pid)+':'+session).encode()).hexdigest(),
                'alive_deadline_applied':require_fresh}


def read_result(path):
    if path.is_symlink() or not path.is_file():raise ValueError('RESULT_FILE_REQUIRED')
    with path.open('rb') as source:raw=source.read(4*MIB+1)
    if len(raw)>4*MIB:raise ValueError('RESULT_FILE_BYTE_LIMIT')
    def unique(items):
        value={}
        for key,item in items:
            if key in value:raise ValueError('RESULT_DUPLICATE_KEY')
            value[key]=item
        return value
    def nonfinite(value):raise ValueError('RESULT_NONFINITE_NUMBER:' + value)
    value=json.loads(raw,object_pairs_hook=unique,parse_constant=nonfinite)
    if type(value) is not dict:raise ValueError('RESULT_OBJECT_REQUIRED')
    return value


def child_failure_brief(index,process,path,row=None,error=None):
    # The complete bounded report and log remain in their original child path.
    # This sidecar preserves only bounded first-cause fields, not another copy.
    brief={'writer_index':index,'returncode':process.returncode,
           'result_relative_path':path.name+'/result.json','result_error':error}
    if row is not None:
        for key in ('ok','reason','writer_error','overflow','submitted_pairs','written_pairs'):
            value=row.get(key)
            brief[key]=value[:4096] if isinstance(value,str) else value if value is None or type(value) in (int,float,bool) else '<invalid non-scalar>'
    return brief


def wait_ready(children, roots, output, max_bytes, deadline):
    pending = set(range(len(children)))
    ready_deadline = min(deadline, time.monotonic() + 30)
    while pending:
        for index in tuple(pending):
            process = children[index]
            if process.poll() is not None:
                raise RuntimeError('CHILD_EXITED_BEFORE_BARRIER')
            available, value = ab.ready_marker(roots[index] / 'ready.json')
            if not available:
                continue
            if type(value) is not dict or type(value.get('pid')) is not int:
                raise ValueError('BARRIER_READY_SCHEMA')
            if value['pid'] != process.pid:
                raise RuntimeError('BARRIER_PID_MISMATCH')
            pending.remove(index)
        if pending:
            ab.guard_budget(output, max_bytes, ready_deadline)
            time.sleep(.01)


def trace_class():
    return sibling('writer_syscall_trace').TraceSession


def trace_unavailable(result):
    return result.get('state') == 'unavailable' and result.get('ok') is not True


class ResourceSampler:
    """One bounded observer covering the barrier through owned cleanup/trace drain."""
    def __init__(self, root, children, args, interval):
        self.root, self.children, self.args, self.interval = root, children, args, interval
        self.stop_event = threading.Event()
        self.error = None
        self.samples = self.used = self.max_gap = 0
        self.rss_peak = self.observer_peak = self.last_ns = None
        self.gaps = {}
        self.limit = max(8 * MIB, args.seconds * 65536)
        self.phase = 'recording'
        self.next_disk = 0
        self.thread = threading.Thread(target=self.run, name='writer-resources', daemon=True)
    def sample(self, log, final=False):
        now = time.monotonic_ns()
        processes = [ab.owned_process_sample(child) for child in self.children]
        valid = [p for p in processes if 'error' not in p]
        observer = ab.process_sample(os.getpid())
        if 'error' not in observer:
            self.observer_peak = max(self.observer_peak or 0, observer['rss_bytes'])
        else:
            key = 'observer:' + observer['error']; self.gaps[key] = self.gaps.get(key, 0) + 1
        row = {'monotonic_ns':now, 'unix_ns':time.time_ns(), 'processes':processes,
               'simultaneous_rss_bytes':sum(p['rss_bytes'] for p in valid),
               'all_children_live':len(valid)==WRITERS, 'observer':observer,
               'phase':self.phase, 'final':final}
        if final or now>=self.next_disk:
            row['disks']=ab.disk_sample();self.next_disk=now+1_000_000_000
            if not row['disks'] or 'error' in row['disks']:
                self.gaps['diskstats'] = self.gaps.get('diskstats', 0) + 1
            if self.args.disk and self.args.disk not in row['disks']:
                self.gaps['requested-disk-missing'] = self.gaps.get('requested-disk-missing', 0) + 1
        for process in processes:
            if process.get('terminal') is True: continue
            for kind in ('error', 'io_error'):
                if kind in process:
                    key = kind + ':' + process[kind]; self.gaps[key] = self.gaps.get(key, 0) + 1
        encoded = json.dumps(row, separators=(',', ':')) + '\n'
        size = len(encoded.encode())
        if self.used + size > self.limit: raise RuntimeError('RESOURCE_LOG_BYTE_LIMIT_BEFORE_WRITE')
        log.write(encoded); self.used += size; self.samples += 1
        if len(valid)==WRITERS:self.rss_peak = max(self.rss_peak or 0, row['simultaneous_rss_bytes'])
        if self.last_ns is not None:self.max_gap = max(self.max_gap, now-self.last_ns)
        self.last_ns = now
    def run(self):
        try:
            with (self.root/'resources.jsonl').open('x', buffering=1) as log:
                while True:
                    self.sample(log)
                    if self.stop_event.wait(self.interval):break
                self.sample(log, final=True)
        except BaseException as failure:
            self.error = type(failure).__name__+':'+str(failure)
    def check(self):
        if self.error:raise RuntimeError('RESOURCE_SAMPLER_FAILED:'+self.error)
    def finish(self):
        self.stop_event.set(); self.thread.join(timeout=10)
        if self.thread.is_alive():raise RuntimeError('RESOURCE_SAMPLER_STOP_TIMEOUT')
        self.check()
        return {'resource_samples':self.samples,'resource_log_bytes':self.used,
                'resource_gap_max_ns':self.max_gap,'resource_log_limit_bytes':self.limit,
                'resource_gaps':self.gaps,'simultaneous_rss_peak_bytes':self.rss_peak,
                'resource_finished_monotonic_ns':self.last_ns,
                'resource_diagnostic_complete':self.rss_peak is not None and not self.gaps,
                'resource_scope':'barrier through owned child cleanup and trace final drain; terminal RSS unavailable, never zero-filled'}


def run_batch(binary, root, pairs, status_on, args, deadline, trace_factory=None, sample_interval=.2):
    root.mkdir()
    observer_started = resource.getrusage(resource.RUSAGE_SELF)
    observer_rss_peak = None
    children = []; child_roots = []; trace = None; trace_result = None
    sampler = None;resource_summary = None
    result = None;first_failure = None;first_child_failure=None;finished_results={};finished_heartbeats={}
    try:
        for index in range(WRITERS):
            child_root = root / ('writer-' + str(index));child_root.mkdir()
            child_roots.append(child_root)
            children.append(launch(binary, child_root, pairs, 'run', status_on, root / 'start.json', args.checkpoint_batching))
        wait_ready(children, child_roots, args.output, args.max_output_bytes, deadline)
        status_monitors=[StatusMonitor(path,child.pid) for path,child in zip(child_roots,children)] if status_on else []
        extra={}
        if getattr(args,'storage_trace',False):
            if trace_factory is not None:raise ValueError('STORAGE_REAL_TRACE_FACTORY_REQUIRED')
            factory=sibling('writer_storage_trace').CombinedSession
            extra={'recording_path':root,'disk':args.disk,'storage_max_bytes':args.storage_trace_max_bytes}
        else:factory = trace_factory or trace_class()
        trace = factory(root / 'trace', max_bytes=args.trace_max_bytes,
                        max_seconds=max(1., deadline - time.monotonic()), buffer_kib=args.trace_buffer_kib,
                        tracefs=args.tracefs, required=not args.allow_unavailable_trace, **extra)
        initial_trace = trace.start(children)
        missing_trace = trace_unavailable(initial_trace)
        if initial_trace.get('ok') is not True and not (missing_trace and args.allow_unavailable_trace):
            raise RuntimeError('TRACE_NOT_READY:' + str(initial_trace.get('reason') or initial_trace.get('error') or initial_trace.get('state')))
        storage_guard(args, deadline)
        target = time.monotonic_ns() + 200_000_000
        ab.write_json(root / 'start.tmp', {'start_monotonic_ns': target, 'start_unix_ns': time.time_ns() + 200_000_000})
        (root / 'start.tmp').rename(root / 'start.json')
        sampler = ResourceSampler(root, children, args, sample_interval)
        sampler.thread.start()
        next_budget = 0
        while True:
            now = time.monotonic_ns()
            # Inspect owned exits before trace/health work: a later
            # diagnostic/cleanup failure must not hide a known native cause.
            for index,(process,child_root) in enumerate(zip(children,child_roots)):
                if process.poll() is None or index in finished_results:continue
                try:child_result=read_result(child_root/'result.json')
                except Exception as failure:
                    first_child_failure=child_failure_brief(index,process,child_root,error=type(failure).__name__+':'+str(failure))
                    raise RuntimeError('CHILD_FAILED:'+child_root.name+':'+str(process.returncode)+':'+str(first_child_failure['result_error'])) from failure
                if process.returncode!=0:
                    first_child_failure=child_failure_brief(index,process,child_root,child_result)
                    raise RuntimeError('CHILD_FAILED:'+child_root.name+':'+str(process.returncode)+':'+str(child_result.get('reason') or child_result.get('writer_error')))
                try:validate_result(child_result,pairs,status_on,source_hashes(),args.seconds)
                except Exception:
                    first_child_failure=child_failure_brief(index,process,child_root,child_result)
                    raise
                finished_results[index]=child_result
                if status_on:
                    # A dead writer no longer owes a live heartbeat. Freeze
                    # its final identity/version once; never age it while
                    # slower peers finish their independent readers.
                    finished_heartbeats[index]=status_monitors[index].sample(require_fresh=False)
            sampler.check()
            for index,monitor in enumerate(status_monitors):
                if children[index].poll() is None and not ab.owned_process_sample(children[index]).get('terminal'):monitor.sample()
            if not missing_trace:
                trace.poll()
                if trace.failure:
                    raise RuntimeError('TRACE_RUNTIME_FAILURE:' + str(trace.failure))
            if now >= next_budget:
                storage_guard(args, deadline);next_budget = now + 1_000_000_000
            if time.monotonic() >= deadline:raise RuntimeError('WALL_TIME_LIMIT')
            if all(p.poll() is not None for p in children):break
            time.sleep(sample_interval)
        writers = [];final_heartbeats=[]
        for index,(process, child_root) in enumerate(zip(children, child_roots)):
            if process.returncode != 0:raise RuntimeError('CHILD_FAILED:' + child_root.name + ':' + str(process.returncode))
            writers.append(finished_results.get(index) or read_result(child_root / 'result.json'))
            if status_on:final_heartbeats.append(finished_heartbeats.get(index) or status_monitors[index].sample(require_fresh=False))
            if writers[-1].get('ok') is True:
                if writers[-1].get('diagnostics', {}).get('checkpoint_batching') != args.checkpoint_batching:
                    raise ValueError('CHECKPOINT_BATCH_POLICY_MISMATCH')
                validate_child_files(child_root, writers[-1], status_on, args.output.stat().st_dev)
        result = {'writers': writers, 'status_background_load': status_on,'final_status_heartbeats':final_heartbeats,
                  'start_skew_ns': max(r['actual_start_monotonic_ns'] for r in writers) - min(r['actual_start_monotonic_ns'] for r in writers)}
    except BaseException as failure:
        first_failure = type(failure).__name__ + ':' + str(failure)
        raise
    finally:
        # Never mask an original writer/trace failure with secondary cleanup.
        cleanup_errors = []
        if sampler is not None:sampler.phase = 'cleanup'
        for process in children:
            try:ab.stop_group(process)
            except Exception as failure:cleanup_errors.append('child:' + type(failure).__name__ + ':' + str(failure))
        if sampler is not None:sampler.phase = 'trace-drain'
        if trace is not None:
            try:trace_result = trace.finish()
            except Exception as failure:
                cleanup_errors.append('trace:' + type(failure).__name__ + ':' + str(failure))
                trace_result = {'ok':False,'state':'finish-failed','error':cleanup_errors[-1]}
        if sampler is not None:
            sampler.phase = 'final'
            try:resource_summary = sampler.finish()
            except Exception as failure:cleanup_errors.append('resource:' + type(failure).__name__ + ':' + str(failure))
        if result is not None:
            if resource_summary is not None:result.update(resource_summary)
            result['trace'] = trace_result
            observer_ended = resource.getrusage(resource.RUSAGE_SELF)
            result['observer'] = {
                'cpu_seconds': (observer_ended.ru_utime + observer_ended.ru_stime)
                    - (observer_started.ru_utime + observer_started.ru_stime),
                'rss_peak_bytes_sampled':sampler.observer_peak if sampler is not None else observer_rss_peak,
                'maxrss_process_lifetime_kib_start':observer_started.ru_maxrss,
                'maxrss_process_lifetime_kib_end':observer_ended.ru_maxrss,
                'scope':'controller+in-process syscall/trace parsing, setup and finish CPU; sampled group RSS and cumulative process-lifetime HWM are separate; excludes writer processes and kernel tracing overhead'}
        if first_failure is not None or cleanup_errors:
            try:
                ab.write_json(root / 'batch-failure.json', {'first_failure':first_failure,
                    'first_child_failure':first_child_failure,'cleanup_errors':cleanup_errors,'trace':trace_result,
                    'child_returncodes':[p.returncode for p in children],'resources':resource_summary})
            except Exception as failure:
                cleanup_errors.append('sidecar:' + type(failure).__name__ + ':' + str(failure))
        if first_failure is None and cleanup_errors:raise RuntimeError('OWNED_CLEANUP_FAILED:' + ';'.join(cleanup_errors))
    return result


def recovery_ready(path):
    try:
        with path.open('rb') as source:raw = source.read(65537)
    except FileNotFoundError:return False,None
    if len(raw)>65536:raise ValueError('RECOVERY_READY_BYTE_LIMIT')
    try:return True,json.loads(raw)
    except json.JSONDecodeError:return False,None


def checkpoint_status_contract(ready, status_on):
    if ready.get('status_background_load') is not status_on:raise ValueError('RECOVERY_STATUS_LOAD_MODE')
    status = ready.get('status_persistence')
    if not status_on:
        if status is not None:raise ValueError('RECOVERY_OFF_STATUS_MUST_BE_NULL')
        return
    if type(status) is not dict or status.get('checkpoint_critical_ack') is not True:
        raise ValueError('RECOVERY_CHECKPOINT_CRITICAL_ACK')
    for key in ('errors','pump_errors'):
        if type(status.get(key)) is not int or status[key]!=0:raise ValueError('RECOVERY_STATUS_ERROR:' + key)
    if (type(status.get('submitted_sequence')) is not int or status['submitted_sequence']<=0
            or type(status.get('completed_sequence')) is not int
            or status['completed_sequence']!=status['submitted_sequence']
            or type(status.get('pending_snapshots')) is not int or status['pending_snapshots']!=0
            or status.get('protected_sequence') is not None):
        raise ValueError('RECOVERY_STATUS_DURABLE_FENCE')


def run_recovery(binary, root, status_on, args, deadline, hashes):
    root.mkdir();child = launch(binary, root, 32, 'crash', status_on, checkpoint_batching=args.checkpoint_batching)
    try:
        while True:
            if child.poll() is not None:raise RuntimeError('CRASH_CHILD_EXITED_BEFORE_CHECKPOINT')
            available, ready = recovery_ready(root / 'recovery-ready.json')
            if available:
                if (type(ready) is not dict or type(ready.get('pairs')) is not int
                        or ready['pairs'] != 32 or type(ready.get('pid')) is not int
                        or ready['pid'] != child.pid):raise ValueError('RECOVERY_PID_OR_PREFIX_MISMATCH')
                checkpoint_status_contract(ready, status_on)
                for key, digest in hashes.items():
                    if ready.get(key) != digest:raise ValueError('RECOVERY_SOURCE_IDENTITY:' + key)
                break
            storage_guard(args, deadline);time.sleep(.02)
        if not ab.signal_group(child, signal.SIGKILL):raise RuntimeError('CRASH_CHILD_EXITED_BEFORE_SIGKILL')
        child.wait(timeout=5)
        if child.returncode != -signal.SIGKILL:raise RuntimeError('SIGKILL_NOT_OBSERVED')
    finally:ab.stop_group(child)
    partial = root / 'capture.partial'
    if partial.stat().st_dev != args.output.stat().st_dev:
        raise ValueError('RECOVERY_FILESYSTEM_DIFFER')
    if status_on and ((root/'state').is_symlink() or (root/'state').stat().st_dev != args.output.stat().st_dev):
        raise ValueError('RECOVERY_STATUS_FILESYSTEM_DIFFER')
    before = {str(p.relative_to(partial)): ab.sha256(p) for p in partial.rglob('*') if p.is_file()}
    reader = launch(binary, root, 32, 'recover', status_on, checkpoint_batching=args.checkpoint_batching)
    try:
        while reader.poll() is None:
            storage_guard(args, deadline);time.sleep(.02)
        if reader.returncode != 0:raise RuntimeError('RECOVERY_READER_FAILED')
    finally:ab.stop_group(reader)
    after = {str(p.relative_to(partial)): ab.sha256(p) for p in partial.rglob('*') if p.is_file()}
    recovered = read_result(root / 'recovery-result.json')
    if recovered.get('status_background_load') is not status_on:
        raise ValueError('RECOVERY_READER_MODE_IDENTITY')
    for key,digest in hashes.items():
        if recovered.get(key)!=digest:raise ValueError('RECOVERY_READER_SOURCE_IDENTITY:' + key)
    if before != after or (root / 'capture/COMPLETE').exists() or (partial / 'COMPLETE').exists():
        raise ValueError('RECOVERY_MUTATED_INPUT_OR_PUBLISHED_COMPLETE')
    if (type(recovered.get('passes')) is not int or recovered['passes'] != 2
            or recovered.get('pairs_per_pass') != [32, 32]
            or any(type(n) is not int for n in recovered['pairs_per_pass'])
            or recovered.get('ordinary_reader_rejected') is not True
            or recovered.get('per_frame_color_depth_evidence_hashes') is not True):
        raise ValueError('RECOVERY_READBACK_HASH_CONTRACT')
    return {'ok': True, 'signal': 'SIGKILL', 'status_background_load': status_on,
            'after_explicit_durable_checkpoint': True, 'source_unchanged_sha256': before,
            'prekill_status_proof': ready, 'readback': recovered,
            'scope': 'Independent process crash, not arbitrary kill timing or physical power failure.'}


def comparison_rows(groups):
    reports = []
    for repeat in sorted({r['repeat'] for r in groups}):
        paired = {r['status_background_load']:r for r in groups if r['repeat']==repeat}
        if set(paired)!={False,True}:continue
        pair = {'repeat':repeat,'off_directory':paired[False]['directory'],'on_directory':paired[True]['directory'],
                'scope':'matched writer positions in sequential paired runs; raw percentiles are separate, never pooled or added; one repetition proves no statistical significance'}
        for status_on,label in ((False,'off'),(True,'on')):
            row=paired[status_on];writers=row['writers'];metrics=[]
            for child in writers:
                metric={key:child.get(key) for key in ('capture_elapsed_ns','capture_cpu_seconds','capture_cpu_cores',
                    'cpu_seconds','cpu_cores','wall_seconds_including_independent_readers','peak_rss_kib',
                    'append_p99_ns','append_max_ns','queue_wait_p99_ns','queue_peak','overflow','producer_late_max_ns')}
                metric['native_verification_ns']=(child.get('diagnostics',{}).get('verification') or {}).get('total_ns')
                metric['additional_two_readers_ns']=(child.get('independent_readback') or {}).get('wall_ns')
                metric['status_persistence']=child.get('status_persistence');metrics.append(metric)
            pair[label]={'writers':metrics,'simultaneous_group_rss_peak_bytes_sampled':row['simultaneous_rss_peak_bytes'],
                'capture_cpu_seconds_sum':sum(w['capture_cpu_seconds'] for w in writers),
                'whole_cpu_seconds_sum':sum(w['cpu_seconds'] for w in writers),'observer':row.get('observer'),
                'trace_complete':(row.get('trace') or {}).get('ok') is True}
        pair['on_minus_off']={key:pair['on'][key]-pair['off'][key] for key in ('capture_cpu_seconds_sum','whole_cpu_seconds_sum')}
        if all(pair[label]['simultaneous_group_rss_peak_bytes_sampled'] is not None for label in ('off','on')):
            pair['on_minus_off']['simultaneous_group_rss_peak_bytes_sampled']=pair['on']['simultaneous_group_rss_peak_bytes_sampled']-pair['off']['simultaneous_group_rss_peak_bytes_sampled']
        pair['append_p99_ns_by_position']={'off':[w['append_p99_ns'] for w in pair['off']['writers']],
                                          'on':[w['append_p99_ns'] for w in pair['on']['writers']]}
        reports.append(pair)
    return reports


def disk_identity(name,sys_block=Path('/sys/class/block'),disks=None):
    if name is None:
        return {'requested':False,'whole_block_device_verified':False,
                'scope':'namespace-wide diskstats only; no attribution to a physical device or host APFS IO'}
    if not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}',name):raise ValueError('WHOLE_DEVICE_NAME_REQUIRED')
    disks=ab.disk_sample() if disks is None else disks
    if name not in disks:raise ValueError('REQUESTED_DEVICE_MISSING')
    entry=sys_block/name
    try:
        resolved=entry.resolve(strict=True)
        if not resolved.is_relative_to(sys_block.parent.parent.resolve()):
            raise ValueError('DISK_SYSFS_RESOLVED_PATH_ESCAPE')
        if (entry/'partition').exists():raise ValueError('WHOLE_DEVICE_REQUIRED_NOT_PARTITION')
        with (entry/'dev').open('rb') as source:raw=source.read(129)
        if len(raw)>128:raise ValueError('DISK_SYSFS_DEV_BYTE_LIMIT')
        text=raw.decode('ascii').strip()
        if re.fullmatch(r'[0-9]+:[0-9]+',text) is None:raise ValueError('DISK_SYSFS_DEV_SCHEMA')
        major,minor=map(int,text.split(':'))
        if entry.resolve(strict=True)!=resolved or (entry/'partition').exists():
            raise ValueError('DISK_SYSFS_IDENTITY_CHANGED')
    except OSError as failure:raise ValueError('DISK_SYSFS_UNAVAILABLE:'+type(failure).__name__) from failure
    observed=disks[name]
    if type(observed) is not dict or observed.get('major')!=major or observed.get('minor')!=minor:
        raise ValueError('DISK_SYSFS_DISKSTATS_IDENTITY_MISMATCH')
    return {'requested':True,'name':name,'sysfs_dev':text,'major':major,'minor':minor,
            'whole_block_device_verified':True,'physical_device_verified':False,
            'scope':'sysfs whole kernel block device matched to this namespace diskstats; logical/stacked devices and host filesystems are not uniquely attributed'}


def run(args, trace_factory=None):
    planned = plan(args)
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):raise ValueError('EXECUTABLE_BINARY_REQUIRED')
    output = args.output.absolute()
    if output.exists() or output.is_symlink():raise ValueError('FRESH_OUTPUT_REQUIRED')
    output.parent.mkdir(parents=True, exist_ok=True)
    fs = os.statvfs(output.parent)
    if fs.f_bavail * fs.f_frsize < planned['planned_upper_bound_bytes'] + FREE_SPACE_RESERVE:raise ValueError('INSUFFICIENT_FREE_SPACE')
    if args.backing_path is not None:
        args.backing_path = args.backing_path.resolve(strict=True)
        if not args.backing_path.is_dir():raise ValueError('BACKING_DIRECTORY_REQUIRED')
        backing_fs = os.statvfs(args.backing_path)
        if backing_fs.f_bavail * backing_fs.f_frsize < planned['planned_upper_bound_bytes'] + FREE_SPACE_RESERVE:
            raise ValueError('INSUFFICIENT_BACKING_FREE_SPACE')
    requested_disk=disk_identity(args.disk)
    output.mkdir(mode=0o700);args.output = output.resolve()
    hashes = source_hashes()
    manifest = {'kind': 'writer-long-status-ab-v1', 'baseline': '8697b4fe1111ad0c99da2a14c961d1e0e39570eb',
        'uname': list(platform.uname()), 'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
        'filesystem': ab.mount_evidence(args.output), 'output': str(args.output),
        'binary': {'path': str(binary), 'sha256': ab.sha256(binary)}, **hashes,
        'controller_sha256': ab.sha256(__file__),
        'trace_tool_sha256': ab.sha256(Path(__file__).with_name('writer_syscall_trace.py')),
        'storage_trace_tool_sha256':ab.sha256(Path(__file__).with_name('writer_storage_trace.py')) if getattr(args,'storage_trace',False) else None,
        'storage_analyzer_sha256':ab.sha256(Path(__file__).with_name('analyze_storage_trace.py')) if getattr(args,'storage_trace',False) else None,
        'seconds_per_recording': args.seconds,
        'repeats': args.repeats, 'status_modes': ['off', 'on'], 'fps': 30, 'imu_hz_per_stream': 200,
        'queue_capacity': 16, 'checkpoint_credit_pairs': 32, 'checkpoint_credit_bytes': 16 * MIB,
        'checkpoint_batching':args.checkpoint_batching,
        'storage_trace_enabled':getattr(args,'storage_trace',False),'storage_trace_max_bytes_per_group':getattr(args,'storage_trace_max_bytes',0) if getattr(args,'storage_trace',False) else 0,
        'backing_path':str(args.backing_path) if args.backing_path is not None else None,
        'runtime_free_space_reserve_bytes':FREE_SPACE_RESERVE,
        'status_period_ms': 500, 'status_deadline_ms':10000, 'status_backend': 'production ServiceQuality/Persistence in each writer process',
        'smoke': bool(args.smoke), 'allow_unavailable_trace': bool(args.allow_unavailable_trace),
        'disk': args.disk, 'disk_identity':requested_disk, 'max_output_bytes': args.max_output_bytes, 'max_wall_seconds': args.max_wall_seconds,
        'trace_max_bytes_per_group': args.trace_max_bytes, 'trace_buffer_kib_per_cpu': args.trace_buffer_kib,
        'tracefs': str(args.tracefs), 'plan': planned, 'limits': LIMITS}
    ab.write_json(args.output / 'manifest.json', manifest)
    rows = [];recoveries = {};error = None;expected = None
    deadline = time.monotonic() + args.max_wall_seconds
    try:
        for repeat in range(args.repeats):
            modes = (False, True) if repeat % 2 == 0 else (True, False)
            for status_on in modes:
                mode = 'on' if status_on else 'off';name = f'round-{repeat:02d}-status-{mode}'
                row = run_batch(binary, args.output / name, planned['pairs_per_writer'], status_on, args, deadline, trace_factory)
                row.update(repeat=repeat, directory=name, writer_count=WRITERS)
                rows.append(row);ab.write_json(args.output / (name + '.json'), row)
                for child in row['writers']:
                    fingerprint = validate_result(child, planned['pairs_per_writer'], status_on, hashes, args.seconds)
                    if expected is None:expected = fingerprint
                    if fingerprint != expected:raise ValueError('ON_OFF_INPUT_OR_ARTIFACT_HASH_MISMATCH')
                if not row['resource_diagnostic_complete']:raise ValueError('RESOURCE_DIAGNOSTIC_INCOMPLETE')
                trace = row['trace'] or {}
                if trace.get('ok') is not True and not (args.allow_unavailable_trace and trace_unavailable(trace)):
                    raise ValueError('TRACE_INCOMPLETE:' + str(trace.get('error') or trace.get('reason') or trace.get('state')))
        for status_on in (False, True):
            mode = 'on' if status_on else 'off'
            recoveries[mode] = run_recovery(binary, args.output / ('recovery-status-' + mode), status_on, args, deadline, hashes)
    except (Exception, KeyboardInterrupt) as failure:
        error = type(failure).__name__ + ':' + str(failure)
    completed_group_traces_complete = bool(rows) and all((r.get('trace') or {}).get('ok') is True for r in rows)
    trace_complete = len(rows) == planned['planned_groups'] and completed_group_traces_complete
    complete = error is None and len(rows) == planned['planned_groups'] and len(recoveries) == 2
    summary = {'ok': complete, 'error': error, 'completed_groups': len(rows), 'planned_groups': planned['planned_groups'],
        'long_duration_verified': complete and args.seconds >= 600,
        'trace_complete': trace_complete,
        'completed_group_traces_complete': completed_group_traces_complete,
        'long_comparison_complete': complete and args.seconds >= 600 and not args.smoke and trace_complete,
        'repeatability_comparison_complete': complete and args.seconds >= 600 and not args.smoke and trace_complete and args.repeats >= 3,
        'hardware_acceptance_verified': False, 'groups': rows, 'paired_comparisons':comparison_rows(rows) if complete else [],
        'recovery': recoveries, 'limits': LIMITS,
        'output_bytes_before_summary_and_sha256': ab.output_size(args.output),
        'observed_aggregate_overrun_bytes': max(0, ab.output_size(args.output) - args.max_output_bytes)}
    ab.write_json(args.output / 'summary.json', summary)
    with (args.output / 'SHA256SUMS').open('x') as sums:
        for p in sorted(args.output.rglob('*')):
            if p.is_file() and p.name != 'SHA256SUMS':sums.write(ab.sha256(p) + '  ' + str(p.relative_to(args.output)) + '\n')
    return summary


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True, help='release capture-probe libtest adapter executable')
    p.add_argument('--output', type=Path, required=True, help='fresh directory on the recording filesystem')
    p.add_argument('--seconds', type=int, default=600)
    p.add_argument('--repeats', type=int, default=3)
    p.add_argument('--max-output-bytes', type=int, default=512 * GIB)
    p.add_argument('--max-wall-seconds', type=int, default=14400)
    p.add_argument('--trace-max-bytes', type=int, default=4 * GIB)
    p.add_argument('--trace-buffer-kib', type=int, default=256)
    p.add_argument('--tracefs', type=Path, default=Path('/sys/kernel/tracing'))
    p.add_argument('--storage-trace',action='store_true',help='required ext4/jbd2/block device-scoped second instance, kernel threads included; direct ext4 device only')
    p.add_argument('--storage-trace-max-bytes',type=int,default=4*GIB,help='additional bounded device event output PER GROUP; included in capacity plan')
    p.add_argument('--checkpoint-batching', choices=['full-credit','half-credit'], default='full-credit',
                   help='test-only checkpoint policy; credits/barriers unchanged; record both modes in separate fresh roots')
    p.add_argument('--backing-path',type=Path,help='visible host/backing directory for loopback/VM capacity checks; sampled reserve is not a quota')
    p.add_argument('--disk', help='verified whole-device diskstats name, not a path')
    p.add_argument('--smoke', action='store_true', help='explicit short correctness run; never long-comparison acceptance')
    p.add_argument('--allow-unavailable-trace', action='store_true', help='retain capability-unavailable evidence; never accept missing tracing as complete')
    return p


def main():
    p = parser();args = p.parse_args()
    previous_term = signal.getsignal(signal.SIGTERM)
    def interrupted(signum, _frame):raise KeyboardInterrupt('signal:' + str(signum))
    signal.signal(signal.SIGTERM, interrupted)
    try:
        result = run(args)
    except (ValueError, OSError) as failure:
        p.exit(2, str(failure) + '\n')
    finally:
        signal.signal(signal.SIGTERM, previous_term)
    print(json.dumps({k: result[k] for k in ('ok', 'error', 'completed_groups', 'planned_groups', 'long_duration_verified', 'trace_complete', 'long_comparison_complete')}, indent=2))
    return 0 if result['ok'] else 1


if __name__ == '__main__':sys.exit(main())
