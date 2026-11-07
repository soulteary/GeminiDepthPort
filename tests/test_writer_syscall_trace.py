"""Private tracefs control and scalar trace contracts; fixtures are not kernel proof."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('writer_syscall_trace',ROOT/'scripts/validation/writer_syscall_trace.py')
trace=importlib.util.module_from_spec(spec);spec.loader.exec_module(trace)


def line(tid,event,fields,at='100.100000',cpu=1):
    return f'     worker-{tid} [{cpu:03d}] d..2 {at}: {event}: {fields}\n'


def process_files(proc,pid,start=91):
    root=proc/str(pid);(root/'task'/str(pid)).mkdir(parents=True,exist_ok=True)
    (root/'ns').mkdir(exist_ok=True)
    if not (root/'ns/time').is_symlink():(root/'ns/time').symlink_to('time:[42]')
    if not (root/'ns/pid').is_symlink():(root/'ns/pid').symlink_to('pid:[42]')
    fields=['0']*24;fields[0]='S';fields[19]=str(start)
    (root/'stat').write_text(f'{pid} (worker) '+' '.join(fields))
    (root/'task'/str(pid)/'comm').write_text('native-depth-re\n')


class FakeTraceSession(trace.TraceSession):
    """Regular-file/FIFO tracefs fixture; never claims the host has tracing."""
    def _capability(self):
        return {'available':True,'events':list(trace.REQUIRED_EVENTS),'scope':'synthetic tracefs fixture only'}

    def _target_abi(self,pid):return {'format':'synthetic ELF64 fixture','kernel_signed_long_bits':64}

    def _calibrate_pid_namespace(self):
        # Darwin native thread IDs are Mach IDs; this regular-file fixture
        # models the Linux main-thread PID contract, never a host capability.
        with patch.object(trace.threading,'get_native_id',return_value=os.getpid()):
            return super()._calibrate_pid_namespace()

    def _make_instance(self):
        super()._make_instance()
        for name in ('tracing_on','current_tracer','trace_clock','buffer_size_kb','set_event_pid','events/enable','trace','trace_marker','options/markers',
                     'options/overwrite','options/event-fork','options/context-info','options/irq-info',
                     'options/raw','options/hex','options/bin','options/verbose','options/latency-format'):
            p=self.instance/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text('0\n')
        (self.instance/'buffer_total_size_kb').write_text('512\n')
        for event in trace.REQUIRED_EVENTS:
            category,name=event.split(':');p=self.instance/'events'/category/name/'enable'
            p.parent.mkdir(parents=True,exist_ok=True);p.write_text('0\n')
        for n in range(2):
            p=self.instance/'per_cpu'/f'cpu{n}'/'stats';p.parent.mkdir(parents=True)
            p.write_text('overrun: 0\ncommit overrun: 0\ndropped events: 0\n')
        os.mkfifo(self.instance/'trace_pipe',0o600)
        self.pipe_writer=os.open(self.instance/'trace_pipe',os.O_RDWR|os.O_NONBLOCK)

    def _write(self,relative,value):
        super()._write(relative,value)
        if relative=='trace_marker':
            (self.instance/'trace').write_text(line(getattr(self,'marker_pid',os.getpid()),'tracing_mark_write',value))

    def _remove_instance(self):
        if self.instance is not None:
            os.close(self.pipe_writer);shutil.rmtree(self.instance)


class DecoderTests(unittest.TestCase):
    def test_linux_custom_syscall_printer_and_sched_header_contract(self):
        # Primary printer contracts: kernel/trace/trace_syscalls.c
        # print_syscall_enter/exit; include/trace/events/sched.h TP_printk.
        output=[];decoder=trace.Decoder([10],output.append,comm={10:'native-depth-re'})
        records=[
            ' worker-10 [001] d..2 100.000001: sched_process_fork: comm=worker pid=10 child_comm=recording child_pid=11',
            ' recording-11 [001] .... 100.000002: sys_writev(fd: 3, vec: 0xdeadbeef, vlen: 3)',
            ' recording-11 [001] .... 100.000003: sys_writev -> 0xfffffffffffffffB',
            ' recording-11 [001] .... 100.000004: sys_fsync(fd: 3)',
            ' recording-11 [001] .... 100.000005: sys_fsync -> 0x0',
            ' recording-11 [001] .... 100.000005: task_rename: pid=11 oldcomm=recording newcomm=recording-check oom_score_adj=0',
            ' recording-11 [001] .... 100.000005: sys_fdatasync(fd: 3)',
            ' recording-11 [001] .... 100.000006: sys_fdatasync -> 0x0',
            ' recording-11 [001] .... 100.000006: sched_process_exit: comm=recording pid=11 prio=120',
        ]
        for row in records:decoder.line(row)
        self.assertEqual(output[0]['parent_tid'],10);self.assertEqual(output[0]['child_tid'],11)
        self.assertEqual(output[2]['root_pid'],10)
        self.assertEqual(output[1]['fd'],3);self.assertEqual(output[1]['vlen'],3)
        self.assertEqual(output[2]['return_value'],-5);self.assertEqual(output[2]['duration_ns'],1000)
        self.assertEqual(output[4]['syscall'],'fsync');self.assertEqual(output[4]['return_value'],0)
        self.assertEqual(output[6]['comm'],'recording-check')
        self.assertEqual(output[7]['comm'],'recording-check')
        self.assertEqual(decoder.report()['paired_syscalls'],3)
        self.assertEqual(decoder.report()['paired_syscalls_by_name']['fdatasync'],1)
        self.assertNotIn('deadbeef',json.dumps(output));self.assertNotIn('vec',json.dumps(output))

    def test_syscall_and_scheduler_intervals_have_real_entry_exit_and_no_vector_pointer(self):
        out=[];decoder=trace.Decoder([10],out.append)
        data=[line(10,'sys_enter_writev','fd: 0x3, vec: 0xdeadbeef, vlen: 0x3'),
            line(10,'sched_switch','prev_comm=PRIVATE prev_pid=10 prev_prio=120 prev_state=D ==> next_comm=OTHER next_pid=91 next_prio=120','100.110000'),
            line(91,'sched_wakeup','comm=PRIVATE pid=10 prio=120 target_cpu=001','100.150000'),
            line(91,'sched_switch','prev_comm=OTHER prev_pid=91 prev_prio=120 prev_state=R ==> next_comm=PRIVATE next_pid=10 next_prio=120','100.200000'),
            line(10,'sys_exit_writev','0xfffffffffffffffb','100.300000')]
        for raw in data:decoder.line(raw)
        self.assertEqual(out[-1]['duration_ns'],200_000_000)
        self.assertEqual(out[-1]['return_value'],-5)
        self.assertEqual(out[-1]['fd'],3);self.assertEqual(out[0]['vlen'],3)
        self.assertEqual(out[3]['resumed']['off_cpu_ns'],90_000_000)
        self.assertEqual(out[3]['resumed']['wakeup_to_run_ns'],50_000_000)
        self.assertEqual(out[-1]['printed_resolution_ns'],1000)
        for secret in ('PRIVATE','OTHER','deadbeef','vec'):self.assertNotIn(secret,json.dumps(out))
        self.assertEqual(decoder.report()['paired_syscalls'],1)
        self.assertEqual(decoder.report()['right_censored_calls'],0)

    def test_future_thread_fork_exit_and_reused_tid_generations_are_bounded(self):
        out=[];decoder=trace.Decoder([10],out.append,max_threads=2)
        for n in range(1500):
            decoder.line(line(10,'sched_process_fork','comm=x parent_pid=10 child_comm=y child_pid=11'))
            decoder.line(line(11,'sys_enter_fsync','fd=3'))
            decoder.line(line(11,'sys_exit_fsync','ret=0','100.200000'))
            decoder.line(line(11,'sched_process_exit','comm=y pid=11 prio=120'))
        self.assertEqual(len(decoder.tids),1)
        self.assertEqual(len(decoder.calls),0);self.assertEqual(len(decoder.off_cpu),0)
        self.assertEqual(out[-1]['tid_generation'],1501)
        self.assertEqual(len(decoder.comm),0);self.assertEqual(len(decoder.seen_syscall),0)
        self.assertEqual(decoder.owners,{10:10})
        decoder.line(line(10,'sched_process_fork','parent_pid=10 child_pid=11'))
        with self.assertRaisesRegex(ValueError,'THREAD_LIMIT'):
            decoder.line(line(10,'sched_process_fork','parent_pid=10 child_pid=12'))

    def test_loss_unbound_threads_mismatches_and_initial_censoring_are_explicit(self):
        decoder=trace.Decoder([10],lambda _:None)
        decoder.line(line(10,'sys_exit_fsync','0x0'))
        self.assertEqual(decoder.report()['left_censored_initial_calls'],1)
        with self.assertRaisesRegex(ValueError,'EXIT_WITHOUT_ENTRY'):decoder.line(line(10,'sys_exit_fsync','ret=0'))
        for raw,code in ((line(11,'sys_enter_fsync','fd=3'),'UNBOUND_SYSCALL'),
            (line(10,'sched_process_fork','parent_pid=11 child_pid=12'),'UNBOUND_FORK'),
            (line(11,'sched_process_exit','pid=11'),'UNBOUND_EXIT'),
            ('CPU:0 [LOST 12 EVENTS]\n','STREAM_LOST'),('not a trace line','LINE_SCHEMA')):
            with self.subTest(code=code),self.assertRaisesRegex(ValueError,code):
                trace.Decoder([10],lambda _:None).line(raw)
        decoder=trace.Decoder([10],lambda _:None);decoder.line(line(10,'sys_enter_fsync','fd=3'))
        with self.assertRaisesRegex(ValueError,'PAIR_MISMATCH'):decoder.line(line(10,'sys_exit_fdatasync','ret=0'))


@unittest.skipUnless(hasattr(os,'mkfifo'),'FIFO trace control fixture')
class TraceSessionTests(unittest.TestCase):
    def test_bounded_output_buffer_preserves_every_event_on_final_close(self):
        session=self.start();self.emit_complete();pid=self.process.pid
        for n in range(64):
            raw=line(pid,'sys_enter_fsync','fd: 0x3',f'{101+n}.100000')
            raw+=line(pid,'sys_exit_fsync','0x0',f'{101+n}.100001')
            os.write(session.pipe_writer,raw.encode());session.poll()
        self.assertEqual(trace.TRACE_OUTPUT_BUFFER_BYTES,256*1024)
        self.assertLess(session.data_bytes,trace.TRACE_OUTPUT_BUFFER_BYTES)
        path=session.output/'trace-events.jsonl'
        self.assertEqual(path.stat().st_size,0)
        report=session.finish();self.assertTrue(report['ok'],report)
        rows=[json.loads(raw) for raw in path.read_bytes().splitlines()]
        self.assertEqual(len(rows),report['decoded']['events'])
        self.assertEqual(report['decoded']['paired_syscalls'],65)
        self.assertEqual(path.stat().st_size,report['event_bytes'])

    def test_long_trace_disk_budget_is_explicit_without_allocating_kernel_memory(self):
        with tempfile.TemporaryDirectory() as d:
            out=Path(d)/'new'
            session=trace.TraceSession(out,max_bytes=4*1024**3,max_seconds=600)
            self.assertEqual(session.max_bytes,4*1024**3)
            self.assertEqual(session.max_memory_bytes,64*1024**2)
            self.assertFalse(out.exists())
            with self.assertRaisesRegex(ValueError,'TRACE_OUTPUT_BOUND'):
                trace.TraceSession(out,max_bytes=8*1024**3+1,max_seconds=600)

    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
        self.process=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])
        self.proc=self.root/'proc';(self.proc/'self/ns').mkdir(parents=True)
        (self.proc/'self/ns/time').symlink_to('time:[42]');process_files(self.proc,self.process.pid)
        (self.proc/'self/ns/pid').symlink_to('pid:[42]')
        (self.proc/'self/timens_offsets').write_text('monotonic 0 0\nboottime 0 0\n')
        self.tracefs=self.root/'tracefs';(self.tracefs/'instances').mkdir(parents=True)
        (self.tracefs/'tracing_on').write_text('GLOBAL-UNCHANGED')
        (self.tracefs/'set_event').write_text('GLOBAL-UNCHANGED')
        self.session=None

    def tearDown(self):
        try:
            if self.session is not None and self.session.state!='not-started':self.session.finish()
        finally:
            if self.process.poll() is None:self.process.kill()
            self.process.wait();self.temp.cleanup()

    def start(self,**kwargs):
        self.session=FakeTraceSession(self.root/'out',1024**2,60,tracefs=self.tracefs,proc=self.proc,**kwargs)
        ready=self.session.start([self.process]);self.assertTrue(ready['ok'],ready)
        self.assertEqual((self.session.instance/'set_event_pid').read_text().strip(),str(self.process.pid))
        self.assertEqual((self.session.instance/'options/event-fork').read_text().strip(),'1')
        self.assertEqual((self.session.instance/'trace_clock').read_text().strip(),'mono')
        self.assertEqual((self.session.instance/'options/overwrite').read_text().strip(),'0')
        return self.session

    def emit_complete(self):
        pid=self.process.pid
        raw=line(pid,'sys_enter_fsync','fd: 0x3')
        raw+=line(pid,'sched_switch',f'prev_pid={pid} prev_state=D ==> next_pid=91','100.110000')
        raw+=line(91,'sched_switch',f'prev_pid=91 prev_state=R ==> next_pid={pid}','100.120000')
        raw+=line(pid,'sys_exit_fsync','0x0','100.130000')
        os.write(self.session.pipe_writer,raw.encode());self.session.poll()

    def test_private_instance_stream_summary_hashes_and_actual_owned_process_are_preserved(self):
        session=self.start();self.emit_complete();instance=session.instance
        self.assertIsNone(self.process.poll())
        result=session.finish();self.assertTrue(result['ok'],result);self.assertEqual(result['lost_events'],0)
        self.assertFalse(instance.exists())
        self.assertEqual((self.tracefs/'tracing_on').read_text(),'GLOBAL-UNCHANGED')
        self.assertEqual((self.tracefs/'set_event').read_text(),'GLOBAL-UNCHANGED')
        self.assertIsNone(self.process.poll(),'trace must never stop its target')
        self.assertEqual(result,json.loads((session.output/'summary.json').read_text()))
        self.assertEqual(result['output_bytes'],sum(p.stat().st_size for p in session.output.iterdir()))
        for row in (session.output/'SHA256SUMS').read_text().splitlines():
            digest,name=row.split('  ');self.assertEqual(hashlib.sha256((session.output/name).read_bytes()).hexdigest(),digest)
        self.assertIs(session.finish(),result)

    def test_missing_capability_never_passes_even_when_explicitly_optional(self):
        for required in (True,False):
            with self.subTest(required=required):
                session=trace.TraceSession(self.root/f'missing-{required}',1024**2,600,tracefs=self.root/'missing',required=required)
                result=session.start([self.process]);self.assertEqual(result['state'],'unavailable')
                self.assertFalse(result['available']);self.assertEqual(bool(session.failure),required)
                final=session.finish();self.assertFalse(final['ok']);self.assertFalse(final['available'])
                self.assertFalse(list((self.tracefs/'instances').iterdir()))

    def test_per_cpu_kernel_drop_or_missing_stats_rejects_complete(self):
        for key in ('overrun','commit overrun','dropped events','missing'):
            with self.subTest(key=key):
                session=self.start();self.emit_complete()
                stats=session.instance/'per_cpu/cpu1/stats'
                text=stats.read_text()
                stats.write_text(''.join(key+': 1\n' if row==key+': 0' else row+'\n' for row in text.splitlines()) if key!='missing' else 'overrun: 0\n')
                final=session.finish();self.assertFalse(final['ok'])
                if key!='missing':self.assertEqual(final['lost_events'],1)
                else:
                    self.assertIn('LOSS_COUNTERS_UNAVAILABLE',final['error'])
                    self.assertIsNone(final['lost_events']);self.assertFalse(final['loss_counters_complete'])
                self.session=None;shutil.rmtree(self.root/'out')

    def test_decoder_failure_still_collects_kernel_loss_and_keeps_original_error(self):
        session=self.start();self.emit_complete()
        stats=session.instance/'per_cpu/cpu1/stats'
        stats.write_text(stats.read_text().replace('dropped events: 0','dropped events: 7'))
        os.write(session.pipe_writer,b'malformed kernel record\n');session.poll()
        first=session.failure;self.assertIn('TRACE_LINE_SCHEMA',first)
        final=session.finish()
        self.assertFalse(final['ok']);self.assertEqual(final['error'],first)
        self.assertEqual(final['lost_events'],7);self.assertTrue(final['loss_counters_complete'])
        self.assertIsNone(final['stats_error'])

    def test_pid_reuse_wall_limit_and_final_partial_line_are_diagnostic_failures(self):
        for fault in ('pid','wall','fragment'):
            with self.subTest(fault=fault):
                session=self.start()
                if fault=='pid':process_files(self.proc,self.process.pid,92)
                elif fault=='wall':session.started_ns=time.monotonic_ns()-61*10**9
                else:os.write(session.pipe_writer,b'incomplete')
                session.poll();final=session.finish();self.assertFalse(final['ok'])
                expected={'pid':'IDENTITY_CHANGED','wall':'WALL_LIMIT','fragment':'TRUNCATED_FINAL_LINE'}[fault]
                self.assertIn(expected,final['error'])
                self.assertIsNone(self.process.poll());self.session=None
                process_files(self.proc,self.process.pid,91);shutil.rmtree(self.root/'out')

    def test_total_kernel_memory_rejected_before_buffer_allocation(self):
        class OversizedSession(FakeTraceSession):
            def _make_instance(inner):
                super()._make_instance()
                for n in range(2,9):
                    p=inner.instance/'per_cpu'/f'cpu{n}'/'stats';p.parent.mkdir();p.write_text('overrun: 0\ncommit overrun: 0\ndropped events: 0\n')
        session=OversizedSession(self.root/'out',1024**2,60,tracefs=self.tracefs,proc=self.proc,max_memory_bytes=1024**2)
        self.session=session;ready=session.start([self.process]);self.assertFalse(ready['ok'])
        self.assertIn('TOTAL_RING_MEMORY_LIMIT',session.failure)
        self.assertEqual((session.instance/'buffer_size_kb').read_text().strip(),'0')
        self.assertFalse(session.finish()['ok'])

    def test_final_drain_consumes_owned_tail_after_a_slice_with_no_emitted_events(self):
        session=self.start();self.emit_complete();pid=self.process.pid
        comments=b'# no output\n'*20
        tail=(line(pid,'sys_enter_fsync','fd=3','100.300000')+line(pid,'sys_exit_fsync','ret=0','100.400000')).encode()
        os.write(session.pipe_writer,comments+tail)
        original=session._read
        with patch.object(session,'_read',side_effect=lambda:original(budget=len(comments))):
            result=session.finish()
        self.assertTrue(result['ok'],result)
        self.assertEqual(result['decoded']['paired_syscalls'],2)

    def test_changed_kernel_cpu_counter_set_never_claims_complete(self):
        for change in ('add','remove'):
            with self.subTest(change=change):
                session=self.start();self.emit_complete()
                if change=='remove':shutil.rmtree(session.instance/'per_cpu/cpu1')
                else:
                    p=session.instance/'per_cpu/cpu2/stats';p.parent.mkdir();p.write_text('overrun: 0\ncommit overrun: 0\ndropped events: 0\n')
                final=session.finish();self.assertFalse(final['ok']);self.assertIn('CPU_SET_CHANGED',final['error'])
                self.session=None;shutil.rmtree(self.root/'out')

    def test_same_time_namespace_with_nonzero_offset_still_rejects_clock_alignment(self):
        (self.proc/'self/timens_offsets').write_text('monotonic 1 0\nboottime 0 0\n')
        session=FakeTraceSession(self.root/'out',1024**2,60,tracefs=self.tracefs,proc=self.proc);self.session=session
        result=session.start([self.process]);self.assertFalse(result['ok'])
        self.assertIn('NAMESPACE_OFFSET',session.failure)
        self.assertFalse(list((self.tracefs/'instances').iterdir()))
        self.assertFalse(session.finish()['ok'])

    def test_private_marker_pid_mismatch_rejects_before_target_events_or_filters(self):
        session=FakeTraceSession(self.root/'out',1024**2,60,tracefs=self.tracefs,proc=self.proc)
        session.marker_pid=os.getpid()+100000;self.session=session
        result=session.start([self.process]);self.assertFalse(result['ok'])
        self.assertIn('PID_NAMESPACE_MISMATCH',session.failure)
        self.assertEqual((session.instance/'set_event_pid').read_text().strip(),'0')
        self.assertTrue(all((session.instance/'events'/event.replace(':','/')/'enable').read_text().strip()=='0' for event in trace.REQUIRED_EVENTS))
        self.assertFalse(session.finish()['ok'])
        self.assertEqual((self.tracefs/'tracing_on').read_text(),'GLOBAL-UNCHANGED')

    def test_reaped_owned_child_never_reopens_pid_and_missing_exit_is_explicit(self):
        session=self.start();self.emit_complete();self.process.kill();self.process.wait()
        with patch.object(trace,'proc_identity',side_effect=AssertionError('reopened reaped numeric PID')):
            session.poll();result=session.finish()
        self.assertFalse(result['ok']);self.assertIn('MISSING_THREAD_EXIT',result['error'])
        self.assertTrue(result['targets_terminal_at_finish'][str(self.process.pid)])

    def test_actual_stream_output_limit_stops_only_trace_and_retains_bounded_evidence(self):
        session=self.start();pid=self.process.pid
        stop=threading.Event();errors=[]
        def feed():
            pair=(line(pid,'sys_enter_fsync','fd=3')+line(pid,'sys_exit_fsync','ret=0','100.200000')).encode()
            while not stop.is_set():
                try:os.write(session.pipe_writer,pair*10)
                except BlockingIOError:time.sleep(.001)
                except OSError:return
        writer=threading.Thread(target=feed);writer.start()
        try:
            deadline=time.monotonic()+5
            while session.active and time.monotonic()<deadline:session.poll()
            self.assertFalse(session.active)
            self.assertIn('OUTPUT_LIMIT',session.failure)
            result=session.finish();self.assertFalse(result['ok'])
            self.assertLessEqual(result['output_bytes'],session.max_bytes)
            self.assertIsNone(self.process.poll())
        finally:stop.set();writer.join(timeout=5)
        self.assertFalse(writer.is_alive())


class ProbeTests(unittest.TestCase):
    def test_kernel_tracing_mark_write_pid_contract_requires_exactly_one_equal_self_pid(self):
        token='gdp-pid-proof-token'
        actual=' python3-123 [002] .... 100.123456: tracing_mark_write: '+token+'\n'
        self.assertTrue(trace.marker_pid_proof(actual,token,123)['verified_equal'])
        for text,expected in ((actual,42),('',123),(actual+actual,123)):
            with self.subTest(text=text,expected=expected),self.assertRaises(ValueError):trace.marker_pid_proof(text,token,expected)

    def test_native_elf64_machine_contract_rejects_compat_and_foreign_abi(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);(proc/'10').mkdir();path=proc/'10/exe'
            header=bytearray(64);header[:6]=b'\x7fELF\x02\x01';header[18:20]=(62).to_bytes(2,'little')
            session=trace.TraceSession(proc/'new',1024**2,600,proc=proc)
            with patch.object(trace.os,'uname',return_value=SimpleNamespace(machine='x86_64')):
                path.write_bytes(header);self.assertEqual(session._target_abi(10)['kernel_signed_long_bits'],64)
                for offset,value in ((4,1),(5,2),(18,183)):
                    changed=header.copy();changed[offset]=value;path.write_bytes(changed)
                    with self.subTest(offset=offset),self.assertRaisesRegex(ValueError,'ABI_UNSUPPORTED'):session._target_abi(10)

    def test_missing_malformed_and_nonzero_time_offsets_are_explicit(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);(proc/'self').mkdir()
            with self.assertRaises(FileNotFoundError):trace.zero_monotonic_offset(proc)
            for value in ('monotonic 1 0\n','monotonic 0 1\n','monotonic nope 0\n','boottime 0 0\n'):
                (proc/'self/timens_offsets').write_text(value)
                with self.subTest(value=value),self.assertRaises(ValueError):trace.zero_monotonic_offset(proc)
            (proc/'self/timens_offsets').write_text('monotonic 0 0\nboottime 3 0\n')
            self.assertEqual(trace.zero_monotonic_offset(proc)['seconds'],0)

    def test_probe_does_not_create_instance_or_assume_root_implies_capability(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);before=list(root.iterdir());result=trace.capability(root)
            self.assertFalse(result['available']);self.assertEqual(list(root.iterdir()),before)

    def test_constructor_rejects_nonfinite_unbounded_and_existing_output(self):
        for fields in ({'max_bytes':100},{'max_seconds':float('nan')},{'buffer_kib':0},{'max_memory_bytes':2**40}):
            args=dict(output=Path('/unused/new'),max_bytes=1024**2,max_seconds=600);args.update(fields)
            with self.subTest(fields=fields),self.assertRaises(ValueError):trace.TraceSession(**args)
