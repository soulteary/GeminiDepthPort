#!/usr/bin/env python3
"""Synthetic ROS wire test. No SDK, USB, device identifiers or real calibration.

Publishes declared synthetic 2x2 YUYV and normalized millimetre depth fixtures in an explicit test namespace.
Checks real Image/CameraInfo/String delivery, matching stamps, byte ownership and
publisher QoS. This is not camera or physical-calibration acceptance.
"""
import argparse
import ctypes as c
import json
import struct
import time

from rclpy.context import Context
from rclpy.node import Node
from rclpy.executors import SingleThreadedExecutor
import rclpy
from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy, HistoryPolicy
from sensor_msgs.msg import Image, CameraInfo
from std_msgs.msg import String


class Config(c.Structure):
    _fields_ = [('abi_version', c.c_uint32), ('struct_size', c.c_uint32),
                ('node_name', c.c_char_p), ('namespace_name', c.c_char_p),
                ('image_topic', c.c_char_p), ('camera_info_topic', c.c_char_p),
                ('metadata_topic', c.c_char_p), ('frame_id', c.c_char_p),
                ('width', c.c_uint32), ('height', c.c_uint32), ('distortion_model', c.c_char_p),
                ('k', c.c_double * 9), ('r', c.c_double * 9), ('p', c.c_double * 12),
                ('d', c.POINTER(c.c_double)), ('d_len', c.c_uint32)]


class Frame(c.Structure):
    _fields_ = [('abi_version', c.c_uint32), ('struct_size', c.c_uint32),
                ('encoding', c.c_uint32), ('width', c.c_uint32), ('height', c.c_uint32),
                ('step', c.c_uint32), ('is_bigendian', c.c_uint32),
                ('stamp_sec', c.c_int32), ('stamp_nanosec', c.c_uint32), ('sequence', c.c_uint64),
                ('has_device_timestamp', c.c_uint32), ('has_system_timestamp', c.c_uint32),
                ('device_timestamp_us', c.c_uint64), ('system_timestamp_us', c.c_uint64),
                ('has_depth_scale', c.c_uint32), ('normalized', c.c_uint32),
                ('source_depth_scale_mm', c.c_float), ('output_depth_scale_mm', c.c_float),
                ('data', c.POINTER(c.c_uint8)), ('data_len', c.c_uint64)]


def require(ok, message):
    if not ok:
        raise AssertionError(message)


def stamp_key(stamp):
    return stamp.sec, stamp.nanosec


def run_case(library, namespace, stream, stamp_source="system", observed_timing=False):
    image_topic = namespace + '/' + stream + '/image_raw'
    info_topic = namespace + '/' + stream + '/camera_info'
    metadata_topic = namespace + '/' + stream + '/frame_metadata'
    frame_id = 'synthetic_test_' + stream + '_optical'
    encoding = 'yuyv' if stream == 'color' else '16UC1'
    depth = stream == 'depth'
    k = (1., 0., 1., 0., 1., 1., 0., 0., 1.)
    r = (1., 0., 0., 0., 1., 0., 0., 0., 1.)
    p = (1., 0., 1., 0., 0., 1., 1., 0., 0., 0., 1., 0.)
    d = (c.c_double * 5)(0., 0., 0., 0., 0.)
    config = Config(1, c.sizeof(Config), ('synthetic_bridge_' + stream + '_source').encode(), namespace.encode(),
                    image_topic.encode(), info_topic.encode(), metadata_topic.encode(), frame_id.encode(),
                    2, 2, b'plumb_bob', (c.c_double * 9)(*k), (c.c_double * 9)(*r),
                    (c.c_double * 12)(*p), d, 5)
    handle = c.c_void_p()
    # Bypass Rust to verify C ABI independently rejects malformed inputs.
    config.abi_version = 999
    require(library.gdp_ros_create(c.byref(config), c.byref(handle)) == 1 and not handle.value, 'C ABI accepted version mismatch')
    config.abi_version = 1
    config.k[0] = float('nan')
    require(library.gdp_ros_create(c.byref(config), c.byref(handle)) == 1 and not handle.value, 'C ABI accepted invalid calibration')
    config.k[0] = 1.
    require(library.gdp_ros_create(c.byref(config), c.byref(handle)) == 0 and handle.value, 'bridge creation failed')
    if observed_timing:
        require(library.gdp_ros_set_session_id(handle, b'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa') == 0, 'session binding failed')
    # Mutate caller calibration after create: bridge must have copied it.
    config.k[0] = 999.
    d[0] = 999.
    context = Context()
    rclpy.init(context=context)
    node = Node('synthetic_bridge_' + stream + '_sink', namespace=namespace, context=context, use_global_arguments=False)
    executor = SingleThreadedExecutor(context=context)
    executor.add_node(node)
    qos = QoSProfile(history=HistoryPolicy.KEEP_LAST, depth=10,
                     reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    images, infos, metadata, expected = {}, {}, {}, {}
    subscriptions = []
    try:
        subscriptions.append(node.create_subscription(Image, image_topic, lambda m: images.__setitem__(stamp_key(m.header.stamp), m), qos))
        subscriptions.append(node.create_subscription(CameraInfo, info_topic, lambda m: infos.__setitem__(stamp_key(m.header.stamp), m), qos))

        def receive_metadata(message):
            require(len(message.data) <= 4096, 'metadata exceeds bound')
            item = json.loads(message.data)
            metadata[(item['stamp_sec'], item['stamp_nanosec'])] = item

        subscriptions.append(node.create_subscription(String, metadata_topic, receive_metadata, qos))
        deadline = time.monotonic() + 8.
        discovered = False
        while time.monotonic() < deadline:
            executor.spin_once(timeout_sec=.05)
            if all(len(node.get_publishers_info_by_topic(t)) == 1 for t in (image_topic, info_topic, metadata_topic)):
                discovered = True
                break
        require(discovered, 'publisher discovery timeout')
        for topic in (image_topic, info_topic, metadata_topic):
            profile = node.get_publishers_info_by_topic(topic)[0].qos_profile
            observed = {'reliability': int(profile.reliability), 'durability': int(profile.durability),
                        'history': int(profile.history), 'depth': profile.depth}
            # Humble Fast DDS graph reports UNKNOWN history/depth 0 even when
            # the publisher's local actual QoS has KEEP_LAST depth 10. Keep the
            # graph checks on observable policies; bridge create independently
            # verifies all four via get_actual_qos before it can publish.
            require(profile.reliability == ReliabilityPolicy.RELIABLE and profile.durability == DurabilityPolicy.VOLATILE,
                    'graph reliability/durability differs: ' + json.dumps(observed))
        for sequence in range(1, 31):
            micros = time.time_ns() // 1000
            global_us = micros - 5000
            selected_us = global_us if stamp_source == 'global' else micros
            sec, nanos = selected_us // 1_000_000, (selected_us % 1_000_000) * 1000
            original = struct.pack('<4H', 0, 1, 10, 6554) if depth else bytes((16 + sequence, 128, 32, 128, 64, 90, 80, 240))
            payload = (c.c_uint8 * 8).from_buffer_copy(original)
            frame = Frame(1, c.sizeof(Frame), 2 if depth else 1, 2, 2, 4, 0, sec, nanos, sequence,
                          1, 1, 0 if sequence == 1 else sequence * 1000, micros, int(depth), int(depth), .1 if depth else 0., 1. if depth else 0., payload, 8)
            if sequence == 1:
                frame.data_len = 7
                require(library.gdp_ros_publish(handle, c.byref(frame)) == 1, 'C ABI accepted malformed image span')
                frame.data_len = 8
                if depth:
                    frame.normalized = 0
                    frame.output_depth_scale_mm = .1
                    require(library.gdp_ros_publish(handle, c.byref(frame)) == 1, 'C ABI accepted non-millimetre 16UC1')
                    frame.normalized = 1
                    frame.output_depth_scale_mm = 1.
            sdk_return = time.time_ns() if observed_timing else None
            if observed_timing:
                require(library.gdp_ros_publish_observed(handle, c.byref(frame), global_us if stamp_source == 'global' else 0,
                        2 if stamp_source == 'global' else 1, sdk_return, 1) == 0, 'observed bridge publication failed')
            elif stamp_source == 'global':
                require(library.gdp_ros_publish(handle, c.byref(frame)) == 1, 'legacy ABI accepted a global stamp as system')
                require(library.gdp_ros_publish_timestamped(handle, c.byref(frame), 0, 2) == 1, 'global zero accepted')
                require(library.gdp_ros_publish_timestamped(handle, c.byref(frame), global_us, 2) == 0, 'global bridge publication failed')
            else:
                require(library.gdp_ros_publish(handle, c.byref(frame)) == 0, 'bridge publication failed')
            expected[(sec, nanos)] = (sequence, original, micros, sdk_return)
            # Make a retained caller pointer observable as corrupted delivery.
            for i in range(8):
                payload[i] ^= 255
            end = time.monotonic() + .05
            while time.monotonic() < end:
                executor.spin_once(timeout_sec=.01)
        deadline = time.monotonic() + 3.
        while time.monotonic() < deadline:
            executor.spin_once(timeout_sec=.05)
        matched = sorted(set(expected) & set(images) & set(infos) & set(metadata))
        require(len(matched) >= 3, 'insufficient matched Image/CameraInfo/metadata messages')
        previous_sequence = 0
        for key in matched:
            sequence, original, micros, sdk_return = expected[key]
            image, info, item = images[key], infos[key], metadata[key]
            require(image.encoding == encoding and image.width == 2 and image.height == 2 and image.step == 4
                    and image.is_bigendian == 0 and bytes(image.data) == original, 'Image layout or owned bytes differ')
            require(image.header.frame_id == info.header.frame_id == frame_id, 'optical frame differs')
            require(info.width == 2 and info.height == 2 and tuple(info.k) == k and tuple(info.r) == r and tuple(info.p) == p
                    and tuple(info.d) == (0.,) * 5 and info.distortion_model == 'plumb_bob', 'copied CameraInfo differs')
            require(item['version'] == 1 and item['sequence'] == sequence > previous_sequence
                    and item['frame_id'] == frame_id and item['encoding'] == encoding
                    and item['width'] == 2 and item['height'] == 2 and item['step'] == 4
                    and item['system_timestamp_us'] == micros and item['device_timestamp_us'] == (0 if sequence == 1 else sequence * 1000)
                    and item['stamp_mapping'] == 'sdk-' + stamp_source + '-unix' and item['normalized'] is depth
                    and item.get('global_timestamp_us') == (micros - 5000 if stamp_source == 'global' else None)
                    and item['publisher_qos_verified'] is True, 'per-frame metadata differs')
            require(item.get('sdk_return_unix_ns') == sdk_return, 'SDK return observation changed')
            require(item['sdk_system_clock_verified'] is observed_timing, 'SDK clock verification flag changed')
            require(item['timing_clock'] == 'host-realtime-unix-ns'
                    and item['metadata_available_after_image_publish'] is True
                    and item['publication_start_unix_ns'] > 0
                    and item['image_publish_finished_unix_ns'] >= item['publication_start_unix_ns']
                    and type(item['image_publish_wall_ns']) is int and item['image_publish_wall_ns'] >= 0,
                    'publication timing metadata differs')
            if sdk_return is not None:
                require(item['publication_start_unix_ns'] >= sdk_return, 'SDK return observation came after publish')
            if depth:
                require(abs(item['source_depth_scale_mm'] - .1) < 1e-7 and item['output_depth_scale_mm'] == 1., 'normalized depth scales differ')
                require(struct.unpack('<4H', bytes(image.data)) == (0, 1, 10, 6554), 'millimetre depth samples differ')
            else:
                require(item['source_depth_scale_mm'] is None and item['output_depth_scale_mm'] is None, 'color depth-scale metadata differs')
            previous_sequence = sequence
        return {'ok': True, 'stream': stream, 'matched_frames': len(matched),
                          'byte_exact': True, 'camera_info_matches': True, 'metadata_matches': True,
                          'graph_reliability_durability_match': True, 'local_actual_qos_verified': True,
                          'graph_history_depth_used_as_evidence': False,
                          'hardware_verified': False, 'physical_calibration_verified': False}
    finally:
        executor.shutdown()
        node.destroy_node()
        context.shutdown()
        library.gdp_ros_destroy(handle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--library', required=True)
    parser.add_argument('--namespace', required=True, help='Dedicated absolute synthetic-test namespace')
    parser.add_argument('--stamp-source', choices=('system', 'global'), default='system')
    parser.add_argument('--observed-timing', action='store_true')
    args = parser.parse_args()
    require(args.library.startswith('/'), 'absolute bridge library required')
    require(args.namespace.startswith('/') and not args.namespace.endswith('/'), 'explicit non-root test namespace required')
    library = c.CDLL(args.library)
    library.gdp_ros_abi_version.restype = c.c_uint32
    library.gdp_ros_create.argtypes = [c.POINTER(Config), c.POINTER(c.c_void_p)]
    library.gdp_ros_create.restype = c.c_int32
    library.gdp_ros_publish.argtypes = [c.c_void_p, c.POINTER(Frame)]
    library.gdp_ros_publish.restype = c.c_int32
    library.gdp_ros_publish_timestamped.argtypes = [c.c_void_p, c.POINTER(Frame), c.c_uint64, c.c_uint32]
    library.gdp_ros_publish_timestamped.restype = c.c_int32
    library.gdp_ros_publish_observed.argtypes = [c.c_void_p, c.POINTER(Frame), c.c_uint64, c.c_uint32, c.c_uint64, c.c_uint32]
    library.gdp_ros_publish_observed.restype = c.c_int32
    library.gdp_ros_set_session_id.argtypes = [c.c_void_p, c.c_char_p]
    library.gdp_ros_set_session_id.restype = c.c_int32
    library.gdp_ros_destroy.argtypes = [c.c_void_p]
    library.gdp_ros_destroy.restype = None
    require(library.gdp_ros_abi_version() == 1, 'ABI version differs')
    results = [run_case(library, args.namespace, stream, args.stamp_source, args.observed_timing) for stream in ('color', 'depth')]
    print(json.dumps({'ok': True, 'kind': 'synthetic-ros-wire-smoke', 'streams': results, 'hardware_verified': False}))


if __name__ == '__main__':
    main()
