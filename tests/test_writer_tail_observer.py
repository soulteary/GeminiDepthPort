"""Read-only attachment to pinned run identities and bounded thread evidence."""
import argparse
import copy
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE=Path(__file__).resolve().parents[1]/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('writer_tail_observer',HERE/'observe_writer_tails.py')
observer=importlib.util.module_from_spec(spec);spec.loader.exec_module(observer)


def write(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value) if isinstance(value,dict) else value);path.chmod(0o600)


def process(proc,pid,parent,start,args,token,namespace):
    root=proc/str(pid);root.mkdir();(root/'ns').mkdir();(root/'ns/pid').symlink_to('pid:[42]')
    fields=['0']*45;fields[0]='S';fields[1]=str(parent);fields[19]=str(start)
    write(root/'stat',f'{pid} (capture-service) '+' '.join(fields));write(root/'status',f'NSpid:\t{pid}\t{namespace}\n')
    (root/'cmdline').write_bytes(b'\0'.join(v.encode() for v in args)+b'\0')
    (root/'environ').write_bytes(b'PRIVATE=do-not-copy\0GDP_CAPTURE_INSTANCE_ID='+token.encode()+b'\0')
    (root/'exe').symlink_to(proc/'binary');expected=root/'root'/observer.rt.SERVICE.lstrip('/');expected.parent.mkdir(parents=True);expected.symlink_to(proc/'binary')
    task=root/'task'/str(pid);task.mkdir(parents=True)
    (task/'stat').write_text((root/'stat').read_text());write(task/'children','')
    write(task/'comm','capture-service');write(task/'stack','[<0>] io_schedule\n');write(task/'wchan','io_schedule');write(task/'schedstat','100 200 3')


def fixture(base):
    root=base/'capture';root.mkdir(mode=0o700);proc=base/'proc';proc.mkdir();write(proc/'binary','ELF-fixture')
    image='sha256:'+'c'*64;run_id='b'*32
    cp=root/'a/config';cp.mkdir(parents=True)
    write(cp/'capture.json',{'service':{'generation':run_id}});config_hash=observer.sha(cp/'capture.json')
    binding={'run_id':run_id,'capture_sha256':config_hash,'image_id':image,'instance_id':'d'*64}
    write(cp/'experiment.json',binding);runtime={'kind':'fixture-runtime'};write(cp/'runtime.json',runtime)
    status={'generation':run_id,'config_sha256':config_hash,'instance_id':'f'*64,'session_id':'a'*32,'pid':17,'restart_attempt':0,
            'raw_recording':{'state':'recording'},'updated_unix_ms':1,'state':'ready','ready':True}
    write(root/'a/state/status.json',status)
    camera={'name':'a','container':'owned-a','status':str(root/'a/state/status.json')}
    run={'experiment':observer.rt.EXPERIMENT,'run_id':run_id,'image_id':image,'runtime_sha256':observer.hw.digest(runtime),'cameras':[camera],
         'files':{str(p.relative_to(root)):observer.sha(p) for p in cp.iterdir()}}
    write(root/'run.json',run)
    process(proc,100,0,100,['tini'],'d'*64,1)
    process(proc,107,100,200,[observer.rt.SERVICE,'--config','/config/capture.json'],'d'*64,7)
    process(proc,117,107,300,[observer.rt.SERVICE,'--worker','--config','/config/capture.json','--expected-config-sha',config_hash,
                            '--restart-attempt','0','--supervisor-pid','7'],'f'*64,17)
    info={'Id':'owned-id','Image':image,'State':{'Running':True,'Pid':100,'StartedAt':'fixed-start'}}
    status_pin={'version':1,'container_id':'owned-id','image_id':image,'container_started_at':'fixed-start','run_id':run_id,
        'config_sha256':config_hash,'launch_instance_id':'d'*64,'instance_id':'f'*64,'session_id':'a'*32,'worker_pid':17,
        'process':{'container_init_pid':100,'pid_namespace':'pid:[42]','worker_host_pid':117,'worker_start_ticks':300,
                   'supervisor_host_pid':107,'supervisor_namespace_pid':7,'supervisor_start_ticks':200}}
    write(root/'a/runtime-identity.json',status_pin)
    write(proc/'diskstats','259 0 nvme0n1 10 0 100 20 30 0 400 50 0 60 70\n')
    write(proc/'meminfo','Dirty: 4 kB\nWriteback: 2 kB\nCached: 16 kB\nMemAvailable: 1024 kB\n')
    return root,proc,info


def args(root,output):
    return argparse.Namespace(run_dir=root,output=output,camera=[],disk=['nvme0n1'],seconds=.5,interval=.05,wait_for_bindings=0,
        tail_seconds=0,stop_file=None,max_output_mib=1,max_threads=64,all_threads=False,stack_bytes=8192,
        perf_seconds=0,perf_frequency=49,perf_max_mib=64)


class WriterTailTests(unittest.TestCase):
    def test_real_procfs_fixture_validates_pinned_chain_and_collects_thread_fields(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info=fixture(Path(d));before={str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()}
            run,targets,hashes=observer.load_attachments(root,['a'],proc,lambda *_:info)
            row=targets[0].sample()
            self.assertEqual(row['state'],'observed');self.assertEqual(row['threads'][0]['wchan']['value'],'io_schedule')
            self.assertEqual(row['threads'][0]['schedstat']['counters']['runqueue_wait_ns'],200)
            self.assertNotIn('do-not-copy',json.dumps(row));self.assertIn('a/runtime-identity.json',hashes)
            self.assertEqual({str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()},before)

    def test_pid_reuse_namespace_exec_and_status_changes_are_rejected_without_following(self):
        for fault in ('pid','namespace','exec','status'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root,proc,info=fixture(Path(d));_,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
                if fault=='pid':write(proc/'117/stat',(proc/'117/stat').read_text().replace(' 300 ',' 301 '))
                elif fault=='namespace':(proc/'117/ns/pid').unlink();(proc/'117/ns/pid').symlink_to('pid:[foreign]')
                elif fault=='exec':write(proc/'other-binary','other');(proc/'117/exe').unlink();(proc/'117/exe').symlink_to(proc/'other-binary')
                else:
                    value=observer.read_json(root/'a/state/status.json');value['session_id']='1'*32;write(root/'a/state/status.json',value)
                row=targets[0].sample();self.assertEqual(row['state'],'identity-rejected');self.assertIsNone(row['threads']);self.assertTrue(targets[0].identity_failed)

    def test_worker_exit_and_thread_exit_or_permission_are_explicit(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info=fixture(Path(d));_,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
            original=observer.read_bytes
            def read(path,limit):
                if Path(path).name=='stack':raise PermissionError(errno.EACCES,'denied')
                return original(path,limit)
            with patch.object(observer,'read_bytes',side_effect=read):row=targets[0].sample()
            self.assertEqual(row['threads'][0]['kernel_stack']['state'],'permission-denied')
            shutil.rmtree(proc/'117');row=targets[0].sample();self.assertEqual(row['state'],'exited');self.assertIsNone(row['threads'])

    def test_thread_reuse_during_batch_is_discarded(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info=fixture(Path(d));_,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
            task=proc/'117/task/117';original=observer.read_field
            def read(path,limit):
                value=original(path,limit)
                if Path(path).name=='stack':write(task/'stat',(task/'stat').read_text().replace(' 300 ',' 301 '))
                return value
            with patch.object(observer,'read_field',side_effect=read):row=targets[0].sample()
            self.assertEqual(row['threads'][0]['state'],'identity-or-schema-rejected')

    def test_full_short_lifecycle_new_output_hashes_and_bounds(self):
        with tempfile.TemporaryDirectory() as d:
            base=Path(d);root,proc,info=fixture(base);a=args(root,base/'observe')
            def exit_worker(_):shutil.rmtree(proc/'117',ignore_errors=True)
            result=observer.run(a,proc,lambda *_:info,sleep=exit_worker)
            self.assertTrue(result['ok']);self.assertEqual(result['stop_reason'],'workers-terminal')
            self.assertFalse(result['hardware_acceptance_verified']);self.assertGreaterEqual(result['samples'],2)
            for line in (a.output/'SHA256SUMS').read_text().splitlines():
                digest,name=line.split('  ');self.assertEqual(hashlib.sha256((a.output/name).read_bytes()).hexdigest(),digest)
            with self.assertRaises(FileExistsError):observer.run(a,proc,lambda *_:info)
            a.output=root/'inside'
            with self.assertRaisesRegex(ValueError,'OUTSIDE_CAPTURE'):observer.run(a,proc,lambda *_:info)
            out=observer.Output(base/'limited',10)
            try:
                with self.assertRaisesRegex(ValueError,'OUTPUT_LIMIT'):out.emit({'large':'x'*20})
            finally:out.close()

    def test_missing_pin_or_bad_container_never_creates_or_replaces_identity(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info=fixture(Path(d));pin=root/'a/runtime-identity.json';before=pin.read_bytes()
            with self.assertRaisesRegex(ValueError,'PIN_RUN_BINDING'):observer.load_attachments(root,[],proc,lambda *_:dict(info,Id='other'))
            self.assertEqual(pin.read_bytes(),before);pin.unlink()
            with self.assertRaises(FileNotFoundError):observer.load_attachments(root,[],proc,lambda *_:info)
            self.assertFalse(pin.exists())

    def test_disk_means_are_interval_counters_and_regressions_are_not_faked(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);write(proc/'diskstats','259 0 nvme0n1 10 0 100 20 30 0 400 50 0 60 70\n')
            first,previous=observer.disk_sample(['nvme0n1'],proc)
            write(proc/'diskstats','259 0 nvme0n1 12 0 120 24 34 0 440 70 0 80 90\n')
            result,previous=observer.disk_sample(['nvme0n1'],proc,previous)
            self.assertEqual(result['interval_deltas']['nvme0n1']['mean_write_request_ms'],5)
            write(proc/'diskstats','259 0 nvme0n1 1 0 1 1 1 0 1 1 0 1 1\n')
            result,_=observer.disk_sample(['nvme0n1'],proc,previous)
            self.assertEqual(result['interval_deltas']['nvme0n1']['state'],'counter-regressed')

    def test_missing_requested_disks_or_regressed_counters_never_report_ok(self):
        for fault in ('missing','regressed'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                base=Path(d);root,proc,info=fixture(base);a=args(root,base/'trace')
                if fault=='missing':a.disk=['absent-disk']
                def exit_worker(_):
                    write(proc/'diskstats','259 0 nvme0n1 1 0 1 1 1 0 1 1 0 1 1\n')
                    shutil.rmtree(proc/'117',ignore_errors=True)
                result=observer.run(a,proc,lambda *_:info,sleep=exit_worker)
                self.assertTrue(result['workflow_completed']);self.assertFalse(result['ok'])
                self.assertFalse(result['requested_disks_complete']);self.assertFalse(result['diagnostic_data_complete'])
                self.assertTrue(any(key.startswith('disk:') for key in result['diagnostic_gaps']))

    def test_compact_status_ignores_large_histories_and_reuses_identical_stacks(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info=fixture(Path(d));_,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
            status=observer.read_json(root/'a/state/status.json');status['pipeline_timing']={'large-history':['x'*1000]*50,'sdk_wait_call':{'maximum_ns':123,'samples':2}}
            original=json.dumps(status);self.assertGreater(len(original),45000);self.assertLess(len(original),70000)
            compact=observer.compact_status(status)
            self.assertLess(len(json.dumps(compact)),3000);self.assertEqual(compact['sdk_wait']['maximum_ns'],123)
            self.assertEqual(json.dumps(status),original)
            first=targets[0].sample();second=targets[0].sample()
            self.assertEqual(first['threads'][0]['kernel_stack']['state'],'available')
            self.assertEqual(second['threads'][0]['kernel_stack']['state'],'same-as-earlier-observation')
            self.assertEqual(second['threads'][0]['kernel_stack']['reference_worker_sample'],1)

    def test_uninstalled_perf_never_installs_or_spawns(self):
        with tempfile.TemporaryDirectory() as d:
            perf=observer.Perf(Path(d),seconds=2)
            with patch.object(observer.shutil,'which',return_value=None),patch.object(observer.subprocess,'Popen') as spawn:
                perf.start([]);self.assertEqual(perf.finish()['state'],'unavailable');spawn.assert_not_called()

    def test_meminfo_snapshots_preserve_zero_and_mark_missing_or_malformed(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d)
            missing=observer.memory_sample(proc)
            self.assertIsNone(missing['fields_bytes']);self.assertNotEqual(missing['state'],'available')
            write(proc/'meminfo','Dirty: 0 kB\nWriteback: 2 kB\nCached: 16 kB\n')
            result=observer.memory_sample(proc)
            self.assertEqual(result['state'],'available');self.assertEqual(result['fields_bytes']['Dirty'],0)
            self.assertEqual(result['fields_bytes']['Writeback'],2048)
            write(proc/'meminfo','Dirty: 4 kB\n')
            result=observer.memory_sample(proc)
            self.assertEqual(result['state'],'requested-fields-missing');self.assertNotIn('Writeback',result['fields_bytes'])
            write(proc/'meminfo','Dirty: invalid kB\nWriteback: 0 kB\n')
            self.assertEqual(observer.memory_sample(proc)['state'],'schema-unavailable')

    def test_perf_rechecks_identity_after_start_and_stops_only_its_own_group(self):
        with tempfile.TemporaryDirectory() as d:
            class Child:
                pid=54321
                returncode=None
                def poll(self):return self.returncode
                def wait(self,timeout=None):self.returncode=-2;return self.returncode
            child=Child();perf=observer.Perf(Path(d),seconds=1)
            target=unittest.mock.Mock(pid=117);target.check.side_effect=[{},ValueError('reused')]
            with patch.object(observer.shutil,'which',return_value='/usr/bin/perf'),patch.object(observer.subprocess,'Popen',return_value=child), \
                 patch.object(observer.os,'killpg') as kill:
                with self.assertRaisesRegex(ValueError,'PERF_TARGET_CHANGED_AFTER_START'):perf.start([target])
            self.assertEqual(target.check.call_count,2);kill.assert_called_once_with(54321,observer.signal.SIGINT)
            self.assertTrue(perf.result['stopped_for_identity_rejection'])

    def test_runtime_identity_rejection_immediately_finishes_owned_perf(self):
        with tempfile.TemporaryDirectory() as d:
            base=Path(d);root,proc,info=fixture(base);a=args(root,base/'trace')
            class FakePerf:
                def __init__(self,*_,**__):
                    self.child=unittest.mock.Mock();self.child.poll.return_value=None
                    self.result={'requested':True,'state':'running'};self.calls=0;self.max_bytes=1024**2
                def start(self,targets):
                    write(proc/'117/stat',(proc/'117/stat').read_text().replace(' 300 ',' 301 '))
                def finish(self):self.calls+=1;self.child.poll.return_value=-2;return self.result
            perf=FakePerf()
            with patch.object(observer,'Perf',return_value=perf):
                result=observer.run(a,proc,lambda *_:info)
            self.assertFalse(result['ok']);self.assertTrue(perf.result['stopped_for_identity_rejection'])
            self.assertGreaterEqual(perf.calls,2)  # Immediate detach and idempotent final cleanup.
            self.assertIn('a',result['identity_rejected_targets'])
