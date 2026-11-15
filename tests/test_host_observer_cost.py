import hashlib
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import run_host_observation as runner
import compare_host_observation as comparison


class ObserverCostTests(unittest.TestCase):
    def test_receipt_ignores_only_fresh_paths_and_checks_all_config_bytes(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);cameras=[]
            for name in 'abc':
                c={};c['name']=name
                for field in ('capture_config','health_config'):
                    p=root/(name+'-'+field+'.json');p.write_text('{}');c[field]=str(p)
                cameras.append(c)
            plan=root/'plan.json';plan.write_text(json.dumps(dict(cameras=cameras)))
            args=SimpleNamespace(source=ROOT,revision='a'*40,recording_path=root,disk='disk',filesystem_uuid='uuid',
                command=['python3',str(ROOT/'experiments/sdk-2.10.6-diagnostics/run_retest.py'),'run',
                         '--plan',str(plan),'--root',str(root/'run-on'),'--seconds','600'])
            first=runner.workload_receipt(args);self.assertTrue(first['supported'])
            args.command[args.command.index('--root')+1]=str(root/'run-off')
            self.assertEqual(runner.workload_receipt(args)['fingerprint'],first['fingerprint'])
            Path(cameras[0]['capture_config']).write_text('{"changed":true}')
            self.assertNotEqual(runner.workload_receipt(args)['fingerprint'],first['fingerprint'])
            args.command=['python3','-c','print(1)']
            self.assertFalse(runner.workload_receipt(args)['supported'])

    def test_failed_or_unproven_runs_never_generate_overhead_delta(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);paths={}
            for mode in ('on','off'):
                directory=root/mode;directory.mkdir();path=directory/'result.json';paths[mode]=path
                path.write_text(json.dumps(dict(observer_mode=mode,source_revision='a'*40,
                    observer_user_cpu_seconds=1,observer_system_cpu_seconds=1,wall_seconds=10,
                    driver_returncode=1,driver_failure=None,workload_receipt=None,observation=dict(capture_complete=True))))
            before={mode:path.read_bytes() for mode,path in paths.items()}
            result=comparison.compare(paths['on'],paths['off'],root/'comparison')
            self.assertFalse(result['comparable']);self.assertIsNone(result['observed_wrapper_cpu_core_difference'])
            self.assertEqual(before,{mode:path.read_bytes() for mode,path in paths.items()})

    def test_matching_successful_receipts_have_only_descriptive_wrapper_delta(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);paths={}
            for mode in ('on','off'):
                directory=root/mode;directory.mkdir();path=directory/'result.json';paths[mode]=path
                path.write_text(json.dumps(dict(observer_mode=mode,source_revision='a'*40,
                    observer_user_cpu_seconds=2 if mode=='on' else 1,observer_system_cpu_seconds=0,wall_seconds=10,
                    driver_returncode=0,driver_failure=None,observation=dict(capture_complete=True),
                    workload_unchanged=True,workload_receipt=dict(supported=True,profile={'fixture':1},fingerprint=hashlib.sha256(b'{"fixture":1}').hexdigest()))))
            result=comparison.compare(paths['on'],paths['off'],root/'comparison')
            self.assertTrue(result['comparable']);self.assertAlmostEqual(result['observed_wrapper_cpu_core_difference'],.1)
            self.assertFalse(result['hardware_acceptance_verified'])


if __name__=='__main__':unittest.main()
