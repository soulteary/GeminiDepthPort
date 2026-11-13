from pathlib import Path
import sys
import unittest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts/validation'))
import fixed_checkpoint_target as build
import fixed_checkpoint_target_run as run


class FixedTargetTests(unittest.TestCase):
    def test_only_isolated_target_changes_barriers_stay(self):
        source=(ROOT/build.phase.TARGET).read_text();patched=build.patch_writer(source)
        for token in ('records.sync_all()','chunk.file.sync_all()','verify_checkpoint(',
                      'persist_checkpoint(', 'p.payload_bytes + active_bytes + length > CHUNK_BYTES'):
            self.assertEqual(source.count(token),patched.count(token))
        self.assertNotIn('first.chunks',patched)
        self.assertNotIn('GDP_EXPERIMENT_FIRST_PAIRS',patched)
        self.assertIn('experiment_target_mib * 1024 * 1024',patched)
        self.assertNotIn('sleep(',patched)
        self.assertIn('Some(credit), Some(reason)',patched)
        with self.assertRaises(ValueError):build.patch_writer(patched)

    def test_queue_sampling_preserves_cap_and_checks_before_clear(self):
        patched=build.patch_driver((ROOT/build.phase.DRIVER).read_text())
        self.assertIn('const CAPACITY: usize = 16;',patched)
        self.assertLess(patched.index('let sample = ExperimentQueueSample'),patched.index('if s.items.len() == CAPACITY'))
        self.assertIn('experiment_history.reserve_exact(108000)',patched)

    def test_actual_oversized_batch_and_consecutive_waits_remain_visible(self):
        credit=dict(payload_limit_bytes=16*1024**2,pair_limit=32,pending_payload_bytes=5*1024**2,
                    active_payload_bytes=9*1024**2,pending_pairs=5,active_pairs=9)
        def event(phase,checkpoint,pairs,stamp,size):
            return dict(phase=phase,checkpoint=checkpoint,pairs=pairs,monotonic_ns=stamp,bytes=size,
                        reason='global_payload_credit',credit=credit)
        events=[event('dispatch',1,5,1,5*1024**2),event('credit_start',1,5,10,5*1024**2),
                event('credit_end',1,5,20,5*1024**2),event('commit_end',1,5,21,5*1024**2),
                event('dispatch',2,14,22,9*1024**2),event('credit_start',2,14,25,9*1024**2),
                event('credit_end',2,14,35,9*1024**2),event('commit_end',2,14,36,9*1024**2)]
        child=dict(ok=True,pairs=14,experiment_phase=dict(target_mib=6,clock='CLOCK_MONOTONIC',dropped=0,events=events),
                   experiment_queue=dict(clock='CLOCK_MONOTONIC',dropped=0,samples=[
                       dict(monotonic_ns=n,sequence=n,queued_pairs=1,oldest_age_ns=n) for n in range(1,15)]),
                   capture_elapsed_ns=1_000_000_000,capture_finished_monotonic_ns=100,diagnostics=dict(fsync=dict(samples=10)))
        report=run.observe_child(child,6)
        self.assertEqual([b['pairs'] for b in report['actual_batches']],[5,9])
        self.assertEqual(report['continuous_wait_episodes'][0]['waits'],2)
        self.assertEqual(report['continuous_wait_episodes'][0]['wait_ns'],20)
        self.assertEqual(report['oldest_queued_age_ns_max'],14)
        self.assertEqual(report['lifecycle_fsync_calls'],10)

    def test_unclosed_waits_are_not_reported_as_complete(self):
        child=dict(ok=False,experiment_phase=dict(target_mib=6,clock='CLOCK_MONOTONIC',dropped=0,
            events=[dict(phase='credit_start',checkpoint=1,pairs=5,bytes=5,monotonic_ns=10,
                reason='global_payload_credit',credit=dict(payload_limit_bytes=16*1024**2,pair_limit=32,
                    pending_payload_bytes=6,active_payload_bytes=8,pending_pairs=5,active_pairs=7))]),
            experiment_queue=dict(clock='CLOCK_MONOTONIC',dropped=0,samples=[]),capture_elapsed_ns=1_000_000_000,
            capture_finished_monotonic_ns=100,diagnostics=dict(fsync=dict(samples=2)))
        report=run.observe_child(child,6)
        self.assertFalse(report['complete']);self.assertEqual(len(report['unclosed_waits']),1)
        with self.assertRaises(ValueError):run.observe_child(child,8)
        child['experiment_queue']['dropped']=1
        with self.assertRaises(ValueError):run.observe_child(child,6)


if __name__=='__main__':unittest.main()
