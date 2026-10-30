# Experimental C ABI subset

The declarations were initially checked against SDK 2.9.3 headers. The deployed
capture image uses SDK 2.10.6 from the fixed public OrbbecSDK_ROS2 source recorded
in `capture.Dockerfile`. The initial header checksums below are historical ABI
provenance, not proof of compatibility with the deployed SDK. No SDK source or
binary is copied into this crate.

Header SHA-256 values observed during implementation:

```text
Context.h  6e67145fe7cbfa011ff5bfd85bec71ee8b07a1bdecedefa082c6a2f5e75f9904
Device.h   2cb1dba94deda334f3ba4fb6d35b9fca481bcf05a1e9b428150e04f85e12a062
Pipeline.h b3d3ee7c195f0df3cff71ec89812b8fda9fe98f27cb1934ae160befd73531ac3
Frame.h    99efd6626ae009cbe69a764624e619a337e40757c9eefeeb1d41082b880775bd
ObTypes.h  a68cc091061c7dd79f1b9156f37557e185363a2c1f0444d3fbc6985905e9811a
Error.h    b46d980f8bbf495a671c2fc9ab140c15e070684b7f416cab729232c67af159c7
Version.h  badd7655a38ca6414aabd4654b3d6f09f8b4b6eb534ed8e03a7515298f7ff9b5
```

Loading requires an explicit absolute path to a trusted native library.
`dlopen` executes native initialization code; symbol presence and a v2 runtime
version are compatibility checks, not authentication or a complete ABI proof.
Validate the binary's provenance, architecture, dependent extensions and exact
ABI against the deployed build before hardware use. Missing symbols fail loading.

The safe adapter polls video and can opt into independent bounded IMU callbacks.
It uses exact explicit profiles and catches panics at the callback boundary. SDK frames keep their parent session
and dynamic library alive, with owned reference releases on drop. Native frame
storage is only exposed through a borrowed immutable slice. SDK stop/delete can
still block or fail; this wrapper cannot add cancellation guarantees to native
calls. No synchronization, depth processing defaults, timestamp mapping, ROS
publishing, hotplug recovery or device compatibility certification is provided.

Calibration functions are optional and resolved only by a calibration query.
`FrameSet::calibration()` reads profiles attached to received frames, under the
same existing session. `Pipeline::calibration()` can read the enabled pipeline
configuration profiles. Both verify exact requested dimensions, format and FPS;
neither opens a second device or picks a catalog default. The SDK's intrinsic,
distortion and extrinsic structures use global **1-byte packing** in `ObTypes.h`;
the bindings preserve size/alignment and the mock exercises by-value ABI returns.

Supported exact distortion mappings are none-with-zero-coefficients to zero
`plumb_bob`, Brown-Conrady to `plumb_bob`, Brown-Conrady K6 to
`rational_polynomial` and Kannala-Brandt4 to `equidistant`. Modified/inverse
Brown-Conrady and incompatible extra coefficients are rejected, never relabeled.
Depth-to-color extrinsics are queried from depth source profile to color target
profile, preserving row-major rotation and converting SDK translation mm to m.
This source/direction convention is explicit and checked with nonidentity tests.
ROS raw-image CameraInfo derives R=identity and P=[K|0] as the stated unrectified
projection convention, not as a measured stereo rectification.

Hardware-independent tests compile a small C dynamic-library fixture and exercise
actual symbol loading and teardown. They require `cc`; they do not execute the
vendor SDK or establish camera correctness.

## Optional IMU ABI

The IMU function subset and packed 12-byte XYZ return value are transcribed from
[SDK v2.10.6 headers](https://github.com/orbbec/OrbbecSDK_v2/tree/v2.10.6/include/libobsensor/h).
Symbols are resolved only when IMU is explicitly requested. Accelerometer and
gyroscope sensor/profile failures propagate; no profile fallback is selected.
Callback frames own one SDK reference and release it on every path. Both sensors
must quiesce before their userdata can be freed: the SDK ImuStreamer snapshots
callbacks before dispatch. If either stop fails, the entire bounded IMU session
is retained until process exit so a late foreign callback cannot use freed data
or an unloaded library. The service supervisor retains its process shutdown bound.
