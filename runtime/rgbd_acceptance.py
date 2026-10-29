#!/usr/bin/env python3
"""Bounded real ROS RGBD consumer observer. No SDK access or public image storage.

--wire-only uses the same standard-image consumer workload for official/Rust
comparisons. Strict mode additionally joins per-frame and pair metadata.
Contract traces contain projections only; they are not image recordings.
"""
import argparse
from collections import Counter, OrderedDict
import hashlib
import json
import math
import os
from pathlib import Path
import re
import struct
import time
from types import SimpleNamespace


class ContractError(ValueError):
    pass


def require(ok, code):
    if not ok:
        raise ContractError(code)


def integer(v, minimum=0):
    return type(v) is int and minimum <= v <= 2**64 - 1


def positive(v):
    return type(v) in (int, float) and math.isfinite(v) and v > 0


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def validate_spec(spec):
    require(isinstance(spec, dict) and spec.get('version') == 1, 'SPEC_VERSION')
    require(spec.get('session_policy') in ('single', 'explicit-restart'), 'SESSION_POLICY')
    require(type(spec.get('require_session_id', True)) is bool, 'SESSION_POLICY')
    require(type(spec.get('payload_digest', False)) is bool, 'DIGEST_POLICY')
    require(integer(spec.get('max_pending'), 1) and spec['max_pending'] <= 4096, 'BUFFER_LIMIT')
    require(integer(spec.get('max_unmatched')) and spec['max_unmatched'] <= 100000, 'UNMATCHED_LIMIT')
    streams = spec.get('streams', {})
    require(isinstance(streams, dict) and set(streams) == {'color', 'depth'}, 'STREAMS_REQUIRED')
    topics = []
    for role, stream in streams.items():
        require(isinstance(stream, dict), 'STREAM_SPEC')
        for key in ('image_topic', 'camera_info_topic', 'metadata_topic'):
            value = stream.get(key)
            require(isinstance(value, str) and len(value) <= 256 and value.startswith('/'), 'TOPIC_INVALID')
            topics.append(value)
        require(isinstance(stream.get('frame_id'), str) and 0 < len(stream['frame_id']) <= 256, 'FRAME_INVALID')
        require(stream.get('timestamp_source', 'host') in ('host', 'global'), 'TIMESTAMP_SOURCE')
        require(stream.get('encoding') == ('yuyv' if role == 'color' else '16UC1'), 'ENCODING_SPEC')
        require(all(integer(stream.get(k), 1) and stream[k] <= 16384 for k in ('width', 'height')), 'DIMENSION_SPEC')
        require(role != 'color' or stream['width'] % 2 == 0, 'DIMENSION_SPEC')
        require(integer(stream.get('min_frames'), 1), 'MIN_FRAMES')
        for key in ('min_fps', 'max_gap_seconds', 'freshness_seconds', 'max_future_seconds'):
            require(positive(stream.get(key)), 'TIMING_SPEC')
        calibration = stream.get('calibration')
        require(isinstance(calibration, dict) and set(calibration) == {'distortion_model', 'k', 'd', 'r', 'p'}, 'CALIBRATION_SPEC')
        count = {'plumb_bob': 5, 'rational_polynomial': 8, 'equidistant': 4}.get(calibration.get('distortion_model'))
        require(count is not None, 'CALIBRATION_SPEC')
        for key, size in (('k', 9), ('d', count), ('r', 9), ('p', 12)):
            require(isinstance(calibration.get(key), list) and len(calibration[key]) == size
                    and all(type(v) in (int, float) and math.isfinite(v) for v in calibration[key]), 'CALIBRATION_SPEC')
        if role == 'depth' and 'depth_reference' in stream:
            ref = stream['depth_reference']
            require(isinstance(ref, dict) and integer(ref.get('x')) and integer(ref.get('y'))
                    and ref['x'] < stream['width'] and ref['y'] < stream['height']
                    and positive(ref.get('distance_mm')) and positive(ref.get('tolerance_mm')), 'DEPTH_REFERENCE')
    pair = spec.get('pair', {})
    require(isinstance(pair, dict) and isinstance(pair.get('topic'), str)
            and pair['topic'].startswith('/') and len(pair['topic']) <= 256, 'PAIR_SPEC')
    require(pair.get('clock') in ('device', 'system') and integer(pair.get('max_delta_us'), 1)
            and integer(pair.get('min_pairs'), 1), 'PAIR_SPEC')
    topics.append(pair['topic'])
    require(len(set(topics)) == len(topics), 'TOPICS_DUPLICATE')
    transform = spec.get('tf', {})
    require(isinstance(transform, dict) and transform.get('parent_frame') == streams['color']['frame_id']
            and transform.get('child_frame') == streams['depth']['frame_id'], 'TF_BINDING')
    for key, size in (('translation_m', 3), ('rotation_xyzw', 4)):
        require(isinstance(transform.get(key), list) and len(transform[key]) == size
                and all(type(v) in (int, float) and math.isfinite(v) for v in transform[key]), 'TF_SPEC')
    require(positive(transform.get('tolerance')), 'TF_SPEC')
    return spec


def stamp_ns(stamp):
    require(type(stamp.sec) is int and 0 <= stamp.sec <= 2**31 - 1
            and type(stamp.nanosec) is int and 0 <= stamp.nanosec < 1000000000, 'STAMP_INVALID')
    result = stamp.sec * 1000000000 + stamp.nanosec
    require(result > 0, 'STAMP_ZERO')
    return result


def image_projection(msg, expected, digest=False):
    require(msg.encoding == expected['encoding'] and msg.width == expected['width']
            and msg.height == expected['height'] and msg.header.frame_id == expected['frame_id'], 'IMAGE_BINDING')
    require(msg.is_bigendian == 0 and msg.step >= msg.width * 2 and msg.step % 2 == 0
            and len(msg.data) == msg.step * msg.height, 'IMAGE_LAYOUT')
    require(msg.encoding != 'yuyv' or msg.step == msg.width * 2, 'YUYV_LAYOUT')
    output = {'stamp_ns': stamp_ns(msg.header.stamp), 'frame_id': msg.header.frame_id,
              'width': msg.width, 'height': msg.height, 'step': msg.step, 'encoding': msg.encoding,
              'is_bigendian': int(msg.is_bigendian), 'payload_len': len(msg.data)}
    if digest:
        output['sha256'] = hashlib.sha256(memoryview(msg.data)).hexdigest()
    if 'depth_reference' in expected:
        ref = expected['depth_reference']
        output['reference_depth_mm'] = struct.unpack_from('<H', memoryview(msg.data), ref['y'] * msg.step + ref['x'] * 2)[0]
    return output


def info_projection(msg):
    return {'stamp_ns': stamp_ns(msg.header.stamp), 'frame_id': msg.header.frame_id,
            'width': msg.width, 'height': msg.height, 'distortion_model': msg.distortion_model,
            # Humble fixed arrays may contain NumPy float64 scalars. Normalize
            # the ROS projection to plain JSON floats before contract checks;
            # otherwise live observation and JSON trace replay disagree.
            'k': [float(v) for v in msg.k], 'd': [float(v) for v in msg.d],
            'r': [float(v) for v in msg.r], 'p': [float(v) for v in msg.p],
            'binning_x': msg.binning_x, 'binning_y': msg.binning_y,
            'roi': [msg.roi.x_offset, msg.roi.y_offset, msg.roi.width, msg.roi.height, bool(msg.roi.do_rectify)]}


def tf_projection(msg, expected):
    result = []
    for transform in msg.transforms:
        if transform.header.frame_id == expected['parent_frame'] and transform.child_frame_id == expected['child_frame']:
            t, q = transform.transform.translation, transform.transform.rotation
            result.append({'parent_frame': transform.header.frame_id, 'child_frame': transform.child_frame_id,
                           'translation_m': [t.x, t.y, t.z], 'rotation_xyzw': [q.x, q.y, q.z, q.w]})
    return result


class StaticTfResolver:
    """Use tf2's static-tree semantics; never subscribe to dynamic transforms.

    Buffer and latest_time are injected so offline tests can verify delegation
    and frame direction without reimplementing transform composition.
    """
    def __init__(self, buffer, expected, latest_time):
        self.buffer, self.expected, self.latest_time = buffer, expected, latest_time

    def feed(self, message):
        for transform in message.transforms:
            self.buffer.set_transform_static(transform, 'gdp_consumer_static')
        try:
            # target=color optical, source=depth optical: p_color = T * p_depth.
            resolved = self.buffer.lookup_transform(self.expected['parent_frame'],
                self.expected['child_frame'], self.latest_time)
        except Exception:
            # Static edges can arrive in separate messages. Missing paths are
            # allowed to complete later; final TF_NOT_RECEIVED still fails.
            return []
        return tf_projection(SimpleNamespace(transforms=[resolved]), self.expected)


class StreamStats:
    # Fixed buckets bound memory for arbitrarily long runs; p99 is an upper bound.
    AGE_BOUNDS_MS = (1, 2, 5, 10, 20, 33, 50, 75, 100, 150, 250, 500, 1000, 2000, 5000, 10000)

    def __init__(self, timestamp_source=None):
        self.timestamp_source = timestamp_source
        self.stamp_mappings = set()
        self.negative_ages = 0
        self.images = self.matched = self.sequence_gaps = self.gap_events = 0
        self.last_stamp = self.last_sequence = self.first_receive = self.last_receive = None
        self.age_sum = self.age_max = 0
        self.age_bins = [0] * (len(self.AGE_BOUNDS_MS) + 1)
        self.errors = Counter()

    def summary(self):
        rank, total, p99 = math.ceil(self.images * .99), 0, None
        for index, count in enumerate(self.age_bins):
            total += count
            if total >= rank and rank:
                p99 = self.AGE_BOUNDS_MS[index] if index < len(self.AGE_BOUNDS_MS) else None
                break
        active = (self.last_receive - self.first_receive) / 1e9 if self.images > 1 else 0
        return {'images': self.images, 'matched_frames': self.matched, 'image_gap_events': self.gap_events,
                'source_sequence_skipped_values': self.sequence_gaps, 'drop_count_verified': False,
                'active_fps': (self.images - 1) / active if active > 0 else None,
                'age_ms': {'mean': self.age_sum / self.images / 1e6 if self.images else None,
                           'maximum': self.age_max / 1e6 if self.images else None,
                           'p99_bucket_upper_bound': p99, 'histogram_bounds_ms': list(self.AGE_BOUNDS_MS),
                           'histogram_counts': self.age_bins,
                           'negative_samples': self.negative_ages,
                           'scope': self.age_scope()},
                'contract_errors': dict(self.errors)}


    def age_scope(self):
        mapping = next(iter(self.stamp_mappings)) if len(self.stamp_mappings) == 1 else None
        if not self.stamp_mappings:
            mapping = {'host': 'sdk-system-unix', 'global': 'sdk-global-unix'}.get(self.timestamp_source)
        if mapping == 'sdk-system-unix':
            return 'subscriber-receive-unix-minus-sdk-system-receive-stamp-not-exposure-latency'
        if mapping == 'sdk-global-unix':
            return 'subscriber-receive-unix-minus-sdk-global-capture-stamp-includes-capture-processing-transport-and-mapping-error'
        return 'subscriber-receive-unix-minus-published-header-unspecified-or-mixed-clock-not-stage-latency'


class Observer:
    def __init__(self, spec, wire_only=False):
        self.spec = validate_spec(spec)
        self.wire_only = wire_only
        self.stats = {role: StreamStats(self.spec['streams'][role].get('timestamp_source')) for role in ('color', 'depth')}
        self.pending = {role: OrderedDict() for role in self.stats}
        self.complete = {role: OrderedDict() for role in self.stats}
        self.pairs = OrderedDict()
        self.errors = Counter()
        self.pair_count = self.pair_sequence_gaps = self.evictions = 0
        self.last_pair = {}
        self.sessions = {role: None for role in self.stats}
        self.retired_sessions = {role: set() for role in self.stats}
        self.max_sessions = 256
        self.qos = {}
        self.tf_received = self.tf_late_join_requested = False
        self.last_receive_ns = 0

    def error(self, code, role=None):
        (self.stats[role].errors if role else self.errors)[code] += 1

    def _bound(self, mapping):
        while len(mapping) > self.spec['max_pending']:
            mapping.popitem(last=False)
            self.evictions += 1

    def contains_image(self, role, stamp):
        """Whether a window's Image is awaiting related topic components."""
        return role in self.pending and (stamp in self.complete[role]
            or 'image' in self.pending[role].get(stamp, {}))

    def _session(self, role, metadata):
        session = metadata.get('session_id')
        if self.spec.get('require_session_id', True):
            require(isinstance(session, str) and re.fullmatch('[0-9a-f]{32}', session) is not None, 'SESSION_MISSING')
        elif session is not None:
            require(isinstance(session, str) and re.fullmatch('[0-9a-f]{32}', session) is not None, 'SESSION_INVALID')
        key = session or 'legacy-no-session'
        previous = self.sessions[role]
        if previous != key:
            require(key not in self.retired_sessions[role], 'SESSION_RETIRED')
            if previous is not None:
                require(self.spec['session_policy'] == 'explicit-restart', 'SESSION_CHANGED')
                require(len(self.retired_sessions[role]) < self.max_sessions, 'SESSION_LIMIT')
                self.retired_sessions[role].add(previous)
            self.sessions[role] = key
            self.stats[role].last_sequence = None
        return key

    def observe(self, kind, role, value, receive_ns):
        self.last_receive_ns = max(self.last_receive_ns, receive_ns)
        try:
            if kind == 'end':
                return
            if kind == 'error':
                require(isinstance(value.get('code'), str) and re.fullmatch('[A-Z_]{1,80}', value['code']) is not None, 'ERROR_EVENT')
                self.error(value['code'], role if role in self.stats else None)
                return
            if kind == 'qos':
                allowed = {'color:image', 'color:info', 'depth:image', 'depth:info', 'tf',
                           'color:metadata', 'depth:metadata', 'pair'}
                require(role in allowed, 'QOS_ROLE')
                require(value.get('ready') is True, 'QOS_CONTRACT')
                self.qos[role] = True
                return
            if kind == 'tf_subscribe':
                self.tf_late_join_requested = value.get('after_both_streams') is True
                return
            if kind == 'tf':
                expected = self.spec['tf']
                require(value.get('parent_frame') == expected['parent_frame'] and value.get('child_frame') == expected['child_frame'], 'TF_BINDING')
                q = value.get('rotation_xyzw', [])
                require(len(q) == 4 and all(type(v) in (int, float) and math.isfinite(v) for v in q), 'TF_ROTATION')
                require(abs(sum(v*v for v in q) - 1) <= expected['tolerance'], 'TF_ROTATION')
                for key in ('translation_m', 'rotation_xyzw'):
                    observed, target = value.get(key, []), expected[key]
                    require(len(observed) == len(target) and all(type(v) in (int, float) and math.isfinite(v) for v in observed), 'TF_VALUES')
                    # Quaternions q/-q encode the same rotation.
                    direct = max(abs(a-b) for a, b in zip(observed, target))
                    opposite = max(abs(a+b) for a, b in zip(observed, target)) if key == 'rotation_xyzw' else math.inf
                    require(min(direct, opposite) <= expected['tolerance'], 'TF_VALUES')
                self.tf_received = True
                return
            if kind == 'pair':
                require(not self.wire_only, 'PAIR_UNEXPECTED')
                require(integer(value.get('pair_sequence')), 'PAIR_SEQUENCE')
                require(integer(value.get('color_stamp_ns'), 1) and integer(value.get('depth_stamp_ns'), 1), 'PAIR_STAMP')
                key = (value['color_stamp_ns'], value['depth_stamp_ns'])
                require(key not in self.pairs, 'PAIR_DUPLICATE')
                self.pairs[key] = value
                self._bound(self.pairs)
            else:
                require(role in self.stats and kind in ('image', 'info', 'metadata'), 'EVENT_TYPE')
                expected, stats = self.spec['streams'][role], self.stats[role]
                if kind == 'metadata':
                    require(integer(value.get('stamp_sec')) and value['stamp_sec'] <= 2**31 - 1
                            and integer(value.get('stamp_nanosec')) and value['stamp_nanosec'] < 1000000000, 'METADATA_STAMP')
                stamp = value.get('stamp_ns') if kind != 'metadata' else value.get('stamp_sec', -1) * 1000000000 + value.get('stamp_nanosec', -1)
                require(integer(stamp, 1) and stamp <= (2**31 - 1) * 1000000000 + 999999999, 'STAMP_INVALID')
                if kind == 'image':
                    require(value.get('encoding') == expected['encoding'] and value.get('width') == expected['width']
                            and value.get('height') == expected['height'] and value.get('frame_id') == expected['frame_id'], 'IMAGE_BINDING')
                    require(integer(value.get('step'), expected['width'] * 2) and value['step'] % 2 == 0
                            and value.get('payload_len') == value['step'] * expected['height'] and value.get('is_bigendian') == 0, 'IMAGE_LAYOUT')
                    require(role != 'color' or value['step'] == expected['width'] * 2, 'YUYV_LAYOUT')
                    age = receive_ns - stamp
                    require(-expected['max_future_seconds'] * 1e9 <= age <= expected['freshness_seconds'] * 1e9, 'IMAGE_AGE')
                    if stats.last_stamp is not None:
                        require(stamp > stats.last_stamp, 'IMAGE_STAMP_NOT_ADVANCING')
                        if stamp - stats.last_stamp > expected['max_gap_seconds'] * 1e9:
                            stats.gap_events += 1
                    stats.last_stamp = stamp
                    stats.first_receive = stats.first_receive if stats.first_receive is not None else receive_ns
                    stats.last_receive = receive_ns
                    stats.images += 1
                    stats.negative_ages += int(age < 0)
                    stats.age_sum += age
                    stats.age_max = max(stats.age_max, age)
                    index = next((i for i, bound in enumerate(stats.AGE_BOUNDS_MS) if age <= bound * 1e6), len(stats.AGE_BOUNDS_MS))
                    stats.age_bins[index] += 1
                    if 'depth_reference' in expected:
                        reference = expected['depth_reference']
                        depth = value.get('reference_depth_mm', 0)
                        require(integer(depth, 1) and abs(depth - reference['distance_mm']) <= reference['tolerance_mm'], 'DEPTH_REFERENCE')
                require(stamp not in self.complete[role], 'FRAME_DUPLICATE')
                item = self.pending[role].setdefault(stamp, {})
                require(kind not in item, 'COMPONENT_DUPLICATE')
                item[kind] = value
                self._bound(self.pending[role])
                needed = ('image', 'info') if self.wire_only else ('image', 'info', 'metadata')
                if all(part in item for part in needed):
                    self._complete(role, stamp, item)
                    del self.pending[role][stamp]
            if not self.wire_only:
                self._join_pairs()
        except (ContractError, KeyError, TypeError, ValueError, OverflowError) as exc:
            code = str(exc) if isinstance(exc, ContractError) else 'MALFORMED_EVENT'
            self.error(code, role if role in self.stats else None)

    def _complete(self, role, stamp, item):
        expected = self.spec['streams'][role]
        image, info = item['image'], item['info']
        require(all(info.get(k) == image.get(k) for k in ('stamp_ns', 'frame_id', 'width', 'height')), 'INFO_IMAGE_MISMATCH')
        require(info.get('binning_x') in (0, 1) and info.get('binning_y') in (0, 1)
                and info.get('roi') == [0, 0, 0, 0, False], 'INFO_CROP')
        calibration = expected['calibration']
        require(info.get('distortion_model') == calibration['distortion_model'], 'CALIBRATION_MISMATCH')
        for key in ('k', 'd', 'r', 'p'):
            observed, target = info.get(key), calibration[key]
            require(isinstance(observed, list) and len(observed) == len(target)
                    and all(type(a) in (int, float) and math.isfinite(a) and abs(a-b) <= 1e-8 for a,b in zip(observed, target)), 'CALIBRATION_MISMATCH')
        if not self.wire_only:
            metadata = item['metadata']
            require(metadata.get('version') == 1 and metadata.get('publisher_qos_verified') is True, 'METADATA_SCHEMA')
            require(all(metadata.get(k) == image.get(k) for k in ('frame_id', 'width', 'height', 'step', 'encoding')), 'METADATA_IMAGE_MISMATCH')
            mapping = metadata.get('stamp_mapping')
            source_key = {'sdk-system-unix': 'system_timestamp_us',
                          'sdk-global-unix': 'global_timestamp_us'}.get(mapping)
            require(source_key is not None and integer(metadata.get('system_timestamp_us'), 1)
                    and integer(metadata.get(source_key), 1)
                    and metadata[source_key] * 1000 == stamp, 'METADATA_TIME_MAPPING')
            if 'timestamp_source' in expected:
                require(mapping == {'host': 'sdk-system-unix', 'global': 'sdk-global-unix'}.get(expected['timestamp_source']),
                        'METADATA_TIME_MAPPING')
            self.stats[role].stamp_mappings.add(mapping)
            require(integer(metadata.get('sequence')), 'METADATA_SEQUENCE')
            session = self._session(role, metadata)
            last = self.stats[role].last_sequence
            if last is not None:
                require(metadata['sequence'] > last, 'SOURCE_SEQUENCE_NOT_ADVANCING')
                self.stats[role].sequence_gaps += metadata['sequence'] - last - 1
            self.stats[role].last_sequence = metadata['sequence']
            item['session'] = session
            if role == 'depth':
                require(positive(metadata.get('source_depth_scale_mm')) and metadata.get('output_depth_scale_mm') == 1
                        and type(metadata.get('normalized')) is bool, 'DEPTH_METADATA')
                require(metadata['normalized'] or metadata['source_depth_scale_mm'] == 1, 'DEPTH_METADATA')
            else:
                require(metadata.get('source_depth_scale_mm') is None and metadata.get('output_depth_scale_mm') is None
                        and metadata.get('normalized') is False, 'COLOR_METADATA')
            self.complete[role][stamp] = item
            self._bound(self.complete[role])
        self.stats[role].matched += 1

    def _join_pairs(self):
        for key, pair in list(self.pairs.items()):
            color, depth = self.complete['color'].get(key[0]), self.complete['depth'].get(key[1])
            if color is None or depth is None:
                continue
            del self.pairs[key]
            del self.complete['color'][key[0]]
            del self.complete['depth'][key[1]]
            require(color['session'] == depth['session'], 'PAIR_SESSION_MISMATCH')
            session = color['session']
            require((pair.get('session_id') or 'legacy-no-session') == session, 'PAIR_SESSION_MISMATCH')
            require(pair.get('version') == 1 and pair.get('publisher_qos_verified') is True
                    and pair.get('same_frameset') is True and pair.get('provenance') == 'sdkframeset'
                    and pair.get('hardware_sync_verified') is False, 'PAIR_PROVENANCE')
            require(pair.get('clock') == self.spec['pair']['clock'], 'PAIR_CLOCK')
            clock_key = 'device_timestamp_us' if pair['clock'] == 'device' else 'system_timestamp_us'
            for role, item in (('color', color), ('depth', depth)):
                require(pair.get(role + '_sequence') == item['metadata']['sequence'], 'PAIR_FRAME_SEQUENCE')
                raw = item['metadata'].get(clock_key)
                require(integer(raw, 1)
                        and pair.get(role + '_timestamp_us') == raw, 'PAIR_FRAME_TIMESTAMP')
            delta = abs(pair['color_timestamp_us'] - pair['depth_timestamp_us'])
            require(pair.get('delta_us') == delta <= self.spec['pair']['max_delta_us'], 'PAIR_DELTA')
            previous = self.last_pair.get(session)
            if previous is not None:
                require(pair['pair_sequence'] > previous['sequence'], 'PAIR_SEQUENCE_NOT_ADVANCING')
                require(pair['color_timestamp_us'] > previous['color_timestamp_us']
                        and pair['depth_timestamp_us'] > previous['depth_timestamp_us'], 'PAIR_TIMESTAMP_NOT_ADVANCING')
                self.pair_sequence_gaps += pair['pair_sequence'] - previous['sequence'] - 1
            require(len(self.last_pair) < self.max_sessions or session in self.last_pair, 'SESSION_LIMIT')
            self.last_pair[session] = {'sequence': pair['pair_sequence'],
                                      'color_timestamp_us': pair['color_timestamp_us'],
                                      'depth_timestamp_us': pair['depth_timestamp_us']}
            self.pair_count += 1

    def summary(self, replay=False):
        errors = self.errors.copy()
        unmatched = self.evictions + len(self.pairs) + sum(len(v) for v in self.pending.values())
        if not self.wire_only:
            unmatched += sum(len(v) for v in self.complete.values())
        if unmatched > self.spec['max_unmatched']:
            errors['UNMATCHED_LIMIT'] += 1
        for role, stats in self.stats.items():
            if stats.images < self.spec['streams'][role]['min_frames']:
                errors['INSUFFICIENT_' + role.upper() + '_FRAMES'] += 1
            if stats.matched < self.spec['streams'][role]['min_frames']:
                errors['INSUFFICIENT_' + role.upper() + '_MATCHES'] += 1
            fps = stats.summary()['active_fps']
            if fps is None or fps < self.spec['streams'][role]['min_fps']:
                errors['LOW_' + role.upper() + '_FPS'] += 1
            if stats.gap_events:
                errors[role.upper() + '_STAMP_GAPS'] += stats.gap_events
            if stats.last_receive is None or self.last_receive_ns - stats.last_receive > self.spec['streams'][role]['freshness_seconds'] * 1e9:
                errors[role.upper() + '_RECEIVE_STALE'] += 1
        expected_qos = {'color:image', 'color:info', 'depth:image', 'depth:info', 'tf'}
        if not self.wire_only:
            expected_qos |= {'color:metadata', 'depth:metadata', 'pair'}
            if self.pair_count < self.spec['pair']['min_pairs']:
                errors['INSUFFICIENT_PAIRS'] += 1
        if not expected_qos <= self.qos.keys():
            errors['QOS_NOT_OBSERVED'] += 1
        if not self.tf_received:
            errors['TF_NOT_RECEIVED'] += 1
        if not self.tf_late_join_requested:
            errors['TF_LATE_JOIN_NOT_EXERCISED'] += 1
        return {'version': 1, 'kind': 'rgbd-consumer-observer',
                'ok': not errors and not any(s.errors for s in self.stats.values()),
                'mode': 'standard-wire' if self.wire_only else 'strict-rgbd',
                'streams': {role: stats.summary() for role, stats in self.stats.items()},
                'matched_pairs': self.pair_count, 'pair_sequence_skipped_values': self.pair_sequence_gaps,
                'unmatched_or_evicted_components': unmatched, 'buffer_limit_per_map': self.spec['max_pending'],
                'association_evictions': self.evictions,
                'contract_errors': dict(errors), 'qos_observed': sorted(self.qos),
                'tf_received': self.tf_received, 'late_tf_subscription_requested': self.tf_late_join_requested,
                'replay': replay, 'raw_images_recorded': False,
                'scope': 'ROS transport-contract-consumer-not-business-consumer-certification',
                'physical_scale_verified': False, 'hardware_sync_verified': False}


class TraceWriter:
    def __init__(self, path, spec, wire_only, max_bytes):
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        self.handle = os.fdopen(descriptor, 'w')
        self.maximum, self.written = max_bytes, 0
        self.write({'kind': 'header', 'version': 1, 'spec_sha256': hashlib.sha256(canonical(spec)).hexdigest(), 'wire_only': wire_only})

    def write(self, event):
        line = canonical(event).decode() + '\n'
        require(self.written + len(line.encode()) <= self.maximum, 'TRACE_SIZE_LIMIT')
        self.handle.write(line)
        self.written += len(line.encode())

    def close(self):
        self.handle.close()


def replay_trace(path, spec, wire_only):
    observer = Observer(spec, wire_only)
    ended = False
    with open(path) as handle:
        first = handle.readline(65537)
        require(len(first) <= 65536, 'TRACE_LINE_LIMIT')
        header = json.loads(first)
        require(header.get('kind') == 'header' and header.get('version') == 1
                and header.get('spec_sha256') == hashlib.sha256(canonical(spec)).hexdigest()
                and header.get('wire_only') == wire_only, 'TRACE_SPEC_MISMATCH')
        while True:
            line = handle.readline(65537)
            if not line:
                break
            require(len(line) <= 65536, 'TRACE_LINE_LIMIT')
            event = json.loads(line)
            require(not ended, 'TRACE_AFTER_END')
            observer.observe(event['kind'], event['role'], event['value'], event['receive_ns'])
            ended = event['kind'] == 'end'
    require(ended, 'TRACE_INCOMPLETE')
    return observer.summary(replay=True)


def run_live(spec, seconds, discovery_seconds, tf_delay_seconds, wire_only=False, trace=None):
    import rclpy
    from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy
    from sensor_msgs.msg import Image, CameraInfo
    from std_msgs.msg import String
    from tf2_msgs.msg import TFMessage
    try:
        from .qos_contract import validate_offered_qos, STATIC_TF_SUBSCRIPTION_DEPTH
    except ImportError:
        from qos_contract import validate_offered_qos, STATIC_TF_SUBSCRIPTION_DEPTH
    observer = Observer(spec, wire_only)
    rclpy.init()
    static_resolver = None
    if wire_only:
        from tf2_ros import Buffer
        from rclpy.time import Time
        static_resolver = StaticTfResolver(Buffer(), spec['tf'], Time())
    node = rclpy.create_node('geminidepthport_consumer_observer')
    qos = QoSProfile(depth=10, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    # /tf_static has multiple independent writers in a camera group. A
    # depth-1 reader can evict other writers' late-join samples before spin.
    tf_qos = QoSProfile(depth=STATIC_TF_SUBSCRIPTION_DEPTH, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.TRANSIENT_LOCAL)
    subscriptions, topic_bindings = [], []
    active = [False]
    collecting_images = [True]
    def emit(kind, role, value, now=None):
        if not active[0]:
            return
        if not collecting_images[0]:
            # Only finish joins for Images already received inside the window.
            # A trailing Info/metadata burst must not manufacture a new frame.
            if kind in ('info', 'metadata'):
                stamp = value.get('stamp_ns') if kind == 'info' else value.get('stamp_sec', -1)*1000000000 + value.get('stamp_nanosec', -1)
                if not observer.contains_image(role, stamp): return
            elif kind == 'pair':
                if not all(observer.contains_image(stream, value.get(stream+'_stamp_ns')) for stream in ('color', 'depth')): return
            elif kind == 'image': return
        now = time.time_ns() if now is None else now
        observer.observe(kind, role, value, now)
        if trace:
            trace.write({'kind': kind, 'role': role, 'value': value, 'receive_ns': now})
    def image_callback(role):
        def callback(msg):
            if not active[0] or not collecting_images[0]: return
            received = time.time_ns()
            try:
                value = image_projection(msg, spec['streams'][role], spec.get('payload_digest', False))
                emit('image', role, value, received)
            except ContractError as exc:
                emit('error', role, {'code': str(exc)})
        return callback
    def metadata_callback(kind, role):
        def callback(msg):
            if not active[0]: return
            try:
                require(len(msg.data) <= 4096, 'METADATA_SIZE')
                value = json.loads(msg.data)
                require(isinstance(value, dict), 'METADATA_SCHEMA')
                emit(kind, role, value)
            except (ContractError, ValueError):
                emit('error', role, {'code': 'METADATA_MALFORMED'})
        return callback
    try:
        for role, stream in spec['streams'].items():
            for kind, message_type, key, callback in (
                ('image', Image, 'image_topic', image_callback(role)),
                ('info', CameraInfo, 'camera_info_topic', lambda msg, role=role: emit('info', role, info_projection(msg))),
            ):
                subscriptions.append(node.create_subscription(message_type, stream[key], callback, qos))
                topic_bindings.append((role + ':' + kind, stream[key], False))
            if not wire_only:
                subscriptions.append(node.create_subscription(String, stream['metadata_topic'], metadata_callback('metadata', role), qos))
                topic_bindings.append((role + ':metadata', stream['metadata_topic'], False))
        if not wire_only:
            subscriptions.append(node.create_subscription(String, spec['pair']['topic'], metadata_callback('pair', 'pair'), qos))
            topic_bindings.append(('pair', spec['pair']['topic'], False))
        deadline = time.monotonic() + discovery_seconds
        while time.monotonic() < deadline:
            rclpy.spin_once(node, timeout_sec=.05)
        active[0] = True
        start, next_qos, tf_created = time.monotonic(), 0., False
        while time.monotonic() - start < seconds:
            elapsed = time.monotonic() - start
            if not tf_created and elapsed >= tf_delay_seconds:
                def receive_tf(msg):
                    try:
                        projections = static_resolver.feed(msg) if wire_only else tf_projection(msg, spec['tf'])
                        for value in projections: emit('tf', 'tf', value)
                    except Exception:
                        emit('error', 'tf', {'code': 'TF_STATIC_INPUT_INVALID'})
                emit('tf_subscribe', 'tf', {'after_both_streams': all(s.images > 0 for s in observer.stats.values())})
                subscriptions.append(node.create_subscription(TFMessage, '/tf_static', receive_tf, tf_qos))
                topic_bindings.append(('tf', '/tf_static', True))
                tf_created = True
                # This late-created binding was absent from the initial poll.
                # Observe its offered QoS inside even a short remaining window.
                next_qos = elapsed
            if elapsed >= next_qos:
                for key, topic, is_tf in topic_bindings:
                    endpoints = node.get_publishers_info_by_topic(topic)
                    if is_tf:
                        # Multiple cameras legitimately publish /tf_static;
                        # every discovered offer must satisfy late-join QoS.
                        ready = bool(endpoints) and all(e.qos_profile.reliability == ReliabilityPolicy.RELIABLE
                                and e.qos_profile.durability == DurabilityPolicy.TRANSIENT_LOCAL for e in endpoints)
                    else:
                        ready = validate_offered_qos([e.qos_profile for e in endpoints])['ready']
                    emit('qos', key, {'ready': ready})
                next_qos = elapsed + 5.
            rclpy.spin_once(node, timeout_sec=.02)
        cutoff = time.time_ns()
        collecting_images[0] = False
        # Drain only existing joins, bounded well below the freshness budget.
        drain_seconds = min(.2, min(s['freshness_seconds'] for s in spec['streams'].values()) / 2)
        deadline = time.monotonic() + drain_seconds
        while time.monotonic() < deadline:
            rclpy.spin_once(node, timeout_sec=min(.01, drain_seconds))
        emit('end', 'observer', {}, cutoff)
        result = observer.summary()
        result['measurement_window_seconds'] = seconds
        result['association_tail_drain_seconds'] = drain_seconds
        return result
    finally:
        node.destroy_node()
        rclpy.shutdown()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--spec', type=Path, required=True)
    parser.add_argument('--seconds', type=float, default=1800)
    parser.add_argument('--discovery-seconds', type=float, default=8)
    parser.add_argument('--tf-delay-seconds', type=float, default=2)
    parser.add_argument('--wire-only', action='store_true')
    parser.add_argument('--record', type=Path)
    parser.add_argument('--max-record-mib', type=int, default=256)
    parser.add_argument('--replay', type=Path)
    parser.add_argument('--summary', type=Path)
    parser.add_argument('--validate-only', action='store_true')
    args = parser.parse_args(argv)
    trace = None
    try:
        require(positive(args.seconds) and args.seconds <= 86400 and positive(args.discovery_seconds)
                and positive(args.tf_delay_seconds) and args.tf_delay_seconds < args.seconds, 'WINDOW_INVALID')
        require(1 <= args.max_record_mib <= 4096 and not (args.record and args.replay), 'RECORD_ARGUMENT')
        require(args.spec.stat().st_size <= 65536, 'SPEC_SIZE')
        spec = validate_spec(json.loads(args.spec.read_text()))
        if args.validate_only:
            result = {'version': 1, 'ok': True, 'spec_valid': True, 'hardware_verified': False}
        elif args.replay:
            result = replay_trace(args.replay, spec, args.wire_only)
        else:
            if args.record:
                trace = TraceWriter(args.record, spec, args.wire_only, args.max_record_mib * 1024**2)
            result = run_live(spec, args.seconds, args.discovery_seconds, args.tf_delay_seconds, args.wire_only, trace)
    except Exception as exc:
        result = {'version': 1, 'kind': 'rgbd-consumer-observer', 'ok': False,
                  'errors': [str(exc) if isinstance(exc, ContractError) else 'OBSERVER_UNAVAILABLE_OR_INVALID_INPUT'],
                  'error_class': type(exc).__name__}
    finally:
        if trace: trace.close()
    if args.summary:
        descriptor = os.open(args.summary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'w') as handle:
            json.dump(result, handle, sort_keys=True, allow_nan=False)
            handle.write('\n')
    print(json.dumps(result, sort_keys=True, allow_nan=False))
    return 0 if result.get('ok') else 1


if __name__ == '__main__':
    raise SystemExit(main())
