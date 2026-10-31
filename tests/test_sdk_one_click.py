"""No physical hardware: exercise orchestration, timeouts, UID and cleanup faults."""
import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
HERE = ROOT/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('sdk_one_click', HERE/'run_hardware.py')
cli = importlib.util.module_from_spec(spec);spec.loader.exec_module(cli)


def args(root):
    a = cli.parser().parse_args(['run', '--plan', str(root/'plan.json'), '--image', 'exp:test',
                                '--root', str(root), '--max-bytes', str(128*1024**3), '--ros-domain-id', '78', '--execute'])
    a.run_dir = root/'capture';a.diagnostics_root = root/'diagnostics';a.pairs = 31680
    a.completion_timeout = 2300
    return a


def config_plan(root, fps=30):
    cameras = []
    for name in 'abc':
        config = root/(name+'.json');cli.atomic(config, {'color':{'fps':fps}, 'depth':{'fps':fps}})
        cameras.append({'name':name, 'capture_config':str(config)})
    cli.atomic(root/'plan.json', {'cameras':cameras})


def status(a, state='recording'):
    rows = []
    for name in 'abc':
        rows.append({'name':name, 'running':True, 'health':'healthy', 'capture':{
            'ready':True, 'state':'ready', 'restart_attempt':0, 'updated_unix_ms':time.time_ns()//10**6,
            'message_timestamp':'global', 'imu':{'ready':True,'shared_ros_context':True,'error':None},
            'stamp_failures':0, 'publish_failures':0, 'monotonic_rejections':0,
            'raw_recording':{'state':state, 'max_pairs':a.pairs, 'written_pairs':a.pairs if state=='complete' else 10,
                             'whole_pair_queue_overflows':0}}})
    return {'cameras':rows}


def run_metadata():
    return {'run_id':'a'*32, 'image_id':'sha256:'+'b'*64, 'runtime_sha256':'c'*64, 'mode':'on',
            'ros_domain_id':78, 'cameras':[{'name':n, 'container':'exp-'+n, 'namespace':'/'+n, 'diagnostics':'/private/diag/'+n} for n in 'abc']}


class PrerequisiteTests(unittest.TestCase):
    def test_uid_and_platform_rejected_before_docker_or_files(self):
        with patch.object(cli.sys,'platform','linux'),patch.object(cli.os,'getuid',return_value=501),patch.object(cli.hw,'command') as call:
            with self.assertRaisesRegex(ValueError,'SUDO_ROOT'):cli.local_rootful()
            call.assert_not_called()
        with patch.object(cli.sys,'platform','darwin'),patch.object(cli.hw,'command') as call:
            with self.assertRaisesRegex(ValueError,'LINUX_HOST'):cli.local_rootful()
            call.assert_not_called()

    def test_rootless_remote_and_context_override_are_rejected(self):
        for host, options, env in [('ssh://server', [], {}), ('unix:///run/docker.sock',['name=rootless'],{}),
                                   ('ssh://server',[],{'DOCKER_CONTEXT':'remote','DOCKER_HOST':'unix:///run/docker.sock'})]:
            context=json.dumps([{'Endpoints':{'docker':{'Host':host}}}])
            with self.subTest(host=host,options=options),patch.object(cli.sys,'platform','linux'), \
                 patch.object(cli.os,'getuid',return_value=0),patch.object(cli.os,'geteuid',return_value=0), \
                 patch.dict(os.environ,env,clear=True),patch.object(cli.hw,'command',side_effect=[context,json.dumps(options)]):
                with self.assertRaises(ValueError):cli.local_rootful()

    def test_rollback_needs_no_compose_or_image_test(self):
        context=json.dumps([{'Endpoints':{'docker':{'Host':'unix:///run/docker.sock'}}}])
        with patch.object(cli.sys,'platform','linux'),patch.object(cli.os,'getuid',return_value=0), \
             patch.object(cli.os,'geteuid',return_value=0),patch.dict(os.environ,{},clear=True), \
             patch.object(cli.hw,'command',side_effect=[context,'[]']) as call:
            cli.local_rootful(require_compose=False);self.assertEqual(call.call_count,2)

    def test_default_pair_target_budgets_health_wait_and_sample(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);config_plan(root);a=args(root);a.pairs=None;a.completion_timeout=None
            result=cli.settings(a)
            self.assertEqual(result.pairs,31680)
            self.assertGreater(result.completion_timeout,600)
            a.pairs=4500
            with self.assertRaisesRegex(ValueError,'PAIR_TARGET_TOO_SHORT'):cli.settings(a)

    def test_nonfinite_or_invalid_timing_and_fps_fail_closed(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);config_plan(root)
            a=args(root);a.seconds=5
            with self.assertRaises(ValueError):cli.settings(a)
            config_plan(root,0)
            with self.assertRaisesRegex(ValueError,'SOURCE_FPS'):cli.settings(args(root))


class PollingTests(unittest.TestCase):
    def test_ready_complete_and_sampling_are_separate_conditions(self):
        with tempfile.TemporaryDirectory() as d:
            a=args(Path(d));run=run_metadata()
            with patch.object(cli.hw,'recording_path'):
                self.assertEqual(cli.check_rows(a,run,status(a)),(True,False))
                self.assertEqual(cli.check_rows(a,run,status(a,'complete')),(False,True))
                with self.assertRaisesRegex(ValueError,'ENDED_OR_UNHEALTHY'):
                    cli.check_rows(a,run,status(a,'complete'),sampling=True)
                empty={'cameras':[{'name':n,'running':True,'health':'starting'} for n in 'abc']}
                self.assertEqual(cli.check_rows(a,run,empty),(False,False))
                with self.assertRaisesRegex(ValueError,'MISSING_CAPTURE'):cli.check_rows(a,run,empty,sampling=True)

    def test_capture_faults_are_not_hidden_by_healthy_container(self):
        with tempfile.TemporaryDirectory() as d:
            a=args(Path(d));run=run_metadata()
            for fault in ('stale','overflow','imu','timestamp','global','context','exit','restart','persist','config'):
                rows=status(a);row=rows['cameras'][0];s=row['capture']
                if fault=='stale':s['updated_unix_ms']=0
                elif fault=='overflow':s['raw_recording']['whole_pair_queue_overflows']=1
                elif fault=='imu':s['imu']['error']='queue-overflow'
                elif fault=='timestamp':s['monotonic_rejections']=1
                elif fault=='global':s['message_timestamp']='system'
                elif fault=='context':s['imu']['shared_ros_context']=False
                elif fault=='exit':row['running']=False
                elif fault=='restart':s['restart_attempt']=1
                elif fault=='persist':s['status_persistence']={'errors':1}
                with self.subTest(fault=fault),patch.object(cli.hw,'recording_path',side_effect=ValueError('binding') if fault=='config' else None):
                    with self.assertRaises(ValueError):cli.check_rows(a,run,rows)

    def test_wait_has_deadline_and_cannot_accept_early_recording_completion(self):
        with tempfile.TemporaryDirectory() as d:
            a=args(Path(d));runner=cli.Runner(a);runner.run=run_metadata()
            with patch.object(runner,'snapshot',return_value={}),patch.object(cli,'check_rows',return_value=(False,False)), \
                 patch.object(cli.time,'monotonic',side_effect=[0,301]):
                with self.assertRaisesRegex(ValueError,'HEALTH_TIMEOUT'):runner.wait_for()
            with patch.object(runner,'snapshot',return_value={}),patch.object(cli,'check_rows',return_value=(False,True)):
                with self.assertRaisesRegex(ValueError,'BEFORE_SAMPLE'):runner.wait_for()
                self.assertEqual(runner.wait_for(completion=True),{})


class WorkflowTests(unittest.TestCase):
    def runner(self,root):
        a=args(root);return cli.Runner(a)

    def test_success_orders_all_phases_and_still_cleans_owned_resources(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.runner(Path(d));phases=[]
            def phase(name):runner.stage=name;phases.append(name)
            with patch.object(runner,'phase',side_effect=phase),patch.object(cli.hw,'prepare'), \
                 patch.object(cli.hw,'load_run',return_value=run_metadata()),patch.object(cli.hw,'start'), \
                 patch.object(runner,'wait_for'),patch.object(runner,'sample'),patch.object(runner,'command'), \
                 patch.object(runner,'acceptance'),patch.object(runner,'evidence'),patch.object(cli.hw,'stop',return_value={'ok':True}) as stop, \
                 patch.object(cli.signal,'signal'):
                self.assertEqual(runner.execute(),0)
                self.assertEqual(phases,['prepare','start','wait-healthy','sample','wait-complete','stop','audit','verify-dds'])
                self.assertEqual(stop.call_count,2)
            result=json.loads((runner.root/'result.json').read_text())
            self.assertTrue(result['ok']);self.assertTrue(result['hardware_acceptance_verified'])
            self.assertFalse(result['production_changed'])

    def test_faults_and_cancellation_keep_failure_stage_evidence_and_rollback(self):
        for fault in ('start','sample','audit','cancel'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                runner=self.runner(Path(d));marker=runner.root/'failed-recording.partial';marker.write_bytes(b'unchanged')
                error=cli.Cancelled('SIGTERM') if fault=='cancel' else ValueError('injected')
                with patch.object(cli.hw,'prepare'),patch.object(cli.hw,'load_run',return_value=run_metadata()), \
                     patch.object(cli.hw,'start',side_effect=error if fault=='start' else None), \
                     patch.object(runner,'wait_for'),patch.object(runner,'sample',side_effect=error if fault in ('sample','cancel') else None), \
                     patch.object(runner,'command',side_effect=error if fault=='audit' else None), \
                     patch.object(runner,'acceptance'),patch.object(runner,'evidence') as evidence,patch.object(cli.hw,'stop',return_value={'ok':True}) as stop, \
                     patch.object(cli.signal,'signal'):
                    self.assertEqual(runner.execute(),1);self.assertGreaterEqual(stop.call_count,1);evidence.assert_called()
                result=cli.rt.read_json(runner.root/'result.json')
                self.assertFalse(result['ok']);self.assertIn('rollback_argv',result)
                self.assertEqual(result['failed_stage'],'sample' if fault=='cancel' else fault)
                self.assertEqual(marker.read_bytes(),b'unchanged')

    def test_cleanup_failure_cannot_leave_success_true(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.runner(Path(d))
            with patch.object(cli.hw,'prepare'),patch.object(cli.hw,'load_run',return_value=run_metadata()), \
                 patch.object(cli.hw,'start'),patch.object(runner,'wait_for'),patch.object(runner,'sample'), \
                 patch.object(runner,'command'),patch.object(runner,'acceptance'),patch.object(runner,'evidence'), \
                 patch.object(cli.hw,'stop',side_effect=[{},ValueError('identity changed')]),patch.object(cli.signal,'signal'):
                self.assertEqual(runner.execute(),1)
            self.assertFalse(runner.result['hardware_acceptance_verified']);self.assertTrue(runner.result['cleanup_errors'])

    def test_partial_prepare_metadata_is_recovered_for_identity_checked_cleanup(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.runner(Path(d));runner.a.run_dir.mkdir();(runner.a.run_dir/'run.json').write_text('{}')
            with patch.object(cli.hw,'prepare',side_effect=ValueError('prepare interrupted')), \
                 patch.object(cli.hw,'load_run',return_value=run_metadata()) as load,patch.object(runner,'acceptance'),patch.object(runner,'evidence'), \
                 patch.object(cli.hw,'stop',return_value={'ok':True}) as stop,patch.object(cli.signal,'signal'):
                self.assertEqual(runner.execute(),1);load.assert_called_with(runner.a.run_dir,verify=False);stop.assert_called_once()

    def test_real_child_timeout_is_reaped_and_log_remains(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.runner(Path(d))
            with self.assertRaisesRegex(ValueError,'COMMAND_TIMEOUT'):
                runner.command([sys.executable,'-c','import time; print("before timeout",flush=True); time.sleep(60)'],'timeout',.05)
            self.assertIsNotNone(runner.children[0][0].poll())
            self.assertIn('before timeout',(runner.root/'timeout.log').read_text());self.assertFalse(runner.logs)

    def test_observer_identity_checked_before_stop_and_foreign_container_never_signalled(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.runner(Path(d));runner.run=run_metadata();runner.observer='observer'
            with patch.object(cli.hw,'command',return_value='observer\n') as command, \
                 patch.object(cli.hw,'own_container',side_effect=ValueError('foreign')),patch.object(cli.hw,'stop'):
                self.assertFalse(runner.cleanup());self.assertEqual(command.call_count,1)
                self.assertIn('foreign',runner.result['cleanup_errors'][0])

    def test_rollback_uid_mismatch_and_broken_image_independence(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root);(root/'capture').mkdir();(root/'capture/run.json').write_text('{}')
            cli.atomic(root/'result.json',{'uid':os.getuid()+1,'gid':os.getgid()})
            with patch.object(cli.hw,'load_run') as load,self.assertRaisesRegex(ValueError,'UID_GID'):cli.rollback(a)
            load.assert_not_called()
            cli.atomic(root/'result.json',{'uid':os.getuid(),'gid':os.getgid(),'run_id':'a'*32})
            with patch.object(cli.hw,'load_run',return_value=run_metadata()) as load, \
                 patch.object(cli.hw,'command',return_value=''),patch.object(cli.hw,'stop',return_value={'ok':True}):
                self.assertTrue(cli.rollback(a)['ok']);load.assert_called_with(root/'capture',verify=False)
            self.assertEqual(len(list(root.glob('rollback-*.json'))),1)


class SamplingAcceptanceTests(unittest.TestCase):
    def fixture(self,root,mode='on'):
        a=args(root);a.seconds=120;a.run_dir.mkdir()
        runner=cli.Runner(a);runner.run=run_metadata();runner.run['mode']=mode
        cp=a.run_dir/'checkpoint-samples';cp.mkdir()
        sdk=a.run_dir/'sdk-samples';sdk.mkdir()
        cli.atomic(cp/'summary.json',{'errors':{},'interrupted':False,'elapsed_seconds':120,
                                     'targets':[{'observation_supported':True,'observation_enabled':True,'lost_events_after_start':0} for _ in range(3)],
                                     'perf':{'state':'recorded_not_validated','data_bytes':20}})
        cli.atomic(sdk/'summary.json',{'ok':True,'elapsed_seconds':120})
        cli.atomic(a.run_dir/'wire.sqlite3.summary.json',{'ok':True,'elapsed_seconds':120})
        return runner

    def fake_spawn(self,runner,calls):
        class Child:
            def poll(self):return 0
        def spawn(argv,name):
            calls.append(argv);(runner.root/(name+'.log')).touch()
            return Child()
        return spawn

    def test_samplers_share_uid_and_observer_has_bounded_owned_identity(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.fixture(Path(d));calls=[]
            with patch.object(runner,'snapshot',return_value={}),patch.object(cli,'check_rows'), \
                 patch.object(runner,'spawn',side_effect=self.fake_spawn(runner,calls)):
                runner.sample()
            observer=next(v for v in calls if v[0]=='docker')
            self.assertEqual(observer[observer.index('--user')+1],str(os.getuid())+':'+str(os.getgid()))
            self.assertIn('--init',observer);self.assertIn(cli.hw.RUN+'='+runner.run['run_id'],observer)
            self.assertEqual(observer[observer.index('--name')+1],runner.observer)
            self.assertTrue(cli.rt.read_json(runner.root/'sampling.json')['ok'])

    def test_off_mode_skips_sdk_collector_but_still_requires_dds_and_checkpoint(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.fixture(Path(d),'off');calls=[]
            (runner.a.run_dir/'sdk-samples/summary.json').unlink()
            with patch.object(runner,'snapshot',return_value={}),patch.object(cli,'check_rows'), \
                 patch.object(runner,'spawn',side_effect=self.fake_spawn(runner,calls)):
                runner.sample()
            self.assertEqual(len(calls),2)
            self.assertFalse(any('collect.py' in arg for argv in calls for arg in argv))

    def test_exit_zero_does_not_mask_incomplete_or_failed_sampling_evidence(self):
        for fault in ('checkpoint','interrupted','short','sdk','wire','perf','observation'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                runner=self.fixture(Path(d));calls=[]
                cp=runner.a.run_dir/'checkpoint-samples/summary.json';value=cli.rt.read_json(cp)
                if fault=='checkpoint':value['errors']={'read-error':1}
                elif fault=='interrupted':value['interrupted']=True
                elif fault=='short':value['elapsed_seconds']=119
                elif fault=='sdk':cli.atomic(runner.a.run_dir/'sdk-samples/summary.json',{'ok':False,'elapsed_seconds':120})
                elif fault=='wire':cli.atomic(runner.a.run_dir/'wire.sqlite3.summary.json',{'ok':False,'elapsed_seconds':120})
                elif fault=='observation':value['targets'][0]['observation_supported']=False
                else:runner.a.perf=True;value['perf']['state']='unavailable'
                cli.atomic(cp,value)
                with patch.object(runner,'snapshot',return_value={}),patch.object(cli,'check_rows'), \
                     patch.object(runner,'spawn',side_effect=self.fake_spawn(runner,calls)),self.assertRaises(ValueError):runner.sample()

    def test_source_gaps_are_reported_separately_and_missing_hash_pass_fails(self):
        with tempfile.TemporaryDirectory() as d:
            runner=self.fixture(Path(d));report={'ok':True,'cameras':[]}
            for name in 'abc':
                report['cameras'].append({'camera':name,'zero_source_gaps':False,'sequence_gaps':{'depth':1},
                    'summary':{'ok':True,'audit_version':2,'native_hashes_verified':True,'independent_audit_passes':2,
                               'recovered':False,'pairs':runner.a.pairs}})
            path=runner.a.run_dir/'dds-native-comparison.json';cli.atomic(path,report)
            runner.acceptance();self.assertFalse(runner.result['source_gap_free']);self.assertFalse(runner.result['crash_recovery_verified'])
            report['cameras'][0]['summary']['independent_audit_passes']=1;cli.atomic(path,report)
            with self.assertRaisesRegex(ValueError,'TWO_READ'):runner.acceptance()

    def test_final_sdk_snapshots_preserve_bytes_and_reject_symlinks(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);runner=self.fixture(root);runner.run['cameras']=[]
            for name in 'abc':
                directory=root/('diag-'+name);directory.mkdir(mode=0o700)
                p=directory/'sdk-diag-1.json';p.write_bytes(b'{"counters":{}}');p.chmod(0o600)
                if name=='b':(directory/'sdk-diag-escape.json').symlink_to(root/'plan.json')
                runner.run['cameras'].append({'name':name,'diagnostics':str(directory)})
            with patch.object(runner,'snapshot'),patch.object(runner,'command'),patch.object(cli.hw,'own_container',return_value={'Id':'owned'}):runner.evidence()
            self.assertEqual((root/'sdk-final-a-0/sdk-diag-1.json').read_bytes(),b'{"counters":{}}')
            self.assertTrue(runner.result['evidence_errors'])
            self.assertFalse((root/'sdk-final-b-0/sdk-diag-escape.json').exists())


class SavedProcessTests(unittest.TestCase):
    def test_reused_pid_never_receives_signal(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);expected={'pid':1234,'uid':os.getuid(),'pgid':1234,'session':1234,'start_ticks':10,'name':'sdk_argv'}
            cli.atomic(root/'children.json',{'uid':os.getuid(),'token':'expected','processes':[expected]})
            current={k:expected[k] for k in ('pid','uid','pgid','session','start_ticks')};current['start_ticks']=11
            with patch.object(cli,'process_identity',return_value=current),patch.object(cli.os,'killpg') as kill:
                self.assertIn('PID_REUSED',cli.stop_saved_children(root,'expected')[0]);kill.assert_not_called()

    @unittest.skipUnless(sys.platform.startswith('linux'),'Linux /proc process birth identity')
    def test_saved_live_sampler_is_stopped_by_birth_uid_group_and_token(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);runner=cli.Runner(args(root))
            child=runner.spawn([sys.executable,'-c','import time; time.sleep(60)'],'orphan-fixture')
            try:
                self.assertEqual(cli.stop_saved_children(root,runner.token),[])
                self.assertIsNotNone(child.wait(timeout=5))
            finally:
                runner.terminate(child)
                for log in runner.logs:log.close()

    def test_build_only_rollback_cleans_saved_children_without_image_or_capture(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);cli.atomic(root/'result.json',{'uid':os.getuid(),'gid':os.getgid(),'stage':'build','run_token':'token'})
            with patch.object(cli,'stop_saved_children',return_value=[]) as stop,patch.object(cli.hw,'load_run') as image:
                result=cli.rollback(args(root));self.assertTrue(result['ok']);stop.assert_called_once_with(root,'token');image.assert_not_called()


class CliProcessTests(unittest.TestCase):
    def simulator(self,root,scenario):
        log=(root.parent/'cli.log').open('w')
        child=subprocess.Popen([sys.executable,str(ROOT/'tests/fixtures/sdk_one_click_simulator.py'),scenario,str(root)],
                               stdout=log,stderr=subprocess.STDOUT,env=dict(os.environ,PYTHONDONTWRITEBYTECODE='1'))
        return child,log

    def assert_reaped(self,root):
        for path in root.glob('pid-*'):
            with self.assertRaises(ProcessLookupError,msg=path.name):os.kill(int(path.read_text()),0)

    def test_real_cli_runs_all_phases_with_real_sampler_children(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)/'run';child,log=self.simulator(root,'success')
            try:self.assertEqual(child.wait(timeout=25),0)
            finally:
                if child.poll() is None:child.kill();child.wait()
                log.close()
            result=cli.rt.read_json(root/'result.json')
            self.assertTrue(result['ok']);self.assertTrue(result['hardware_acceptance_verified'])
            self.assertEqual(result['uid'],os.getuid());self.assertTrue((root/'capture-stopped').exists())
            self.assertEqual((root/'capture/recording.partial').read_bytes(),b'synthetic original evidence')
            self.assert_reaped(root)

    def test_real_sigterm_and_failed_sampler_preserve_evidence_and_stop_owned_capture(self):
        for scenario in ('cancel','sample-fail'):
            with self.subTest(scenario=scenario),tempfile.TemporaryDirectory() as d:
                root=Path(d)/'run';child,log=self.simulator(root,scenario)
                try:
                    if scenario=='cancel':
                        deadline=time.monotonic()+15
                        while not (root/'sampler-ready').exists() and child.poll() is None:
                            self.assertLess(time.monotonic(),deadline);time.sleep(.05)
                        child.send_signal(signal.SIGTERM)
                    self.assertEqual(child.wait(timeout=25),1)
                finally:
                    if child.poll() is None:child.kill();child.wait()
                    log.close()
                result=cli.rt.read_json(root/'result.json')
                self.assertFalse(result['ok']);self.assertEqual(result['failed_stage'],'sample')
                self.assertTrue((root/'capture-stopped').exists());self.assertTrue(result['rollback_argv'])
                self.assertEqual((root/'capture/recording.partial').read_bytes(),b'synthetic original evidence')
                self.assert_reaped(root)


if __name__=='__main__':unittest.main()
