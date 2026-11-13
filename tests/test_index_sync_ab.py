import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('index_sync_ab', ROOT / 'scripts/validation/index_sync_ab.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class IndexSyncExperimentTests(unittest.TestCase):
    def test_candidate_changes_only_index_sync_and_diagnostic_name(self):
        source = (ROOT / m.TARGET).read_text()
        changed = m.candidate(source)
        self.assertEqual(changed.replace(m.NEW, m.OLD), source)
        self.assertEqual(changed.count('chunk.file.sync_all()'), source.count('chunk.file.sync_all()'))
        self.assertIn('persist_checkpoint(&stage, &checkpoint, &raw', changed)
        self.assertIn('verify_checkpoint(', changed)

    def test_unknown_or_reapplied_source_rejected(self):
        for source in ('', m.OLD * 2, m.NEW, m.OLD + m.NEW):
            with self.assertRaisesRegex(ValueError, 'SOURCE_CONTRACT'):
                m.candidate(source)

    def test_production_keeps_sync_all(self):
        source = (ROOT / m.TARGET).read_text()
        self.assertIn(m.OLD, source)
        self.assertNotIn(m.NEW, source)


if __name__ == '__main__':
    unittest.main()
