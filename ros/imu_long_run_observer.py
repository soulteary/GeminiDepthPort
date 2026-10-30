#!/usr/bin/env python3
"""Bounded real-DDS evidence database. No camera/SDK/config mutation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import sqlite3
import time


def database(path, max_mib):
    fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(fd)
    db = sqlite3.connect(path)
    db.execute('PRAGMA journal_mode=DELETE')
    db.execute('PRAGMA synchronous=FULL')
    db.execute('PRAGMA max_page_count=' + str(max_mib * 256))
    db.execute('CREATE TABLE messages(ns TEXT,stream TEXT,stamp INTEGER,data TEXT,PRIMARY KEY(ns,stream,stamp))')
    db.execute('CREATE TABLE graph(ns TEXT,observed REAL,data TEXT)')
    db.execute('CREATE TABLE result(data TEXT)')
    db.commit()
    return db


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--namespaces', nargs=3, required=True)
    p.add_argument('--seconds', type=float, default=3900)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--max-mib', type=int, default=4096)
    a = p.parse_args()
    if not (0 < a.seconds <= 86400 and 16 <= a.max_mib <= 65536 and len(set(a.namespaces)) == 3
            and all(n.startswith('/') and not n.endswith('/') for n in a.namespaces)):
        p.error('three distinct absolute namespaces and bounded duration/size required')
    os.umask(0o077)
    import rclpy
    from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy
    from sensor_msgs.msg import Image, Imu
    from std_msgs.msg import String
    from rclpy.serialization import serialize_message
    from imu_recording import validate_message
    db = database(a.output, a.max_mib)
    rclpy.init()
    node = rclpy.create_node('gdp_45992a4_long_test')
    qos = QoSProfile(depth=100, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    image_qos = QoSProfile(depth=10, reliability=ReliabilityPolicy.RELIABLE, durability=DurabilityPolicy.VOLATILE)
    progress, last, counts, graphs, subscriptions = {}, {}, {}, {}, []
    error = None
    started, cpu0 = time.monotonic(), time.process_time()
    def insert(ns, stream, stamp, data):
        key = (ns, stream)
        if stamp <= last.get(key, 0):
            raise ValueError('DDS_TIMESTAMP_NOT_STRICT:' + ns + '/' + stream)
        db.execute('INSERT INTO messages VALUES(?,?,?,?)', (ns, stream, stamp, json.dumps(data, separators=(',', ':'))))
        last[key] = stamp
        counts[ns + '/' + stream] = counts.get(ns + '/' + stream, 0) + 1
    def image(ns, stream, msg):
        stamp = msg.header.stamp.sec * 10**9 + msg.header.stamp.nanosec
        insert(ns, stream, stamp, {'sha256': hashlib.sha256(bytes(msg.data)).hexdigest(),
               'frame_id': msg.header.frame_id, 'width': msg.width, 'height': msg.height,
               'step': msg.step, 'encoding': msg.encoding, 'is_bigendian': msg.is_bigendian})
    def imu(ns, stream, msg):
        _, stamp = validate_message(ns + '/' + stream + '/sample', msg, progress.setdefault(ns, {}), ns)
        xyz = msg.linear_acceleration if stream == 'accel' else msg.angular_velocity
        insert(ns, stream, stamp, {'frame_id': msg.header.frame_id, 'xyz': [xyz.x, xyz.y, xyz.z],
               'cdr_sha256': hashlib.sha256(serialize_message(msg)).hexdigest()})
    def metadata(ns, stream, msg):
        data = json.loads(msg.data)
        stamp = data['stamp_sec'] * 10**9 + data['stamp_nanosec']
        if data['stamp_mapping'] != 'sdk-global-unix' or not data['global_timestamp_us'] or stamp != data['global_timestamp_us'] * 1000:
            raise ValueError('IMAGE_GLOBAL_MAPPING')
        insert(ns, stream + '_metadata', stamp, data)
    try:
        for ns in a.namespaces:
            for stream in ('color', 'depth'):
                subscriptions.append(node.create_subscription(Image, ns + '/' + stream + '/image_raw', lambda m,n=ns,s=stream:image(n,s,m), image_qos))
                subscriptions.append(node.create_subscription(String, ns + '/' + stream + '/frame_metadata', lambda m,n=ns,s=stream:metadata(n,s,m), qos))
            for stream in ('accel', 'gyro'):
                subscriptions.append(node.create_subscription(Imu, ns + '/' + stream + '/sample', lambda m,n=ns,s=stream:imu(n,s,m), qos))
        Path(str(a.output) + '.ready').write_text('subscriptions-created; DDS matching still requires warmup\n')
        next_graph = started
        while time.monotonic() - started < a.seconds:
            rclpy.spin_once(node, timeout_sec=.05)
            if time.monotonic() >= next_graph:
                prefixes = []
                for ns in a.namespaces:
                    gids = []
                    for suffix in ('color/image_raw', 'depth/image_raw', 'color/camera_info', 'depth/camera_info', 'color/frame_metadata', 'depth/frame_metadata', 'accel/sample', 'gyro/sample'):
                        infos = node.get_publishers_info_by_topic(ns + '/' + suffix)
                        if not infos and time.monotonic() - started < 60:
                            break
                        if len(infos) != 1:
                            raise ValueError('EXACTLY_ONE_PUBLISHER_REQUIRED:' + ns + '/' + suffix)
                        info = infos[0]
                        if info.qos_profile.reliability != ReliabilityPolicy.RELIABLE or info.qos_profile.durability != DurabilityPolicy.VOLATILE:
                            raise ValueError('PUBLISHER_QOS')
                        gids.append(bytes(info.endpoint_gid)[:12].hex())
                    if len(gids) == 8:
                        if len(set(gids)) != 1 or gids[0] == '00' * 12:
                            raise ValueError('SHARED_CONTEXT_GID_MISMATCH')
                        if ns in graphs and graphs[ns] != gids[0]:
                            raise ValueError('PARTICIPANT_CHANGED')
                        graphs[ns] = gids[0]
                        prefixes.append(gids[0])
                        db.execute('INSERT INTO graph VALUES(?,?,?)', (ns, time.time(), json.dumps(gids)))
                if len(prefixes) != len(set(prefixes)):
                    raise ValueError('CAMERA_CONTEXTS_NOT_ISOLATED')
                db.commit()
                next_graph = time.monotonic() + (5 if len(graphs) == 3 else .5)
        if len(graphs) != 3 or any(not counts.get(ns + '/' + s) for ns in a.namespaces for s in ('color', 'depth', 'accel', 'gyro', 'color_metadata', 'depth_metadata')):
            raise ValueError('MISSING_STREAM_OR_CONTEXT_EVIDENCE')
    except (Exception, KeyboardInterrupt) as exc:
        error = type(exc).__name__ + ':' + str(exc)
    finally:
        result = {'ok': error is None, 'error': error, 'counts': counts, 'elapsed_seconds': time.monotonic()-started,
                  'observer_cpu_seconds': time.process_time()-cpu0, 'observer_peak_rss_kib': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                  'scope': 'Actual DDS observations; CDR digest is observer-derived, not independent acquisition authenticity'}
        db.rollback() if error else db.commit()
        try:
            db.execute('INSERT INTO result VALUES(?)', (json.dumps(result),)); db.commit()
        finally:
            db.close(); node.destroy_node(); rclpy.shutdown()
        Path(str(a.output) + '.summary.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0 if error is None else 1

if __name__ == '__main__':
    raise SystemExit(main())
