#!/usr/bin/env python3
"""Build a private observer spec from actual calibration and depth-to-color TF.

Does not access devices, invent calibration, or publish the resulting bindings.
"""
import argparse
import json
import math
import os
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from runtime.rgbd_acceptance import validate_spec


def rotation_quaternion(rotation):
    """Convert an observed proper row-major rotation without changing direction."""
    if not isinstance(rotation, list) or len(rotation) != 9 or not all(type(v) in (int, float) and math.isfinite(v) for v in rotation):
        raise ValueError('ROTATION_INVALID')
    r = rotation
    for row in range(3):
        for col in range(3):
            if abs(sum(r[row*3+j] * r[col*3+j] for j in range(3)) - int(row == col)) > 1e-5:
                raise ValueError('ROTATION_INVALID')
    det = r[0]*(r[4]*r[8]-r[5]*r[7])-r[1]*(r[3]*r[8]-r[5]*r[6])+r[2]*(r[3]*r[7]-r[4]*r[6])
    if abs(det - 1) > 1e-5: raise ValueError('ROTATION_INVALID')
    if r[0] + r[4] + r[8] > 0:
        s = math.sqrt(r[0] + r[4] + r[8] + 1) * 2
        q = [(r[7]-r[5])/s, (r[2]-r[6])/s, (r[3]-r[1])/s, s/4]
    elif r[0] > r[4] and r[0] > r[8]:
        s = math.sqrt(1+r[0]-r[4]-r[8])*2
        q = [s/4, (r[1]+r[3])/s, (r[2]+r[6])/s, (r[7]-r[5])/s]
    elif r[4] > r[8]:
        s = math.sqrt(1+r[4]-r[0]-r[8])*2
        q = [(r[1]+r[3])/s, s/4, (r[5]+r[7])/s, (r[2]-r[6])/s]
    else:
        s = math.sqrt(1+r[8]-r[0]-r[4])*2
        q = [(r[2]+r[6])/s, (r[5]+r[7])/s, s/4, (r[3]-r[1])/s]
    norm = math.sqrt(sum(v*v for v in q))
    return [v/norm for v in q]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--color-calibration', type=Path, required=True)
    parser.add_argument('--depth-calibration', type=Path, required=True)
    parser.add_argument('--transform', type=Path, required=True, help='Observed JSON: translation_m[3] plus rotation_xyzw[4] or rotation_row_major[9]')
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--min-fps', type=float, required=True)
    parser.add_argument('--min-frames', type=int, required=True)
    parser.add_argument('--max-gap-seconds', type=float, required=True)
    parser.add_argument('--freshness-seconds', type=float, required=True)
    parser.add_argument('--max-future-seconds', type=float, default=.1)
    parser.add_argument('--max-pair-delta-us', type=int, required=True)
    parser.add_argument('--timestamp-source', choices=('host', 'global'), default='host')
    parser.add_argument('--pair-clock', choices=('device', 'system'), required=True)
    parser.add_argument('--allow-session-restart', action='store_true')
    parser.add_argument('--legacy-no-session', action='store_true')
    parser.add_argument('--payload-digest', action='store_true')
    args = parser.parse_args()
    streams = {}
    for role, path in (('color', args.color_calibration), ('depth', args.depth_calibration)):
        observed = json.loads(path.read_text())
        base = args.namespace.rstrip('/') + '/' + role
        streams[role] = {'image_topic': base + '/image_raw', 'camera_info_topic': base + '/camera_info',
                         'metadata_topic': base + '/frame_metadata', 'frame_id': observed['frame_id'],
                         'width': observed['width'], 'height': observed['height'],
                         'encoding': 'yuyv' if role == 'color' else '16UC1',
                         'timestamp_source': args.timestamp_source,
                         'min_fps': args.min_fps, 'min_frames': args.min_frames,
                         'max_gap_seconds': args.max_gap_seconds, 'freshness_seconds': args.freshness_seconds,
                         'max_future_seconds': args.max_future_seconds,
                         'calibration': {key: observed[key] for key in ('distortion_model', 'k', 'd', 'r', 'p')}}
    transform = json.loads(args.transform.read_text())
    if ('rotation_xyzw' in transform) == ('rotation_row_major' in transform):
        raise ValueError('EXPLICIT_ROTATION_REQUIRED')
    quaternion = transform.get('rotation_xyzw') if 'rotation_xyzw' in transform else rotation_quaternion(transform['rotation_row_major'])
    spec = {'version': 1, 'session_policy': 'explicit-restart' if args.allow_session_restart else 'single',
            'require_session_id': not args.legacy_no_session, 'payload_digest': args.payload_digest,
            'max_pending': 128, 'max_unmatched': 30, 'streams': streams,
            'pair': {'topic': args.namespace.rstrip('/') + '/rgbd/pair_metadata',
                     'clock': args.pair_clock, 'max_delta_us': args.max_pair_delta_us, 'min_pairs': args.min_frames},
            'tf': {'parent_frame': streams['color']['frame_id'], 'child_frame': streams['depth']['frame_id'],
                   'translation_m': transform['translation_m'], 'rotation_xyzw': quaternion, 'tolerance': 1e-6}}
    validate_spec(spec)
    descriptor = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as handle:
        json.dump(spec, handle, sort_keys=True, allow_nan=False)
        handle.write('\n')
    print(json.dumps({'ok': True, 'private_spec_written': True}))


if __name__ == '__main__':
    main()
