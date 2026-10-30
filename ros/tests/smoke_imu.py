#!/usr/bin/env python3
"""Actual DDS + ROSbag2 CDR readback test; synthetic IMU, no hardware."""
import ctypes as c
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import struct
import tempfile
import time
import rclpy
from rclpy.serialization import serialize_message
from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy
from sensor_msgs.msg import Imu
import rosbag2_py

spec = importlib.util.spec_from_file_location('imu_recording', Path(__file__).resolve().parents[1] / 'imu_recording.py')
validation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validation)
class Frame(c.Structure):
    _fields_ = [('abi_version', c.c_uint32), ('struct_size', c.c_uint32), ('kind', c.c_uint32), ('timestamp_source', c.c_uint32),
                ('stamp_sec', c.c_int32), ('stamp_nanosec', c.c_uint32), ('sequence', c.c_uint64),
                ('device_timestamp_us', c.c_uint64), ('system_timestamp_us', c.c_uint64), ('global_timestamp_us', c.c_uint64), ('xyz', c.c_double * 3)]


def main():
    lib = c.CDLL('/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so')
    lib.gdp_ros_imu_create.argtypes = [c.c_char_p] * 6 + [c.POINTER(c.c_void_p)]
    lib.gdp_ros_imu_publish.argtypes = [c.c_void_p, c.POINTER(Frame)]
    lib.gdp_ros_imu_destroy.argtypes = [c.c_void_p]
    ns, handle = '/gdp_imu_smoke', c.c_void_p()
    assert lib.gdp_ros_imu_create(b'gdp_imu_smoke', ns.encode(), (ns + '/accel/sample').encode(), (ns + '/gyro/sample').encode(), b'accel_frame', b'gyro_frame', c.byref(handle)) == 0
    rclpy.init()
    node = rclpy.create_node('gdp_imu_reader')
    received = []
    qos = QoSProfile(depth=100, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    subscriptions = [node.create_subscription(Imu, ns + '/' + kind + '/sample', lambda msg, kind=kind: received.append((ns + '/' + kind + '/sample', msg)), qos) for kind in ('accel', 'gyro')]
    sequence, deadline = 0, time.monotonic() + 12
    try:
        while len(received) < 10 and time.monotonic() < deadline:
            sequence += 1
            for kind in (1, 2):
                micros = 1_700_000_000_000_000 + sequence * 5000
                source = 2 if sequence % 2 else 1
                mapped = micros + 1 if source == 2 else micros
                system = micros if source == 1 else micros - 5000
                frame = Frame(1, c.sizeof(Frame), kind, source, mapped // 1_000_000, mapped % 1_000_000 * 1000, sequence, sequence * 5000, system, mapped if source == 2 else 0, (1., 2., 3.))
                if source == 2 and sequence > 1:
                    bad = Frame.from_buffer_copy(frame); bad.timestamp_source = 1
                    bad.stamp_sec = system // 1_000_000; bad.stamp_nanosec = system % 1_000_000 * 1000
                    assert lib.gdp_ros_imu_publish(handle, c.byref(bad)) == 1
                assert lib.gdp_ros_imu_publish(handle, c.byref(frame)) == 0
            rclpy.spin_once(node, timeout_sec=0.03)
        assert len(received) >= 10
        for kind in ('accel', 'gyro'):
            infos = node.get_publishers_info_by_topic(ns + '/' + kind + '/sample')
            assert infos and all(i.qos_profile.reliability == ReliabilityPolicy.RELIABLE and i.qos_profile.durability == DurabilityPolicy.VOLATILE for i in infos)
        assert lib.gdp_ros_imu_publish(handle, c.byref(frame)) == 1  # duplicate
        for field, value in [('kind', 3), ('struct_size', 0), ('timestamp_source', 0), ('stamp_nanosec', 1_000_000_000)]:
            bad = Frame.from_buffer_copy(frame)
            setattr(bad, field, value)
            assert lib.gdp_ros_imu_publish(handle, c.byref(bad)) == 1
        with tempfile.TemporaryDirectory(prefix='gdp-imu-bag-') as root:
            bag = Path(root) / 'capture'
            writer = rosbag2_py.SequentialWriter()
            writer.open(rosbag2_py.StorageOptions(uri=str(bag), storage_id='sqlite3'), rosbag2_py.ConverterOptions('', ''))
            for kind in ('accel', 'gyro'):
                writer.create_topic(rosbag2_py.TopicMetadata(name=ns + '/' + kind + '/sample', type='sensor_msgs/msg/Imu', serialization_format='cdr'))
            expected, progress = [], {}
            for topic, msg in received:
                kind, stamp = validation.validate_message(topic, msg, progress, ns)
                assert msg.header.frame_id == kind + '_frame'
                vector = msg.linear_acceleration if kind == 'accel' else msg.angular_velocity
                assert (vector.x, vector.y, vector.z) == (1., 2., 3.)
                raw = serialize_message(msg)
                writer.write(topic, raw, stamp)
                expected.append({'topic': topic, 'stamp_ns': stamp, 'sha256': hashlib.sha256(raw).hexdigest()})
            del writer
            result = validation.validate_bag(bag, ns, expected)
            assert result['readback_hashes_matched']
            database = next(bag.glob('*.db3'))
            with sqlite3.connect(database) as conn:
                row = conn.execute('select id,data from messages order by id limit 1').fetchone()
                corrupt = bytearray(row[1])
                offset = corrupt.index(struct.pack('<3d', 1., 2., 3.))
                corrupt[offset:offset + 8] = struct.pack('<d', 1.25)
                conn.execute('update messages set data=? where id=?', (bytes(corrupt), row[0]))
            try:
                validation.validate_bag(bag, ns, expected)
            except ValueError as error:
                assert str(error) == 'IMU readback hash mismatch'
            else:
                raise AssertionError('corrupt IMU recording accepted')
        print(json.dumps({'ok': True, 'dds_messages': len(received), 'rosbag2_readback': True, 'corruption_rejected': True, 'hardware_opened': False}))
    finally:
        lib.gdp_ros_imu_destroy(handle)
        for subscription in subscriptions:
            node.destroy_subscription(subscription)
        node.destroy_node(); rclpy.shutdown()


if __name__ == '__main__':
    main()
