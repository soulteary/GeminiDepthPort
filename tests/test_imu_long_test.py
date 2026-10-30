import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts/validation'))
import imu_long_test as longtest
spec = importlib.util.spec_from_file_location('long_observer',ROOT/'ros/imu_long_run_observer.py')
observer = importlib.util.module_from_spec(spec);spec.loader.exec_module(observer)


class LongRunTests(unittest.TestCase):
    def fixture(self, root):
        dbpath = root/'wire.db'
        db = observer.database(dbpath,16)
        lines = []
        for n in (1,2,3):
            stamp = n*10**9
            pair = {'kind':'pair','sequence':n}
            for stream in ('color','depth','accel','gyro'):
                data = {'sequence':n,'device_us':n*1000,'system_us':100+(n//2),'stamp_ns':stamp,
                        'sha256':'ab'*32,'frame_id':stream,'width':2,'height':2,'step':4}
                wire = {k:data[k] for k in ('sha256','frame_id','width','height','step')}
                if stream in ('color','depth'):
                    pair[stream]=data
                    meta = {'global_timestamp_us':stamp//1000,'stamp_mapping':'sdk-global-unix','sequence':n,'device_timestamp_us':data['device_us'],'system_timestamp_us':data['system_us']}
                    db.execute('INSERT INTO messages VALUES(?,?,?,?)',('/a',stream+'_metadata',stamp,json.dumps(meta)))
                else:
                    sample = {'kind':stream,'sequence':n,'device_timestamp_us':n*1000,'system_timestamp_us':100+n//2,'global_timestamp_us':stamp//1000,'xyz':[1.,2.,3.],'frame_id':stream}
                    lines.append({'kind':'imu','stamp_ns':stamp,'sample':sample})
                    wire = {'xyz':[1.,2.,3.],'frame_id':stream}
                db.execute('INSERT INTO messages VALUES(?,?,?,?)',('/a',stream,stamp,json.dumps(wire)))
            lines.append(pair)
        lines.append({'kind':'summary','ok':True,'pairs':3,'imu':[3,3],'recovered':False,'independent_audit_passes':2})
        path=root/'audit.jsonl';path.write_text(''.join(json.dumps(v)+'\n' for v in lines))
        db.commit()
        return db, path, lines

    def test_exact_source_and_dds_match_with_equal_system_metadata(self):
        with tempfile.TemporaryDirectory() as d:
            db,path,_ = self.fixture(Path(d))
            try:
                result=longtest.compare_native(db,'/a',path,0)
                self.assertTrue(result['zero_source_gaps'])
                self.assertEqual(result['matched_interior_samples'],dict(color=3,depth=3,accel=3,gyro=3))
                self.assertEqual(result['equal_system_samples']['gyro'],1)
            finally:db.close()

    def test_drops_corruption_clock_regression_and_missing_second_pass_fail_closed(self):
        for fault in ('missing-wire','extra-wire','xyz','hash','mapping','global','system','device','summary','after-summary'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                db,path,lines=self.fixture(Path(d))
                if fault=='missing-wire':db.execute("DELETE FROM messages WHERE stream='gyro' AND stamp=2000000000")
                elif fault=='extra-wire':db.execute("INSERT INTO messages VALUES('/a','gyro',1500000000,'{}')")
                elif fault=='xyz':lines[0]['sample']['xyz'][0]=999
                elif fault=='hash':next(v for v in lines if v['kind']=='pair')['depth']['sha256']='bad'
                elif fault=='mapping':db.execute("UPDATE messages SET data=? WHERE stream='color_metadata'",(json.dumps({'global_timestamp_us':1,'stamp_mapping':'sdk-system-unix'}),))
                elif fault in ('global','system','device'):
                    samples=[v['sample'] for v in lines if v['kind']=='imu' and v['sample']['kind']=='accel']
                    key={'global':'global_timestamp_us','system':'system_timestamp_us','device':'device_timestamp_us'}[fault]
                    samples[1][key]=samples[0][key]-(1 if fault=='system' else 0)
                elif fault=='summary':lines[-1]['independent_audit_passes']=1
                else:lines.append(lines[0])
                path.write_text(''.join(json.dumps(v)+'\n' for v in lines))
                try:
                    with self.assertRaises((ValueError,sqlite3.IntegrityError)):longtest.compare_native(db,'/a',path,0)
                finally:db.close()

    def test_three_camera_final_counts_and_production_readback_counters(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);db,path,_=self.fixture(root)
            for ns in ('/b','/c'):
                db.execute('INSERT INTO messages SELECT ?,stream,stamp,data FROM messages WHERE ns=?',(ns,'/a'))
            db.execute('INSERT INTO result VALUES(?)',(json.dumps({'ok':True}),));db.commit();db.close()
            cameras=[]
            for name in ('a','b','c'):
                status={'version':1,'kind':'capture-service-status','state':'stopped','ready':False,'config_sha256':'digest','session_id':'session','message_timestamp':'global','imu':{'shared_ros_context':True,'error':None},'restart_attempt':0,'publish_failures':0,'stamp_failures':0,'monotonic_rejections':0,
                        'raw_recording':{'state':'complete','whole_pair_queue_overflows':0,'written_pairs':3,'imu_samples_submitted':6,'payload_bytes_submitted':24,'disk_stages':{'validation_imu_samples':12,'validation_image_bytes':48,'append':{'samples':3,'p99_ns':{'lower_bound_ns':1,'upper_bound_ns':2}}}}}
                target=root/(name+'.json');target.write_text(json.dumps(status));cameras.append({'name':name,'namespace':'/'+name,'status_host':str(target)})
            (root/'summary.json').write_text(json.dumps({'ok':True}))
            (root/'identity.json').write_text(json.dumps({'cameras':[{'config_sha256':'digest','session_id':'session'}]*3}))
            class Args:pass
            args=Args();args.sample_dir=root;args.wire=root/'wire.db';args.audit=[n+'='+str(path) for n in ('a','b','c')];args.trim_seconds=0
            self.assertTrue(longtest.check(args,{'cameras':cameras})['ok'])
            status['raw_recording']['disk_stages']['validation_imu_samples']=6
            Path(cameras[0]['status_host']).write_text(json.dumps(status))
            with self.assertRaisesRegex(ValueError,'PRODUCTION_TWO_READBACKS'):longtest.check(args,{'cameras':cameras})

    def test_mount_binding_prefers_specific_bind_and_rejects_unnamed_volume(self):
        info={'Mounts':[{'Type':'bind','Source':'/tmp/a','Destination':'/data'}, {'Type':'bind','Source':'/tmp/b','Destination':'/data/raw'}]}
        self.assertEqual(longtest.bound_host_path(info,'/data/raw/session'),Path('/tmp/b/session').resolve())
        with self.assertRaises(ValueError):longtest.bound_host_path(info,'/data/../etc/passwd')
        info['Mounts'][1]['Type']='volume'
        with self.assertRaisesRegex(ValueError,'HOST_BIND_MOUNT_REQUIRED'):longtest.bound_host_path(info,'/data/raw/session')

    def test_recovery_requires_stopped_container_and_exact_checkpoint_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);partial=root/'capture.partial';partial.mkdir()
            config=root/'config.json';config.write_text('{}')
            checkpoint={'record_count':3};(partial/'checkpoint.json').write_text(json.dumps(checkpoint))
            injection=root/'injection';injection.mkdir()
            import hashlib
            before={'identity':{'config_sha256':hashlib.sha256(config.read_bytes()).hexdigest(),'container_id':'pinned-id'},'checkpoint':{'record_count':2}}
            (injection/'before.json').write_text(json.dumps(before))
            auditor=root/'auditor';auditor.write_text('#!'+sys.executable+'\nimport sys,json\nif "--recovered" not in sys.argv:sys.exit(1)\nprint(json.dumps({"kind":"summary","ok":True,"recovered":True,"pairs":3,"independent_audit_passes":2}))\n');auditor.chmod(0o700)
            class Args:pass
            args=Args();args.camera='b';args.injection=injection;args.auditor=auditor;args.output=root/'recovery'
            plan={'cameras':[{'name':'b','config_host':str(config),'recording_host':str(partial)}]}
            with patch.object(longtest,'command',return_value=json.dumps([{'State':{'Running':False}}])):
                result=longtest.recover(args,plan)
                self.assertTrue(result['durable_prefix_preserved']);self.assertFalse(result['complete_recording'])
                self.assertFalse((partial/'COMPLETE').exists())
                checkpoint['record_count']=4;(partial/'checkpoint.json').write_text(json.dumps(checkpoint));args.output=root/'bad-recovery'
                with self.assertRaisesRegex(ValueError,'RECOVERY_PREFIX_COUNT'):longtest.recover(args,plan)
            args.output=root/'running-recovery'
            with patch.object(longtest,'command',return_value=json.dumps([{'State':{'Running':True}}])):
                with self.assertRaisesRegex(ValueError,'CONTAINER_MUST_REMAIN_STOPPED'):longtest.recover(args,plan)

    def test_no_crash_without_explicit_execution_and_private_bounded_database(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'wire.db';db=observer.database(path,16)
            self.assertEqual(path.stat().st_mode&0o777,0o600)
            self.assertEqual(db.execute('PRAGMA max_page_count').fetchone()[0],4096)
            db.close()
            with self.assertRaises(FileExistsError):observer.database(path,16)
        class Args:execute=False
        with self.assertRaisesRegex(ValueError,'EXPLICIT_EXECUTE_REQUIRED'):longtest.crash(Args(),{})

if __name__=='__main__':unittest.main()
