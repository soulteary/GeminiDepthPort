"""Full lifecycle telemetry and c063450 contracts without Docker or cameras."""
import copy
import json
import subprocess
from pathlib import Path
import sys
import tempfile
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
                    self.assertTrue(any(v['kind']=='controller-status' and any((c.get('capture') or {}).get('imu',{}).get('error')=='IMU_QUEUE_OVERFLOW' for c in v['cameras']) for v in trace))
