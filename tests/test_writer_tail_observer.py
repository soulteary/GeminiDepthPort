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
import time
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
    write(task/'status',f'NSpid:\t{pid}\t{namespace}\n');write(task/'syscall','202 0x1 0x2 0x3 0x4 0x5 0x6 0x7 0x8')
    (task/'fd').mkdir()


def elf(machine=62,elf_class=2):
    value=bytearray(64);value[:7]=b'\x7fELF'+bytes((elf_class,1,1))
    value[16:18]=(3).to_bytes(2,'little');value[18:20]=machine.to_bytes(2,'little')
    value[20:24]=(1).to_bytes(4,'little');value[52:54]=(64).to_bytes(2,'little')
    return bytes(value)


def fixture(base):
    root=base/'capture';root.mkdir(mode=0o700);proc=base/'proc';proc.mkdir();(proc/'binary').write_bytes(elf())
    image='sha256:'+'c'*64;run_id='b'*32
    cp=root/'a/config';cp.mkdir(parents=True)
    write(cp/'capture.json',{'service':{'generation':run_id}});config_hash=observer.sha(cp/'capture.json')
    binding={'run_id':run_id,'capture_sha256':config_hash,'image_id':image,'instance_id':'d'*64}
    write(cp/'experiment.json',binding);runtime={'kind':'fixture-runtime'};write(cp/'runtime.json',runtime)
    status={'generation':run_id,'config_sha256':config_hash,'instance_id':'f'*64,'session_id':'a'*32,'pid':17,'restart_attempt':0,
            'raw_recording':{'state':'recording'},'updated_unix_ms':1,'state':'ready','ready':True}
    write(root/'a/state/status.json',status)
    (root/'a/recordings').mkdir(mode=0o700)
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
            self.assertEqual(row['threads'][0]['namespace_tid'],17)
            self.assertEqual(row['threads'][0]['host_tid'],117)
            self.assertEqual(row['worker_abi']['name'],'linux-x86_64')
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

    def test_compact_failed_status_retains_frozen_boundary_after_writer_drains(self):
        frozen={'version':1,'detected_monotonic_ns':100,'queue_at_detection':{'queued_pairs':16,
            'oldest_queued_age_ns':500_000_000},'writer_context':{
                'active_append':{'pair_sequence':8},'active_checkpoint_spans':[{'phase':'verify_wait',
                    'wait_reason':'chunk_rotation','info':{'checkpoint_id':1}}]}}
        status={'raw_recording':{'state':'failed','reason':'RAW_QUEUE_OVERFLOW',
            'overflow_observation':frozen,'writer_queue':{'queued_pairs':16},
            'disk_stages':{'checkpoint_observation':{'enabled':True,
                'active_append':{'pair_sequence':8},'events':['x'*10000]*64}}}}
        before=json.dumps(status,sort_keys=True)
        failed=observer.compact_status(status)
        self.assertEqual(failed['raw']['overflow_observation'],frozen)
        self.assertEqual(failed['checkpoint']['active_append'],{'pair_sequence':8})
        self.assertLess(len(json.dumps(failed)),5000)
        self.assertEqual(json.dumps(status,sort_keys=True),before)
        status['raw_recording']['writer_queue']['queued_pairs']=0
        status['raw_recording']['disk_stages']['checkpoint_observation']['active_append']=None
        drained=observer.compact_status(status)
        self.assertEqual(drained['raw']['overflow_observation'],frozen)
        self.assertEqual(drained['writer_queue']['queued_pairs'],0)
        self.assertIsNone(drained['checkpoint']['active_append'])
        self.assertIsNone(observer.compact_status({})['raw']['overflow_observation'])

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

    def test_alive_worker_with_missing_or_corrupt_status_keeps_bound_thread_observation(self):
        for fault in ('missing','json','schema'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root,proc,info=fixture(Path(d));_,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
                path=root/'a/state/status.json';original=path.read_bytes();pin=(root/'a/runtime-identity.json').read_bytes()
                if fault=='missing':path.unlink()
                elif fault=='json':path.write_text('{broken-json')
                else:write(path,{'raw_recording':{'state':'recording'}})
                row=targets[0].sample()
                self.assertEqual(row['state'],'status-unavailable');self.assertIsNone(row['status'])
                self.assertTrue(row['threads']);self.assertFalse(targets[0].terminal);self.assertFalse(targets[0].identity_failed)
                path.write_bytes(original)
                self.assertEqual(targets[0].sample()['state'],'observed')
                self.assertEqual((root/'a/runtime-identity.json').read_bytes(),pin)

    def test_status_gap_is_not_worker_exit_and_never_reports_ok_after_real_exit(self):
        for fault in ('missing','json'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                base=Path(d);root,proc,info=fixture(base);a=args(root,base/'trace');pin=(root/'a/runtime-identity.json').read_bytes()
                original_sample=observer.Attachment.sample;calls=[0]
                def sample(target,*args,**kwargs):
                    calls[0]+=1
                    if calls[0]==1:
                        path=root/'a/state/status.json'
                        if fault=='missing':path.unlink()
                        else:path.write_text('{broken-json')
                    return original_sample(target,*args,**kwargs)
                def exit_worker(_):shutil.rmtree(proc/'117',ignore_errors=True)
                with patch.object(observer.Attachment,'sample',sample):result=observer.run(a,proc,lambda *_:info,sleep=exit_worker)
                self.assertTrue(result['workflow_completed']);self.assertFalse(result['ok'])
                self.assertTrue(result['full_selected_worker_lifecycle_end_observed'])
                self.assertFalse(result['status_evidence_complete']);self.assertFalse(result['diagnostic_data_complete'])
                self.assertIn('status-unavailable:a',result['diagnostic_gaps'])
                rows=[json.loads(line) for line in (a.output/'samples.jsonl').read_text().splitlines()]
                self.assertEqual(rows[0]['targets'][0]['state'],'status-unavailable')
                self.assertTrue(rows[0]['targets'][0]['threads'])
                self.assertEqual(rows[-1]['targets'][0]['state'],'exited')
                self.assertEqual((root/'a/runtime-identity.json').read_bytes(),pin)


class PointEvidenceTests(unittest.TestCase):
    def attach(self,base,machine=62):
        root,proc,info=fixture(base);(proc/'binary').write_bytes(elf(machine))
        _,targets,_=observer.load_attachments(root,[],proc,lambda *_:info)
        return root,proc,info,targets[0],proc/'117/task/117'

    def sync_file(self,root,task,number=74,name='chunk.bin'):
        target=root/'a/recordings'/name;target.write_bytes(b'owned image bytes')
        (task/'fd/9').symlink_to(target)
        write(task/'syscall',str(number)+' 0x9 0xdeadbeef 0x2 0x3 0x4 0x5 0x6 0x7')
        return target

    def test_non_atomic_windows_namespace_mapping_and_changed_task_state_are_explicit(self):
        with tempfile.TemporaryDirectory() as d:
            root,proc,info,target,task=self.attach(Path(d))
            write(task/'stat',(task/'stat').read_text().replace(') S ',') R '))
            write(task/'stack','[<0>] jbd2_log_wait_commit+0x1/0x2\n')
            original=observer.read_field
            def read(path,limit):
                value=original(path,limit)
                if Path(path)==task/'stack':write(task/'stat',(task/'stat').read_text().replace(') R ',') D '))
                return value
            with patch.object(observer,'read_field',side_effect=read):row=target.sample()
            thread=row['threads'][0]
            self.assertEqual((thread['host_tid'],thread['namespace_tid']),(117,17))
            self.assertEqual(thread['namespace_tids'],[117,17])
            self.assertEqual((thread['task_state_before'],thread['task_state_after']),('R','D'))
            self.assertEqual(thread['wait_evidence']['classification'],'wait-path-snapshot-only')
            self.assertTrue(thread['wait_evidence']['task_state_changed'])
            self.assertIn('not a syscall start/end',thread['wait_evidence']['scope'])
            self.assertLessEqual(row['read_start_monotonic_ns'],thread['read_start_monotonic_ns'])
            self.assertLessEqual(thread['read_end_monotonic_ns'],row['read_end_monotonic_ns'])
            self.assertLessEqual(thread['read_start_unix_ns'],thread['read_end_unix_ns'])

    def test_abi_verified_sync_fd_matches_owned_host_file_without_register_export(self):
        for machine,number,operation in ((62,74,'fsync'),(62,75,'fdatasync'),(183,82,'fsync'),(183,83,'fdatasync')):
            with self.subTest(machine=machine,operation=operation),tempfile.TemporaryDirectory() as d:
                root,proc,info,target,task=self.attach(Path(d),machine)
                file=self.sync_file(root,task,number)
                row=target.sample();observed=row['threads'][0]['sync_file_observation']
                self.assertEqual(observed['state'],'consistent-sampled-observation')
                self.assertEqual(observed['operation'],operation)
                self.assertEqual(observed['inode'],file.stat().st_ino)
                self.assertEqual(observed['path'],'recordings/chunk.bin')
                self.assertIn('cannot be excluded',observed['consistency'])
                self.assertNotIn('deadbeef',json.dumps(row));self.assertNotIn(str(file),json.dumps(observed))

    def test_unknown_arch_compat_running_bad_schema_and_permission_never_claim_a_sync_file(self):
        for fault in ('machine','elf32','x32','running','no-current','schema','permission','mapping'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root,proc,info=fixture(Path(d));task=proc/'117/task/117'
                self.sync_file(root,task)
                if fault=='machine':(proc/'binary').write_bytes(elf(243))
                elif fault=='elf32':(proc/'binary').write_bytes(elf(62,1))
                elif fault=='x32':write(task/'syscall',str(0x40000000+74)+' 9 0 0 0 0 0 0 0')
                elif fault=='running':write(task/'syscall','running')
                elif fault=='no-current':write(task/'syscall','-1 0x123 0x456')
                elif fault=='schema':write(task/'syscall','74 9 incomplete')
                elif fault=='mapping':(task/'status').unlink()
                original=observer.read_bytes
                def read(path,limit):
                    if fault=='permission' and Path(path)==task/'syscall':raise PermissionError(errno.EACCES,'denied')
                    return original(path,limit)
                with patch.object(observer,'read_bytes',side_effect=read):
                    _,targets,_=observer.load_attachments(root,[],proc,lambda *_:info);row=targets[0].sample()
                value=row['threads'][0]['sync_file_observation']
                self.assertEqual(value['state'],'unknown');self.assertNotIn('path',value)
                self.assertNotIn('inode',value)

    def test_fd_reuse_or_syscall_change_is_unknown_but_a_new_sample_can_observe_new_file(self):
        for fault in ('fd','syscall'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root,proc,info,target,task=self.attach(Path(d));first=self.sync_file(root,task)
                second=root/'a/recordings/second.bin';second.write_bytes(b'other owned bytes')
                original=observer.owned_fd;calls=[0]
                def read(path,fd,owned):
                    value=original(path,fd,owned);calls[0]+=1
                    if calls[0]==1:
                        if fault=='fd':(task/'fd/9').unlink();(task/'fd/9').symlink_to(second)
                        else:write(task/'syscall','75 0x9 0xdeadbeef 0x2 0x3 0x4 0x5 0x6 0x7')
                    return value
                with patch.object(observer,'owned_fd',side_effect=read):value=target.sample()['threads'][0]['sync_file_observation']
                self.assertEqual(value['state'],'unknown');self.assertNotIn('inode',value)
                new=target.sample()['threads'][0]['sync_file_observation']
                self.assertEqual(new['state'],'consistent-sampled-observation')
                self.assertEqual(new['inode'],(second if fault=='fd' else first).stat().st_ino)

    def test_unowned_or_symlink_escaping_fd_paths_are_redacted(self):
        for fault in ('outside','escape','deleted'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                base=Path(d);root,proc,info,target,task=self.attach(base)
                outside=base/'private-do-not-export';outside.write_bytes(b'not owned by run')
                self.sync_file(root,task);(task/'fd/9').unlink()
                if fault=='escape':
                    escape=root/'a/recordings/escape';escape.symlink_to(outside);(task/'fd/9').symlink_to(escape)
                elif fault=='deleted':(task/'fd/9').symlink_to(str(outside)+' (deleted)')
                else:(task/'fd/9').symlink_to(outside)
                value=target.sample()['threads'][0]['sync_file_observation']
                self.assertEqual(value['state'],'unknown')
                self.assertNotIn('private-do-not-export',json.dumps(value));self.assertNotIn('path',value)

    def test_namespace_tid_or_worker_pid_change_discards_file_claim(self):
        for fault in ('namespace-tid','worker-pid'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root,proc,info,target,task=self.attach(Path(d));self.sync_file(root,task)
                original=observer.read_field
                def read(path,limit):
                    value=original(path,limit)
                    if Path(path)==task/'stack':
                        if fault=='namespace-tid':write(task/'status','NSpid:\t117\t18\n')
                        else:write(proc/'117/stat',(proc/'117/stat').read_text().replace(' 300 ',' 301 '))
                    return value
                with patch.object(observer,'read_field',side_effect=read):row=target.sample()
                if fault=='worker-pid':self.assertEqual(row['state'],'identity-rejected');self.assertIsNone(row['threads'])
                else:
                    self.assertEqual(row['threads'][0]['state'],'identity-or-schema-rejected')
                    self.assertNotIn('sync_file_observation',row['threads'][0])

    def test_sampling_gap_or_collection_emission_overrun_rejects_ok_not_workflow_completion(self):
        for fault in ('gap','collection','emission'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                base=Path(d);root,proc,info=fixture(base);a=args(root,base/'observe')
                original_read=observer.read_field;original_emit=observer.Output.emit;delayed=[False]
                def read(path,limit):
                    if fault=='collection' and Path(path)==proc/'meminfo' and not delayed[0]:
                        delayed[0]=True;time.sleep(.08)
                    return original_read(path,limit)
                def emit(out,value):
                    if fault=='emission' and value['kind']=='writer-tail-sample' and not delayed[0]:
                        delayed[0]=True;time.sleep(.08)
                    return original_emit(out,value)
                def exit_worker(_):
                    if fault=='gap':time.sleep(.09)
                    shutil.rmtree(proc/'117',ignore_errors=True)
                with patch.object(observer,'read_field',side_effect=read),patch.object(observer.Output,'emit',emit):
                    result=observer.run(a,proc,lambda *_:info,sleep=exit_worker)
                self.assertTrue(result['workflow_completed']);self.assertFalse(result['ok'])
                self.assertFalse(result['sampling_timing_complete']);self.assertFalse(result['diagnostic_data_complete'])
                self.assertIn('sampling-gap-over-threshold' if fault=='gap' else 'sampling-overrun',result['diagnostic_gaps'])
                self.assertTrue(result['capabilities']['sampled_syscall_only'])
                self.assertFalse(result['capabilities']['block_request_timeline']['available'])
                if fault!='gap':
                    rows=list(map(json.loads,(a.output/'samples.jsonl').read_text().splitlines()))
                    self.assertTrue(any(row['kind']=='writer-tail-overrun' for row in rows))

    def test_compact_fence_fields_and_abi_header_change_are_not_hidden(self):
        compact=observer.compact_status({'status_persistence':{'protected_sequence':9,'pump_errors':2}})
        self.assertEqual(compact['persistence']['protected_sequence'],9);self.assertEqual(compact['persistence']['pump_errors'],2)
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'elf';path.write_bytes(elf());original=observer.os.read
            def read(fd,limit):
                result=original(fd,limit);path.write_bytes(elf(183));return result
            with patch.object(observer.os,'read',side_effect=read),self.assertRaisesRegex(ValueError,'EXECUTABLE_CHANGED'):
                observer.executable_abi(path)
