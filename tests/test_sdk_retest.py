"""Pipeline retest orchestration; synthetic SDK/Docker, no camera commands."""
import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import sys
import tempfile
import unittest
from unittest.mock import patch, Mock

HERE = Path(__file__).resolve().parents[1]/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0, str(HERE))
import run_retest as retest
import run_hardware as one
import run_ab as batch
import hardware as hw
import experiment_runtime as rt


def source_plan(root):
    cameras = []
    for n in 'abc':
        path = root/n; path.mkdir()
        one.atomic(path/'capture.json', {'serial': n, 'color': {'width':2,'height':1,'fps': 30}, 'depth': {'width':2,'height':1,'fps': 30},
                   'service': {'generation': 'original'}, 'raw_recording': {'queue_capacity': 7, 'checkpoint_pipeline': False}})
        one.atomic(path/'health.json', {'fixture': n})
        cameras.append({'name': n, 'capture_config': str(path/'capture.json'), 'health_config': str(path/'health.json')})
    one.atomic(root/'plan.json', {'cameras': cameras})
    return root/'plan.json'


def rows(stopped=False):
    return {'cameras': [{'name': n, 'running': not (stopped and n == 'b'), 'health': 'healthy', 'capture': {
        'ready': True, 'state': 'ready', 'message_timestamp': 'global', 'imu': {'ready': True, 'shared_ros_context': True, 'error': None,'queue_high_water':3,'drain_interval':{'samples':2}},
        'status_persistence':{'queue_wait':{},'critical_wait':{},'critical_event_capacity':16,'critical_events':[]},'cleanup':{'history_capacity':8,'completed':[]},'raw_recording': {'state': 'recording', 'whole_pair_queue_overflows': 0,'disk_stages':{'failed_calls':{}}}}} for n in 'abc']}


def audit():
    return {'ok': True, 'audits': [{'camera': 'b', 'checkpoint_sha256': 'e'*64, 'summary': {
        'ok': True, 'audit_version': 2, 'recovered': True, 'native_hashes_verified': True,
        'independent_audit_passes': 2, 'pairs': 32, 'imu': [200, 200]}}]}


class ConfigTests(unittest.TestCase):
    def test_only_copies_enable_pipeline_and_keep_original_bytes_and_queue(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); plan = source_plan(root); original = (root/'a/capture.json').read_bytes()
            output = root/'output'; output.mkdir()
            with patch.object(rt, 'validate_health'):
                manifest = retest.prepare_configs(plan, output)
            config = rt.read_json(output/'frozen/a/capture.json')
            self.assertEqual(config['raw_recording'], {'queue_capacity': 7, 'checkpoint_pipeline': True, 'storage_format': 'chunked-v3'})
            self.assertEqual((root/'a/capture.json').read_bytes(), original)
            self.assertEqual((output/'frozen/a/health.json').read_bytes(), (root/'a/health.json').read_bytes())
            batch.check_frozen(output, manifest)
            self.assertEqual((output/'frozen/a/capture.json').stat().st_mode & 0o777, 0o600)
            (output/'frozen/a/capture.json').write_text('{}')
            with self.assertRaisesRegex(ValueError, 'FROZEN_CONFIG_CHANGED'): batch.check_frozen(output, manifest)

    def test_minimum_three_rounds_and_rootful_run_cli(self):
        a = retest.parser().parse_args(['run', '--root', '/tmp/new', '--plan', '/tmp/p', '--image', 'tag',
                                     '--max-bytes', '100', '--ros-domain-id', '78'])
        self.assertEqual(a.rounds, 3); self.assertEqual(a.camera, 'b'); self.assertFalse(a.execute)
        self.assertEqual(a.source_gap_policy,'reject')
        self.assertEqual(a.checkpoint_batching,'half-credit')
        with self.assertRaises(ValueError): batch.schedule(2, 'on')
        with patch.object(retest.one, 'local_rootful', side_effect=ValueError('rootful required')), \
             patch.object(retest, 'execute') as execute, patch.object(sys, 'argv', ['run_retest.py', 'rollback', '--root', '/tmp/fake']):
            self.assertEqual(retest.main(), 1); execute.assert_not_called()

    def test_storage_admission_retains_proof_and_rejects_before_camera_work(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);plan=source_plan(root)
            a=argparse.Namespace(root=root,plan=plan,pairs=18000,seconds=600,rounds=3,source_gap_policy='reject')
            fs=argparse.Namespace(f_bavail=0,f_frsize=4096)
            with patch.object(retest.os,'statvfs',return_value=fs),self.assertRaisesRegex(ValueError,'INSUFFICIENT_RETEST_STORAGE'):
                retest.storage_preflight(a)
            proof=rt.read_json(root/'storage-preflight.json')
            spec=importlib.util.spec_from_file_location('storage_observer_contract',HERE.parents[1]/'ros/imu_long_run_observer.py')
            observer=importlib.util.module_from_spec(spec);spec.loader.exec_module(observer)
            db=observer.database(root/'wire.sqlite3',hw.DDS_DATABASE_MAX_MIB)
            self.assertEqual(db.execute('PRAGMA max_page_count').fetchone()[0]*db.execute('PRAGMA page_size').fetchone()[0],proof['dds_bytes_per_group'])
            self.assertEqual(db.execute('PRAGMA journal_mode').fetchone()[0],'delete');db.close()
            self.assertEqual(proof['normal_groups'],6)
            self.assertGreater(proof['estimated_required_bytes'],proof['native_bytes_per_group']*6)
            fs.f_bavail=proof['estimated_required_bytes']//4096+1
            with patch.object(retest.os,'statvfs',return_value=fs):retest.storage_preflight(a)

    def test_real_controller_command_has_poll_interval_and_prunes_reaped_children(self):
        with tempfile.TemporaryDirectory() as d:
            controller = one.Runner(argparse.Namespace(root=Path(d), poll_seconds=.01))
            batch.controller_command(controller, [sys.executable, '-c', 'print("done")'], 'child', 10)
            self.assertIn('done', (Path(d)/'child.log').read_text())
            self.assertEqual(rt.read_json(Path(d)/'children.json')['processes'], [])


class AcceptanceTests(unittest.TestCase):
    def test_recovery_requires_two_reads_hashes_imu_and_surviving_cameras(self):
        with tempfile.TemporaryDirectory() as d:
            a = argparse.Namespace(root=Path(d), camera='b')
            for n in 'abc':
                cp=a.root/'crash'/n/'config';cp.mkdir(parents=True)
                one.atomic(cp/'capture.json',{'imu':{'queue_capacity':128}})
            with patch.object(hw, 'recording_path'), patch.object(hw, 'status', return_value=rows(True)):
                self.assertTrue(retest.recovery_acceptance(a, {'cameras': [{'name': n} for n in 'abc']}, audit())['ok'])
                for key, value in [('independent_audit_passes', 1), ('native_hashes_verified', False),
                                   ('recovered', False), ('pairs', 0), ('imu', [0, 200])]:
                    broken = audit(); broken['audits'][0]['summary'][key] = value
                    with self.subTest(key=key), self.assertRaises(ValueError): retest.recovery_acceptance(a, {'cameras': [{'name': n} for n in 'abc']}, broken)
            for fault in ['running', 'imu', 'overflow']:
                state = rows(True)
                if fault == 'running': state['cameras'][0]['running'] = False
                elif fault == 'imu': state['cameras'][0]['capture']['imu']['error'] = 'bad'
                else: state['cameras'][0]['capture']['raw_recording']['whole_pair_queue_overflows'] = 1
                with patch.object(hw, 'recording_path'), patch.object(hw, 'status', return_value=state), self.assertRaises(ValueError): retest.recovery_acceptance(a, {'cameras': [{'name': n} for n in 'abc']}, audit())
            with patch.object(hw, 'recording_path'), patch.object(hw, 'status', return_value=rows()), self.assertRaisesRegex(ValueError, 'TARGET_RUNNING'):
                retest.recovery_acceptance(a, {'cameras': [{'name': n} for n in 'abc']}, audit())

    def test_wait_pins_all_three_and_rejects_errors_even_while_another_camera_starts(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a = argparse.Namespace(root=root, healthy_timeout=30, poll_seconds=1, camera='b')
            run = {'cameras': [{'name': n} for n in 'abc']}
            for n in 'abc':
                (root/n).mkdir(); one.atomic(root/n/'checkpoint.json', {'record_count': 16})
            with patch.object(retest, 'assert_active'), patch.object(hw, 'status', return_value=rows()), \
                 patch.object(hw, 'recording_path', side_effect=lambda args, camera, s, recovered: root/camera['name']) as pin:
                retest.wait_checkpoint(a, argparse.Namespace(), run, {})
                self.assertEqual([c.args[1]['name'] for c in pin.call_args_list], list('abc'))
            bad = rows(); bad['cameras'][0]['running'] = False
            bad['cameras'][2]['capture']['raw_recording']['state'] = 'failed'
            with self.assertRaisesRegex(ValueError, 'CRASH_CAPTURE_FAILED'): retest.healthy(bad, list('abc'))

    def test_crash_configuration_and_image_binding_rejects_tamper_or_mismatch(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); cp = root/'a/config'; cp.mkdir(parents=True)
            config = {'service': {'generation': 'new'}, 'raw_recording': {'storage_format': 'chunked-v3', 'checkpoint_pipeline': True}}
            one.atomic(cp/'capture.json', config); one.atomic(cp/'health.json', {})
            binding = {'capture_sha256': rt.sha(cp/'capture.json'), 'health_sha256': rt.sha(cp/'health.json'), 'diagnostics': True}
            one.atomic(cp/'experiment.json', binding)
            normalized = copy.deepcopy(config); del normalized['service']['generation']
            expected = {'a': hw.digest({'capture': normalized, 'health_sha256': binding['health_sha256']})}
            run = {'image_id': 'image', 'runtime_sha256': 'runtime', 'mode': 'on',
                   'cameras': [{'name': 'a', 'status': str(root/'a/state/status.json')}]}
            retest.crash_binding(run, expected, 'image', 'runtime')
            for changed in [dict(run, image_id='other'), dict(run, runtime_sha256='other'), dict(run, mode='off')]:
                with self.assertRaisesRegex(ValueError, 'IMAGE_CHANGED'): retest.crash_binding(changed, expected, 'image', 'runtime')
            with self.assertRaisesRegex(ValueError, 'CONFIG_CHANGED'): retest.crash_binding(run, {'a': 'other'}, 'image', 'runtime')
            one.atomic(cp/'capture.json', {})
            with self.assertRaisesRegex(ValueError, 'CONFIG_BINDING'): retest.crash_binding(run, expected, 'image', 'runtime')

    def test_wait_timeout_and_abort_never_inject(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a = argparse.Namespace(root=root, healthy_timeout=30, poll_seconds=1, camera='b')
            state = rows(); state['cameras'][0]['running'] = False
            with patch.object(retest, 'assert_active'), patch.object(hw, 'status', return_value=state), \
                 patch.object(retest.time, 'monotonic', side_effect=[0, 31]), self.assertRaisesRegex(ValueError, 'TIMEOUT'):
                retest.wait_checkpoint(a, None, {'cameras': [{'name': n} for n in 'abc']}, {})
            one.atomic(root/'ABORT.json', {})
            with self.assertRaisesRegex(ValueError, 'RETEST_ABORTED'): retest.assert_active(a, {})


class FlowTests(unittest.TestCase):
    def execute(self, root, failure=None, source_gap_policy='reject'):
        plan = source_plan(root)
        a = retest.parser().parse_args(['run', '--root', str(root/'run'), '--plan', str(plan), '--image', 'tag',
             '--max-bytes', '137438953472', '--ros-domain-id', '78', '--source-gap-policy',source_gap_policy,'--build', '--execute'])
        calls = []; image = 'sha256:'+'a'*64; inventory = {'fixture': 'sdk', 'gdp_revision': retest.CHECKPOINT_BASE}
        def prepare(args):
            args.run_dir.mkdir()
            for n in 'abc':
                cp=args.run_dir/n/'config';cp.mkdir(parents=True)
                one.atomic(cp/'capture.json',{'imu':{'queue_capacity':128}})
            one.atomic(args.run_dir/'run.json', {'image_id': image, 'runtime_sha256': hw.digest(inventory), 'mode': 'on',
                       'cameras': [{'name': n} for n in 'abc']})
        def command(controller, argv, name, timeout):
            self.assertGreater(timeout, 0); self.assertEqual(controller.a.poll_seconds, 5)
            calls.append((name, argv))
            if failure == name: raise ValueError('synthetic '+name)
            if failure == 'signal' and name == 'ab': raise one.Cancelled('SIGNAL:15')
            if name == 'ab':
                (a.root/'ab').mkdir()
                one.atomic(a.root/'ab/result.json', {'ok': True, 'image_id': image, 'runtime_sha256': hw.digest(inventory)})
                one.atomic(a.root/'ab/summary.json', {'complete': True, 'batch_ok': True, 'comparison': {'cpu': {'on_median': 1, 'off_median': 1}},
                           'accepted_groups': 4 if failure == 'incomplete' else 6, 'required_rounds': 3, 'groups': [dict(g, summary={'ok': True, 'source_gap_free': True, 'normalized_config_hashes': {'a': 'fixed'}}) for g in batch.schedule(3, 'on')]})
            if name == 'crash-audit': one.atomic(controller.root/'crash-audit.log', audit())
        cleanup = {'ok': failure != 'cleanup', 'errors': [], 'recordings_retained': True}
        with patch.object(rt, 'validate_health'), patch.object(hw, 'command', return_value=''), patch.object(hw, 'image_manifest', return_value=(image, inventory)) as resolve, \
             patch.object(retest.os,'statvfs',return_value=argparse.Namespace(f_bavail=1<<30,f_frsize=4096)), \
             patch.object(one,'Lifecycle',return_value=Mock(stop=Mock(return_value={"sampling_ok":True}))), \
             patch.object(one.Runner, 'command', side_effect=command, autospec=True), \
             patch.object(hw, 'prepare', side_effect=prepare) as prep, patch.object(hw, 'load_run', side_effect=lambda p, **kw: rt.read_json(p/'run.json')), \
             patch.object(retest, 'crash_binding'), patch.object(hw, 'recording_path'), patch.object(hw, 'start') as start, \
             patch.object(retest, 'wait_checkpoint', return_value=rows()), patch.object(hw, 'crash', return_value={'ok': True}) as crash, \
             patch.object(hw, 'status', return_value=rows(True)), patch.object(retest, 'rollback', return_value=cleanup) as rollback:
            code = retest.execute(a)
            self.assertEqual(resolve.call_count, 0 if failure == 'build' else 1)
        return a, code, calls, prep, start, crash, rollback

    def test_build_once_fixed_image_same_plan_limits_and_independent_recovery(self):
        with tempfile.TemporaryDirectory() as d:
            a, code, calls, prep, start, crash, cleanup = self.execute(Path(d))
            self.assertEqual(code, 0); self.assertEqual([n for n, _ in calls], ['build', 'ab', 'crash-audit'])
            argv = dict(calls)['ab']; self.assertIn('--observe-lifecycle',argv); self.assertEqual(argv[argv.index('--image')+1], 'sha256:'+'a'*64)
            self.assertEqual(argv[argv.index('--rounds')+1], '3'); self.assertNotIn('--build', argv)
            self.assertEqual(prep.call_args.args[0].pairs, 31680)
            self.assertEqual(prep.call_args.args[0].run_dir, a.root/'crash')
            self.assertEqual(prep.call_args.args[0].image, 'sha256:'+'a'*64)
            self.assertEqual(crash.call_count, 1)
            self.assertTrue(rt.read_json(a.root/'summary.json')['ok']); self.assertTrue(cleanup.called)

    def test_report_only_policy_never_claims_strict_600_second_acceptance(self):
        with tempfile.TemporaryDirectory() as d:
            a,code,*_=self.execute(Path(d),source_gap_policy='report')
            self.assertEqual(code,0)
            result=rt.read_json(a.root/'summary.json')
            self.assertTrue(result['ok']);self.assertFalse(result['strict_600s_acceptance_passed'])
            self.assertEqual(result['source_gap_policy'],'report')

    def test_failures_stop_later_stages_and_retain_summary_and_rollback(self):
        for failure in ['build', 'ab', 'crash-audit', 'signal', 'cleanup', 'incomplete']:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as d:
                a, code, calls, prep, start, crash, cleanup = self.execute(Path(d), failure)
                self.assertEqual(code, 1); self.assertFalse(rt.read_json(a.root/'summary.json')['ok'])
                self.assertTrue((a.root/'result.json').exists()); self.assertTrue(cleanup.called)
                self.assertIn('rollback', rt.read_json(a.root/'result.json')['rollback_argv'])
                if failure in ('build', 'ab', 'signal', 'incomplete'): crash.assert_not_called(); start.assert_not_called()

    def test_rollback_stops_controller_then_nested_batch_and_crash_without_verifying_config(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a = argparse.Namespace(root=root); calls = []
            one.atomic(root/'result.json', {'uid': os.getuid(), 'gid': os.getgid(), 'run_token': 'token', 'audit_container': 'gdp-retest-token-audit'})
            (root/'ab').mkdir(); one.atomic(root/'ab/result.json', {})
            (root/'crash').mkdir(); one.atomic(root/'crash/run.json', {})
            def children(*args):
                self.assertTrue((root/'ABORT.json').exists()); calls.append('controller'); return []
            with patch.object(one, 'stop_saved_children', side_effect=children), \
                 patch.object(batch, 'rollback', side_effect=lambda *args, **kw: calls.append('ab') or {'ok': True}), \
                 patch.object(hw, 'load_run', return_value={}) as load, patch.object(hw, 'command', return_value=''), \
                 patch.object(hw, 'stop', side_effect=lambda run: calls.append('crash')):
                self.assertTrue(retest.rollback(a)['ok'])
                self.assertEqual(calls, ['controller', 'ab', 'crash']); self.assertFalse(load.call_args.kwargs['verify'])

    def test_rollback_publishes_abort_then_waits_for_inflight_launch_transition(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            one.atomic(root/'result.json', {'uid': os.getuid(), 'gid': os.getgid(), 'run_token': 'token'})
            code = """import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import run_retest as r
import argparse
def stopped(root, token):
    (root.parent/'cleanup-started').write_text('yes')
    return []
r.one.stop_saved_children = stopped
assert r.rollback(argparse.Namespace(root=Path(sys.argv[2])))['ok']
"""
            child = None
            try:
                with retest.transition(root):
                    child = subprocess.Popen([sys.executable, '-c', code, str(HERE), str(root)])
                    deadline = time.monotonic()+10
                    while not (root/'ABORT.json').exists():
                        self.assertLess(time.monotonic(), deadline)
                        self.assertIsNone(child.poll()); time.sleep(.01)
                    self.assertFalse((root/'cleanup-started').exists())
                self.assertEqual(child.wait(timeout=10), 0)
                self.assertTrue((root/'cleanup-started').exists())
            finally:
                if child is not None and child.poll() is None: child.kill(); child.wait()

    def test_named_auditor_is_identity_checked_and_stopped_before_capture_cleanup(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a = argparse.Namespace(root=root); name = 'gdp-retest-token-audit'; calls = []
            one.atomic(root/'result.json', {'uid': os.getuid(), 'gid': os.getgid(), 'run_token': 'token', 'audit_container': name})
            (root/'crash').mkdir(); one.atomic(root/'crash/run.json', {})
            def command(argv, **kw):
                if argv[1] == 'ps': return name+'\n'
                calls.append(argv); return ''
            with patch.object(one, 'stop_saved_children', return_value=[]), patch.object(hw, 'load_run', return_value={}), \
                 patch.object(hw, 'command', side_effect=command), \
                 patch.object(hw, 'own_container', return_value={'Id': 'owned-id', 'State': {'Running': True}}) as own, \
                 patch.object(hw, 'stop') as stop:
                self.assertTrue(retest.rollback(a)['ok'])
                self.assertEqual(own.call_args.args[1]['container'], name)
                self.assertEqual(calls[0][-1], 'owned-id'); stop.assert_called_once()
            with patch.object(one, 'stop_saved_children', return_value=[]), patch.object(hw, 'load_run', return_value={}), \
                 patch.object(hw, 'command', return_value=name+'\n') as command, \
                 patch.object(hw, 'own_container', side_effect=ValueError('foreign identity')), patch.object(hw, 'stop'):
                self.assertFalse(retest.rollback(a)['ok'])
                self.assertTrue(all(call.args[0][1] == 'ps' for call in command.call_args_list))

    def test_rollback_rejects_wrong_uid_and_still_cleans_when_configs_invalid(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a = argparse.Namespace(root=root)
            one.atomic(root/'result.json', {'uid': os.getuid()+1, 'gid': os.getgid(), 'run_token': 'token', 'audit_container': 'gdp-retest-token-audit'})
            with self.assertRaisesRegex(ValueError, 'UID_GID'): retest.rollback(a)
            self.assertFalse((root/'ABORT.json').exists())
            one.atomic(root/'result.json', {'uid': os.getuid(), 'gid': os.getgid(), 'run_token': 'token', 'audit_container': 'gdp-retest-token-audit'})
            (root/'crash').mkdir(); one.atomic(root/'crash/run.json', {})
            with patch.object(one, 'stop_saved_children', return_value=[]), patch.object(hw, 'load_run', return_value={}) as load, patch.object(hw, 'command', return_value=''), \
                 patch.object(hw, 'stop', side_effect=ValueError('identity mismatch')):
                value = retest.rollback(a)
                self.assertFalse(value['ok']); self.assertTrue((root/'ABORT.json').exists())
                self.assertFalse(load.call_args.kwargs['verify'])


if __name__ == '__main__': unittest.main()
