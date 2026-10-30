import importlib.util
from pathlib import Path
from types import SimpleNamespace as NS
import unittest

spec = importlib.util.spec_from_file_location('imu_recording', Path(__file__).resolve().parents[1] / 'ros/imu_recording.py')
imu = importlib.util.module_from_spec(spec)
spec.loader.exec_module(imu)


def message(kind='accel', stamp=1):
    return NS(header=NS(frame_id=kind + '_frame', stamp=NS(sec=1700000000, nanosec=stamp)),
              linear_acceleration=NS(x=1. if kind == 'accel' else 0., y=0., z=0.),
              angular_velocity=NS(x=1. if kind == 'gyro' else 0., y=0., z=0.),
              orientation_covariance=[-1.] + [0.] * 8,
              linear_acceleration_covariance=[0.] * 9 if kind == 'accel' else [-1.] + [0.] * 8,
              angular_velocity_covariance=[0.] * 9 if kind == 'gyro' else [-1.] + [0.] * 8)


class ImuRecordingTests(unittest.TestCase):
    def test_raw_independent_streams_and_progress(self):
        progress = {}
        for kind in ('accel', 'gyro'):
            msg = message(kind)
            self.assertEqual(imu.validate_message('/camera/' + kind + '/sample', msg, progress, '/camera')[0], kind)
            with self.assertRaises(ValueError):
                imu.validate_message('/camera/' + kind + '/sample', msg, progress, '/camera')
            msg.header.stamp.nanosec += 1
            msg.header.frame_id = 'changed'
            with self.assertRaises(ValueError):
                imu.validate_message('/camera/' + kind + '/sample', msg, progress, '/camera')

    def test_invalid_headers_values_and_covariances_fail(self):
        for case in range(8):
            msg = message()
            if case == 0:
                msg.header.stamp.sec = -1
            elif case == 1:
                msg.header.stamp.nanosec = 1_000_000_000
            elif case == 2:
                msg.header.frame_id = ''
            elif case == 3:
                msg.linear_acceleration.x = float('nan')
            elif case == 4:
                msg.angular_velocity.x = 1.
            elif case == 5:
                msg.orientation_covariance[0] = 0.
            elif case == 6:
                msg.angular_velocity_covariance[0] = 0.
            else:
                msg.linear_acceleration_covariance[1] = 1.
            with self.subTest(case=case), self.assertRaises(ValueError):
                imu.validate_message('/camera/accel/sample', msg, {}, '/camera')
        with self.assertRaises(ValueError):
            imu.validate_message('/other/sample', message(), {}, '/camera')
