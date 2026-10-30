#!/usr/bin/env python3
"""Read-only validation of GDP raw IMU topics in a ROSbag2 recording.

SHA256 comparison requires an independent --expected acquisition manifest;
without it this validates message semantics and reports a digest, not authenticity.
"""
import argparse
import hashlib
import json
import math


def validate_message(topic, message, progress, namespace):
    prefix = namespace.rstrip('/')
    if topic == prefix + '/accel/sample':
        kind, present, absent = 'accel', message.linear_acceleration, message.angular_velocity
        measured_cov, absent_cov = message.linear_acceleration_covariance, message.angular_velocity_covariance
    elif topic == prefix + '/gyro/sample':
        kind, present, absent = 'gyro', message.angular_velocity, message.linear_acceleration
        measured_cov, absent_cov = message.angular_velocity_covariance, message.linear_acceleration_covariance
    else:
        raise ValueError('unexpected IMU topic')
    stamp = message.header.stamp.sec * 1_000_000_000 + message.header.stamp.nanosec
    frame = message.header.frame_id
    if not frame or len(frame)>256 or message.header.stamp.sec < 0 or not 0 <= message.header.stamp.nanosec < 1_000_000_000 or stamp <= 0:
        raise ValueError('invalid IMU header')
    if not all(math.isfinite(v) for v in (present.x, present.y, present.z)):
        raise ValueError('nonfinite IMU measurement')
    if (absent.x, absent.y, absent.z) != (0., 0., 0.):
        raise ValueError('unexpected fused measurement')
    if list(message.orientation_covariance) != [-1.] + [0.] * 8 or list(absent_cov) != [-1.] + [0.] * 8 or list(measured_cov) != [0.] * 9:
        raise ValueError('invalid unavailable/unknown covariance flags')
    previous = progress.get(kind)
    if previous and (stamp <= previous[0] or frame != previous[1]):
        raise ValueError('IMU timestamp regression or frame changed')
    if any(k != kind and value[1] == frame for k, value in progress.items()):
        raise ValueError('IMU sensor frames must differ')
    progress[kind] = (stamp, frame)
    return kind, stamp


def validate_bag(path, namespace, expected=None):
    import rosbag2_py
    from rclpy.serialization import deserialize_message
    from sensor_msgs.msg import Imu
    reader = rosbag2_py.SequentialReader()
    reader.open(rosbag2_py.StorageOptions(uri=str(path), storage_id='sqlite3'), rosbag2_py.ConverterOptions('', ''))
    topics = {x.name: x.type for x in reader.get_all_topics_and_types()}
    wanted = [namespace.rstrip('/') + '/' + kind + '/sample' for kind in ('accel', 'gyro')]
    if any(topics.get(topic) != 'sensor_msgs/msg/Imu' for topic in wanted):
        raise ValueError('both raw IMU topics required')
    reference = None
    if expected is not None:
        reference = {(r['topic'], r['stamp_ns']): r['sha256'] for r in expected}
        if len(reference) != len(expected):
            raise ValueError('duplicate expected IMU timestamp')
    progress, counts, digest = {}, {'accel': 0, 'gyro': 0}, hashlib.sha256()
    while reader.has_next():
        topic, raw, _record_timestamp = reader.read_next()
        if topic not in wanted:
            continue
        if len(raw)>4096:
            raise ValueError('IMU CDR size limit')
        msg = deserialize_message(raw, Imu)
        kind, stamp = validate_message(topic, msg, progress, namespace)
        key = (topic, stamp)
        sha = hashlib.sha256(raw).hexdigest()
        if reference is not None and reference.pop(key, None) != sha:
            raise ValueError('IMU readback hash mismatch')
        counts[kind] += 1
        digest.update(topic.encode('utf-8') + b'\0' + stamp.to_bytes(8, 'little') + bytes.fromhex(sha))
    if not all(counts.values()) or (reference is not None and reference):
        raise ValueError('IMU samples missing')
    return {'valid': True, 'counts': counts, 'readback_hashes_matched': reference is not None,
            'stream_digest_sha256': digest.hexdigest(), 'orientation_available': False,
            'extrinsics_verified': False, 'exposure_sync_verified': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bag', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--expected', help='JSON list of acquisition topic/stamp_ns/sha256 records')
    args = parser.parse_args()
    try:
        expected = None
        if args.expected:
            with open(args.expected, encoding='utf-8') as f:
                expected = json.load(f)
        result = validate_bag(args.bag, args.namespace, expected)
    except Exception as error:
        print(json.dumps({'valid': False, 'error': type(error).__name__}))
        return 1
    print(json.dumps(result))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
