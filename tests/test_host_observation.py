"""No tracefs/USB/Docker: proc generations, contracts and real driver exit."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
HERE=ROOT/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
import host_kernel_trace as host
import host_trace_analysis as analysis
import run_host_observation as runner
spec=importlib.util.spec_from_file_location('host_syscalls',ROOT/'scripts/validation/writer_syscall_trace.py')
sc=importlib.util.module_from_spec(spec);spec.loader.exec_module(sc)

class HostObservationTests(unittest.TestCase):
    def test_errors_empty_map_and_merged_history_are_not_lost_events(self):
        targets={n:{'lost_events_after_start':0,'history_events_before_start':1190} for n in 'abc'}
        source={'event_union_coverage':[{'target':n,'missing_through_last_observed':0} for n in 'abc'],
            'source_summaries':{'lifecycle':{'sampling_ok':True,'errors':[],'event_targets':targets},
             'checkpoint':{'errors':{},'targets':[dict(index=i,observation_supported=True,observation_enabled=True,**v) for i,v in enumerate(targets.values())]}}}
        self.assertEqual(analysis.original_observer_gaps(source),[])
        source['event_union_coverage'][0]['missing_through_last_observed']=1
        self.assertTrue(analysis.original_observer_gaps(source))
        source['source_summaries']['checkpoint']['errors']={'read':1}
        self.assertTrue(any(g['reason']=='original-checkpoint-sampling-errors' for g in analysis.original_observer_gaps(source)))

    def test_generation_namespace_tid_and_pid_reuse_are_strict(self):
        g=analysis.Generations(20)
        g.event('sched_process_fork','parent_pid=10 child_pid=20',100,sc.scalar)
        pin={'container_id':'container','process':{'worker_host_pid':10,'worker_start_ticks':3,'pid_namespace':'pid:[7]'}}
        thread={'tgid':10,'pid_namespace':'pid:[7]','monotonic_offset_ns':0,'tid':20,'nspid':[20,64],'start_ticks':4}
        sample={'group':'on-01','camera':'a','runtime_identity':pin,'threads':[thread],
                'sample_started_monotonic_ns':110,'sample_finished_monotonic_ns':120}
        g.anchor(sample,{('a',10,3)})
        chain={'window_start_monotonic_ns':100,'window_end_monotonic_ns':500,
               'events':[{'target':'a','event_id':7,'checkpoint_id':8,'phase':'index_sync','tid':64,'start_monotonic_ns':130,'wall_ns':100}]}
        rows=[];a=analysis.Association(sc,g,chain,rows.append)
        a.output({'syscall':'fsync'},g.at(20,140),140,200)
        self.assertIn(('a',7),a.strict_sync)
        chain['events'][0]['tid']=65
        a=analysis.Association(sc,g,chain,rows.append);a.output({'syscall':'fsync'},g.at(20,140),140,200)
        self.assertEqual(a.strict_sync,set())
        g.event('sched_process_exit','pid=20',250,sc.scalar)
        g.event('sched_process_fork','parent_pid=99 child_pid=20',300,sc.scalar)
        self.assertIsNone(g.at(20,310)['owner'])
        self.assertIsNone(g.at(20,240,310))

    def test_fork_callback_captures_short_lived_namespace_identity_and_rejects_reuse(self):
        with tempfile.TemporaryDirectory() as d:
            h=host.HostKernelTrace(ROOT,Path(d)/'trace',Path(d),Path(d)/'run',disk='testdisk',expected_uuid='testuuid',expected_revision='a'*40)
            h.output.mkdir();h.syscalls=sc
            pin={'container_id':'testcontainer','process':{'worker_host_pid':10,'worker_start_ticks':3,'pid_namespace':'pid:[7]'}}
            h.thread_pins[10]=('on-01','a',pin)
            child={'tid':20,'start_ticks':4,'tgid':10,'nspid':[20,64],'pid_namespace':'pid:[7]','cgroup':'/docker/testcontainer','monotonic_offset_ns':0}
            line='worker-10 [000] .... 0.000000100: sched_process_fork: parent_pid=10 child_pid=20'
            with patch.object(host,'proc_record',return_value={'start_ticks':3}),patch.object(host,'stable_identity',return_value=child):h.anchor_fork(line)
            row=json.loads((h.output/'identities.jsonl').read_text())
            self.assertEqual(row['threads'][0]['nspid'],[20,64]);self.assertEqual(row['basis'],'fork-triggered-proc')
            with patch.object(host,'proc_record',return_value={'start_ticks':9}):h.anchor_fork(line)
            h.budget.close();last=json.loads((h.output/'identities.jsonl').read_text().splitlines()[-1])
            self.assertEqual(last['kind'],'fork-identity-unavailable')

    def test_trace_failure_preserves_real_driver_completion(self):
        class FailedTrace:
            def __init__(self,*a,**kw):pass
            def start(self):pass
            def poll(self):return 'TRACE_OUTPUT_LIMIT'
            def finish(self):raise ValueError('TRACE_FINALIZATION_FAILED')
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);done=root/'driver-finished'
            args=SimpleNamespace(execute=True,observer='on',source=ROOT,output=root/'new',recording_path=root,
                retest_root=root/'run',disk='test',filesystem_uuid='test',revision='a'*40,max_bytes=16*1024**2,max_seconds=600,
                command=[sys.executable,'-c',"import pathlib,time;time.sleep(.1);pathlib.Path(__import__('sys').argv[1]).write_text('finished')",str(done)])
            with patch.object(runner,'HostKernelTrace',FailedTrace):self.assertEqual(runner.run(args),2)
            self.assertEqual(done.read_text(),'finished')
            result=json.loads((args.output/'result.json').read_text())
            self.assertEqual(result['driver_returncode'],0);self.assertFalse(result['hardware_acceptance_verified'])
            self.assertEqual((args.output/'result.json').stat().st_mode&0o777,0o600)

if __name__=='__main__':unittest.main()
