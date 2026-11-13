"""Real CPython append/low-level write against a controlled Linux tracefs model."""
import errno
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import host_kernel_trace as host
import host_trace_analysis as analysis


class PidBindingTests(unittest.TestCase):
    def test_open_error_retains_syscall_context_and_does_not_create_file(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'missing'
            with self.assertRaises(host.PidFilterError) as caught:host.seed_pid_filter(path,[12],10)
            context=caught.exception.syscall_context
            self.assertEqual(context['operation'],'open');self.assertEqual(context['errno'],errno.ENOENT)
            self.assertEqual(context['requested_bytes'],3);self.assertFalse(path.exists())
            self.assertFalse(context['open_flags'] & (os.O_APPEND|os.O_TRUNC|os.O_CREAT))

    def test_empty_invalid_and_symlink_controls_are_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'control';path.write_text('unchanged')
            for tids in ([],[0],[True],[1,True],[-2],[1,2,3]):
                with self.assertRaises(ValueError):host.seed_pid_filter(path,tids,2)
            alias=Path(d)/'alias';alias.symlink_to(path)
            with self.assertRaises(host.PidFilterError):host.seed_pid_filter(alias,[1],2)
            self.assertEqual(path.read_text(),'unchanged')

    def test_zero_partial_and_error_identity_cannot_be_complete(self):
        g=analysis.Generations(20);expected={(c,i,3) for c,i in zip('abc',(10,20,30))}
        summary=dict(identity_samples=0,identity_errors=0,bound_workers=[])
        self.assertFalse(analysis.identity_history_status(g,summary,expected)['complete'])
        for c,pid,start in expected:
            owner=dict(camera=c,worker_host_pid=pid,worker_start_ticks=start)
            g.rows.append(dict(owner=owner,basis='fork'))
            summary['bound_workers'].append(dict(camera=c,worker_host_pid=pid,start_ticks=start))
        summary['identity_samples']=3
        self.assertTrue(analysis.identity_history_status(g,summary,expected)['complete'])
        summary['identity_errors']=1
        self.assertFalse(analysis.identity_history_status(g,summary,expected)['complete'])
        summary['identity_errors']=0;summary['bound_workers'].pop()
        self.assertFalse(analysis.identity_history_status(g,summary,expected)['complete'])

    def test_real_analyzer_rejects_empty_identity_history_with_complete_raw_receipt(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);trace=root/'trace';official=root/'official';trace.mkdir();official.mkdir()
            helpers={str(ROOT/p):analysis.digest(ROOT/p) for p in (
                'scripts/validation/writer_syscall_trace.py','scripts/validation/writer_storage_trace.py',
                'experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py')}
            ready=dict(schema='gdp-independent-host-trace-v1',revision='a'*40,binding={},
                       limits=dict(max_total_file_bytes=16*1024**2,max_lifecycle_events=128),helper_and_source_sha256_before=helpers)
            summary=dict(schema=ready['schema'],capture_complete=True,identity_samples=0,identity_errors=0,
                         bound_workers=[],instances=[],failure=None)
            for name,value in [('summary.json',summary),('ready.json',ready)]:
                (trace/name).write_text(json.dumps(value))
            for name in ('identities.jsonl','host-raw.trace','storage-raw.trace'):(trace/name).write_text('')
            chain=dict(window_start_monotonic_ns=1,anchor_monotonic_ns=1_000_000_001,window_end_monotonic_ns=1_100_000_001,
                       events=[],queue_samples=[],wait_links=[],internal_queue_history=[],anchor_basis='overflow_detection')
            (official/'summary.json').write_text(json.dumps(dict(three_writer_wait_chain=chain,
                worker_epochs=[dict(target=c,host_pid=i,start_ticks=3) for c,i in zip('abc',(10,20,30))])))
            (official/'resources.json').write_text('[]')
            for directory in (trace,official):
                (directory/'SHA256SUMS').write_text(''.join(analysis.digest(p)+'  '+p.name+'\n' for p in sorted(directory.iterdir())))
            before={str(p):analysis.digest(p) for directory in (trace,official) for p in directory.iterdir()}
            result=analysis.analyze(ROOT,trace,official,root/'new-analysis')
            self.assertFalse(result['overall_identity_history_complete']);self.assertFalse(result['association_complete'])
            self.assertIn('zero-or-invalid-identity-samples',result['identity_history']['reasons'])
            self.assertTrue(any(g['reason']=='identity-history-incomplete' for g in result['gaps']))
            self.assertEqual(before,{str(p):analysis.digest(p) for directory in (trace,official) for p in directory.iterdir()})

    def test_binding_and_finish_span_recording_draining_and_readback(self):
        self.binding_lifecycle(False)

    def test_failed_seed_never_publishes_identity_and_zero_finish_is_incomplete(self):
        self.binding_lifecycle(True)

    def test_pid_reuse_after_control_write_never_publishes_binding(self):
        self.binding_lifecycle(False,reuse=True)

    def binding_lifecycle(self, missing, reuse=False):
        import threading,time
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);runroot=root/'run';capture=runroot/'ab/on-01/capture';capture.mkdir(parents=True)
            (capture/'run.json').write_text(json.dumps(dict(run_id='run',image_id='image',cameras=[dict(name=c) for c in 'abc'])))
            for c,pid in zip('abc',(10,20,30)):
                place=capture/c; (place/'config').mkdir(parents=True)
                process=dict(worker_host_pid=pid,worker_start_ticks=3,pid_namespace='pid:[7]',container_init_pid=pid)
                pin=dict(run_id='run',image_id='image',config_sha256='config',launch_instance_id='launch',
                         worker_pid=pid,container_id='container',instance_id='instance',process=process)
                (place/'runtime-identity.json').write_text(json.dumps(pin))
                (place/'config/experiment.json').write_text(json.dumps(dict(run_id='run',image_id='image',capture_sha256='config',instance_id='launch')))
            h=host.HostKernelTrace(ROOT,root/'output',root,runroot,disk='test',expected_uuid='test',expected_revision='a'*40)
            h.output.mkdir();h.started_ns=time.monotonic_ns()
            h.hardware=SimpleNamespace(_read_runtime_identity=lambda p:json.loads(p.read_text()),
                                       observe_runtime_process=lambda info,status,binding,previous:previous)
            class Instance:
                def __init__(self,name):
                    self.name=name;self.path=root/name;self.path.mkdir();self.thread=None;self.stop_reader=threading.Event();self.stopped=None
                def stop_capture(self):
                    if self.stopped is None:self.stopped=time.monotonic_ns()
                def finish(self):return dict(instance=self.name,complete_initial_and_final_counters=True,removed=True)
            h.instances=[Instance('host'),Instance('storage')]
            if not missing:(h.instances[0].path/'set_event_pid').write_text('')
            root_checks={}
            def identity(path):
                pid=int(path.name);ticks=3
                if path.parent==Path("/proc"):
                    root_checks[pid]=root_checks.get(pid,0)+1
                    if reuse and root_checks[pid]>=4:ticks=9
                return dict(tid=pid,start_ticks=ticks,state='R',tgid=pid,nspid=[pid],pid_namespace='pid:[7]',
                            cgroup='/docker/container',monotonic_offset_ns=0)
            class Entries:
                def __init__(self,rows):self.rows=rows
                def __iter__(self):return iter(self.rows)
                def __enter__(self):return self
                def __exit__(self,*args):pass
            def entries(path):
                path=Path(path);pid=int(path.parent.name)
                return Entries([SimpleNamespace(name=str(pid),path=str(path/str(pid)))])
            with patch.object(host,'proc_record',side_effect=identity),patch.object(host,'stable_identity',side_effect=identity),patch.object(host.os,'scandir',side_effect=entries),patch.object(host.os,'listdir',side_effect=lambda path:[e.name for e in entries(path)]):
                for phase in ('recording','draining','readback'):
                    h.sample_identities()
                    h.budget.json('lifecycle-test.jsonl',dict(phase=phase,monotonic_ns=time.monotonic_ns()))
                    self.assertTrue(all(i.stopped is None for i in h.instances))
            end=time.monotonic_ns();result=h.finish()
            self.assertTrue(all(i.stopped>=end for i in h.instances))
            self.assertIs(result['capture_complete'],not (missing or reuse),result)
            self.assertIs(result['identity_binding_complete'],not (missing or reuse))
            if missing or reuse:
                self.assertEqual(h.known_threads,{});self.assertEqual(h.thread_pins,{})
                self.assertEqual(h.seeded_workers,set());self.assertEqual(h.bound,{})
                rows=[json.loads(s) for s in (h.output/'identities.jsonl').read_text().splitlines()]
                if missing:self.assertTrue(all(r.get('syscall_context') and r['syscall_context']['operation']=='open' for r in rows),rows)
                else:self.assertTrue(any('WORKER_CHANGED_DURING_PID_FILTER_WRITE' in r['reason'] for r in rows))
            else:
                self.assertEqual(h.identity_samples,9);self.assertEqual(len(h.bound),3)
                self.assertEqual(len(h.seeded_workers),3)
            self.assertEqual(analysis.verify_manifest(h.output,16*1024**2),analysis.digest(h.output/'SHA256SUMS'))

    @unittest.skipUnless(sys.platform=='linux','Linux preload syscall contract model')
    def test_real_append_repro_and_additive_write_error_context(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);control=root/'set_event_pid';lib=root/'model.so';log=root/'calls'
            subprocess.run(['cc','-shared','-fPIC','-std=c11','-Wall','-Wextra','-Werror',str(ROOT/'tests/support/tracefs_pid_fault.c'),'-ldl','-o',str(lib)],check=True,capture_output=True)
            env={k:v for k,v in os.environ.items() if not k.startswith('GDP_PID_TEST_') and k!='LD_PRELOAD'}
            env.update(LD_PRELOAD=str(lib),GDP_PID_TEST_CONTROL=str(control),GDP_PID_TEST_LOG=str(log),PYTHONDONTWRITEBYTECODE='1')
            control.write_text('10\n')
            old=subprocess.run([sys.executable,'-c',"from pathlib import Path;import sys;f=Path(sys.argv[1]).open('a');f.write('20\\n');f.close()",str(control)],env=env,capture_output=True)
            self.assertNotEqual(old.returncode,0);self.assertIn(b'Invalid argument',old.stderr)
            self.assertIn('lseek_end',log.read_text());self.assertEqual(control.read_text(),'10\n')
            script="""import sys,json
sys.path.insert(0,sys.argv[2]);import host_kernel_trace as h
try: print(json.dumps(h.seed_pid_filter(sys.argv[1],[20,30],10)))
except h.PidFilterError as e: print(json.dumps(e.syscall_context));sys.exit(7)
"""
            prior_seek_count=log.read_text().count('lseek_end')
            new=subprocess.run([sys.executable,'-c',script,str(control),str(ROOT/'experiments/sdk-2.10.6-diagnostics')],env=env,capture_output=True,text=True)
            self.assertEqual(new.returncode,0,new.stderr);self.assertEqual(control.read_text(),'10\n20 30\n')
            receipt=json.loads(new.stdout);self.assertTrue(receipt['ok']);self.assertEqual(receipt['written_bytes'],6)
            self.assertEqual(log.read_text().count('lseek_end'),prior_seek_count)
            for fault in ('eio','short'):
                result=subprocess.run([sys.executable,'-c',script,str(control),str(ROOT/'experiments/sdk-2.10.6-diagnostics')],env=dict(env,GDP_PID_TEST_FAULT=fault),capture_output=True,text=True)
                self.assertEqual(result.returncode,7,result.stderr)
                context=json.loads(result.stdout);self.assertEqual(context['operation'],'write');self.assertEqual(context['errno'],errno.EIO)
                self.assertFalse(context['ok']);self.assertEqual(control.read_text(),'10\n20 30\n')


if __name__=='__main__':unittest.main()
