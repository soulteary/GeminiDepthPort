"""Explicit new-run retention: real files, durable receipts, no camera access."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE=Path(__file__).resolve().parents[1]/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
import retention
import run_retest as retest
import run_hardware as one


def fixture(root):
    group=root/'on-01';group.mkdir(mode=0o700)
    one.atomic(group/'result.json',{'ok':True,'hardware_acceptance_verified':True,'uid':os.getuid(),'gid':os.getgid()})
    cameras=[]
    for name in 'abc':
        job=group/'capture'/name/'recordings'/'capture-session';job.mkdir(parents=True)
        (job/'payload-00000000000000000001.bin').write_bytes(b'original pixels')
        (job/'records.jsonl').write_text('retained index\n')
        (job/'COMPLETE').write_text('complete')
        one.atomic(job/'manifest.json',{'version':3,'record_count':32})
        cameras.append({'camera':name,'summary':{'ok':True,'audit_version':2,'independent_audit_passes':2,
                         'native_hashes_verified':True,'recovered':False,'pairs':32}})
    one.atomic(group/'capture/dds-native-comparison.json',{'ok':True,'cameras':cameras})
    (group/'capture/wire.sqlite3').write_bytes(b'compared DDS hashes')
    return group


class RetentionTests(unittest.TestCase):
    def test_capacity_modes_do_not_lower_targets_or_normal_full_retention_formula(self):
        native=113281597440;metadata=3*31680*65536;wire=4096*1024**2
        full=retention.capacity(native,metadata,6,wire,'all')
        rolling=retention.capacity(native,metadata,6,wire,'verified-window')
        self.assertEqual(full['estimated_required_bytes'],1056712228864)
        self.assertLess(rolling['estimated_required_bytes'],full['estimated_required_bytes']//3)
        self.assertEqual(rolling['normal_groups'],6)
        self.assertEqual(rolling['native_bytes_per_group'],native)
        self.assertFalse(rolling['successful_raw_payloads_retained'])
        self.assertGreater(rolling['estimated_required_bytes'],native+6*metadata)
        with self.assertRaises(ValueError):retention.capacity(native,metadata,6,wire,'unknown')

    def test_release_only_payloads_after_audits_keeps_old_failure_and_indices(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);group=fixture(root);old=root/'old.partial';old.mkdir();(old/'pixel.bin').write_bytes(b'old failure')
            result=retention.release(group,32,stopped=True)
            self.assertFalse(result['raw_replay_available'])
            receipt=json.loads((group/'payload-retention.json').read_text())
            self.assertEqual(receipt['state'],'released');self.assertEqual(len(receipt['files']),4)
            for name in 'abc':
                job=group/'capture'/name/'recordings/capture-session'
                self.assertFalse((job/'payload-00000000000000000001.bin').exists())
                self.assertEqual((job/'records.jsonl').read_text(),'retained index\n')
                self.assertTrue((job/'COMPLETE').exists())
            self.assertFalse((group/'capture/wire.sqlite3').exists())
            self.assertEqual((old/'pixel.bin').read_bytes(),b'old failure')
            with self.assertRaisesRegex(ValueError,'NO_RESUME'):retention.release(group,32,stopped=True)

    def test_refusals_leave_every_payload_unchanged(self):
        for mode in ('running','failed','audit','passes','pairs','partial','symlink','hardlink','database-live','missing-complete','changed'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as tmp:
                root=Path(tmp);group=fixture(root);job=group/'capture/a/recordings/capture-session'
                outside=root/'outside';outside.write_bytes(b'do not touch')
                if mode=='failed':one.atomic(group/'result.json',{'ok':False,'hardware_acceptance_verified':True})
                if mode in ('audit','passes','pairs'):
                    path=group/'capture/dds-native-comparison.json';v=json.loads(path.read_text())
                    if mode=='audit':v['ok']=False
                    else:v['cameras'][0]['summary']['independent_audit_passes' if mode=='passes' else 'pairs']=1
                    one.atomic(path,v)
                if mode=='partial':job.rename(job.with_name('capture.partial'))
                if mode in ('symlink','hardlink'):
                    payload=job/'payload-00000000000000000001.bin';payload.unlink()
                    if mode=='symlink':payload.symlink_to(outside)
                    else:os.link(outside,payload)
                if mode=='database-live':(group/'capture/wire.sqlite3-journal').write_bytes(b'active')
                if mode=='missing-complete':(job/'COMPLETE').unlink()
                original_atomic=retention.one.atomic
                def changed_after_receipt(path,value):
                    original_atomic(path,value)
                    if Path(path).name=='payload-retention.json':(job/'payload-00000000000000000001.bin').write_bytes(b'new bytes')
                with patch.object(retention.one,'atomic',side_effect=changed_after_receipt if mode=='changed' else original_atomic):
                    with self.assertRaises((ValueError,OSError)):retention.release(group,32,stopped=mode!='running')
                self.assertEqual(outside.read_bytes(),b'do not touch')
                for name in 'bc':self.assertEqual((group/f'capture/{name}/recordings/capture-session/payload-00000000000000000001.bin').read_bytes(),b'original pixels')

    def test_capacity_cli_is_read_only_without_docker_or_output_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);plan={'cameras':[]}
            for name in 'abc':
                p=root/(name+'.json');one.atomic(p,{'color':{'width':640,'height':480,'fps':30},'depth':{'width':640,'height':400,'fps':30}})
                plan['cameras'].append({'name':name,'capture_config':str(p)})
            one.atomic(root/'plan.json',plan)
            args=['run_retest.py','capacity','--root',str(root/'not-created'),'--plan',str(root/'plan.json'),'--image','unused','--max-bytes','137438953472','--ros-domain-id','78','--retention','verified-window']
            with patch.object(sys,'argv',args),patch.object(one,'local_rootful',side_effect=AssertionError('Docker must not be used')):
                self.assertIn(retest.main(),(0,1))
            self.assertFalse((root/'not-created').exists())



class RollingBatchIntegration(unittest.TestCase):
    def test_real_summary_and_release_allow_six_groups_and_keep_failed_group(self):
        import test_sdk_ab as fixtures
        for fail_second in (False,True):
            with self.subTest(fail_second=fail_second),tempfile.TemporaryDirectory() as tmp:
                a=fixtures.BatchWorkflowTests().setup_case(Path(tmp));a.retention='verified-window'
                calls=[];image='sha256:'+'a'*64
                def command(controller,argv,name,timeout):
                    calls.append(name);root=a.root/name;root.mkdir()
                    manifest=fixtures.rt.read_json(a.root/'manifest.json')
                    fixtures.fixture(root,name.split('-')[0])
                    audits=[]
                    for n in 'abc':
                        cp=root/'capture'/n/'config';binding=fixtures.rt.read_json(cp/'experiment.json')
                        binding['source_config_sha256']=manifest['source_configs'][n];one.atomic(cp/'experiment.json',binding)
                        job=root/'capture'/n/'recordings/capture-session';job.mkdir(parents=True)
                        (job/'payload-00000000000000000001.bin').write_bytes(b'synthetic accepted payload')
                        (job/'COMPLETE').write_text('complete');(job/'records.jsonl').write_text('retained index\n')
                        one.atomic(job/'manifest.json',{'version':3,'record_count':a.pairs})
                        audits.append({'camera':n,'summary':{'ok':True,'audit_version':2,'independent_audit_passes':2,'native_hashes_verified':True,'recovered':False,'pairs':a.pairs}})
                    one.atomic(root/'capture/dds-native-comparison.json',{'ok':True,'cameras':audits})
                    (root/'capture/wire.sqlite3').write_bytes(b'audited database')
                    for rel in ('result.json','capture/run.json'):
                        path=root/rel;v=fixtures.rt.read_json(path);v['runtime_sha256']=manifest['runtime_sha256'];one.atomic(path,v)
                    if fail_second and name=='off-01':
                        path=root/'final-status.json';v=fixtures.rt.read_json(path)
                        v['cameras'][0]['capture']['raw_recording']['disk_stages']['failed_calls']={'fsync':1};one.atomic(path,v)
                with patch.object(fixtures.rt,'validate_health'),patch.object(one.Runner,'command',command),patch.object(fixtures.hw,'image_manifest',return_value=(image,{'fixed':'inventory'})),patch.object(fixtures.hw,'load_run',return_value={'cameras':[{'name':n} for n in 'abc']}),patch.object(fixtures.hw,'own_container',return_value={'State':{'Running':False}}),patch.object(one,'rollback',return_value={'ok':True}):
                    code=fixtures.batch.execute(a)
                self.assertEqual(code,1 if fail_second else 0)
                self.assertEqual(len(calls),2 if fail_second else 6)
                self.assertEqual(json.loads((a.root/'on-01/payload-retention.json').read_text())['state'],'released')
                summary=json.loads((a.root/'summary.json').read_text())
                self.assertEqual(summary['retention_policy'],'verified-window')
                self.assertFalse(summary['successful_raw_payloads_retained'])
                self.assertEqual(summary['accepted_groups'],1 if fail_second else 6)
                if fail_second:
                    self.assertFalse((a.root/'off-01/payload-retention.json').exists())
                    self.assertEqual((a.root/'off-01/capture/a/recordings/capture-session/payload-00000000000000000001.bin').read_bytes(),b'synthetic accepted payload')

if __name__ == "__main__": unittest.main()
