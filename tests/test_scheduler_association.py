"""Sanitized R02-shaped raw events; real parser/association, no clock repair."""
import sys
from pathlib import Path
import unittest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import host_trace_analysis as a
sc=a.load('scheduler_test_syscalls',ROOT/'scripts/validation/writer_syscall_trace.py')


def generations():
    g=a.Generations(100)
    g.event('sched_process_fork','parent_pid=10 child_pid=20',1,sc.scalar)
    g.anchor(dict(group='on-01',camera='a',runtime_identity=dict(container_id='synthetic',
        process=dict(worker_host_pid=10,worker_start_ticks=3,pid_namespace='pid:[7]')),
        threads=[dict(tid=20,start_ticks=4,tgid=10,nspid=[20,64],pid_namespace='pid:[7]',monotonic_offset_ns=0)],
        sample_started_monotonic_ns=2,sample_finished_monotonic_ns=3),{('a',10,3)})
    return g


RAW=(
 'writer-20 [009] d..2. 182444.287013: sched_switch: prev_pid=20 prev_state=S next_pid=0',
 'writer-10 [005] d..3. 182444.287011: sched_wakeup: pid=20 target_cpu=009',
 '<idle>-0 [009] d..2. 182444.287016: sched_switch: prev_pid=0 prev_state=R next_pid=20',
)


class SchedulerAssociationTests(unittest.TestCase):
    def replay(self,start,end):
        rows=[];gaps=[];chain=dict(window_start_monotonic_ns=start,window_end_monotonic_ns=end,
            events=[dict(target='a',event_id=1,checkpoint_id=1,phase='index_sync',tid=64,start_monotonic_ns=end-50,wall_ns=50)])
        obj=a.Association(sc,generations(),chain,rows.append,gap_emit=gaps.append)
        for n,line in enumerate(RAW,1):obj.event(*a.parse(sc,line),source=dict(line_number=n,byte_offset=n*100,line_bytes=len(line)+1))
        return obj,rows,gaps

    def test_cross_cpu_pre_switch_wake_keeps_raw_clock_and_no_latency(self):
        obj,rows,gaps=self.replay(182444287000000,182444287030000)
        self.assertEqual(obj.window_gap_count,1)
        switch=next(r for r in rows if r.get('direction')=='switch-in')
        self.assertEqual(switch['off_cpu_ns'],3000)
        self.assertEqual(switch['wakeup_monotonic_ns'],182444287011000)
        self.assertFalse(switch['wakeup_pair_proven']);self.assertNotIn('wakeup_to_run_ns',switch)
        self.assertEqual(gaps[0]['wakeup'][1],5);self.assertEqual(gaps[0]['switch_out'][2],9)
        self.assertEqual(gaps[0]['switch_in_source']['line_number'],3)

    def test_outside_anomaly_does_not_prevent_later_strict_fsync(self):
        start=182505975783927;end=start+1_100_000_000
        obj,rows,gaps=self.replay(start,end)
        self.assertEqual(obj.gaps,[]);self.assertEqual(obj.outside_gap_count,1)
        obj.event(20,9,end-40,1,'sys_enter_fsync','fd=7')
        obj.event(20,9,end-10,1,'sys_exit_fsync','ret=0')
        self.assertIn(('a',1),obj.strict_sync)
        self.assertEqual(rows[-1]['duration_ns'],30)
        self.assertTrue(rows[-1]['syscall_pair_proven'])

    def test_in_window_anomaly_remains_gap_but_independent_fsync_is_parsed(self):
        start=182444287000000;end=182444287030000
        obj,rows,gaps=self.replay(start,end)
        obj.event(20,9,end-40,1,'sys_enter_fsync','fd=7')
        obj.event(20,9,end-10,1,'sys_exit_fsync','ret=0')
        self.assertIn(('a',1),obj.strict_sync);self.assertEqual(len(obj.gaps),1)

    def test_history_bound_does_not_abort_long_trace_or_hide_gap_counts(self):
        emitted=[];obj=a.Association(sc,generations(),dict(window_start_monotonic_ns=100000,
            window_end_monotonic_ns=200000,events=[]),lambda r:None,gap_emit=emitted.append)
        for n in range(1500):
            t=100+n*10
            obj.event(20,9,t,1,'sched_switch','prev_pid=20 prev_state=S next_pid=0')
            obj.event(10,5,t-2,1,'sched_wakeup','pid=20')
            obj.event(0,9,t+3,1,'sched_switch','prev_pid=0 prev_state=R next_pid=20')
        report=obj.gap_report()
        self.assertEqual(report['outside_window_count'],1500);self.assertEqual(len(emitted),1500)
        self.assertEqual(len(obj.outside_gaps),1024);self.assertEqual(report['outside_window_history_omitted'],476)

    def test_unproven_and_censored_syscall_cannot_mark_strict_sync(self):
        obj=a.Association(sc,generations(),dict(window_start_monotonic_ns=100,window_end_monotonic_ns=300,
            events=[dict(target='a',event_id=1,checkpoint_id=1,phase='index_sync',tid=64,start_monotonic_ns=100,wall_ns=200)]),lambda r:None)
        obj.event(20,9,200,1,'sys_enter_fsync','fd=7')
        obj.event(20,9,190,1,'sys_exit_fsync','ret=0')
        self.assertEqual(obj.strict_sync,set());self.assertEqual(obj.gaps[0]['reason'],'syscall-pair-unproven')
        obj.event(20,9,201,1,'sys_enter_fsync','fd=7')
        obj.event(20,9,202,1,'sys_exit_fsync','ret=-5')
        self.assertEqual(obj.strict_sync,set())
        obj.event(20,9,210,1,'sys_enter_fsync','fd=7');obj.finish()
        self.assertEqual(obj.strict_sync,set());self.assertEqual(obj.gaps[-1]['reason'],'right-censored-syscall')

    def test_census_has_bounded_context_categories_not_causal_claims(self):
        census=a.TraceCensus(10,dict(window_start_monotonic_ns=100,window_end_monotonic_ns=200))
        for tid in (20,10,0):census.observe((tid,0,150,1,'sched_wakeup','pid=20'),80,generations())
        census.observe(None,10,generations())
        report=census.report();self.assertEqual(report['raw_bytes'],250)
        self.assertTrue(all(r['events']==1 for r in report['classes'].values()))


if __name__=='__main__':unittest.main()
