"""Bounded, independent-stream ROS timing diagnostics, optionally with raw writes.

This diagnostic recorder is not native V2 or an R2 instrumentation replacement.
It does not infer exposure sync, SDK queue occupancy, DDS delivery guarantees or
storage durability from write() success. Unix cross-process differences require
one synchronized host clock; all local intervals use monotonic time.
"""
import argparse
from collections import Counter, OrderedDict
import json
import math
import os
from pathlib import Path
import queue
import threading
import time

ROLES = ('color', 'depth')


def integer(value, minimum=0):
    return type(value) is int and value >= minimum


class Timing:
    """Fixed memory; quantiles are bucket upper bounds, maxima remain exact."""
    BOUNDS = tuple(range(0, 1000000001, 1000000)) + (2000000000, 5000000000, 10000000000)

    def __init__(self):
        self.count = self.total = self.maximum = self.negative = 0
        self.minimum = self.negative_min = None
        self.bins = [0] * (len(self.BOUNDS) + 1)

    def observe(self, ns):
        if not integer(ns) and not (type(ns) is int and ns < 0):
            raise ValueError('TIMING_INTEGER_REQUIRED')
        if ns < 0:
            self.negative += 1
            self.negative_min = min(self.negative_min, ns) if self.negative_min is not None else ns
            return
        import bisect
        self.bins[bisect.bisect_left(self.BOUNDS, ns)] += 1
        self.count += 1
        self.total += ns
        self.maximum = max(self.maximum, ns)
        self.minimum = min(self.minimum, ns) if self.minimum is not None else ns

    def report(self):
        result = {'count': self.count, 'min_ns': self.minimum,
                  'max_ns': self.maximum if self.count else None,
                  'mean_ns': self.total / self.count if self.count else None,
                  'negative_samples': self.negative, 'negative_min_ns': self.negative_min,
                  'quantile_method': 'fixed-1ms-buckets-through-1s-then-coarse-upper-bound'}
        for percent in (50, 95, 99):
            rank, count, upper = math.ceil(self.count * percent / 100), 0, None
            for index, samples in enumerate(self.bins):
                count += samples
                if rank and count >= rank:
                    upper = self.BOUNDS[index] if index < len(self.BOUNDS) else None
                    break
            result[f'p{percent}_upper_bound_ns'] = upper
        return result


class PipelineDiagnostics:
    """Join by stream/header, then bind metadata session/sequence; no pair needed."""
    def __init__(self, max_pending=64):
        if not integer(max_pending, 1) or max_pending > 4096:
            raise ValueError('PENDING_LIMIT')
        self.limit = max_pending
        self.pending = {role: OrderedDict() for role in ROLES}
        self.completed = {role: OrderedDict() for role in ROLES}
        self.retired_sessions = {role: set() for role in ROLES}
        self.stats = {role: {'images': 0, 'metadata': 0, 'joined': 0,
                            'pending_high_water': 0, 'join_evictions': 0,
                            'sequence_gaps': 0, 'session_changes': 0,
                            'errors': Counter(), 'unavailable_stages': Counter(), 'timing': {}, 'last_receive': None,
                            'last_identity': None, 'last_header': None} for role in ROLES}

    def duration(self, role, stage, ns):
        self.stats[role]['timing'].setdefault(stage, Timing()).observe(ns)

    def _item(self, role, stamp, kind, value):
        if role not in ROLES or not integer(stamp, 1):
            raise ValueError('DIAGNOSTIC_BINDING')
        pending, stats = self.pending[role], self.stats[role]
        if stamp not in pending and len(pending) >= self.limit:
            pending.popitem(last=False)
            stats['join_evictions'] += 1
        item = pending.setdefault(stamp, {})
        if kind in item:
            stats['errors']['DUPLICATE_COMPONENT'] += 1
            item['ambiguous'] = True
            return
        item[kind] = value
        stats['pending_high_water'] = max(stats['pending_high_water'], len(pending))
        if 'image' in item and 'metadata' in item:
            pending.pop(stamp, None)
            if not item.get('ambiguous'):
                self._join(role, stamp, item)

    def image(self, role, stamp_ns, frame_id, receive_unix_ns, receive_mono_ns):
        if role not in ROLES:
            raise ValueError('DIAGNOSTIC_ROLE')
        stats = self.stats[role]
        stats['images'] += 1
        if stats['last_receive'] is not None:
            self.duration(role, 'subscriber_interarrival', receive_mono_ns - stats['last_receive'])
        stats['last_receive'] = receive_mono_ns
        self._item(role, stamp_ns, 'image', {'frame_id': frame_id,
                   'receive_unix_ns': receive_unix_ns, 'receive_mono_ns': receive_mono_ns})

    def metadata(self, role, value):
        if role not in ROLES or not isinstance(value, dict):
            raise ValueError('DIAGNOSTIC_METADATA')
        if not integer(value.get('stamp_sec')) or not integer(value.get('stamp_nanosec')) or value['stamp_nanosec'] >= 1000000000:
            self.stats[role]['errors']['METADATA_STAMP'] += 1
            return
        self.stats[role]['metadata'] += 1
        self._item(role, value['stamp_sec'] * 1000000000 + value['stamp_nanosec'], 'metadata', value)

    def _join(self, role, stamp, item):
        stats, image, meta = self.stats[role], item['image'], item['metadata']
        session, sequence = meta.get('session_id'), meta.get('sequence')
        if not (isinstance(session, str) and len(session) == 32
                and all(c in '0123456789abcdef' for c in session)
                and integer(sequence) and meta.get('frame_id') == image['frame_id']):
            stats['errors']['IDENTITY_BINDING'] += 1
            return
        source = {'sdk-system-unix': 'system_timestamp_us', 'sdk-global-unix': 'global_timestamp_us'}.get(meta.get('stamp_mapping'))
        if source is None or not integer(meta.get(source), 1) or meta[source] * 1000 != stamp:
            stats['errors']['TIMESTAMP_MAPPING'] += 1
            return
        seen = self.completed[role]
        if stamp in seen:
            stats['errors']['AMBIGUOUS_SESSION_HEADER' if seen[stamp] != (session, sequence) else 'DUPLICATE_JOIN'] += 1
            return
        if session in self.retired_sessions[role]:
            stats['errors']['RETIRED_SESSION'] += 1
            return
        previous = stats['last_identity']
        if previous and previous[0] == session and sequence <= previous[1]:
            stats['errors']['SEQUENCE_NOT_ADVANCING'] += 1
            return
        if stats['last_header'] is not None and stamp <= stats['last_header']:
            stats['errors']['HEADER_NOT_ADVANCING'] += 1
            return
        if previous:
            if previous[0] == session:
                stats['sequence_gaps'] += sequence - previous[1] - 1
            else:
                if len(self.retired_sessions[role]) >= 64:
                    stats['errors']['SESSION_TRACK_LIMIT'] += 1
                    return
                self.retired_sessions[role].add(previous[0])
                stats['session_changes'] += 1
        stats['last_identity'] = (session, sequence)
        stats['last_header'] = stamp
        if len(seen) >= self.limit:
            seen.popitem(last=False)
        seen[stamp] = (session, sequence)
        stats['joined'] += 1
        received = image['receive_unix_ns']
        self.duration(role, 'header_age_' + ('global_capture' if source.startswith('global') else 'system_receive'), received - stamp)
        if integer(meta.get('image_publish_wall_ns')):
            self.duration(role, 'ros_image_publish_wall', meta['image_publish_wall_ns'])
        fields = {'sdk_system_to_return': (meta.get('sdk_return_unix_ns'), meta.get('system_timestamp_us', 0) * 1000 if integer(meta.get('system_timestamp_us'), 1) else None),
                  'sdk_return_to_publication_start': (meta.get('publication_start_unix_ns'), meta.get('sdk_return_unix_ns')),
                  'publication_start_to_subscriber': (received, meta.get('publication_start_unix_ns')),
                  'image_publish_call_unix': (meta.get('image_publish_finished_unix_ns'), meta.get('publication_start_unix_ns')),
                  'image_publish_finish_to_subscriber': (received, meta.get('image_publish_finished_unix_ns'))}
        clock_declared = meta.get('timing_clock') == 'host-realtime-unix-ns'
        for stage, (end, start) in fields.items():
            sdk_clock_valid = stage != 'sdk_system_to_return' or meta.get('sdk_system_clock_verified') is True
            if clock_declared and sdk_clock_valid and integer(end, 1) and integer(start, 1):
                self.duration(role, stage, end - start)
            else:
                stats['unavailable_stages'][stage] += 1

    def summary(self):
        streams = {}
        for role in ROLES:
            stats = self.stats[role]
            streams[role] = {key: dict(value) if key in ('errors', 'unavailable_stages') else value for key, value in stats.items()
                             if key not in ('timing', 'last_receive', 'last_identity', 'last_header')}
            streams[role]['pending'] = len(self.pending[role])
            streams[role]['image_callbacks_without_join'] = stats['images'] - stats['joined']
            streams[role]['metadata_callbacks_without_join'] = stats['metadata'] - stats['joined']
            streams[role]['timing'] = {key: value.report() for key, value in stats['timing'].items()}
        return {'version': 1, 'kind': 'independent-stream-pipeline-diagnostics', 'streams': streams,
                'clock_scope': 'local intervals monotonic; cross-process differences host Unix realtime',
                'negative_difference_scope': 'clock changes or overlap; subscriber may run before publisher returns',
                'sdk_internal_queue_depth': None, 'sdk_internal_queue_observable': False,
                'association_scope': 'bounded header/frame_id join with publisher-reported session; Image has no independent session',
                'delivery_verified': False, 'source_loss_verified': False}


class DiagnosticWriter:
    """FIFO, byte/count bounded, asynchronous raw writes with batched fsync.

    Queue overflow or write/sync error permanently prevents completed=true. No
    disk operation runs on submit(), and no per-frame digest is computed here.
    """
    def __init__(self, directory, capacity=32, max_queue_bytes=128 * 1024 * 1024,
                 sync_every=30, sync_interval=1.0, monotonic_ns=time.monotonic_ns,
                 fsync=os.fsync, before_write=None):
        if not integer(capacity, 1) or capacity > 4096 or not integer(max_queue_bytes, 1):
            raise ValueError('WRITER_QUEUE_LIMIT')
        if not integer(sync_every, 1) or sync_every > 1024 or not (0 < sync_interval <= 60):
            raise ValueError('WRITER_SYNC_LIMIT')
        self.directory = Path(directory)
        self.directory.mkdir(mode=0o700, parents=False, exist_ok=False)
        self.capacity, self.max_bytes, self.sync_every, self.sync_interval = capacity, max_queue_bytes, sync_every, sync_interval
        self.clock, self.fsync, self.before_write = monotonic_ns, fsync, before_write
        self.queue, self.lock, self.closing = queue.Queue(capacity), threading.Lock(), threading.Event()
        self.accepting, self.error, self.completed = True, None, False
        self.enqueued_at = OrderedDict()
        self.accepted = self.written = self.durable = self.overflows = self.queued = self.queued_bytes = self.high_water = self.bytes_high_water = 0
        self.in_flight = self.in_flight_bytes = self.pending_sync = self.pending_sync_high_water = 0
        self.retained_bytes_high_water = 0
        self.accepted_images = self.written_images = self.accepted_metadata = self.written_metadata = 0
        self.timings = {stage: Timing() for stage in ('queue_wait', 'write', 'index_write', 'worker_item', 'sync', 'enqueue')}
        self.thread = threading.Thread(target=self._run, name='gdp-diagnostic-writer', daemon=True)
        self.thread.start()

    def _fail(self, code):
        with self.lock:
            if self.error is None:
                self.error = code
            self.accepting = False

    def submit(self, role, stamp_ns, payload, receive_mono_ns=None):
        return self._submit(role, stamp_ns, payload, receive_mono_ns, None)

    def submit_metadata(self, role, value):
        if not isinstance(value, dict) or not integer(value.get('stamp_sec')) or not integer(value.get('stamp_nanosec')) or value['stamp_nanosec'] >= 1000000000:
            raise ValueError('WRITER_METADATA')
        encoded = json.dumps(value, separators=(',', ':'), allow_nan=False).encode()
        if len(encoded) > 8192:
            raise ValueError('WRITER_METADATA_LIMIT')
        return self._submit(role, value['stamp_sec'] * 1000000000 + value['stamp_nanosec'], encoded, None, value)

    def _submit(self, role, stamp_ns, payload, receive_mono_ns, metadata):
        started = self.clock()
        if role not in ROLES or not integer(stamp_ns, 1):
            raise ValueError('WRITER_INPUT')
        try:
            span = memoryview(payload)
        except TypeError as exc:
            raise ValueError('WRITER_INPUT') from exc
        size = span.nbytes
        with self.lock:
            if not self.accepting or self.closing.is_set():
                return False
            if self.queued >= self.capacity or self.queued_bytes + self.in_flight_bytes + size > self.max_bytes:
                self.overflows += 1
                self.error, self.accepting = 'QUEUE_OVERFLOW', False
                return False
            payload = bytes(span)
            self.accepted += 1
            if metadata is None:
                self.accepted_images += 1
            else:
                self.accepted_metadata += 1
            self.queued += 1
            self.queued_bytes += len(payload)
            self.high_water = max(self.high_water, self.queued)
            self.bytes_high_water = max(self.bytes_high_water, self.queued_bytes)
            self.retained_bytes_high_water = max(self.retained_bytes_high_water, self.queued_bytes + self.in_flight_bytes)
            enqueued = self.clock()
            self.enqueued_at[self.accepted] = enqueued
            self.queue.put_nowait((self.accepted, role, stamp_ns, payload, enqueued, receive_mono_ns, json.loads(payload) if metadata is not None else None))
            self.timings['enqueue'].observe(self.clock() - started)
        return True

    def _run(self):
        pending = []
        index = None
        last_sync = self.clock()
        records_since_sync = 0
        try:
            index = os.fdopen(os.open(self.directory / 'frames.jsonl', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w')
            while True:
                try:
                    item = self.queue.get(timeout=.05)
                except queue.Empty:
                    if (pending or records_since_sync) and (self.clock() - last_sync >= self.sync_interval * 1e9 or self.closing.is_set()):
                        self._sync(pending, index)
                        last_sync = self.clock()
                        records_since_sync = 0
                    if self.closing.is_set():
                        break
                    continue
                number, role, stamp, payload, enqueued, received, metadata = item
                dequeued = self.clock()
                with self.lock:
                    self.enqueued_at.pop(number, None)
                    self.queued -= 1
                    self.queued_bytes -= len(payload)
                    self.in_flight = 1
                    self.in_flight_bytes = len(payload)
                    self.timings['queue_wait'].observe(dequeued - enqueued)
                started = self.clock()
                if self.before_write:
                    self.before_write()
                name = None
                if metadata is None:
                    name = f'{role}-{number:012d}.raw'
                    handle = os.fdopen(os.open(self.directory / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb')
                    try:
                        if handle.write(payload) != len(payload):
                            raise OSError('short diagnostic payload write')
                        handle.flush()
                    except BaseException:
                        handle.close()
                        raise
                    pending.append(handle)
                finished = self.clock()
                index.write(json.dumps({'version': 1, 'kind': 'image' if metadata is None else 'metadata',
                                        'role': role, 'stamp_ns': stamp, 'file': name,
                                        'metadata': metadata,
                                        'payload_bytes': len(payload) if metadata is None else 0, 'receive_mono_ns': received,
                                        'enqueue_mono_ns': enqueued, 'dequeue_mono_ns': dequeued,
                                        'write_start_mono_ns': started, 'write_finished_mono_ns': finished}, separators=(',', ':')) + '\n')
                indexed = self.clock()
                records_since_sync += 1
                with self.lock:
                    self.timings['index_write'].observe(indexed - finished)
                    self.timings['worker_item'].observe(indexed - dequeued)
                    self.written += 1
                    if metadata is None:
                        self.written_images += 1
                    else:
                        self.written_metadata += 1
                    self.in_flight = 0
                    self.in_flight_bytes = 0
                    self.pending_sync = len(pending)
                    self.pending_sync_high_water = max(self.pending_sync_high_water, len(pending))
                    self.timings['write'].observe(finished - started)
                del payload, item, metadata
                if records_since_sync >= self.sync_every or self.clock() - last_sync >= self.sync_interval * 1e9:
                    self._sync(pending, index)
                    last_sync = self.clock()
                    records_since_sync = 0
            self._sync(pending, index)
            index.close()
            index = None
            directory = os.open(self.directory, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
            try:
                started = self.clock()
                self.fsync(directory)
                with self.lock:
                    self.timings['sync'].observe(self.clock() - started)
            finally:
                os.close(directory)
            with self.lock:
                self.completed = self.error is None and self.written == self.accepted and self.written_images > 0
        except Exception:
            self._fail('WRITER_IO_FAILED')
        finally:
            for handle in pending:
                try:
                    handle.close()
                except OSError:
                    self._fail('WRITER_IO_FAILED')
            if index:
                try:
                    index.close()
                except OSError:
                    self._fail('WRITER_IO_FAILED')

    def _sync(self, pending, index):
        started = self.clock()
        for handle in pending:
            self.fsync(handle.fileno())
        index.flush()
        self.fsync(index.fileno())
        count = len(pending)
        for handle in pending:
            handle.close()
        pending.clear()
        with self.lock:
            self.durable += count
            self.pending_sync = 0
            self.timings['sync'].observe(self.clock() - started)

    def close(self, timeout=30):
        if not (0 < timeout <= 300):
            raise ValueError('WRITER_CLOSE_LIMIT')
        self.closing.set()
        with self.lock:
            self.accepting = False
        self.thread.join(timeout)
        if self.thread.is_alive():
            self._fail('WRITER_SHUTDOWN_TIMEOUT')
        return self.summary()

    def summary(self):
        with self.lock:
            oldest = next(iter(self.enqueued_at.values()), None)
            return {'kind': 'diagnostic-raw-stream-writer', 'accepted': self.accepted,
                    'written': self.written, 'accepted_images': self.accepted_images,
                    'written_images': self.written_images, 'accepted_metadata': self.accepted_metadata,
                    'written_metadata': self.written_metadata, 'fsynced_payloads': self.durable,
                    'completed': self.completed and self.error is None, 'error': self.error,
                    'queue': {'capacity': self.capacity, 'max_bytes': self.max_bytes,
                              'depth': self.queued, 'bytes': self.queued_bytes,
                              'oldest_queued_age_ns': self.clock() - oldest if oldest is not None else None,
                              'high_water': self.high_water, 'bytes_high_water': self.bytes_high_water,
                              'in_flight': self.in_flight, 'in_flight_bytes': self.in_flight_bytes,
                              'retained_bytes_high_water': self.retained_bytes_high_water,
                              'byte_budget_scope': 'queued plus active writer payload and encoded metadata, excludes Python object overhead',
                              'overflows': self.overflows,
                              'pending_sync_files': self.pending_sync,
                              'pending_sync_high_water': self.pending_sync_high_water},
                    'timing': {key: value.report() for key, value in self.timings.items()},
                    'durability_scope': 'payload and index fsync plus directory fsync at successful close; writes alone are not durable',
                    'native_recording_format': False, 'source_completeness_verified': False}


def run_live(namespace, seconds, max_pending=64, writer=None, pressure=None):
    import rclpy
    from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy
    from sensor_msgs.msg import Image
    from std_msgs.msg import String
    diagnostics = PipelineDiagnostics(max_pending)
    if pressure is None:
        try:
            from .host_pressure import HostPressure
        except ImportError:
            from host_pressure import HostPressure
        pressure = HostPressure()
    pressure.sample()
    next_pressure = time.monotonic() + 1
    rclpy.init()
    node = rclpy.create_node('geminidepthport_pipeline_diagnostics')
    subscriptions = []
    qos = QoSProfile(depth=max_pending, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    def image_callback(role):
        def callback(message):
            started, received = time.monotonic_ns(), time.time_ns()
            stamp = message.header.stamp.sec * 1000000000 + message.header.stamp.nanosec
            try:
                diagnostics.image(role, stamp, message.header.frame_id, received, started)
                if message.width <= 0 or message.height <= 0 or message.step < message.width * 2 or len(message.data) != message.step * message.height:
                    raise ValueError('IMAGE_LAYOUT')
                if writer:
                    writer.submit(role, stamp, message.data, started)
            except (ValueError, TypeError):
                diagnostics.stats[role]['errors']['IMAGE_CALLBACK_INVALID'] += 1
            finally:
                diagnostics.duration(role, 'subscriber_callback', time.monotonic_ns() - started)
        return callback
    def metadata_callback(role):
        def callback(message):
            started = time.monotonic_ns()
            try:
                if len(message.data) > 8192:
                    raise ValueError('METADATA_LIMIT')
                metadata = json.loads(message.data)
                diagnostics.metadata(role, metadata)
                if writer:
                    writer.submit_metadata(role, metadata)
            except (ValueError, TypeError):
                diagnostics.stats[role]['errors']['METADATA_CALLBACK_INVALID'] += 1
            finally:
                diagnostics.duration(role, 'metadata_callback', time.monotonic_ns() - started)
        return callback
    try:
        for role in ROLES:
            subscriptions.append(node.create_subscription(Image, f'{namespace}/{role}/image_raw', image_callback(role), qos))
            subscriptions.append(node.create_subscription(String, f'{namespace}/{role}/frame_metadata', metadata_callback(role), qos))
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if time.monotonic() >= next_pressure:
                pressure.sample()
                next_pressure = time.monotonic() + 1
            rclpy.spin_once(node, timeout_sec=min(.1, max(0, deadline - time.monotonic())))
    finally:
        node.destroy_node()
        rclpy.shutdown()
    pressure.sample()
    result = diagnostics.summary()
    result['host_pressure'] = pressure.summary()
    result['subscriber_history'] = {'configured_depth': max_pending, 'actual_queue_depth': None, 'actual_queue_observable': False}
    if writer:
        result['writer'] = writer.close()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--seconds', type=float, default=120)
    parser.add_argument('--max-pending', type=int, default=64)
    parser.add_argument('--output-dir', type=Path, help='New private directory; enables asynchronous diagnostic raw recording')
    parser.add_argument('--queue-capacity', type=int, default=32)
    parser.add_argument('--queue-bytes', type=int, default=128 * 1024 * 1024)
    parser.add_argument('--sync-every', type=int, default=30)
    parser.add_argument('--sync-interval', type=float, default=1)
    args = parser.parse_args()
    if not args.namespace.startswith('/') or args.namespace == '/' or args.namespace.endswith('/') or not math.isfinite(args.seconds) or not 0 < args.seconds <= 86400:
        parser.error('explicit non-root namespace and bounded positive duration required')
    writer = DiagnosticWriter(args.output_dir, args.queue_capacity, args.queue_bytes, args.sync_every, args.sync_interval) if args.output_dir else None
    try:
        result = run_live(args.namespace, args.seconds, args.max_pending, writer)
    except (Exception, KeyboardInterrupt) as exc:
        result = {'version': 1, 'kind': 'independent-stream-pipeline-diagnostics',
                  'error': 'DIAGNOSTICS_RUNTIME_FAILED', 'error_type': type(exc).__name__}
        if writer:
            result['writer'] = writer.close()
        print(json.dumps(result, sort_keys=True))
        return 1
    print(json.dumps(result, sort_keys=True))
    return int(writer is not None and not result['writer']['completed'])


if __name__ == '__main__':
    raise SystemExit(main())
