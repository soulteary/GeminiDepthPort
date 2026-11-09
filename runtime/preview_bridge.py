"""ROS image subscriber: atomic latest JPEGs and telemetry for a host-local web UI."""
import json
import os
import time
from pathlib import Path
import cv2
import numpy as np
import rclpy
from rclpy.qos import qos_profile_sensor_data
from sensor_msgs.msg import Image


def image_array(msg):
    # Respect ROS row stride and endianness; never assume packed rows.
    encoding = msg.encoding.lower()
    if encoding == 'yuyv':
        if (msg.width <= 0 or msg.width % 2 or msg.height <= 0 or msg.step != msg.width * 2
                or len(msg.data) != msg.height * msg.step):
            raise ValueError('Invalid native YUYV layout')
        return np.frombuffer(bytes(msg.data), dtype=np.uint8).reshape(msg.height, msg.width, 2).copy(), encoding
    depth = encoding in ('16uc1', 'mono16', '32fc1')
    if depth:
        code = 'f4' if encoding == '32fc1' else 'u2'
        dtype = np.dtype(('>' if msg.is_bigendian else '<') + code)
        return np.ndarray((msg.height, msg.width), dtype=dtype, buffer=bytes(msg.data),
                          strides=(msg.step, dtype.itemsize)).astype(np.float32), encoding
    if encoding not in ('rgb8', 'bgr8', 'rgba8', 'bgra8', 'mono8'):
        raise ValueError(f'Unsupported preview encoding: {msg.encoding}')
    channels = 1 if encoding == 'mono8' else 4 if 'a8' in encoding else 3
    shape = (msg.height, msg.width) if channels == 1 else (msg.height, msg.width, channels)
    strides = (msg.step, 1) if channels == 1 else (msg.step, channels, 1)
    return np.ndarray(shape, dtype=np.uint8, buffer=bytes(msg.data), strides=strides).copy(), encoding


def jpeg_image(msg, stream):
    array, encoding = image_array(msg)
    if stream == 'depth':
        if encoding not in ('16uc1', 'mono16', '32fc1'):
            raise ValueError(f'Unsupported depth encoding: {encoding}')
        mm = array * 1000 if encoding == '32fc1' else array
        valid = np.isfinite(mm) & (mm > 0)
        normalized = ((np.clip(np.nan_to_num(mm), 200, 3000) - 200) / 2800 * 255).astype(np.uint8)
        array = cv2.applyColorMap(255 - normalized, cv2.COLORMAP_TURBO)
        array[~valid] = 0
    elif encoding == 'yuyv': array = cv2.cvtColor(array, cv2.COLOR_YUV2BGR_YUY2)
    elif encoding == 'rgb8': array = cv2.cvtColor(array, cv2.COLOR_RGB2BGR)
    elif encoding == 'rgba8': array = cv2.cvtColor(array, cv2.COLOR_RGBA2BGR)
    elif encoding == 'bgra8': array = cv2.cvtColor(array, cv2.COLOR_BGRA2BGR)
    elif encoding not in ('bgr8', 'mono8'): raise ValueError('Unsupported color encoding')
    if array.shape[1] > 960:
        array = cv2.resize(array, (960, round(array.shape[0] * 960 / array.shape[1])))
    ok, encoded = cv2.imencode('.jpg', array, [cv2.IMWRITE_JPEG_QUALITY, 75])
    if not ok: raise ValueError('JPEG encoder failed')
    return encoded.tobytes()


def atomic(path, content):
    tmp = path.with_suffix('.tmp'); tmp.write_bytes(content); tmp.replace(path)


def main():
    from device_capabilities import startup_report
    role = os.environ['CAMERA_ROLE']; output = Path('/out')
    params = __import__('yaml').safe_load(Path('/config/camera.yaml').read_text())
    stats = {}; subscriptions = []; last_status = 0
    rclpy.init(); node = rclpy.create_node(role + '_web_preview')
    def callback_for(stream):
        count = [0]; last_render = [0.0]; window = [time.monotonic(), 0]
        def callback(msg):
            now = time.monotonic(); count[0] += 1; window[1] += 1
            item = stats.setdefault(stream, {'fps': 0})
            item.update(received_at=time.time(), width=msg.width, height=msg.height, encoding=msg.encoding,
                        frames=count[0], stamp_sec=msg.header.stamp.sec, stamp_nanosec=msg.header.stamp.nanosec)
            if now - window[0] >= 1:
                item['fps'] = round(window[1] / (now-window[0]), 1); window[:] = [now, 0]
            if now - last_render[0] < 0.2: return  # Preview at most 5 FPS; capture runs at requested FPS.
            last_render[0] = now
            try:
                atomic(output / (stream + '.jpg'), jpeg_image(msg, stream)); item.pop('error', None)
            except Exception as exc: item['error'] = str(exc)
        return callback
    try:
        for stream in ('color', 'depth'):
            if params['enable_'+stream]:
                subscriptions.append(node.create_subscription(Image, os.environ.get('CAMERA_NAMESPACE', '/' + role + '_camera') + '/' + stream + '/image_raw', callback_for(stream), qos_profile_sensor_data))
        while rclpy.ok():
            rclpy.spin_once(node, timeout_sec=0.1)
            if time.monotonic()-last_status > 0.5:
                report = startup_report()
                if report is not None: stats['device_capabilities'] = report
                atomic(output/'status.json', json.dumps(stats).encode()); last_status = time.monotonic()
    finally:
        node.destroy_node(); rclpy.shutdown()

if __name__ == '__main__': main()
