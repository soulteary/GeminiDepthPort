"""Synthetic clocks and I/O faults; no camera, ROS or physical latency claims."""
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest

from runtime.pipeline_diagnostics import DiagnosticWriter, PipelineDiagnostics, Timing

BASE = 1700000000000000000


def metadata(sequence=1, role='color', global_time=False):
    stamp = BASE + sequence * 33000000
    return {'version': 1, 'sequence': sequence, 'session_id': 'a' * 32,
            'frame_id': role + '_optical', 'stamp_sec': stamp // 1000000000,
            'stamp_nanosec': stamp % 1000000000,
            'stamp_mapping': 'sdk-global-unix' if global_time else 'sdk-system-unix',
            'system_timestamp_us': (stamp + 5000000 if global_time else stamp) // 1000,
            'global_timestamp_us': stamp // 1000 if global_time else None,
            'timing_clock': 'host-realtime-unix-ns', 'sdk_system_clock_verified': True,
            'sdk_return_unix_ns': stamp + 6000000,
            'publication_start_unix_ns': stamp + 8000000,
            'image_publish_finished_unix_ns': stamp + 12000000}


def image(diagnostics, value, received=15000000, mono=None, role='color'):
    stamp = value['stamp_sec'] * 1000000000 + value['stamp_nanosec']
    diagnostics.image(role, stamp, value['frame_id'], stamp + received, stamp if mono is None else mono)


class TimingTests(unittest.TestCase):
    def test_fixed_memory_quantiles_and_negative_differences_are_not_clamped(self):
        stats = Timing()
        self.assertIsNone(stats.report()['p99_upper_bound_ns'])
        length = len(stats.bins)
        for _ in range(10000):
            stats.observe(355000000)
        stats.observe(-7000)
        report = stats.report()
        self.assertEqual(len(stats.bins), length)
        self.assertEqual(report['count'], 10000)
        self.assertEqual(report['max_ns'], 355000000)
        self.assertEqual(report['p99_upper_bound_ns'], 355000000)
        self.assertEqual(report['negative_samples'], 1)
        self.assertEqual(report['negative_min_ns'], -7000)
        stats.observe(11000000000)
        for bad in [True, 1.5, None]:
            with self.assertRaises(ValueError):
                stats.observe(bad)


class AssociationTests(unittest.TestCase):
    def test_both_delivery_orders_preserve_stage_meaning_for_both_header_clocks(self):
        for global_time in [False, True]:
            for metadata_first in [False, True]:
                diagnostics = PipelineDiagnostics(2)
                value = metadata(global_time=global_time)
                if metadata_first:
                    diagnostics.metadata('color', value)
                image(diagnostics, value)
                if not metadata_first:
                    diagnostics.metadata('color', value)
                stats = diagnostics.summary()['streams']['color']
                self.assertEqual(stats['joined'], 1)
                self.assertEqual(stats['timing']['sdk_system_to_return']['max_ns'], 1000000 if global_time else 6000000)
                self.assertEqual(stats['timing']['sdk_return_to_publication_start']['max_ns'], 2000000)
                self.assertEqual(stats['timing']['publication_start_to_subscriber']['max_ns'], 7000000)
                scope = 'header_age_global_capture' if global_time else 'header_age_system_receive'
                self.assertEqual(stats['timing'][scope]['max_ns'], 15000000)
                self.assertFalse(diagnostics.summary()['sdk_internal_queue_observable'])
                self.assertIsNone(diagnostics.summary()['sdk_internal_queue_depth'])

    def test_delivery_may_precede_publish_return_without_becoming_zero(self):
        diagnostics = PipelineDiagnostics()
        value = metadata()
        image(diagnostics, value, received=10000000)
        diagnostics.metadata('color', value)
        stage = diagnostics.summary()['streams']['color']['timing']['image_publish_finish_to_subscriber']
        self.assertEqual(stage['negative_samples'], 1)
        self.assertEqual(stage['negative_min_ns'], -2000000)
        self.assertEqual(stage['count'], 0)

    def test_pending_join_memory_is_bounded_and_missing_components_remain_visible(self):
        diagnostics = PipelineDiagnostics(2)
        for sequence in range(1, 100):
            image(diagnostics, metadata(sequence))
        stats = diagnostics.summary()['streams']['color']
        self.assertEqual(stats['pending'], 2)
        self.assertEqual(stats['pending_high_water'], 2)
        self.assertEqual(stats['join_evictions'], 97)
        self.assertEqual(stats['joined'], 0)
        self.assertEqual(stats['images'], 99)

    def test_ambiguous_components_and_identity_mismatch_never_forge_latency(self):
        diagnostics = PipelineDiagnostics()
        value = metadata()
        image(diagnostics, value)
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['joined'], 0)
        image(diagnostics, metadata(2))
        bad = metadata(2)
        bad['frame_id'] = 'other'
        diagnostics.metadata('color', bad)
        self.assertEqual(diagnostics.summary()['streams']['color']['errors']['IDENTITY_BINDING'], 1)

    def test_unknown_clock_and_missing_fields_are_unavailable_not_zero(self):
        diagnostics = PipelineDiagnostics()
        value = metadata()
        value.pop('timing_clock')
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        stats = diagnostics.summary()['streams']['color']
        self.assertEqual(stats['joined'], 1)
        self.assertNotIn('publication_start_to_subscriber', stats['timing'])
        self.assertEqual(stats['unavailable_stages']['publication_start_to_subscriber'], 1)
        value = metadata(2)
        value['global_timestamp_us'] = 0
        value['stamp_mapping'] = 'sdk-global-unix'
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['errors']['TIMESTAMP_MAPPING'], 1)

    def test_sdk_system_clock_requires_explicit_realtime_verification(self):
        for verified in [None, False, 'true']:
            diagnostics = PipelineDiagnostics()
            value = metadata()
            if verified is None:
                value.pop('sdk_system_clock_verified')
            else:
                value['sdk_system_clock_verified'] = verified
            image(diagnostics, value)
            diagnostics.metadata('color', value)
            report = diagnostics.summary()['streams']['color']
            self.assertNotIn('sdk_system_to_return', report['timing'])
            self.assertEqual(report['unavailable_stages']['sdk_system_to_return'], 1)
            self.assertIn('publication_start_to_subscriber', report['timing'])

    def test_sequence_gap_session_restart_and_monotonic_receive_interval(self):
        diagnostics = PipelineDiagnostics()
        for sequence in [1, 3, 2]:
            value = metadata(sequence)
            image(diagnostics, value, mono=sequence * 33000000)
            diagnostics.metadata('color', value)
        stats = diagnostics.summary()['streams']['color']
        self.assertEqual(stats['sequence_gaps'], 1)
        self.assertEqual(stats['errors']['SEQUENCE_NOT_ADVANCING'], 1)
        self.assertEqual(stats['timing']['subscriber_interarrival']['negative_samples'], 1)
        value = metadata(4)
        value['session_id'] = 'b' * 32
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['session_changes'], 1)
        for capacity in [0, 4097, True]:
            with self.assertRaises(ValueError):
                PipelineDiagnostics(capacity)
        with self.assertRaises(ValueError):
            diagnostics.image('unknown', BASE, 'x', BASE, 0)
        diagnostics.metadata('color', {'stamp_sec': -1, 'stamp_nanosec': 0})
        self.assertEqual(diagnostics.summary()['streams']['color']['errors']['METADATA_STAMP'], 1)

    def test_session_collision_retired_session_and_exact_publisher_wall_are_visible(self):
        diagnostics = PipelineDiagnostics()
        value = metadata()
        value['image_publish_wall_ns'] = 355000000
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['timing']['ros_image_publish_wall']['max_ns'], 355000000)
        value['session_id'] = 'b' * 32
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['errors']['AMBIGUOUS_SESSION_HEADER'], 1)
        value = metadata(2)
        value['session_id'] = 'b' * 32
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        value = metadata(3)
        image(diagnostics, value)
        diagnostics.metadata('color', value)
        self.assertEqual(diagnostics.summary()['streams']['color']['errors']['RETIRED_SESSION'], 1)



class WriterTests(unittest.TestCase):
    def test_async_raw_bytes_metadata_and_fsync_are_distinct_and_private(self):
        with tempfile.TemporaryDirectory() as parent:
            directory = Path(parent) / 'recording'
            calls = []
            def sync(fd):
                calls.append(fd)
            writer = DiagnosticWriter(directory, sync_every=2, fsync=sync)
            source = bytearray([0, 1, 2, 3])
            self.assertTrue(writer.submit('color', BASE, source, 7))
            source[:] = b'\xff' * 4
            self.assertTrue(writer.submit_metadata('color', metadata()))
            result = writer.close()
            self.assertTrue(result['completed'], result)
            self.assertEqual(result['written_images'], 1)
            self.assertEqual(result['written_metadata'], 1)
            self.assertEqual(result['fsynced_payloads'], 1)
            self.assertEqual(result['queue']['depth'], 0)
            records = [json.loads(line) for line in (directory / 'frames.jsonl').read_text().splitlines()]
            self.assertEqual((directory / records[0]['file']).read_bytes(), b'\0\1\2\3')
            self.assertEqual(records[1]['metadata']['sequence'], 1)
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
            self.assertTrue(all(path.stat().st_mode & 0o777 == 0o600 for path in directory.iterdir()))
            self.assertGreaterEqual(len(calls), 3)
            self.assertFalse(writer.submit('color', BASE, b'x'))

    def test_slow_writer_exposes_backlog_and_does_not_block_producer_on_disk(self):
        with tempfile.TemporaryDirectory() as parent:
            entered, release = threading.Event(), threading.Event()
            def delayed():
                entered.set()
                release.wait(2)
            writer = DiagnosticWriter(Path(parent) / 'recording', capacity=2, max_queue_bytes=12, before_write=delayed)
            self.assertTrue(writer.submit('color', BASE, b'1234'))
            self.assertTrue(entered.wait(2))
            started = time.monotonic()
            self.assertTrue(writer.submit('depth', BASE + 1, b'1234'))
            self.assertTrue(writer.submit('color', BASE + 2, b'1234'))
            self.assertFalse(writer.submit('depth', BASE + 3, b'1234'))
            self.assertLess(time.monotonic() - started, .2)
            sampled = writer.summary()
            self.assertEqual(sampled['queue']['high_water'], 2)
            self.assertEqual(sampled['queue']['bytes_high_water'], 8)
            self.assertEqual(sampled['queue']['overflows'], 1)
            release.set()
            result = writer.close()
            self.assertFalse(result['completed'])
            self.assertEqual(result['error'], 'QUEUE_OVERFLOW')
            self.assertEqual(result['written'], 3)

    def test_sync_failure_never_claims_durable_completion(self):
        with tempfile.TemporaryDirectory() as parent:
            def failed(fd):
                raise OSError('private details must not be in report')
            writer = DiagnosticWriter(Path(parent) / 'recording', sync_every=1, fsync=failed)
            writer.submit('depth', BASE, b'\0\1')
            result = writer.close()
            self.assertFalse(result['completed'])
            self.assertEqual(result['fsynced_payloads'], 0)
            self.assertEqual(result['error'], 'WRITER_IO_FAILED')
            self.assertNotIn('private details', json.dumps(result))

    def test_io_error_and_shutdown_deadline_are_bounded(self):
        with tempfile.TemporaryDirectory() as parent:
            def failed():
                raise OSError('injected')
            writer = DiagnosticWriter(Path(parent) / 'failure', before_write=failed)
            writer.submit('color', BASE, b'a')
            self.assertEqual(writer.close()['error'], 'WRITER_IO_FAILED')
            entered, release = threading.Event(), threading.Event()
            def blocked():
                entered.set()
                release.wait(2)
            writer = DiagnosticWriter(Path(parent) / 'timeout', before_write=blocked)
            writer.submit('color', BASE, b'a')
            self.assertTrue(entered.wait(2))
            self.assertEqual(writer.close(.01)['error'], 'WRITER_SHUTDOWN_TIMEOUT')
            release.set()
            self.assertFalse(writer.close()['completed'])

    def test_directory_sync_error_prevents_completion_after_payload_sync(self):
        import os
        import stat
        with tempfile.TemporaryDirectory() as parent:
            def failed_directory(fd):
                if stat.S_ISDIR(os.fstat(fd).st_mode):
                    raise OSError('directory sync injected')
            writer = DiagnosticWriter(Path(parent) / 'failure', sync_every=1, fsync=failed_directory)
            writer.submit('color', BASE, b'a')
            report = writer.close()
            self.assertEqual(report['fsynced_payloads'], 1)
            self.assertFalse(report['completed'])
            self.assertEqual(report['error'], 'WRITER_IO_FAILED')

    def test_injected_355ms_disk_delay_is_measured_on_writer_not_callback(self):
        class Clock:
            value = 0
            def __call__(self):
                return self.value
        clock = Clock()
        def delayed():
            clock.value += 355000000
        with tempfile.TemporaryDirectory() as parent:
            writer = DiagnosticWriter(Path(parent) / 'delay', before_write=delayed, monotonic_ns=clock)
            writer.submit('color', BASE, b'a')
            report = writer.close()
            self.assertTrue(report['completed'])
            self.assertEqual(report['timing']['write']['max_ns'], 355000000)
            self.assertEqual(report['timing']['enqueue']['max_ns'], 0)
            self.assertEqual(report['timing']['worker_item']['max_ns'], 355000000)

    def test_queue_byte_bound_and_invalid_inputs_fail_before_writing(self):
        with tempfile.TemporaryDirectory() as parent:
            for options in [{'capacity': 0}, {'max_queue_bytes': 0}, {'sync_every': 0}, {'sync_interval': 0}]:
                with self.assertRaises(ValueError):
                    DiagnosticWriter(Path(parent) / 'bad', **options)
            writer = DiagnosticWriter(Path(parent) / 'valid', max_queue_bytes=2)
            for args in [('invalid', BASE, b'a'), ('color', 0, b'a'), ('color', BASE, 'text')]:
                with self.assertRaises(ValueError):
                    writer.submit(*args)
            self.assertFalse(writer.submit('color', BASE, b'123'))
            self.assertEqual(writer.close()['queue']['overflows'], 1)
            with self.assertRaises(ValueError):
                writer.close(0)
            with self.assertRaises(ValueError):
                writer.submit_metadata('color', {'stamp_sec': -1})
            bad_metadata = metadata()
            bad_metadata['extra'] = 'x' * 8192
            with self.assertRaises(ValueError):
                writer.submit_metadata('color', bad_metadata)
            with self.assertRaises(FileExistsError):
                DiagnosticWriter(Path(parent) / 'valid')


class LiveAdapterTests(unittest.TestCase):
    def test_ros_adapter_observes_both_unpaired_streams_and_writes_raw_asynchronously(self):
        import sys
        from types import SimpleNamespace as NS
        from unittest.mock import patch
        from runtime import pipeline_diagnostics as module
        subscriptions, closed = [], []
        def subscribe(kind, topic, callback, qos):
            subscriptions.append((topic, callback))
            return object()
        node = NS(create_subscription=subscribe, destroy_node=lambda: closed.append('node'))
        tick = [0.0]
        pending = []
        def monotonic():
            tick[0] += .05
            return tick[0]
        def spin(node, timeout_sec):
            if not pending and not closed:
                for role in ('color', 'depth'):
                    value = metadata(role=role)
                    stamp = NS(sec=value['stamp_sec'], nanosec=value['stamp_nanosec'])
                    image_message = NS(header=NS(stamp=stamp, frame_id=value['frame_id']), width=2, height=2, step=4,
                                       data=bytearray(range(8)))
                    callbacks = dict(subscriptions)
                    pending.append((callbacks['/synthetic/' + role + '/image_raw'], image_message))
                    pending.append((callbacks['/synthetic/' + role + '/frame_metadata'], NS(data=json.dumps(value))))
                closed.append('scheduled')
            if pending:
                callback, message = pending.pop(0)
                callback(message)
        qos = NS(QoSProfile=lambda **kwargs: kwargs, ReliabilityPolicy=NS(RELIABLE=1), DurabilityPolicy=NS(VOLATILE=0))
        modules = {'rclpy': NS(init=lambda: None, shutdown=lambda: closed.append('shutdown'),
                              create_node=lambda name: node, spin_once=spin),
                   'rclpy.qos': qos, 'sensor_msgs': NS(), 'sensor_msgs.msg': NS(Image=object),
                   'std_msgs': NS(), 'std_msgs.msg': NS(String=object)}
        samples = []
        pressure = NS(sample=lambda: samples.append(1), summary=lambda: {'samples': len(samples)})
        with tempfile.TemporaryDirectory() as parent:
            writer = DiagnosticWriter(Path(parent) / 'raw')
            with patch.dict(sys.modules, modules), patch.object(module.time, 'monotonic', monotonic):
                report = module.run_live('/synthetic', 2, writer=writer, pressure=pressure)
            self.assertTrue(report['writer']['completed'], report)
            self.assertEqual(report['writer']['written_images'], 2)
            self.assertEqual(report['writer']['written_metadata'], 2)
            self.assertIsNone(report['subscriber_history']['actual_queue_depth'])
            self.assertGreaterEqual(report['host_pressure']['samples'], 2)
            for role in ('color', 'depth'):
                self.assertEqual(report['streams'][role]['joined'], 1)
                self.assertEqual(report['streams'][role]['timing']['subscriber_callback']['count'], 1)
            self.assertIn('node', closed)
            self.assertIn('shutdown', closed)

    def test_cli_default_observes_only_and_runtime_failure_closes_writer(self):
        import io
        from unittest.mock import patch
        from contextlib import redirect_stdout
        from runtime import pipeline_diagnostics as module
        output = io.StringIO()
        with patch('sys.argv', ['diagnostics', '--namespace', '/synthetic', '--seconds', '1']), \
             patch.object(module, 'run_live', return_value={'writer': None}) as live, redirect_stdout(output):
            self.assertEqual(module.main(), 0)
            self.assertIsNone(live.call_args.args[3])
        with tempfile.TemporaryDirectory() as parent:
            output = io.StringIO()
            directory = Path(parent) / 'failure'
            with patch('sys.argv', ['diagnostics', '--namespace', '/synthetic', '--output-dir', str(directory)]), \
                 patch.object(module, 'run_live', side_effect=RuntimeError('private raw diagnostic details')), redirect_stdout(output):
                self.assertEqual(module.main(), 1)
            report = json.loads(output.getvalue())
            self.assertEqual(report['error'], 'DIAGNOSTICS_RUNTIME_FAILED')
            self.assertFalse(report['writer']['completed'])
            self.assertNotIn('private raw', output.getvalue())



if __name__ == '__main__':
    unittest.main()
