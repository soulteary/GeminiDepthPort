#!/usr/bin/env python3
"""Frozen pipeline-on three-camera SDK A/B plus independent SIGKILL acceptance."""
import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import signal
import sys
import time

import experiment_runtime as rt
import hardware as hw
import run_ab as batch
import run_hardware as one

HERE = Path(__file__).resolve().parent
CHECKPOINT_BASE = 'c063450c6c28f0bdec1d6a4f180871bd95fead96'


@contextmanager
def transition(root):
    fd = os.open(root/'transition.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def prepare_configs(plan, root, checkpoint_batching=None):
    rt.require(checkpoint_batching in (None,'half-credit','full-credit'),'CHECKPOINT_BATCH_POLICY')
    files, originals = batch.freeze(plan, root)
    for camera in rt.read_json(root/'frozen/plan.json')['cameras']:
        path = Path(camera['capture_config'])
        config = rt.read_json(path)
        raw = config.get('raw_recording')
        if raw is None: raw = {}
        rt.require(isinstance(raw, dict), 'RAW_RECORDING_OBJECT')
        raw.update(storage_format='chunked-v3', checkpoint_pipeline=True)
        if checkpoint_batching == 'full-credit': raw['checkpoint_batching'] = checkpoint_batching
        elif checkpoint_batching == 'half-credit': raw.pop('checkpoint_batching',None)
        raw.setdefault('queue_capacity', 16)
        config['raw_recording'] = raw
        one.atomic(path, config); path.chmod(0o600)
        files[str(path.relative_to(root))] = rt.sha(path)
    return {'frozen_files': files, 'original_source_configs': originals}


def assert_active(a, manifest):
    rt.require(not (a.root/'ABORT.json').exists(), 'RETEST_ABORTED')
    batch.check_frozen(a.root, manifest)


def batch_args(a, image):
    argv = ['run', '--plan', str(a.root/'frozen/plan.json'), '--image', image,
            '--root', str(a.root/'ab'), '--observe-lifecycle', '--execute']
    for name in ('rounds', 'first', 'seconds', 'max_bytes', 'ros_domain_id', 'pairs',
                 'healthy_timeout', 'completion_timeout', 'audit_timeout', 'poll_seconds',
                 'trim_seconds', 'cooldown_seconds'):
        argv += ['--'+name.replace('_', '-'), str(getattr(a, name))]
    for disk in a.disk: argv += ['--disk', disk]
    argv += ['--source-gap-policy', getattr(a,'source_gap_policy','reject'), '--retention', getattr(a,'retention','all')]
    return argv


def storage_preflight(a, write=True):
    """Conservative retained-data estimate; no deletion or filesystem tuning.

    Covers all normal groups plus a full native crash run. The DDS observer
    stores hashes/metadata (not pixels), with a 4096 MiB database cap and DELETE
    journal; reserve the cap plus an equal rollback journal, then 25% + 1 GiB.
    This is an estimate, not a quota or a guarantee against another writer.
    """
    cameras = rt.read_json(a.plan)['cameras']
    import retention
    native = 0; metadata = 0; wire = hw.DDS_DATABASE_MAX_MIB * 1024**2
    for camera in cameras:
        config = rt.read_json(camera['capture_config'])
        sizes = {}
        for stream in ('color','depth'):
            profile = config[stream]
            for key in ('width','height','fps'):
                rt.require(type(profile.get(key)) is int and profile[key] > 0, 'STORAGE_PROFILE_INTEGER')
            sizes[stream] = profile['width'] * profile['height']
        native += a.pairs * (2*sizes['color'] + 2*sizes['depth'] + 65536)
        metadata += a.pairs * 65536
    groups = a.rounds * 2
    proof = retention.capacity(native, metadata, groups, wire, getattr(a,'retention','all'))
    path=a.root
    if not write:
        while not path.exists():path=path.parent
    fs = os.statvfs(path)
    available = fs.f_bavail * fs.f_frsize
    total = getattr(fs,'f_blocks',fs.f_bavail) * fs.f_frsize
    proof.update(filesystem_probe_path=str(path),available_bytes=available,filesystem_total_bytes=total,
                 capacity_exceeds_filesystem=proof['estimated_required_bytes']>total,
                 capacity_sufficient=available>=proof['estimated_required_bytes'],
                 missing_bytes=max(0,proof['estimated_required_bytes']-available),
                 source_gap_policy=getattr(a,'source_gap_policy','reject'))
    if write:
        one.atomic(a.root/'storage-preflight.json',proof)
        rt.require(proof['capacity_sufficient'], 'INSUFFICIENT_RETEST_STORAGE')
    return proof



def crash_binding(run, expected_hashes, image, runtime):
    rt.require(run['image_id'] == image and run['runtime_sha256'] == runtime and run['mode'] == 'on', 'CRASH_IMAGE_CHANGED')
    hashes = {}
    for camera in run['cameras']:
        cp = Path(camera['status']).parent.parent/'config'
        binding = rt.read_json(cp/'experiment.json')
        rt.require(rt.sha(cp/'capture.json') == binding['capture_sha256']
                   and rt.sha(cp/'health.json') == binding['health_sha256']
                   and binding['diagnostics'] is True, 'CRASH_CONFIG_BINDING')
        config = rt.read_json(cp/'capture.json')
        rt.require(config['raw_recording']['checkpoint_pipeline'] is True
                   and config['raw_recording']['storage_format'] == 'chunked-v3', 'PIPELINE_NOT_ENABLED')
        del config['service']['generation']
        hashes[camera['name']] = hw.digest({'capture': config, 'health_sha256': binding['health_sha256']})
    rt.require(hashes == expected_hashes, 'CRASH_CONFIG_CHANGED')


def healthy(rows, names):
    rt.require(len(rows['cameras']) == 3 and {r['name'] for r in rows['cameras']} == set(names), 'THREE_STATUS_ROWS')
    ready = True
    for row in rows['cameras']:
        s = row.get('capture') or {}; raw = s.get('raw_recording') or {}
        rt.require(s.get('state') not in ('failed', 'stopped') and raw.get('state') not in ('failed', 'complete'), 'CRASH_CAPTURE_FAILED:'+row['name'])
        rt.require(not raw.get('whole_pair_queue_overflows', 0), 'CRASH_QUEUE_OVERFLOW')
        rt.require(not any(s.get(k, 0) for k in ('stamp_failures', 'publish_failures', 'monotonic_rejections')), 'CRASH_DATA_ERROR')
        imu = s.get('imu') or {}
        rt.require(imu.get('error') is None, 'CRASH_IMU_ERROR')
        rt.require(not (s.get('status_persistence') or {}).get('errors', 0), 'CRASH_STATUS_PERSISTENCE_ERROR')
        if s.get('ready'):
            rt.require(s.get('message_timestamp') == 'global' and imu.get('ready') is True
                       and imu.get('shared_ros_context') is True, 'CRASH_GLOBAL_SHARED_CONTEXT')
        if not (row['running'] and row['health'] == 'healthy' and s.get('ready') is True
                and raw.get('state') == 'recording'): ready = False
    return ready


def wait_checkpoint(a, cargs, run, manifest):
    deadline = time.monotonic()+a.healthy_timeout
    names = [c['name'] for c in run['cameras']]
    while True:
        assert_active(a, manifest)
        rows = hw.status(run); one.atomic(a.root/'crash-latest-status.json', rows)
        if healthy(rows, names):
            checkpoint_ready = False
            for camera, row in zip(run['cameras'], rows['cameras']):
                # Pin all three real worker identities before injection.
                rt.require(camera['name'] == row['name'], 'STATUS_CAMERA_ORDER')
                s = row['capture']; path = hw.recording_path(cargs, camera, s, True)
                rt.require(s['message_timestamp'] == 'global' and s['imu']['shared_ros_context'] is True,
                           'CRASH_GLOBAL_SHARED_CONTEXT')
                if camera['name'] == a.camera:
                    cp = path/'checkpoint.json'
                    if cp.is_file() and rt.read_json(cp)['record_count'] > 0:
                        checkpoint_ready = True
            if checkpoint_ready: return rows
        rt.require(time.monotonic() < deadline, 'CRASH_CHECKPOINT_TIMEOUT')
        time.sleep(a.poll_seconds)


def recovery_acceptance(a, run, audit):
    rt.require(audit.get('ok') is True and len(audit.get('audits', [])) == 1, 'RECOVERY_AUDIT_REQUIRED')
    item = audit['audits'][0]; s = item['summary']
    rt.require(item['camera'] == a.camera and s.get('ok') is True and s.get('audit_version') == 2
               and s.get('recovered') is True and s.get('native_hashes_verified') is True
               and s.get('independent_audit_passes') == 2 and s.get('pairs', 0) > 0
               and len(s.get('imu', [])) == 2 and all(n > 0 for n in s['imu']), 'RECOVERY_TWO_READ_IMU_REQUIRED')
    rows = hw.status(run); one.atomic(a.root/'crash-after-audit-status.json', rows)
    rt.require(len(rows['cameras']) == 3 and {r['name'] for r in rows['cameras']} == {c['name'] for c in run['cameras']},
               'THREE_STATUS_ROWS')
    for camera, row in zip(run['cameras'], rows['cameras']):
        rt.require(camera['name'] == row['name'], 'STATUS_CAMERA_ORDER')
        hw.recording_path(argparse.Namespace(run_dir=a.root/'crash'), camera, row['capture'], True)
        cp=a.root/'crash'/camera['name']/'config/capture.json'
        one.validate_status(row['capture'],rt.read_json(cp)['imu']['queue_capacity'])
    target = next(row for row in rows['cameras'] if row['name'] == a.camera)
    rt.require(not target['running'], 'CRASH_TARGET_RUNNING')
    others = [row for row in rows['cameras'] if row['name'] != a.camera]
    # Check surviving cameras independently; target is intentionally stopped.
    for row in others:
        capture = row.get('capture') or {}; raw = capture.get('raw_recording') or {}
        rt.require(row['running'] and row['health'] == 'healthy' and capture.get('ready') is True
                   and raw.get('state') == 'recording' and not raw.get('whole_pair_queue_overflows')
                   and (capture.get('imu') or {}).get('error') is None
                   and (capture.get('imu') or {}).get('ready') is True
                   and (capture.get('imu') or {}).get('shared_ros_context') is True
                   and capture.get('message_timestamp') == 'global'
                   and not (capture.get('status_persistence') or {}).get('errors', 0)
                   and not any(capture.get(k, 0) for k in ('publish_failures', 'stamp_failures', 'monotonic_rejections')),
                   'SURVIVING_CAMERA_FAILED:'+row['name'])
    return {'ok': True, 'camera': a.camera, 'summary': s, 'checkpoint_sha256': item['checkpoint_sha256'],
            'scope': 'SIGKILL confirmed-prefix recovery; not power-loss or complete-target acceptance'}


def rollback(a, mark_abort=True):
    saved = rt.read_json(a.root/'result.json')
    rt.require(saved['uid'] == os.getuid() and saved['gid'] == os.getgid(), 'RETEST_UID_GID_MISMATCH')
    if mark_abort: one.atomic(a.root/'ABORT.json', {'token': saved['run_token'], 'time_ns': time.time_ns()})
    with transition(a.root):
        return cleanup_resources(a, saved, mark_abort)


def cleanup_resources(a, saved, mark_abort):
    errors = one.stop_saved_children(a.root/'controller', saved['run_token'])
    if (a.root/'ab/result.json').exists():
        try:
            value = batch.rollback(argparse.Namespace(root=a.root/'ab'), mark_abort=mark_abort)
            if not value['ok']: errors += value['errors']
        except Exception as error: errors.append('ab:'+str(error))
    if (a.root/'crash/run.json').exists():
        try:
            run = hw.load_run(a.root/'crash', verify=False)
            auditor = saved.get('audit_container')
            rt.require(auditor == 'gdp-retest-'+saved['run_token']+'-audit', 'AUDITOR_NAME_BINDING')
            if auditor in hw.command(['docker', 'ps', '-a', '--format', '{{.Names}}']).splitlines():
                info = hw.own_container(run, {'container': auditor})
                if info['State']['Running']: hw.command(['docker', 'stop', '--time', '10', info['Id']], timeout=30)
        except Exception as error: errors.append('auditor:'+str(error))
        try: hw.stop(hw.load_run(a.root/'crash', verify=False))
        except Exception as error: errors.append('crash:'+str(error))
    value = {'ok': not errors, 'errors': errors, 'production_changed': False, 'recordings_retained': True}
    one.atomic(a.root/('rollback-'+str(time.time_ns())+'.json'), value)
    return value


def execute(a):
    rt.require(a.execute, 'EXPLICIT_EXECUTE_REQUIRED'); batch.schedule(a.rounds, a.first)
    rt.require(0 <= a.cooldown_seconds <= 600, 'COOLDOWN_LIMIT')
    rt.require(not a.root.exists(), 'FRESH_RETEST_ROOT_REQUIRED'); a.root.mkdir(mode=0o700, parents=True)
    (a.root/'controller').mkdir(mode=0o700)
    controller = one.Runner(argparse.Namespace(root=a.root/'controller', poll_seconds=a.poll_seconds)); controller.save()
    result = {'version': 1, 'ok': False, 'uid': os.getuid(), 'gid': os.getgid(), 'run_token': controller.token,
              'stage': 'prepare', 'audit_container': 'gdp-retest-'+controller.token+'-audit', 'production_changed': False, 'retention_policy':getattr(a,'retention','all'), 'rollback_argv':
              [sys.executable, str(HERE/'run_retest.py'), 'rollback', '--root', str(a.root)]}
    lifecycle = None
    def save(): one.atomic(a.root/'result.json', result)
    def phase(stage):
        result['stage'] = stage; save(); print(json.dumps({'stage': stage, 'root': str(a.root)}), flush=True)
    original_spawn = controller.spawn
    def guarded_spawn(argv, name):
        # Publish the child identity under the same lock used by rollback's
        # resource snapshot. ABORT is written before rollback waits for the lock.
        with transition(a.root):
            rt.require(not (a.root/'ABORT.json').exists(), 'RETEST_ABORTED')
            return original_spawn(argv, name)
    controller.spawn = guarded_spawn
    def command(argv, name, timeout): batch.controller_command(controller, argv, name, timeout)
    def cancelled(sig, _frame): raise one.Cancelled('SIGNAL:'+str(sig))
    save(); previous = {s: signal.signal(s, cancelled) for s in (signal.SIGINT, signal.SIGTERM)}
    try:
        manifest = prepare_configs(a.plan, a.root, a.checkpoint_batching)
        a.plan = a.root/'frozen/plan.json'; one.settings(a)
        manifest['storage_preflight'] = storage_preflight(a)
        names = [c['name'] for c in rt.read_json(a.plan)['cameras']]
        rt.require(a.camera in names, 'CRASH_CAMERA_NOT_FOUND')
        if a.build: command(['bash', str(HERE/'build_runtime.sh'), a.image, a.platform], 'build', a.build_timeout)
        image, inventory = hw.image_manifest(a.image)
        # Refuse the earlier verification-only pipeline images for this retest.
        hw.command(['git', '-C', str(HERE.parents[1]), 'merge-base', '--is-ancestor',
                    CHECKPOINT_BASE, inventory['gdp_revision']])
        manifest['gdp_revision'] = inventory['gdp_revision']
        manifest.update(image_id=image, runtime_sha256=hw.digest(inventory), pairs=a.pairs, camera=a.camera)
        one.atomic(a.root/'manifest.json', manifest); result.update(image_id=image, runtime_sha256=manifest['runtime_sha256']); save()
        assert_active(a, manifest); phase('ab')
        limit = a.rounds*2*(a.healthy_timeout+a.seconds+a.completion_timeout+2*a.audit_timeout+600+a.cooldown_seconds)
        command([sys.executable, str(HERE/'run_ab.py'), *batch_args(a, image)], 'ab', limit)
        batch.check_observer_gate()
        ab_result = rt.read_json(a.root/'ab/result.json'); summary = rt.read_json(a.root/'ab/summary.json')
        rt.require(ab_result.get('ok') is True and summary.get('complete') is True and summary.get('batch_ok') is True
                   and ab_result['image_id'] == image and ab_result['runtime_sha256'] == manifest['runtime_sha256'], 'AB_ACCEPTANCE_FAILED')
        expected = batch.schedule(a.rounds, a.first)
        rt.require(summary.get('accepted_groups') == len(expected) and summary.get('required_rounds') == a.rounds
                   and len(summary['groups']) == len(expected)
                   and all({k: group[k] for k in ('name', 'round', 'mode')} == planned and group['summary']['ok'] is True
                           for group, planned in zip(summary['groups'], expected)), 'AB_ROUNDS_INCOMPLETE')
        if getattr(a,'source_gap_policy','reject') == 'reject':
            rt.require(all(group['summary'].get('source_gap_free') is True for group in summary['groups']), 'AB_SOURCE_SEQUENCE_GAP')
        result['ab'] = summary; save(); assert_active(a, manifest); phase('crash-prepare')
        cargs = argparse.Namespace(plan=a.plan, image=image, run_dir=a.root/'crash',
                                  diagnostics_root=Path('/dev/shm')/('gdp-retest-'+controller.token), mode='on',
                                  pairs=a.pairs, max_bytes=a.max_bytes, ros_domain_id=a.ros_domain_id, execute=True,
                                  camera=a.camera, recovered=True, max_audit_mib=2048)
        hw.prepare(cargs); run = hw.load_run(cargs.run_dir)
        crash_binding(run, summary['groups'][0]['summary']['normalized_config_hashes'], image, manifest['runtime_sha256'])
        capacities={c['name']:rt.read_json(cargs.run_dir/c['name']/'config/capture.json')['imu']['queue_capacity'] for c in run['cameras']}
        lifecycle=one.Lifecycle(a.root/'crash-lifecycle',run,capacities,lambda:result['stage'],a.disk)
        with transition(a.root):
            assert_active(a, manifest); hw.start(cargs, run)
        phase('crash-wait')
        before=wait_checkpoint(a, cargs, run, manifest)
        lifecycle.observe(before)
        one.atomic(a.root/'crash-before-injection.json', before)
        with transition(a.root):
            assert_active(a, manifest); phase('crash-inject')
            one.atomic(a.root/'crash-injection.json', hw.crash(cargs, run))
        phase('crash-audit')
        command([sys.executable, str(HERE/'hardware.py'), 'audit', '--run-dir', str(cargs.run_dir),
                 '--camera', a.camera, '--recovered', '--audit-container', result['audit_container']], 'crash-audit', a.audit_timeout)
        audit = rt.read_json(a.root/'controller/crash-audit.log')
        result['recovery'] = recovery_acceptance(a, run, audit)
        assert_active(a, manifest); result['ok'] = True
    except Exception as error: result['error'] = type(error).__name__+':'+str(error)
    finally:
        for sig in previous: signal.signal(sig, signal.SIG_IGN)
        if lifecycle:phase('crash-cleanup')
        if (a.root/'crash/run.json').exists():
            try:
                one.atomic(a.root/'crash-final-status.json', hw.status(hw.load_run(a.root/'crash', verify=False)))
            except Exception as error: result['evidence_error'] = str(error)
        try:
            for child, _ in controller.children: controller.terminate(child)
            cleanup = rollback(a, mark_abort=not result['ok']); result['cleanup'] = cleanup
            if not cleanup['ok']: result['ok'] = False
        except Exception as error: result.update(ok=False, cleanup_error=str(error))
        if lifecycle:
            try:
                result['lifecycle']=lifecycle.stop()
                rt.require(result['lifecycle']['sampling_ok'],'CRASH_LIFECYCLE_INCOMPLETE')
            except Exception as error:result.update(ok=False,lifecycle_error=str(error))
        for log in controller.logs: log.close()
        result['finished_unix_ns'] = time.time_ns(); save()
        report = {'ok': result['ok'], 'ab': result.get('ab'), 'recovery': result.get('recovery'),
                  'error': result.get('error'), 'cleanup': result.get('cleanup'), 'rollback_argv': result['rollback_argv']}
        report['retention_policy'] = getattr(a,'retention','all')
        report['successful_raw_payloads_retained'] = report['retention_policy']=='all'
        report['source_gap_policy'] = getattr(a,'source_gap_policy','reject')
        report['strict_600s_acceptance_passed'] = (result['ok'] is True and a.seconds >= 600
            and a.rounds >= 3 and report['source_gap_policy'] == 'reject')
        one.atomic(a.root/'summary.json', report)
        lines = ['# Three-camera pipeline retest', '', f"Accepted: {result['ok']}", '',
                 'CPU/RSS: sampled workers; append P99/verification: recording lifetime.',
                 'SDK off metrics are unavailable, not zero. Detailed evidence: ab/summary.json.', '']
        if result.get('ab'):
            lines += [f"A/B accepted groups: {result['ab']['accepted_groups']}"]
            for key, value in (result['ab'].get('comparison') or {}).items():
                lines.append(f"- {key}: on={value['on_median']}, off={value['off_median']}")
        if result.get('recovery'):
            recovered = result['recovery']['summary']
            lines += ['', f"SIGKILL recovered pairs: {recovered['pairs']}; IMU: {recovered['imu']}"]
        if result.get('error'): lines += ['', 'Failure: '+result['error']]
        lines += ['', 'SIGKILL prefix recovery is separate from normal target/DDS acceptance; no power-loss claim.',
                  'Rollback argv is saved in result.json; all evidence retained.']
        (a.root/'summary.md').write_text('\n'.join(lines)+'\n')
        for sig, handler in previous.items(): signal.signal(sig, handler)
    return 0 if result['ok'] else 1


def parser():
    p = argparse.ArgumentParser(description=__doc__); sub = p.add_subparsers(dest='action', required=True)
    r = sub.add_parser('run', aliases=['capacity']); r.add_argument('--plan', type=Path, required=True); r.add_argument('--image', required=True)
    r.add_argument('--rounds', type=int, default=3); r.add_argument('--first', choices=('on', 'off'), default='on')
    r.add_argument('--camera', default='b'); r.add_argument('--build', action='store_true')
    r.add_argument('--platform', choices=('linux/amd64', 'linux/arm64'), default='linux/amd64')
    r.add_argument('--seconds', type=int, default=600); r.add_argument('--max-bytes', type=int, required=True)
    r.add_argument('--ros-domain-id', type=int, required=True); r.add_argument('--pairs', type=int)
    r.add_argument('--healthy-timeout', type=int, default=300); r.add_argument('--completion-timeout', type=int)
    r.add_argument('--audit-timeout', type=int, default=3600); r.add_argument('--build-timeout', type=int, default=3600)
    r.add_argument('--poll-seconds', type=float, default=5); r.add_argument('--trim-seconds', type=float, default=5)
    r.add_argument('--cooldown-seconds', type=float, default=30); r.add_argument('--disk', action='append', default=[])
    r.add_argument('--execute', action='store_true'); sub.add_parser('rollback')
    r.add_argument('--source-gap-policy', choices=('reject','report'), default='reject',
                   help='formal retest rejects source gaps by default; report retains a limited recording-only result')
    r.add_argument('--retention',choices=('all','verified-window'),default='all',help='opt-in release of NEW accepted stopped groups after two audits; failures/old roots retained')
    r.add_argument('--checkpoint-batching',choices=('half-credit','full-credit'),default='half-credit',
                   help='full-credit is an explicit experimental rollout; credits and durability barriers stay fixed')
    for command in {id(v):v for v in sub.choices.values()}.values(): command.add_argument('--root', type=Path, required=True)
    return p


def main():
    os.umask(0o077); a = parser().parse_args(); a.root = a.root.absolute()
    try:
        if a.action == 'capacity':
            batch.schedule(a.rounds,a.first); one.settings(a)
            proof=storage_preflight(a,write=False)
            print(json.dumps(proof,indent=2)); return 0 if proof['capacity_sufficient'] else 1
        one.local_rootful(require_compose=a.action == 'run')
        if a.action == 'rollback':
            value = rollback(a); print(json.dumps(value, indent=2)); return 0 if value['ok'] else 1
        return execute(a)
    except Exception as error:
        print(json.dumps({'ok': False, 'error': str(error)}), file=sys.stderr); return 1


if __name__ == '__main__': raise SystemExit(main())
