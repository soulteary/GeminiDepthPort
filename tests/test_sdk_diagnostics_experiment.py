import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('sdk_collect',ROOT/'experiments/sdk-2.10.6-diagnostics/collect.py')
collect=importlib.util.module_from_spec(spec);spec.loader.exec_module(collect)


class SdkDiagnosticCollectorTests(unittest.TestCase):
    def test_bounded_private_regular_snapshots_and_initial_empty_file(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);path=root/'snapshot.json';path.touch(mode=0o600)
            self.assertIsNone(collect.read_snapshot(path))
            value={'version':1,'sdk_base':collect.BASE,'events':[]}
            path.write_text(json.dumps(value));self.assertEqual(collect.read_snapshot(path),value)
            link=root/'link';link.symlink_to(path)
            with self.assertRaises(OSError):collect.read_snapshot(link)
            path.chmod(0o644)
            with self.assertRaises(ValueError):collect.read_snapshot(path)
            path.chmod(0o600);path.write_bytes(b'x'*(collect.LIMIT+1))
            with self.assertRaises(ValueError):collect.read_snapshot(path)
            value['events']=[{}]*65;path.write_text(json.dumps(value))
            with self.assertRaises(ValueError):collect.read_snapshot(path)

    def test_cursor_reports_initial_truncation_separately_from_later_omissions(self):
        cursor=collect.Cursor()
        def row(stamp,ids):return {'snapshot_monotonic_ns':stamp,'events':[{'id':v} for v in ids]}
        self.assertEqual(len(cursor.accept(row(10,[3,4]))),2)
        self.assertEqual(cursor.history_missing,2);self.assertEqual(cursor.events_missing,0)
        self.assertIsNone(cursor.accept(row(10,[3,4])))
        cursor.accept(row(20,[7,8]));self.assertEqual(cursor.events_missing,2)
        self.assertEqual(cursor.accept(row(30,[])),[])
        with self.assertRaises(ValueError):cursor.accept(row(5,[]))

if __name__=='__main__':unittest.main()
