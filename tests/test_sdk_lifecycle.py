"""Full lifecycle telemetry and c063450 contracts without Docker or cameras."""
import copy
import json
import subprocess
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import sdk_lifecycle as lifecycle
import run_hardware as one
import run_ab as batch
import run_retest as retest
from test_sdk_one_click import args,run_metadata,status


def observed(a,state='recording'):
    rows=status(a,state)
    for row in rows['cameras']:
        s=row['capture'];s['imu'].update(queue_high_water=4,drain_interval={'samples':2})
        s['status_persistence']={'queue_wait':{},'critical_wait':{},'critical_event_capacity':16,
            'critical_events':[{'sequence':1,'completed_sequence':1,'entered_unix_ms':1,'returned_unix_ms':2,'wall_ns':100,'ok':True}]}
        s['cleanup']={'history_capacity':8,'completed':[]}
        s['raw_recording']['disk_stages']={'failed_calls':{}}
    return rows


class Target:
    def sample(self,now):return {'process':{'host_pid':123,'cpu_ticks':12},'status':{'state':'ready'}},[]


class LifecycleTests(unittest.TestCase):
    def test_full_trace_continues_past_window_and_keeps_failure_and_cleanup(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root);stage=['start']
            collector=lifecycle.Lifecycle(root/'trace',run_metadata(),dict.fromkeys('abc',128),lambda:stage[0],factory=lambda _:Target(),interval=.2)
            for name,state in [('wait-healthy','recording'),('sample','recording'),('wait-complete','draining'),('stop','complete')]:
                stage[0]=name;collector.observe(observed(a,state));time.sleep(.22)
            result=collector.stop(successful=True)
            self.assertTrue(result['sampling_ok']);self.assertTrue(result['complete_observed'])
            self.assertEqual(collector.stop(),result)
            rows=[json.loads(v) for v in (root/'trace/samples.jsonl').read_text().splitlines()]
            self.assertTrue(any(v.get('phase')=='wait-complete' and v['kind']=='resource-sample' for v in rows))
            self.assertEqual(rows[0]['kind'],'lifecycle-start')
            self.assertIn('draining',result['states']['b'])
            with self.assertRaises(FileExistsError):lifecycle.Lifecycle(root/'trace',run_metadata(),{},lambda:'start')

    def test_schema_rejects_missing_telemetry_overflow_hidden_errors_and_bad_events(self):
        with tempfile.TemporaryDirectory() as d:
            a=args(Path(d));good=observed(a)['cameras'][0]['capture']
            lifecycle.validate_status(good,128)
            for fault in ['capacity','missing','error','disk','event','history']:
                value=copy.deepcopy(good)
                if fault=='capacity':value['imu']['queue_high_water']=129
                elif fault=='missing':del value['imu']['drain_interval']
                elif fault=='error':value['imu']['error']='IMU_QUEUE_OVERFLOW'
                elif fault=='disk':value['raw_recording']['disk_stages']['failed_calls']={'fsync':1}
                elif fault=='event':value['status_persistence']['critical_events'][0]['ok']=False
                else:value['status_persistence']['critical_events']*=17
                with self.subTest(fault=fault),self.assertRaises(ValueError):lifecycle.validate_status(value,128)
            good['ready']=False;good['imu']['error']='IMU_QUEUE_OVERFLOW'
            lifecycle.validate_status(good,128)

    def test_failed_samples_and_output_limit_cannot_claim_complete(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root)
            def fail(_):raise ValueError('missing process')
            c=lifecycle.Lifecycle(root/'error',run_metadata(),dict.fromkeys('abc',128),lambda:'wait-complete',factory=fail,interval=.2)
            c.observe(observed(a,'complete'));time.sleep(.22)
            with self.assertRaisesRegex(ValueError,'LIFECYCLE_INCOMPLETE'):c.stop(successful=True)
            self.assertFalse(c.finished['sampling_ok'])
            c=lifecycle.Lifecycle(root/'limit',run_metadata(),dict.fromkeys('abc',128),lambda:'sample',factory=lambda _:Target(),interval=.2,max_bytes=300)
            time.sleep(.22);summary=c.stop()
            self.assertTrue(summary['errors']);self.assertLessEqual(summary['bytes'],300)

    def test_runner_starts_before_capture_and_stops_after_hardware(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root);a.observe_lifecycle=True
            for n in 'abc':
                cp=a.run_dir/n/'config';cp.mkdir(parents=True);one.atomic(cp/'capture.json',{'imu':{'queue_capacity':128}})
            r=one.Runner(a);run=run_metadata();order=[]
            real=lifecycle.Lifecycle
            def make(*args,**kwargs):
                order.append('observer-start')
                collector=real(*args,**kwargs,factory=lambda _:Target(),interval=.2)
                deadline=time.monotonic()+2
                while collector.samples==0:
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.01)
                return collector
            def stop(_):order.append('hardware-stop')
            def snap(_):return observed(a,'complete' if r.stage in ('stop','audit','verify-dds') else 'recording')
            with patch.object(one.hw,'prepare'),patch.object(one.hw,'load_run',return_value=run),patch.object(one,'Lifecycle',side_effect=make), \
                 patch.object(one.hw,'start',side_effect=lambda *_:order.append('hardware-start')),patch.object(one.hw,'stop',side_effect=stop), \
                 patch.object(one.hw,'status',side_effect=snap),patch.object(r,'wait_for'),patch.object(r,'sample'),patch.object(r,'command'), \
                 patch.object(r,'acceptance'),patch.object(r,'evidence'):
                self.assertEqual(r.execute(),0)
            self.assertEqual(order[:2],['observer-start','hardware-start'])
            self.assertTrue(r.result['lifecycle']['complete_observed'])
            self.assertGreater(r.result['lifecycle']['finished_monotonic_ns'],r.result['lifecycle']['started_monotonic_ns'])
            a.observe_lifecycle=True
            self.assertIn('--observe-lifecycle',batch.forward(a,'on',root/'x','image'))


    def test_real_cli_lifecycle_success_and_failure_preserve_recording(self):
        for scenario in ('lifecycle','lifecycle-fail'):
            with self.subTest(scenario=scenario),tempfile.TemporaryDirectory() as d:
                root=Path(d)/'new-run'
                result=subprocess.run([sys.executable,str(ROOT/'tests/fixtures/sdk_one_click_simulator.py'),scenario,str(root)],capture_output=True,text=True,timeout=20)
                self.assertEqual(result.returncode,0 if scenario=='lifecycle' else 1,result.stdout+result.stderr)
                saved=json.loads((root/'result.json').read_text())
                self.assertEqual(saved['ok'],scenario=='lifecycle')
                trace=[json.loads(v) for v in (root/'lifecycle/samples.jsonl').read_text().splitlines()]
                self.assertTrue(any(v.get('phase')=='wait-complete' for v in trace))
                self.assertTrue((root/'lifecycle/summary.json').exists())
                self.assertEqual((root/'capture/recording.partial').read_bytes(),b'synthetic original evidence')
                if scenario=='lifecycle-fail':
                    self.assertEqual(saved['failed_stage'],'wait-complete')
                    self.assertIn('CaptureStatusFailure',saved['error'])
                    self.assertEqual([row['camera'] for row in saved['capture_failure']['cameras']],list('abc'))
                    self.assertTrue(all(row['imu_error']=='IMU_QUEUE_OVERFLOW' for row in saved['capture_failure']['cameras']))
                    self.assertTrue(any(v['kind']=='controller-status' and any((c.get('capture') or {}).get('imu',{}).get('error')=='IMU_QUEUE_OVERFLOW' for c in v['cameras']) for v in trace))

class SplitCadenceTests(unittest.TestCase):
    class Target:
        def __init__(self,losses=()):
            from types import SimpleNamespace
            self.trace=SimpleNamespace(lost=0,before_start=0,epoch=0)
            self.resources=0;self.reads=0;self.losses=losses
        def _events(self):
            if self.reads<len(self.losses):self.trace.lost=self.losses[self.reads]
            self.reads+=1
            return []
        def sample(self,now):
            self.resources+=1
            return {'process':{'host_pid':1},'status':{'updated_unix_ms':time.time_ns()//1_000_000-100}},self._events()
        def events(self,now):
            return {'source_updated_unix_ms':time.time_ns()//1_000_000-100,'epoch':self.trace.epoch},self._events()

    def test_resource_rate_remains_one_second_while_event_polls_are_fast_and_small(self):
        with tempfile.TemporaryDirectory() as d:
            target=self.Target()
            run={'cameras':[{'name':'a'}]}
            c=lifecycle.Lifecycle(Path(d)/'trace',run,{'a':128},lambda:'sample',factory=lambda _:target)
            deadline=time.monotonic()+3
            while target.reads<7:
                self.assertLess(time.monotonic(),deadline);time.sleep(.01)
            summary=c.stop()
            self.assertLessEqual(target.resources,3);self.assertGreaterEqual(target.resources,2)
            self.assertGreater(summary['event_polls'],summary['samples'])
            self.assertEqual(summary['event_interval_seconds'],.2)
            self.assertEqual(summary['resource_interval_seconds'],1.)
            self.assertGreater(summary['event_targets']['a']['maximum_read_gap_ns'],0)
            self.assertGreaterEqual(summary['event_targets']['a']['maximum_status_age_ms'],100)
            rows=[json.loads(v) for v in (Path(d)/'trace/samples.jsonl').read_text().splitlines()]
            fast=[r for r in rows if r['kind']=='event-sample']
            self.assertTrue(fast)
            self.assertTrue(all('process' not in r['cameras'][0] and 'disks' not in r for r in fast))
            self.assertTrue(all(r['cameras'][0]['poll']['read_gap_ns']>0 for r in fast))

    def test_repeated_cumulative_loss_is_reported_once_but_new_gaps_remain_terminal(self):
        with tempfile.TemporaryDirectory() as d:
            target=self.Target([0,3,3,3,5,5,5])
            c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:'wait-complete',factory=lambda _:target)
            deadline=time.monotonic()+3
            while target.reads<7:
                self.assertLess(time.monotonic(),deadline);time.sleep(.01)
            summary=c.stop()
            self.assertFalse(summary['sampling_ok'])
            self.assertEqual(summary['error_count'],2)
            self.assertEqual(summary['event_targets']['a']['lost_events_after_start'],5)
            self.assertEqual(summary['event_targets']['a']['loss_reports'],2)
            self.assertEqual(summary['errors'],['a:CHECKPOINT_EVENTS_LOST:3','a:CHECKPOINT_EVENTS_LOST:5'])
            rows=[json.loads(v) for v in (Path(d)/'trace/samples.jsonl').read_text().splitlines()]
            deltas=[r['cameras'][0]['poll']['new_lost_events'] for r in rows if r['kind'] in ('resource-sample','event-sample')]
            self.assertEqual(sum(deltas),5)

    def test_initial_history_gaps_and_new_epoch_history_are_distinct_and_terminal(self):
        class InitialTarget(self.Target):
            def _events(self):
                self.trace.before_start=4 if self.reads<3 else 7
                self.trace.epoch=0 if self.reads<3 else 1
                return super()._events()
        with tempfile.TemporaryDirectory() as d:
            target=InitialTarget()
            c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:'wait-complete',factory=lambda _:target)
            deadline=time.monotonic()+3
            while target.reads<6:
                self.assertLess(time.monotonic(),deadline);time.sleep(.01)
            with self.assertRaisesRegex(ValueError,'LIFECYCLE_INCOMPLETE'):c.stop(successful=True)
            summary=c.finished
            self.assertFalse(summary['sampling_ok'])
            self.assertEqual(summary['event_targets']['a']['lost_events_after_start'],0)
            self.assertEqual(summary['event_targets']['a']['history_events_before_start'],7)
            self.assertEqual(summary['event_targets']['a']['initial_history_reports'],2)
            self.assertEqual(summary['event_targets']['a']['epoch'],1)
            self.assertEqual(summary['errors'],['a:CHECKPOINT_HISTORY_BEFORE_FIRST_READ:4','a:CHECKPOINT_HISTORY_BEFORE_FIRST_READ:7'])


class ReadErrorScopeTests(unittest.TestCase):
    def wait(self,condition):
        deadline=time.monotonic()+3
        while not condition():
            self.assertLess(time.monotonic(),deadline);time.sleep(.005)

    def test_read_crossing_active_phase_boundary_remains_a_sampling_error(self):
        for started,finished in (('sample','cleanup'),('wait-complete','cleanup'),('cleanup','sample')):
            with self.subTest(started=started,finished=finished),tempfile.TemporaryDirectory() as d:
                entered=threading.Event();release=threading.Event();phase=[started]
                class Target(SplitCadenceTests.Target):
                    def events(self,now):
                        entered.set()
                        if not release.wait(2):raise TimeoutError('test release')
                        raise FileNotFoundError('worker exited during read')
                target=Target()
                c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:phase[0],factory=lambda _:target)
                try:
                    self.assertTrue(entered.wait(2));phase[0]=finished;release.set()
                    self.wait(lambda:c.read_errors['count']>=1)
                finally:release.set();summary=c.stop()
                self.assertFalse(summary['sampling_ok'])
                self.assertGreater(summary['read_errors']['fatal_count'],0)
                self.assertEqual(summary['read_errors']['by_transition'][started+'->'+finished],1)
                self.assertGreater(summary['event_targets']['a']['successful_reads'],0)
                self.assertGreater(summary['event_targets']['a']['failed_reads'],0)
                rows=[json.loads(v) for v in (Path(d)/'trace/samples.jsonl').read_text().splitlines()]
                cross=[v for row in rows for v in row.get('cameras',[]) if v.get('read_started_phase')==started and v.get('read_finished_phase')==finished]
                self.assertEqual(len(cross),1);self.assertTrue(cross[0]['counted_as_sampling_error'])

    def test_cleanup_read_errors_are_counted_without_claiming_all_reads_succeeded(self):
        with tempfile.TemporaryDirectory() as d:
            class Target(SplitCadenceTests.Target):
                def events(self,now):raise FileNotFoundError('stopped target')
            c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:'cleanup',factory=lambda _:Target())
            self.wait(lambda:c.read_errors['count']>=1);summary=c.stop()
            self.assertTrue(summary['sampling_ok']);self.assertFalse(summary['all_target_reads_succeeded'])
            self.assertEqual(summary['error_count'],0)
            self.assertGreater(summary['read_errors']['by_started_phase']['cleanup'],0)
            self.assertEqual(summary['read_errors']['fatal_count'],0)
            self.assertIn('not hardware or payload acceptance',summary['sampling_ok_scope'])

    def test_expected_recovery_exit_retains_crash_phase_names(self):
        with tempfile.TemporaryDirectory() as d:
            entered=threading.Event();release=threading.Event();phase=['crash-wait']
            class Target(SplitCadenceTests.Target):
                def events(self,now):
                    entered.set()
                    if not release.wait(2):raise TimeoutError('test release')
                    raise FileNotFoundError('expected SIGKILL target exit')
            c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:phase[0],factory=lambda _:Target())
            try:
                self.assertTrue(entered.wait(2));phase[0]='crash-cleanup';release.set()
                self.wait(lambda:c.read_errors['count']>=1)
            finally:release.set();summary=c.stop()
            self.assertTrue(summary['sampling_ok'])
            self.assertEqual(summary['read_errors']['fatal_count'],0)
            self.assertEqual(summary['read_errors']['by_transition']['crash-wait->crash-cleanup'],1)
            self.assertNotIn('other',summary['read_errors']['by_started_phase'])
            self.assertNotIn('other',summary['read_errors']['by_finished_phase'])

    def test_thread_resource_failure_keeps_extracted_events_and_fails_active_sampling(self):
        with tempfile.TemporaryDirectory() as d:
            class Target(SplitCadenceTests.Target):
                pid=123
                def sample(self,now):
                    result,_=super().sample(now)
                    return result,[{'event_id':1,'stage':'verify_wait'}]
            with patch.object(lifecycle.probe,'thread_cpu',side_effect=FileNotFoundError('thread tree exited')):
                c=lifecycle.Lifecycle(Path(d)/'trace',{'cameras':[{'name':'a'}]},{'a':128},lambda:'sample',factory=lambda _:Target())
                self.wait(lambda:c.samples>=1);summary=c.stop()
            self.assertFalse(summary['sampling_ok'])
            self.assertEqual(summary['read_errors']['by_operation'],{'thread-cpu':1})
            self.assertFalse(summary['all_target_reads_succeeded'])
            self.assertEqual(summary['resource_samples']['a'],1)
            rows=[json.loads(v) for v in (Path(d)/'trace/samples.jsonl').read_text().splitlines()]
            resource=next(row for row in rows if row['kind']=='resource-sample')['cameras'][0]
            self.assertEqual(resource['checkpoint_events'],[{'event_id':1,'stage':'verify_wait'}])
            self.assertTrue(resource['thread_cpu_read_error']['counted_as_sampling_error'])
