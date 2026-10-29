import tempfile
from pathlib import Path
import unittest
from runtime.host_pressure import HostPressure


class HostPressureTests(unittest.TestCase):
    def test_unavailable_is_not_zero_pressure(self):
        with tempfile.TemporaryDirectory() as folder:
            collector = HostPressure(Path(folder))
            collector.sample()
            report = collector.summary()
            self.assertFalse(report['available'])
            self.assertEqual(report['maximum_gauges'], {})
            self.assertIn('pressure/io', report['unavailable_or_invalid'])
            self.assertEqual(report['latest_intervals'][0]['counter_deltas'], {})

    def test_io_delta_reclaim_and_dirty_gauges_are_bounded_and_reset_is_explicit(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root/'pressure').mkdir(); (root/'self').mkdir()
            tick = [1000]
            collector = HostPressure(root, unix_ns=lambda: tick[0]+1000000, mono_ns=lambda: tick[0])
            for index in range(20):
                (root/'pressure/io').write_text(f'some avg10=0.00 total={index*355000}\nfull avg10=0.00 total={index*100}\n')
                (root/'meminfo').write_text(f'Dirty: {index} kB\nWriteback: 7 kB\nMemAvailable: 100 kB\n')
                (root/'vmstat').write_text(f'pgscan_direct {index*10}\noom_kill 0\n')
                (root/'self/io').write_text(f'write_bytes: {index*4096}\nsyscw: {index}\n')
                tick[0] += 1000000000
                collector.sample()
            report = collector.summary()
            self.assertEqual(report['samples'],20)
            self.assertEqual(len(report['latest_intervals']),8)
            self.assertEqual(len(report['largest_io_some_intervals']),8)
            self.assertEqual(report['maximum_gauges']['dirty_bytes'],19*1024)
            self.assertEqual(report['latest_intervals'][-1]['counter_deltas']['io_some_stall_us'],355000)
            self.assertEqual(report['latest_intervals'][-1]['counter_deltas']['vm_pgscan_direct'],10)
            self.assertFalse(report['causation_verified'])
            (root/'pressure/io').write_text('some total=1\nfull total=0\n')
            collector.sample()
            self.assertEqual(collector.summary()['counter_resets']['io_some_stall_us'],1)

    def test_invalid_proc_values_and_clock_regression_do_not_abort_capture(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);(root/'pressure').mkdir();(root/'self').mkdir()
            for name in ('pressure/io','meminfo','vmstat','self/io'):
                (root/name).write_text('some total=-1\nDirty: -3 kB\npgscan_direct oops\nwrite_bytes: nope\n')
            ticks=iter([2,1])
            collector=HostPressure(root,mono_ns=lambda:next(ticks))
            collector.sample();collector.sample()
            report=collector.summary()
            self.assertIn('pressure/io:parse',report['unavailable_or_invalid'])
            self.assertIn('meminfo:parse',report['unavailable_or_invalid'])
            self.assertIn('vmstat:parse',report['unavailable_or_invalid'])
            self.assertIn('self/io:parse',report['unavailable_or_invalid'])
            self.assertIn('monotonic-regression',report['unavailable_or_invalid'])
            (root/'meminfo').write_text('x'*65537)
            # Separate collector avoids consuming the finite test clock again.
            other=HostPressure(root);other.sample()
            self.assertIn('meminfo',other.summary()['unavailable_or_invalid'])
