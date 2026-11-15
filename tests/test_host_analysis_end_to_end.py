"""Real offline analyzer over immutable SHA-verified three-writer synthetic input."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import host_trace_analysis as a


def write_fixture(root, anomaly_in_window=False):
    trace=root/'trace';official=root/'official';trace.mkdir();official.mkdir()
    start=182505975783927;anchor=start+1_000_000_000;end=anchor+100_000_000
    def line(tid,cpu,at,kind,fields):return f'task-{tid} [{cpu:03d}] .... {at//10**9}.{at%10**9:09d}: {kind}: {fields}\n'
    raw=''.join(line(1,0,n,'sched_process_fork',f'parent_pid=1 child_pid={pid}') for n,pid in enumerate((10,20,30),1))
    at=start+100 if anomaly_in_window else 182444287013000
    raw+=line(10,9,at,'sched_switch','prev_pid=10 prev_state=S next_pid=0')
    raw+=line(30,5,at-2000,'sched_wakeup','pid=10 target_cpu=009')
    raw+=line(0,9,at+3000,'sched_switch','prev_pid=0 prev_state=R next_pid=10')
    samples=[];events=[];resources=[]
    for index,(camera,pid) in enumerate(zip('abc',(10,20,30))):
        base=start+10000+index*1000
        raw+=line(pid,index,base,'sched_switch',f'prev_pid={pid} prev_state=S next_pid=0')
        raw+=line(0,index,base+10,'sched_wakeup',f'pid={pid}')
        raw+=line(0,index,base+20,'sched_switch',f'prev_pid=0 prev_state=R next_pid={pid}')
        raw+=line(pid,index,base+30,'sys_enter_fsync','fd=7')
        raw+=line(pid,index,base+40,'sys_exit_fsync','ret=0')
        samples.append(dict(kind='identity-sample',group='on-01',camera=camera,
            runtime_identity=dict(container_id='synthetic-'+camera,process=dict(worker_host_pid=pid,worker_start_ticks=3,pid_namespace='pid:['+camera+']')),
            threads=[dict(tid=pid,tgid=pid,nspid=[pid,17],start_ticks=3,pid_namespace='pid:['+camera+']',monotonic_offset_ns=0)],
            sample_started_monotonic_ns=100,sample_finished_monotonic_ns=110))
        events.append(dict(target=camera,event_id=index+1,checkpoint_id=1,phase='index_sync',tid=17,start_monotonic_ns=base+25,wall_ns=20))
        for stamp in (start-1,start+1,anchor+1):
            resources.append(dict(target=camera,identity=[pid,3],sample_id=stamp,source='synthetic',epoch=0,
                monotonic_ns=stamp,writer_queue=dict(capacity_pairs=16,queued_pairs=0,oldest_queued_age_ns=None)))
    for pid in (10,20,30):raw+=line(pid,0,end+1000,'sched_process_exit',f'pid={pid}')
    (trace/'host-raw.trace').write_text(raw)
    (trace/'storage-raw.trace').write_text(line(90,0,start+20000,'ext4_sync_file_enter','dev 259,2 ino 1 datasync 0')+
        line(90,0,start+20010,'ext4_sync_file_exit','dev 259,2 ino 1 ret 0'))
    (trace/'identities.jsonl').write_text(''.join(json.dumps(s)+'\n' for s in samples))
    helpers={str(ROOT/p):a.digest(ROOT/p) for p in ('scripts/validation/writer_syscall_trace.py','scripts/validation/writer_storage_trace.py',
        'experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py')}
    ready=dict(schema='gdp-independent-host-trace-v1',revision='a'*40,observer_pid=99,
               binding=dict(filesystem_dev=[259,2],block_dev=[259,0]),
               limits=dict(max_total_file_bytes=16*1024**2,max_lifecycle_events=128),helper_and_source_sha256_before=helpers)
    summary=dict(schema=ready['schema'],capture_complete=True,identity_binding_complete=True,
                 identity_samples=3,identity_errors=0,failure=None,
                 bound_workers=[dict(camera=c,worker_host_pid=pid,start_ticks=3) for c,pid in zip('abc',(10,20,30))],
                 instances=[dict(instance=name,start_monotonic_ns=0,stop_monotonic_ns=end+2000) for name in ('host','storage')])
    (trace/'ready.json').write_text(json.dumps(ready));(trace/'summary.json').write_text(json.dumps(summary))
    original=dict(worker_epochs=[dict(target=c,host_pid=pid,start_ticks=3) for c,pid in zip('abc',(10,20,30))],
        event_union_coverage=[dict(target=c,missing_through_last_observed=0) for c in 'abc'],
        source_summaries=dict(lifecycle=dict(sampling_ok=True,errors=[],event_targets={c:dict(lost_events_after_start=0) for c in 'abc'}),
            checkpoint=dict(errors={},targets=[dict(index=i,lost_events_after_start=0,observation_supported=True,observation_enabled=True) for i in range(3)])),
        three_writer_wait_chain=dict(window_start_monotonic_ns=start,anchor_monotonic_ns=anchor,window_end_monotonic_ns=end,
            anchor_basis='overflow_detection',queue_samples=resources,events=events,wait_links=[],
            internal_queue_history=[dict(target=c,window_bracketed=True,missing_between_selected_events=0) for c in 'abc']))
    (official/'summary.json').write_text(json.dumps(original));(official/'resources.json').write_text(json.dumps(resources))
    for directory in (trace,official):
        (directory/'SHA256SUMS').write_text(''.join(a.digest(p)+'  '+p.name+'\n' for p in sorted(directory.iterdir())))
    return trace,official


class HostAnalyzerEndToEndTests(unittest.TestCase):
    def test_outside_anomaly_is_retained_and_does_not_poison_window(self):
        self.check(False)
    def test_inside_anomaly_keeps_window_incomplete_and_preserves_other_events(self):
        self.check(True)
    def check(self,inside):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);trace,official=write_fixture(root,inside)
            before={str(p):a.digest(p) for directory in (trace,official) for p in directory.iterdir()}
            result=a.analyze(ROOT,trace,official,root/'derived')
            self.assertEqual(result['association_complete'],not inside,result['gaps'])
            self.assertEqual(result['association_gap_report']['window_count'],int(inside))
            self.assertEqual(result['association_gap_report']['outside_window_count'],int(not inside))
            self.assertTrue(all(v['fsync_intervals']==1 for v in result['by_camera'].values()))
            self.assertTrue(result['overall_identity_history_complete'])
            self.assertFalse(result['hardware_acceptance_verified'])
            self.assertTrue(result['analysis_scan_complete']);self.assertFalse(result['association_pair_history_complete'])
            gaps=[json.loads(s) for s in (root/'derived/association-gaps.jsonl').read_text().splitlines()]
            self.assertEqual(len(gaps),1);self.assertIn('line_number',gaps[0]['switch_in_source'])
            self.assertEqual(before,{str(p):a.digest(p) for directory in (trace,official) for p in directory.iterdir()})


if __name__=='__main__':unittest.main()
