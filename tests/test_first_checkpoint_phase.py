import importlib.util
from pathlib import Path
import sys
import unittest
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts/validation'))
import first_checkpoint_phase as build
import first_checkpoint_phase_run as run


class FirstPhaseTests(unittest.TestCase):
    def test_patch_preserves_sync_credit_and_readers(self):
        source=(ROOT/build.TARGET).read_text(); patched=build.patch_writer(source)
        for token in ('records.sync_all()', 'chunk.file.sync_all()', 'verify_checkpoint(',
                      'persist_checkpoint(', 'CHUNK_BYTES: u64 = 16 * 1024 * 1024',
                      'p.payload_bytes + active_bytes + length > CHUNK_BYTES'):
            self.assertEqual(source.count(token),patched.count(token))
        self.assertIn('first.chunks <= 1',patched)
        self.assertIn('else { CHUNK_PAIRS / 2 }',patched)
        self.assertNotIn('sleep(',patched)
        with self.assertRaises(ValueError):build.patch_writer(patched)

    def test_failed_writer_without_readback_keeps_summary(self):
        rows=[dict(variant='aligned',writer_count=3,simultaneous_rss_peak_bytes=None,
                   writers=[dict(independent_readback=None,diagnostics={})])]
        summary=run.ab.summarize_groups(rows,{'aligned':None})
        self.assertIsNone(summary[0]['per_process_rows'][0]['extra_two_readers_ns'])
        self.assertFalse(summary[0]['resource_diagnostic_complete'])

    def test_phase_observation_requires_first_counts_and_complete_commit(self):
        def child(n):
            return dict(barrier_target_monotonic_ns=100,experiment_phase=dict(clock='CLOCK_MONOTONIC',dropped=0,
                events=[dict(phase=p,checkpoint=1,pairs=n,monotonic_ns=t) for p,t in
                        [('dispatch',110),('index_start',120),('index_end',130),('commit_end',140)]]))
        row={'writers':[child(n) for n in (3,5,7)]}
        result=run.phase_report(row,[3,5,7])
        self.assertEqual(result['index_sync_concurrency_wall_ns'][3],10)
        with self.assertRaises(ValueError):run.phase_report(row,[7,7,7])
        row['writers'][0]['experiment_phase']['dropped']=1
        with self.assertRaises(ValueError):run.phase_report(row,[3,5,7])


if __name__=='__main__':unittest.main()
