"""Artificial contract fixtures only: no device or business-consumer certification."""
import copy
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace as NS
import unittest

from runtime import rgbd_acceptance as a
from tools.validation import compare_observers
from tools.validation.make_observer_spec import rotation_quaternion

BASE = 1700000000000000000
SESSION = 'a' * 32


def spec():
    streams = {}
    for role in ('color', 'depth'):
        streams[role] = {'image_topic': '/synthetic/' + role + '/image_raw',
                         'camera_info_topic': '/synthetic/' + role + '/camera_info',
                         'metadata_topic': '/synthetic/' + role + '/frame_metadata',
                         'frame_id': 'synthetic_' + role + '_optical', 'width': 2, 'height': 2,
                         'encoding': 'yuyv' if role == 'color' else '16UC1',
                         'min_frames': 3, 'min_fps': 20, 'max_gap_seconds': .1,
                         'freshness_seconds': .1, 'max_future_seconds': .01,
                         'calibration': {'distortion_model': 'plumb_bob', 'k': [1,0,1,0,1,1,0,0,1],
                                         'd': [0]*5, 'r': [1,0,0,0,1,0,0,0,1],
                                         'p': [1,0,1,0,0,1,1,0,0,0,1,0]}}
    return {'version': 1, 'session_policy': 'single', 'require_session_id': True,
            'payload_digest': False, 'max_pending': 8, 'max_unmatched': 0, 'streams': streams,
            'pair': {'topic': '/synthetic/rgbd/pair_metadata', 'clock': 'device', 'max_delta_us': 2000, 'min_pairs': 3},
            'tf': {'parent_frame': streams['color']['frame_id'], 'child_frame': streams['depth']['frame_id'],
                   'translation_m': [.1, .2, .3], 'rotation_xyzw': [0, 0, 0, 1], 'tolerance': 1e-6}}


def frame_events(s, role, index, session=SESSION):
    stream = s['streams'][role]
    stamp = BASE + index * 33000000 + (1000000 if role == 'depth' else 0)
    sequence = index + (100 if role == 'depth' else 0)
    image = {'stamp_ns': stamp, 'frame_id': stream['frame_id'], 'width': 2, 'height': 2,
             'step': 4, 'encoding': stream['encoding'], 'is_bigendian': 0, 'payload_len': 8}
    info = dict(stream['calibration'], stamp_ns=stamp, frame_id=stream['frame_id'], width=2, height=2,
                binning_x=0, binning_y=0, roi=[0, 0, 0, 0, False])
    metadata = {'version': 1, 'session_id': session, 'publisher_qos_verified': True,
                'stamp_sec': stamp // 1000000000, 'stamp_nanosec': stamp % 1000000000,
                'frame_id': stream['frame_id'], 'width': 2, 'height': 2, 'step': 4,
                'encoding': stream['encoding'], 'sequence': sequence,
                'system_timestamp_us': stamp // 1000, 'device_timestamp_us': stamp // 1000 - BASE // 1000,
                'stamp_mapping': 'sdk-system-unix', 'normalized': role == 'depth',
                'source_depth_scale_mm': .1 if role == 'depth' else None,
                'output_depth_scale_mm': 1 if role == 'depth' else None}
    return [('image', role, image, stamp + 5000000), ('info', role, info, stamp + 5001000),
            ('metadata', role, metadata, stamp + 5002000)]


def events(s, wire_only=False):
    result = []
    qos = ['color:image', 'color:info', 'depth:image', 'depth:info', 'tf']
    if not wire_only: qos += ['color:metadata', 'depth:metadata', 'pair']
    result += [('qos', role, {'ready': True}, BASE) for role in qos]
    for index in range(1, 5):
        color, depth = frame_events(s, 'color', index), frame_events(s, 'depth', index)
        result += color[:2] if wire_only else color
        result += depth[:2] if wire_only else depth
        if not wire_only:
            c, d = color[2][2], depth[2][2]
            result.append(('pair', 'pair', {'version': 1, 'session_id': SESSION,
                'pair_sequence': index, 'color_sequence': c['sequence'], 'depth_sequence': d['sequence'],
                'color_stamp_ns': color[0][2]['stamp_ns'], 'depth_stamp_ns': depth[0][2]['stamp_ns'],
                'color_timestamp_us': c['device_timestamp_us'], 'depth_timestamp_us': d['device_timestamp_us'],
                'delta_us': 1000, 'clock': 'device', 'same_frameset': True, 'provenance': 'sdkframeset',
                'hardware_sync_verified': False, 'publisher_qos_verified': True}, depth[0][3]+3000))
        if index == 1:
            result.append(('tf_subscribe', 'tf', {'after_both_streams': True}, depth[0][3]+4000))
            result.append(('tf', 'tf', dict(s['tf']), depth[0][3]+5000))
    result.append(('end', 'observer', {}, BASE+140000000))
    return result


class RgbdConsumerTests(unittest.TestCase):
    def run_events(self, data, s=None, wire_only=False):
        observer = a.Observer(s or spec(), wire_only)
        for event in data: observer.observe(*event)
        return observer

    def test_strict_association_and_common_wire_workload_pass(self):
        s = spec()
        for wire_only in (False, True):
            report = self.run_events(events(s, wire_only), s, wire_only).summary()
            self.assertTrue(report['ok'], report)
            self.assertEqual(report['streams']['depth']['matched_frames'], 4)
            self.assertEqual(report['matched_pairs'], 0 if wire_only else 4)
            self.assertFalse(report['hardware_sync_verified'])
            self.assertFalse(report['physical_scale_verified'])
            self.assertNotIn('synthetic', json.dumps(report))

    def test_age_scope_follows_actual_global_or_system_mapping(self):
        s = spec()
        report = self.run_events(events(s), s).summary()
        self.assertIn('sdk-system-receive', report['streams']['color']['age_ms']['scope'])
        data = events(s)
        for kind, role, value, received in data:
            if kind == 'metadata':
                value['global_timestamp_us'] = value['system_timestamp_us']
                value['system_timestamp_us'] += 1000
                value['stamp_mapping'] = 'sdk-global-unix'
        report = self.run_events(data, s).summary()
        self.assertTrue(report['ok'], report)
        self.assertIn('sdk-global-capture', report['streams']['color']['age_ms']['scope'])
        report = self.run_events(events(s, True), s, True).summary()
        self.assertIn('unspecified-or-mixed', report['streams']['color']['age_ms']['scope'])

    def test_mismatch_and_unsupported_claims_fail(self):
        s = spec()
        for kind, field, value in [('pair', 'depth_sequence', 999), ('pair', 'hardware_sync_verified', True),
                                   ('metadata', 'output_depth_scale_mm', .1), ('info', 'k', [0]*9),
                                   ('image', 'frame_id', 'wrong'), ('pair', 'session_id', 'b'*32)]:
            data = copy.deepcopy(events(s))
            target = next(event for event in data if event[0] == kind and (kind == 'pair' or event[1] == 'depth'))
            target[2][field] = value
            self.assertFalse(self.run_events(data, s).summary()['ok'], (kind, field))

    def test_global_stamps_preserve_system_receipt_and_validate_selected_clock(self):
        s = spec()
        for stream in s['streams'].values():
            stream['timestamp_source'] = 'global'
        data = events(s)
        for kind, _, value, _ in data:
            if kind == 'metadata':
                value['global_timestamp_us'] = value['system_timestamp_us']
                value['system_timestamp_us'] += 5000
                value['stamp_mapping'] = 'sdk-global-unix'
        self.assertTrue(self.run_events(data, s).summary()['ok'])
        for field, invalid in [('global_timestamp_us', None), ('global_timestamp_us', 0),
                               ('global_timestamp_us', 17), ('system_timestamp_us', 0),
                               ('stamp_mapping', 'sdk-system-unix'), ('stamp_mapping', 'unknown')]:
            broken = copy.deepcopy(data)
            metadata = next(e[2] for e in broken if e[0] == 'metadata')
            metadata[field] = invalid
            self.assertFalse(self.run_events(broken, s).summary()['ok'], (field, invalid))
        # Explicit global metadata cannot satisfy a system-configured observer.
        s['streams']['color']['timestamp_source'] = 'host'
        self.assertFalse(self.run_events(data, s).summary()['ok'])

    def test_missing_session_and_sequence_regression_fail(self):
        s = spec()
        data = copy.deepcopy(events(s))
        next(event for event in data if event[0] == 'metadata')[2].pop('session_id')
        self.assertFalse(self.run_events(data, s).summary()['ok'])
        data = copy.deepcopy(events(s))
        metadata = [event[2] for event in data if event[0] == 'metadata' and event[1] == 'depth']
        metadata[1]['sequence'] = metadata[0]['sequence']
        report = self.run_events(data, s).summary()
        self.assertIn('SOURCE_SEQUENCE_NOT_ADVANCING', report['streams']['depth']['contract_errors'])

    def test_pair_selected_clock_rejects_zero_freeze_and_regression(self):
        s = spec()
        for raw, code in [(0, 'PAIR_FRAME_TIMESTAMP'), (33000, 'PAIR_TIMESTAMP_NOT_ADVANCING'),
                          (32000, 'PAIR_TIMESTAMP_NOT_ADVANCING')]:
            data = copy.deepcopy(events(s))
            for kind, role, value, _ in data:
                if kind == 'metadata' and value['sequence'] in (2, 102):
                    value['device_timestamp_us'] = raw + (1000 if role == 'depth' else 0)
                if kind == 'pair' and value['pair_sequence'] == 2:
                    value['color_timestamp_us'] = raw
                    value['depth_timestamp_us'] = raw + 1000
            report = self.run_events(data, s).summary()
            self.assertIn(code, report['streams']['depth']['contract_errors'] or report['contract_errors'])

    def test_metadata_invalid_stamp_fields_are_not_normalized_into_valid_time(self):
        s = spec()
        data = copy.deepcopy(events(s))
        metadata = next(event[2] for event in data if event[0] == 'metadata')
        metadata['stamp_sec'] -= 1
        metadata['stamp_nanosec'] += 1000000000
        report = self.run_events(data, s).summary()
        self.assertIn('METADATA_STAMP', report['streams']['color']['contract_errors'])

    def test_explicit_restart_cannot_mix_or_resurrect_sessions(self):
        s = spec(); s['session_policy'] = 'explicit-restart'
        data = events(s)
        # A new explicit epoch can reset sequence; timestamps remain real Unix.
        for kind, role, value, _ in data:
            if kind in ('metadata', 'pair'):
                index = value.get('pair_sequence', value.get('sequence', 0) - (100 if role == 'depth' else 0))
                if index >= 3:
                    value['session_id'] = 'b' * 32
                    if kind == 'metadata': value['sequence'] -= 2
                    else:
                        value['pair_sequence'] -= 2; value['color_sequence'] -= 2; value['depth_sequence'] -= 2
        observer = self.run_events(data, s)
        self.assertTrue(observer.summary()['ok'], observer.summary())
        old = frame_events(s, 'color', 5)
        for event in old: observer.observe(*event)
        self.assertIn('SESSION_RETIRED', observer.stats['color'].errors)

    def test_bounded_memory_and_missing_components_are_counted(self):
        s = spec(); s['max_pending'] = 2
        observer = a.Observer(s)
        for index in range(1, 30):
            observer.observe(*frame_events(s, 'color', index)[0])
        self.assertLessEqual(len(observer.pending['color']), 2)
        self.assertEqual(observer.evictions, 27)
        self.assertFalse(observer.summary()['ok'])

    def test_tail_join_lookup_only_recognizes_received_window_images(self):
        s = spec(); observer = a.Observer(s)
        color = frame_events(s, 'color', 1)
        stamp = color[0][2]['stamp_ns']
        observer.observe(*color[1])
        self.assertFalse(observer.contains_image('color', stamp))
        observer.observe(*color[0])
        self.assertTrue(observer.contains_image('color', stamp))
        self.assertFalse(observer.contains_image('depth', stamp))
        observer.observe(*color[2])
        self.assertTrue(observer.contains_image('color', stamp))

    def test_stamp_age_gap_qos_and_tf_failures_are_visible(self):
        s = spec()
        data = copy.deepcopy(events(s))
        image = next(event for event in data if event[0] == 'image')
        index = data.index(image)
        data[index] = (*image[:3], image[3]+1000000000)
        report = self.run_events(data, s).summary()
        self.assertIn('IMAGE_AGE', report['streams']['color']['contract_errors'])
        data = [event for event in events(s) if event[0] != 'tf']
        self.assertIn('TF_NOT_RECEIVED', self.run_events(data, s).summary()['contract_errors'])
        data = events(s); data[0][2]['ready'] = False
        self.assertIn('QOS_CONTRACT', self.run_events(data, s).summary()['contract_errors'])

    def test_trace_roundtrip_is_bounded_private_and_requires_completion(self):
        s = spec()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'trace.jsonl'
            writer = a.TraceWriter(path, s, False, 1024*1024)
            for kind, role, value, receive_ns in events(s):
                writer.write({'kind': kind, 'role': role, 'value': value, 'receive_ns': receive_ns})
            writer.close()
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            result = a.replay_trace(path, s, False)
            self.assertTrue(result['ok'], result)
            self.assertTrue(result['replay'])
            self.assertNotIn('payload', path.read_text().replace('payload_len', ''))
            lines = path.read_text().splitlines()
            path.write_text('\n'.join(lines[:-1])+'\n')
            with self.assertRaises(a.ContractError): a.replay_trace(path, s, False)

    def test_image_projection_checks_bytes_and_reference_without_retention(self):
        expected = spec()['streams']['depth']
        expected['depth_reference'] = {'x': 1, 'y': 1, 'distance_mm': 1000, 'tolerance_mm': 10}
        msg = NS(width=2, height=2, step=4, encoding='16UC1', is_bigendian=0,
                 data=bytearray(struct.pack('<4H', 0, 1, 2, 1000)),
                 header=NS(frame_id=expected['frame_id'], stamp=NS(sec=1700000000, nanosec=1)))
        projection = a.image_projection(msg, expected, True)
        self.assertEqual(projection['reference_depth_mm'], 1000)
        self.assertEqual(len(projection['sha256']), 64)
        self.assertNotIn('data', projection)
        msg.is_bigendian = 1
        with self.assertRaises(a.ContractError): a.image_projection(msg, expected)

    def test_ros_fixed_array_numeric_scalars_become_json_floats(self):
        class RosFloat(float): pass
        s = spec(); calibration = s['streams']['color']['calibration']
        msg = NS(header=NS(frame_id=s['streams']['color']['frame_id'], stamp=NS(sec=1700000000, nanosec=1)),
                 width=2, height=2, distortion_model='plumb_bob', binning_x=0, binning_y=0,
                 roi=NS(x_offset=0,y_offset=0,width=0,height=0,do_rectify=False),
                 **{key:[RosFloat(v) for v in calibration[key]] for key in ('k','d','r','p')})
        projection = a.info_projection(msg)
        self.assertTrue(all(type(v) is float for key in ('k','d','r','p') for v in projection[key]))
        self.assertEqual(json.loads(json.dumps(projection)), projection)

    def test_wire_static_tree_delegates_all_edges_and_exact_frame_direction(self):
        expected = spec()['tf']
        resolved = NS(header=NS(frame_id=expected['parent_frame']), child_frame_id=expected['child_frame'],
                      transform=NS(translation=NS(x=.1,y=.2,z=.3),rotation=NS(x=0.,y=0.,z=0.,w=1.)))
        latest = object()
        class MockBuffer:
            def __init__(self): self.edges = []; self.lookups = []
            def set_transform_static(self, edge, authority): self.edges.append((edge, authority))
            def lookup_transform(self, target, source, at):
                self.lookups.append((target, source, at))
                if len(self.edges) < 2: raise LookupError('static path incomplete')
                return resolved
        buffer = MockBuffer()
        resolver = a.StaticTfResolver(buffer, expected, latest)
        first, second = object(), object()
        self.assertEqual(resolver.feed(NS(transforms=[first])), [])
        projection = resolver.feed(NS(transforms=[second]))
        self.assertEqual([edge for edge,_ in buffer.edges], [first,second])
        self.assertEqual(buffer.lookups, [(expected['parent_frame'],expected['child_frame'],latest)]*2)
        self.assertEqual(projection[0]['translation_m'], expected['translation_m'])
        observer = a.Observer(spec(), True)
        observer.observe('tf', 'tf', projection[0], BASE)
        self.assertTrue(observer.tf_received)

    def test_missing_wire_static_path_cannot_pass_tf_acceptance(self):
        class MissingBuffer:
            def set_transform_static(self, edge, authority): pass
            def lookup_transform(self, target, source, at): raise LookupError('missing')
        s = spec(); resolver = a.StaticTfResolver(MissingBuffer(),s['tf'],object())
        self.assertEqual(resolver.feed(NS(transforms=[object()])), [])
        data = [event for event in events(s, True) if event[0] != 'tf']
        report = self.run_events(data,s,True).summary()
        self.assertFalse(report['ok'])
        self.assertIn('TF_NOT_RECEIVED',report['contract_errors'])

    def test_comparison_rejects_capture_counts_different_sdk_and_mixed_modes(self):
        s = spec()
        report = self.run_events(events(s, True), s, True).summary()
        manifest = {key: 'a'*64 for key in ('sdk_sha256', 'profile_sha256', 'consumer_workload_sha256', 'platform_sha256')}
        manifest.update(trial_id='synthetic-run', implementation='synthetic-fixture')
        self.assertTrue(compare_observers.compare(report, report, manifest, manifest)['ok'])
        different = dict(manifest, sdk_sha256='b'*64)
        with self.assertRaises(ValueError): compare_observers.compare(report, report, manifest, different)
        with self.assertRaises(ValueError): compare_observers.compare({'captured': 100}, report, manifest, manifest)
        strict = self.run_events(events(s), s).summary()
        with self.assertRaises(ValueError): compare_observers.compare(strict, report, manifest, manifest)

    def test_observed_rotation_conversion_preserves_direction_and_rejects_reflection(self):
        q = rotation_quaternion([0, -1, 0, 1, 0, 0, 0, 0, 1])
        self.assertAlmostEqual(q[2], 2**-.5)
        self.assertAlmostEqual(q[3], 2**-.5)
        for matrix, index in [([1,0,0,0,-1,0,0,0,-1], 0), ([-1,0,0,0,1,0,0,0,-1], 1),
                               ([-1,0,0,0,-1,0,0,0,1], 2)]:
            self.assertAlmostEqual(abs(rotation_quaternion(matrix)[index]), 1)
        with self.assertRaises(ValueError): rotation_quaternion([-1,0,0,0,1,0,0,0,1])


class LiveSchedulingTests(unittest.TestCase):
    def test_short_window_observes_qos_when_delayed_tf_subscription_is_created(self):
        import sys
        from types import ModuleType
        from unittest.mock import patch
        s = spec()
        for stream in s['streams'].values(): stream['min_frames'] = 100
        s['pair']['min_pairs'] = 100
        clock = [0.0]; callbacks = {}; queries = []; tf_created = []
        class Policy:
            RELIABLE = 1; VOLATILE = 2; TRANSIENT_LOCAL = 1
        class Node:
            def create_subscription(self, typ, topic, callback, qos):
                callbacks[topic] = callback
                if topic == '/tf_static':
                    tf_created.append(clock[0])
                    # Model an actual late-join static sample, independent of
                    # the graph's offered-QoS observation.
                    callback([dict(s['tf'])])
                return callback
            def get_publishers_info_by_topic(self, topic):
                queries.append((topic, clock[0]))
                return [NS(qos_profile=NS(reliability=Policy.RELIABLE,
                    durability=Policy.TRANSIENT_LOCAL if topic == '/tf_static' else Policy.VOLATILE))]
            def destroy_node(self): pass
        index = [0]
        def spin(node, timeout_sec):
            clock[0] += .02; index[0] += 1
            projected = {}
            for role, stream in s['streams'].items():
                frames = frame_events(s, role, index[0])
                stamp = BASE + round(clock[0]*1e9) + (1000000 if role == 'depth' else 0)
                for kind, _, value, _ in frames:
                    if kind in ('image', 'info'): value['stamp_ns'] = stamp
                    else:
                        value.update(stamp_sec=stamp//1000000000, stamp_nanosec=stamp%1000000000,
                                     system_timestamp_us=stamp//1000,
                                     device_timestamp_us=(stamp-BASE)//1000)
                    topic = stream[{'image':'image_topic','info':'camera_info_topic','metadata':'metadata_topic'}[kind]]
                    callbacks[topic](NS(data=json.dumps(value)) if kind == 'metadata' else value)
                projected[role] = (stamp, frames[2][2])
            color, depth = projected['color'], projected['depth']
            callbacks[s['pair']['topic']](NS(data=json.dumps({
                'version':1, 'session_id':SESSION, 'pair_sequence':index[0],
                'color_sequence':color[1]['sequence'], 'depth_sequence':depth[1]['sequence'],
                'color_stamp_ns':color[0], 'depth_stamp_ns':depth[0],
                'color_timestamp_us':color[1]['device_timestamp_us'],
                'depth_timestamp_us':depth[1]['device_timestamp_us'],
                'delta_us':1000, 'clock':'device', 'same_frameset':True,
                'provenance':'sdkframeset', 'hardware_sync_verified':False,
                'publisher_qos_verified':True})))
        ros = ModuleType('rclpy'); ros.init = lambda: None; ros.shutdown = lambda: None
        ros.create_node = lambda name: Node(); ros.spin_once = spin
        qos = ModuleType('rclpy.qos'); qos.QoSProfile = lambda **kw: NS(**kw)
        qos.ReliabilityPolicy = Policy; qos.DurabilityPolicy = Policy
        modules = {'rclpy':ros, 'rclpy.qos':qos}
        for package, names in [('sensor_msgs',('Image','CameraInfo')),('std_msgs',('String',)),('tf2_msgs',('TFMessage',))]:
            modules[package] = ModuleType(package); messages = ModuleType(package+'.msg')
            for name in names: setattr(messages,name,type(name,(),{}))
            modules[package+'.msg'] = messages
        with patch.dict(sys.modules,modules), \
             patch.object(a.time,'monotonic',side_effect=lambda:clock[0]), \
             patch.object(a.time,'time_ns',side_effect=lambda:BASE+round(clock[0]*1e9)), \
             patch.object(a,'image_projection',side_effect=lambda msg,*args:msg), \
             patch.object(a,'info_projection',side_effect=lambda msg:msg), \
             patch.object(a,'tf_projection',side_effect=lambda msg,*args:msg):
            report = a.run_live(s, seconds=5, discovery_seconds=2, tf_delay_seconds=1)
        self.assertTrue(report['ok'],report)
        self.assertGreaterEqual(report['matched_pairs'],100)
        self.assertTrue(report['tf_received'])
        self.assertTrue(report['late_tf_subscription_requested'])
        self.assertIn('tf',report['qos_observed'])
        tf_queries = [at for topic, at in queries if topic == '/tf_static']
        self.assertEqual(len(tf_created),1)
        self.assertTrue(tf_queries)
        self.assertLessEqual(tf_queries[0]-tf_created[0],.02)
        self.assertLess(tf_queries[0],2+5)


if __name__ == '__main__': unittest.main()
