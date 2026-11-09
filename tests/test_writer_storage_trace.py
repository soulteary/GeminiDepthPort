"""Device scope, kernel-thread evidence and conservative attribution contracts."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
def module(name):
    spec=importlib.util.spec_from_file_location(name,ROOT/'scripts/validation'/(name+'.py'))
    result=importlib.util.module_from_spec(spec);spec.loader.exec_module(result);return result
storage=module('writer_storage_trace');analysis=module('analyze_storage_trace')
import test_writer_syscall_trace as original
BINDING={'filesystem_dev':[259,2],'block_dev':[259,0], 'filesystem_kernel_dev':(259<<20)|2,'block_kernel_dev':259<<20}

def line(tid,name,fields,at):return original.line(tid,name,fields,at=at)
def records():
    return [line(77,'ext4_sync_file_enter','dev 259,2 ino 42 parent 1 datasync 0','10.000000'),
            line(888,'jbd2_start_commit','dev 259,2 transaction 4 sync 1','10.010000'),
            line(999,'block_rq_issue','259,0 F 0 () 0 + 0 [kworker]','10.020000'),
            line(1000,'block_rq_complete','259,0 F () 0 + 0 [0]','10.300000'),
            line(888,'jbd2_end_commit','dev 259,2 transaction 4 sync 1 head 3','10.320000'),
            line(77,'ext4_sync_file_exit','dev 259,2 ino 42 ret 0','10.330000')]

class FakeStorage(storage.StorageSession):
    def _capability(self):return {'available':True,'events':list(storage.EVENTS),'scope':'synthetic fixture'}
    def _target_abi(self,pid):return {'format':'synthetic'}
    def _calibrate_pid_namespace(self):
        with patch.object(storage.syscalls.threading,'get_native_id',return_value=os.getpid()):return super()._calibrate_pid_namespace()
    def _make_instance(self):
        super()._make_instance()
        for name in ('tracing_on','current_tracer','trace_clock','buffer_size_kb','set_event_pid','events/enable','trace','trace_marker','options/markers','options/overwrite','options/event-fork','options/context-info','options/irq-info'):
            p=self.instance/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text('0\n')
        (self.instance/'buffer_total_size_kb').write_text('512\n')
        for event in storage.EVENTS:
            cat,name=event.split(':');directory=self.instance/'events'/cat/name;directory.mkdir(parents=True)
            (directory/'enable').write_text('0\n');(directory/'filter').write_text('none\n')
        for n in range(2):
            p=self.instance/'per_cpu'/f'cpu{n}'/'stats';p.parent.mkdir(parents=True)
            p.write_text('overrun: 0\ncommit overrun: 0\ndropped events: 0\n')
        os.mkfifo(self.instance/'trace_pipe',0o600);self.writer=os.open(self.instance/'trace_pipe',os.O_RDWR|os.O_NONBLOCK)
    def _write(self,relative,value):
        super()._write(relative,value)
        if relative=='trace_marker':(self.instance/'trace').write_text(line(os.getpid(),'tracing_mark_write',value,'10.0'))
    def _remove_instance(self):
        if self.instance is not None:os.close(self.writer);shutil.rmtree(self.instance)

class StorageDecoderTests(unittest.TestCase):
    def test_kernel_threads_flush_partial_and_requeue_are_kept_without_fake_request_pairing(self):
        rows=[];decoder=storage.StorageDecoder(BINDING,rows.append)
        for raw in records():decoder.line(raw)
        decoder.line(line(999,'block_rq_requeue','259,0 WS () 128 + 8 [kworker]','10.340000'))
        decoder.line(line(1000,'block_rq_complete','259,0 WS () 128 + 4 [0]','10.350000'))
        decoder.line(line(1000,'block_rq_complete','259,0 WS () 132 + 4 [0]','10.360000'))
        self.assertEqual(rows[2]['context_tid'],999);self.assertTrue(rows[2]['flush'])
        self.assertFalse(rows[2]['unique_request_identity']);self.assertNotIn('duration_ns',rows[3])
        self.assertEqual(rows[-1]['nr_sectors'],4);self.assertEqual(rows[-4]['duration_ns'],330000000)
        self.assertEqual(decoder.report()['paired_journal_transactions'],1)
        self.assertEqual(decoder.report()['requeue_events'],1)
        self.assertNotIn('kworker',json.dumps(rows));self.assertNotIn('native',json.dumps(rows))
    def test_rwbs_preflush_fua_and_ambiguous_flush_remain_distinct(self):
        self.assertEqual(storage.rwbs_flags('FWS'),{'operation':'W','preflush':True,'fua':False,'flush':True})
        self.assertEqual(storage.rwbs_flags('WFS'),{'operation':'W','preflush':False,'fua':True,'flush':False})
        self.assertTrue(storage.rwbs_flags('FWFS')['fua'])
        self.assertIsNone(storage.rwbs_flags('FF')['fua'])
        self.assertFalse(storage.rwbs_flags('F')['fua'])
        with self.assertRaises(ValueError):storage.rwbs_flags('SECRET/path')
    def test_unknown_schema_filter_escape_loss_and_state_limits_fail_closed(self):
        for raw in [line(1,'block_rq_issue','8,0 WS 512 () 0 + 1 [task]','10.0'),'CPU:0 LOST 2 EVENTS',line(1,'ext4_sync_file_enter','dev 259,2 ino 1','10.0'),line(1,'other','dev 259,2','10.0')]:
            with self.subTest(raw=raw),self.assertRaises(ValueError):storage.StorageDecoder(BINDING,lambda r:None).line(raw)
        decoder=storage.StorageDecoder(BINDING,lambda r:None,max_pending=1)
        decoder.line(records()[0])
        with self.assertRaisesRegex(ValueError,'PENDING_LIMIT'):decoder.line(line(78,'ext4_sync_file_enter','dev 259,2 ino 43 datasync 0','10.0'))
    def test_empty_and_censored_device_activity_is_not_claimed_as_attribution(self):
        session=storage.StorageSession(Path('/unused'),1024**2,1,binding=BINDING)
        self.assertEqual(session._decoder_problem({}),'STORAGE_NO_PAIRED_EXT4_SYNCS')
        self.assertEqual(session._decoder_problem({'paired_ext4_syncs':1}),'STORAGE_NO_BLOCK_EVENTS')
        self.assertEqual(session._decoder_problem({'paired_ext4_syncs':1,'block_events':1}),'STORAGE_NO_PAIRED_JOURNAL_COMMITS')

class StorageSessionTests(unittest.TestCase):
    def test_private_device_filters_have_no_pid_filter_and_final_drain_is_hashed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);proc=root/'proc';original.process_files(proc,10)
            (proc/'self/ns').mkdir(parents=True);(proc/'self/ns/time').symlink_to('time:[42]');(proc/'self/ns/pid').symlink_to('pid:[42]')
            (proc/'self/timens_offsets').write_text('monotonic 0 0\nboottime 0 0\n')
            tracefs=root/'tracefs';(tracefs/'instances').mkdir(parents=True)
            session=FakeStorage(root/'out',4*1024**2,5,tracefs=tracefs,proc=proc,binding=BINDING)
            self.assertTrue(session.start([SimpleNamespace(pid=10,poll=lambda:None)])['ok'])
            instance=session.instance
            self.assertEqual((instance/'set_event_pid').read_text().strip(),'')
            self.assertEqual((instance/'events/block/block_rq_issue/filter').read_text().strip(),f"dev == {259<<20}")
            self.assertEqual((instance/'events/ext4/ext4_sync_file_enter/filter').read_text().strip(),f"dev == {(259<<20)|2}")
            os.write(session.writer,''.join(records()).encode())
            result=session.finish()
            self.assertTrue(result['ok'],result);self.assertEqual(result['decoded']['paired_ext4_syncs'],1)
            self.assertFalse(instance.exists());self.assertTrue((root/'out/SHA256SUMS').is_file())
            self.assertEqual(session.finish(),result)

    def test_two_independent_fifo_readers_preserve_readback_bursts_with_slow_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);proc=root/'proc';(proc/'self/ns').mkdir(parents=True)
            for name in ('time','pid'):(proc/'self/ns'/name).symlink_to(name+':[42]')
            original.process_files(proc,10)
            (proc/'self/timens_offsets').write_text('monotonic 0 0\nboottime 0 0\n')
            tracefs=root/'tracefs';(tracefs/'instances').mkdir(parents=True)
            writer=original.FakeTraceSession(root/'writer',32*1024**2,10,tracefs=tracefs,proc=proc)
            device=FakeStorage(root/'storage',32*1024**2,10,tracefs=tracefs,proc=proc,binding=BINDING)
            target=SimpleNamespace(pid=10,poll=lambda:None);errors=[]
            class SlowOutput:
                def __init__(self,stream):self.stream=stream;self.calls=0
                def write(self,data):
                    self.calls+=1
                    if self.calls%256==0:time.sleep(.001)
                    return self.stream.write(data)
                def close(self):self.stream.close()
            sessions=[writer,device]
            try:
                for session in sessions:
                    self.assertTrue(session.start([target])['ok']);session.stream=SlowOutput(session.stream)
                    session.start_draining()
                # A complete sched interval makes the writer fixture meaningful.
                raws=[(original.line(10,'sched_switch','prev_pid=10 prev_state=D ==> next_pid=91')+
                       original.line(91,'sched_switch','prev_pid=91 prev_state=R ==> next_pid=10','100.100001')+
                       ''.join(original.line(10,'sys_enter_fsync','fd=3',f'{101+n}.000000')+
                               original.line(10,'sys_exit_fsync','ret=0',f'{101+n}.000001') for n in range(4000))).encode(),
                      (''.join(records())*4000).encode()]
                def produce(fd,raw):
                    sent=0;deadline=time.monotonic()+8
                    try:
                        while sent<len(raw):
                            if time.monotonic()>deadline:raise AssertionError('bounded FIFO producer stalled')
                            try:sent+=os.write(fd,raw[sent:sent+65536])
                            except BlockingIOError:time.sleep(.001)
                    except Exception as error:errors.append(str(error))
                threads=[threading.Thread(target=produce,args=(fd,raw)) for fd,raw in zip([writer.pipe_writer,device.writer],raws)]
                for thread in threads:thread.start()
                for thread in threads:thread.join(9);self.assertFalse(thread.is_alive())
                self.assertEqual(errors,[])
                reports=[session.finish() for session in sessions]
                self.assertTrue(all(report['ok'] for report in reports),reports)
                self.assertEqual(reports[0]['decoded']['paired_syscalls'],4000)
                self.assertEqual(reports[1]['decoded']['paired_ext4_syncs'],4000)
                self.assertEqual(reports[1]['decoded']['block_events'],8000)
                self.assertTrue(all(not session.drain_thread.is_alive() for session in sessions))
                self.assertTrue(all(report['loss_observation_bytes']<=report['loss_observation_limit_bytes'] for report in reports))
            finally:
                for session in sessions:
                    if session.state!='not-started':session.finish()


class AttributionTests(unittest.TestCase):
    def test_role_requires_inode_and_fd_sample_inside_matching_syscall_and_no_block_latency_is_invented(self):
        events=[];decoder=storage.StorageDecoder(BINDING,events.append)
        for raw in records():decoder.line(raw)
        call={'tid':77,'syscall':'fsync','fd':3,'entry_monotonic_ns':9990000000,'monotonic_ns':10340000000,'duration_ns':350000000,
              'fd_sample':{'available':True,'role':'payload','dev':[259,2],'inode':42,'observed_monotonic_ns':10050000000,'sample_finished_monotonic_ns':10060000000}}
        result=analysis.correlate([call],events,9990000000,10400000000)
        self.assertEqual(result['fsync_correlations'][0]['file_role'],'payload');self.assertFalse(result['hardware_cause_verified'])
        self.assertEqual(result['fsync_correlations'][0]['journal_overlap_transactions'],[4])
        self.assertEqual(result['fsync_correlations'][0]['block_event_overlap_counts'],{'block_rq_issue':1,'block_rq_complete':1})
        self.assertEqual(result['fsync_correlations'][0]['flush_flag_event_overlap_count'],2)
        call['fd_sample']['sample_finished_monotonic_ns']=10400000000
        self.assertIsNone(analysis.correlate([call],events,9990000000,10400000000)['fsync_correlations'][0]['file_role'])
        call['fd_sample']['sample_finished_monotonic_ns']=10060000000;call['fd_sample']['inode']=43
        self.assertIsNone(analysis.correlate([call],events,9990000000,10400000000)['fsync_correlations'][0]['file_role'])
        self.assertNotIn('duration_ns',result['block_events'][0])


class BindingAndCapabilityTests(unittest.TestCase):
    def test_direct_ext4_partition_matches_whole_disk_and_other_mounts_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);mount=root/'recordings';mount.mkdir()
            block=root/'sys/class/block';block.mkdir(parents=True)
            actual=root/'sys/devices/pci/nvme0n1';actual.mkdir(parents=True);(actual/'dev').write_text('259:0\n')
            part=actual/'nvme0n1p2';part.mkdir();(part/'partition').write_text('2\n')
            (block/'nvme0n1').symlink_to(actual)
            dev=root/'sys/dev/block';dev.mkdir(parents=True);(dev/'259:2').symlink_to(part)
            info=root/'mountinfo';info.write_text(f'1 0 259:2 / {mount.resolve()} rw - ext4 /dev/nvme0n1p2 rw\n')
            with patch.object(storage.os,'major',return_value=259),patch.object(storage.os,'minor',return_value=2):
                result=storage.device_binding(mount,'nvme0n1',block,info)
                self.assertEqual(result['block_dev'],[259,0]);self.assertEqual(result['filesystem_kernel_dev'],(259<<20)|2)
                info.write_text(f'1 0 259:2 / {mount.resolve()} rw - xfs /dev/nvme0n1p2 rw\n')
                with self.assertRaisesRegex(ValueError,'EXT4_REQUIRED'):storage.device_binding(mount,'nvme0n1',block,info)
            with self.assertRaisesRegex(ValueError,'WHOLE_DISK'):storage.device_binding(mount,'../other',block,info)
    def test_probe_requires_all_events_and_real_dev_field_without_mutation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'available_events').write_text('\n'.join(storage.EVENTS))
            for event in storage.EVENTS:
                cat,name=event.split(':');folder=root/'events'/cat/name;folder.mkdir(parents=True)
                (folder/'format').write_text('field:dev_t dev; offset:8; size:4; signed:0;\n')
            with patch.object(storage.syscalls,'capability',return_value={'available':True}):
                result=storage.capability(root,BINDING);self.assertTrue(result['available']);self.assertEqual(len(result['event_format_sha256']),11)
                (root/'events/block/block_rq_issue/format').write_text('field:void *rq; offset:8; size:8; signed:0;')
                self.assertFalse(storage.capability(root,BINDING)['available'])
                (root/'available_events').write_text('')
                self.assertFalse(storage.capability(root,BINDING)['available'])
            self.assertFalse((root/'instances').exists())

class FdRoleTests(unittest.TestCase):
    def test_fd_identity_is_sampled_bounded_and_never_exports_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);proc=root/'proc';original.process_files(proc,10);original.process_files(proc,77)
            target=root/'payload-00000000000000000001.bin';target.write_bytes(b'private pixels')
            (proc/'77/fd').mkdir();(proc/'77/fd/3').symlink_to(target)
            session=storage.FileSession(root/'out',1024**2,1,proc=proc)
            session.targets=[{'process':SimpleNamespace(pid=10,poll=lambda:None),'identity':{'pid':10,'start_ticks':91}}]
            rows=[]
            with patch.object(storage.syscalls.TraceSession,'_emit',side_effect=rows.append):
                session._emit({'kind':'sys_enter_fsync','tid':77,'tid_generation':1,'root_pid':10,'fd':3})
                session._emit({'kind':'sys_exit_fsync','tid':77,'tid_generation':1,'root_pid':10,'fd':3})
                self.assertEqual(rows[0]['fd_sample']['role'],'payload')
                self.assertEqual(rows[0]['fd_sample'],rows[1]['fd_sample']);self.assertEqual(session.fd_samples,{})
                with patch.object(storage.syscalls,'proc_identity',side_effect=AssertionError('rate-limited lookup touched proc')):
                    session.next_fd_sample_ns=time.monotonic_ns()+1_000_000_000
                    session._emit({'kind':'sys_enter_fsync','tid':77,'tid_generation':1,'root_pid':10,'fd':3})
                    self.assertEqual(rows[-1]['fd_sample']['reason'],'STORAGE_FD_SAMPLE_RATE_LIMIT')
                    self.assertFalse(rows[-1]['fd_sample']['available'])
                    session._emit({'kind':'sys_exit_fsync','tid':77,'tid_generation':1,'root_pid':10,'fd':3})
                session.next_fd_sample_ns=0
                (proc/'77/fd/3').unlink()
                session._emit({'kind':'sys_enter_fsync','tid':77,'tid_generation':1,'root_pid':10,'fd':3})
                self.assertFalse(rows[-1]['fd_sample']['available'])
                session._emit({'kind':'sched_process_exit','tid':77,'tid_generation':1,'context_tid':77})
                self.assertEqual(session.fd_samples,{})
            encoded=json.dumps(rows);self.assertNotIn(str(root),encoded);self.assertNotIn('private pixels',encoded)

class LongPlanTests(unittest.TestCase):
    def test_device_trace_has_separate_budget_and_cannot_be_optional(self):
        import test_writer_long_run as previous
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);a=previous.args(root,root/'binary')
            with patch.object(previous.long.sys,'platform','linux'):base=previous.long.plan(a)
            a.storage_trace=True;a.disk='nvme0n1';a.storage_trace_max_bytes=previous.long.MIB;a.max_output_bytes=4*previous.long.GIB
            with patch.object(previous.long.sys,'platform','linux'):
                plan=previous.long.plan(a)
                self.assertEqual(plan['planned_upper_bound_bytes']-base['planned_upper_bound_bytes'],4*previous.long.MIB)
                a.allow_unavailable_trace=True
                with self.assertRaisesRegex(ValueError,'MUST_BE_REQUIRED'):previous.long.plan(a)


class AnalysisEvidenceTests(unittest.TestCase):
    def test_end_to_end_hashes_time_domain_and_tampering_are_enforced(self):
        import hashlib
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);dev=root/'storage';dev.mkdir()
            rows=[];decoder=storage.StorageDecoder(BINDING,rows.append)
            for raw in records():decoder.line(raw)
            call={'kind':'sys_exit_fsync','tid':77,'syscall':'fsync','fd':3,'entry_monotonic_ns':9990000000,'monotonic_ns':10340000000,'duration_ns':350000000}
            for directory,data in ((root,[call]),(dev,rows)):
                (directory/'trace-events.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in data))
                (directory/'summary.json').write_text(json.dumps({'ok':True,'loss_counters_complete':True,'lost_events':0,'started_monotonic_ns':0,'finished_monotonic_ns':20000000000}))
                (directory/'ready.json').write_text(json.dumps({'clock':'mono','targets':[{'pid':77,'start_ticks':91}]}))
                def seal(directory):
                    (directory/'SHA256SUMS').write_text(''.join(hashlib.sha256((directory/name).read_bytes()).hexdigest()+'  '+name+'\n' for name in ('ready.json','summary.json','trace-events.jsonl')))
                seal(directory)
            result=analysis.analyze(root,9900000000,11000000000)
            self.assertEqual(len(result['fsync_correlations']),1);self.assertFalse(result['unique_block_request_identity'])
            with self.assertRaisesRegex(ValueError,'WINDOW_BOUND'):analysis.analyze(root,0,31000000000)
            with self.assertRaisesRegex(ValueError,'OUTSIDE_CAPTURE'):analysis.analyze(root,19000000000,21000000000)
            (dev/'ready.json').write_text(json.dumps({'clock':'mono','targets':[{'pid':77,'start_ticks':92}]}));seal(dev)
            with self.assertRaisesRegex(ValueError,'TARGET_OR_CLOCK'):analysis.analyze(root,9900000000,11000000000)
            (dev/'ready.json').write_bytes((root/'ready.json').read_bytes());seal(dev)
            (dev/'trace-events.jsonl').write_text((dev/'trace-events.jsonl').read_text()+'\n')
            with self.assertRaises((ValueError,json.JSONDecodeError)):analysis.analyze(root,9900000000,11000000000)

class CombinedCleanupTests(unittest.TestCase):
    def test_storage_finish_error_still_finishes_owned_syscall_trace_and_cannot_pass(self):
        class Session:
            state='running';failure=None
            def __init__(self,root,fail=False):self.output=root;self.fail=fail;self.closed=False
            def finish(self):
                self.closed=True
                if self.fail:raise OSError('synthetic device drain failure')
                return {'ok':True,'loss_counters_complete':True,'lost_events':0,'event_bytes':1}
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);trace=root/'trace';trace.mkdir()
            combined=object.__new__(storage.CombinedSession);combined.file=Session(trace);combined.storage=Session(trace/'storage',True)
            combined.binding=BINDING;combined.failure=None;combined.finished=None
            result=combined.finish();self.assertFalse(result['ok']);self.assertTrue(combined.file.closed)
            self.assertIsNone(result['lost_events']);self.assertIsNotNone(result['storage_finish_error'])
            self.assertTrue((root/'trace-combined.json').is_file());self.assertEqual(combined.finish(),result)

if __name__=='__main__':unittest.main()
