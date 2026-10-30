"""No-device capture-image contract and executable compatibility smoke test."""
import hashlib
import json
import os
import signal
import time
import ctypes
from pathlib import Path
import subprocess
import tempfile
from sdk_licenses import verify


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def check_recovery_parent_death(helper):
    # No USB: helper waits for serial input while its named parent exits.
    # Retain a writer in this process so EOF cannot explain the helper's exit.
    require(ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0, 'test subreaper setup failed')
    incoming, writer = os.pipe()
    notice, sender = os.pipe()
    parent = os.fork()
    if parent == 0:
        os.close(notice)
        child = subprocess.Popen([helper, '--parent-pid', str(os.getpid())], stdin=incoming,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.write(sender, str(child.pid).encode())
        time.sleep(.25)
        os._exit(0)
    os.close(incoming)
    os.close(sender)
    helper_pid = int(os.read(notice, 32))
    os.close(notice)
    os.waitpid(parent, 0)
    deadline = time.monotonic() + 5
    waited = 0
    try:
        while True:
            waited, status = os.waitpid(helper_pid, os.WNOHANG)
            if waited:
                require(os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL,
                        'recovery helper did not die with its parent')
                break
            require(time.monotonic() < deadline, 'recovery helper survived parent')
            time.sleep(.01)
    finally:
        os.close(writer)
        if not waited:
            os.kill(helper_pid, signal.SIGKILL)
            os.waitpid(helper_pid, 0)


def main():
    manifest = json.loads(Path('/opt/gdp-capture/runtime.json').read_text())
    require(manifest['version'] == 1 and manifest['sdk_version'] == '2.10.6'
            and manifest['upstream_commit'] == 'c153462518ad674650bafa4464fda72c27ab797a', 'runtime source/version differs')
    source = manifest['sdk_source']
    license_inventory = verify('/opt/third-party/orbbec')
    require(source.get('license_source') == license_inventory['sdk_license_source'], 'SDK license provenance differs')
    require(source['repository'] == 'https://github.com/orbbec/OrbbecSDK_ROS2'
            and source['commit'] == manifest['upstream_commit'] and source['license_files'], 'source/license inventory missing')
    for name, expected in manifest['files'].items():
        require(hashlib.sha256(Path(name).read_bytes()).hexdigest() == expected, 'runtime file hash differs')
    for name, expected in source['license_files'].items():
        require(hashlib.sha256(Path(name).read_bytes()).hexdigest() == expected, 'license hash differs')
    required_license_paths = {'/opt/third-party/orbbec/' + name for name in license_inventory['files']}
    required_license_paths.add('/opt/third-party/orbbec/license-inventory.json')
    require(set(source['license_files']) == required_license_paths, 'license inventory omits packaged files')
    require(required_license_paths.issubset(manifest['files']), 'runtime omits license files')
    sdk = '/opt/orbbec-sdk/lib/libOrbbecSDK.so'
    bridge = '/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so'
    service = '/usr/local/bin/capture-service'
    for path, key in ((sdk, 'sdk_sha256'), (bridge, 'bridge_sha256'), (service, 'service_sha256')):
        require(hashlib.sha256(Path(path).read_bytes()).hexdigest() == manifest[key] == manifest['files'][path], 'binary identity differs')
    require(hashlib.sha256(Path('/opt/orbbec-sdk/lib/libOrbbecSDK.so.2.10.6').read_bytes()).hexdigest()
            == manifest['sdk_sha256'], 'actual SDK version file differs')
    with tempfile.TemporaryDirectory(prefix='capture-image-smoke-', dir='/tmp') as directory:
        previous_directory = os.getcwd()
        try:
            os.chdir(directory)
            # Resolve optional IMU extensions in the pinned deployable image without
            # creating a Context, enumerating devices or starting any sensor.
            sdk_api = ctypes.CDLL(sdk)
            for symbol in ('ob_device_get_sensor', 'ob_delete_sensor', 'ob_sensor_get_stream_profile_list',
                           'ob_stream_profile_list_get_accel_stream_profile', 'ob_stream_profile_list_get_gyro_stream_profile',
                           'ob_sensor_start', 'ob_sensor_stop', 'ob_frame_get_type',
                           'ob_accel_frame_get_value', 'ob_gyro_frame_get_value'):
                require(hasattr(sdk_api, symbol), 'pinned SDK IMU extension missing')
            bridge_api = ctypes.CDLL(bridge)
            for symbol in ('gdp_ros_imu_create', 'gdp_ros_imu_publish', 'gdp_ros_imu_destroy',
                           'gdp_ros_context_abi_version', 'gdp_ros_context_create', 'gdp_ros_context_destroy',
                           'gdp_ros_create_in_context', 'gdp_ros_imu_create_in_context'):
                require(hasattr(bridge_api, symbol), 'IMU/context bridge extension missing')
            bridge_api.gdp_ros_context_abi_version.restype = ctypes.c_uint32
            require(bridge_api.gdp_ros_context_abi_version() == 1, 'ROS context extension ABI differs')
        finally:
            os.chdir(previous_directory)
        config = {'version': 1, 'sdk_library': sdk, 'serial': 'SYNTHETIC_NO_DEVICE_ACCESS',
                  'color': {'width': 2, 'height': 2, 'fps': 30, 'frame_id': 'synthetic_color_optical'},
                  'depth': {'width': 2, 'height': 2, 'fps': 30, 'frame_id': 'synthetic_depth_optical', 'unit_policy': 'normalize-mm'},
                  'pairing': {'clock': 'device', 'max_delta_us': 20000}, 'poll_ms': 100, 'capacity': 4,
                  'startup_discovery_seconds': 2,
                  'ros': {'bridge': bridge, 'namespace': '/synthetic_no_device', 'stamp_source': 'sdk-system-unix'},
                  'runtime_contract': {key: manifest[key] for key in ('sdk_sha256', 'bridge_sha256', 'service_sha256')},
                  'service': {'status_path': directory + '/status/status.json', 'generation': 'synthetic-image-smoke',
                              'idle_timeout_ms': 2000, 'startup_timeout_ms': 15000, 'heartbeat_ms': 500,
                              'heartbeat_timeout_ms': 10000, 'max_restarts': 3, 'restart_backoff_ms': 1000,
                              'shutdown_timeout_ms': 5000, 'ready_min_pairs': 5}}
        if 'usb_recovery' in manifest:
            recovery = manifest['usb_recovery']
            require(recovery['helper'] == '/opt/gdp-ros/bin/gdp-usb-recover'
                    and manifest['files'][recovery['helper']] == recovery['sha256'], 'USB helper identity differs')
            config['usb_recovery'] = dict(recovery)
            refusal = subprocess.run([recovery['helper'], '--invalid'], capture_output=True, timeout=5)
            require(refusal.returncode != 0 and json.loads(refusal.stdout)['code'] == 'USB_RECOVERY_ARGUMENTS', 'USB helper offline argument check failed')
            refused_parent = subprocess.run([recovery['helper'], '--parent-pid', '2147483647'], capture_output=True, timeout=5)
            require(refused_parent.returncode != 0 and json.loads(refused_parent.stdout)['code'] == 'USB_RECOVERY_PARENT', 'USB helper wrong-parent check failed')
            for invalid_serial in (b'synthetic\x00ignored', b'A' * 129, b'synthetic\n'):
                invalid = subprocess.run([recovery['helper'], '--parent-pid', str(os.getpid())], input=invalid_serial, capture_output=True, timeout=5)
                require(invalid.returncode != 0 and json.loads(invalid.stdout)['code'] == 'USB_RECOVERY_INPUT', 'USB helper failed to reject invalid serial before access')
            check_recovery_parent_death(recovery['helper'])
        path = Path(directory) / 'smoke.json'
        path.write_text(json.dumps(config))
        result = subprocess.run([service, '--validate-config', str(path)], text=True, capture_output=True, timeout=15)
        require(result.returncode == 0, 'capture executable/contract check failed: ' + result.stderr[:1000])
        report = json.loads(result.stdout)
        require(report.get('usb_recovery_artifact_verified') is ('usb_recovery' in manifest), 'USB recovery validation differs')
        require(report.get('valid') is True and report.get('runtime_contract_verified') is True
                and report.get('hardware_verified') is False, 'validation report differs')
        if 'usb_recovery' in manifest:
            config['usb_recovery']['sha256'] = '0' * 64
            path.write_text(json.dumps(config))
            refused = subprocess.run([service, '--validate-config', str(path)], capture_output=True, timeout=15)
            require(refused.returncode != 0, 'tampered USB recovery artifact accepted')
            config['usb_recovery'] = dict(manifest['usb_recovery'])
        config['runtime_contract']['sdk_sha256'] = '0' * 64
        path.write_text(json.dumps(config))
        rejected = subprocess.run([service, '--validate-config', str(path)], capture_output=True, timeout=15)
        require(rejected.returncode != 0, 'tampered binary contract accepted')
    print(json.dumps({'ok': True, 'kind': 'capture-image-no-device-smoke', 'runtime_contract_verified': True,
                      'binary_compatible': True, 'tamper_rejected': True, 'hardware_verified': False}))


if __name__ == '__main__':
    main()
