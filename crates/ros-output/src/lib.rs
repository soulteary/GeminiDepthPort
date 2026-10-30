//! Synchronous owned-message ROS bridge. The SDK stays in Rust; a thin C++
//! rclcpp library copies validated payloads before returning. No zero-copy or
//! cross-process atomicity is claimed. Explicit SDK system/global Unix mappings
//! and little-endian millimetre depth are supported.
use anyhow::{Context, Result, bail, ensure};
use frame_core::{FrameEncoding, PacketMetadata, StampGuard, UnixStamp};
use serde_json::Value;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    marker::PhantomData,
    mem,
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr::{self, NonNull},
    rc::Rc,
};
mod imu;
pub use imu::{ImuFrameRaw, ImuPublisher};
mod auxiliary;
pub use auxiliary::{AuxBinding, RosAuxPublisher, StaticTransform};

pub const ABI_VERSION: u32 = 1;
/// The caller declares the mapped host clock; unavailable global never falls back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampSource {
    System,
    Global,
}
const MAX_PAYLOAD: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Binding {
    pub node_name: String,
    pub namespace: String,
    pub image_topic: String,
    pub camera_info_topic: String,
    pub metadata_topic: String,
    pub frame_id: String,
}
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn absolute_name(s: &str, root: bool) -> bool {
    s.len() <= 256 && (s == "/" && root || s.starts_with('/') && s[1..].split('/').all(token))
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            token(&self.node_name) && token(&self.frame_id),
            "invalid node or optical frame name"
        );
        ensure!(
            absolute_name(&self.namespace, true),
            "explicit absolute namespace required"
        );
        let prefix = if self.namespace == "/" {
            "/".into()
        } else {
            format!("{}/", self.namespace)
        };
        for topic in [
            &self.image_topic,
            &self.camera_info_topic,
            &self.metadata_topic,
        ] {
            ensure!(
                absolute_name(topic, false) && topic.starts_with(&prefix),
                "explicit absolute topic inside namespace required"
            );
        }
        ensure!(
            self.image_topic != self.camera_info_topic
                && self.image_topic != self.metadata_topic
                && self.camera_info_topic != self.metadata_topic,
            "publisher topics must differ"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Calibration {
    pub width: u32,
    pub height: u32,
    pub frame_id: String,
    pub distortion_model: String,
    pub k: [f64; 9],
    pub d: Vec<f64>,
    pub r: [f64; 9],
    pub p: [f64; 12],
}
fn near(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}
impl Calibration {
    /// Construct unrectified CameraInfo from the actual SDK profile. R=identity
    /// and P=[K|0] are the explicitly declared raw-image projection convention,
    /// not measured stereo rectification or a fabricated physical calibration.
    pub fn from_sdk(value: &orbbec_safe::StreamCalibration, frame_id: &str) -> Result<Self> {
        let result = Self {
            width: value.profile.width,
            height: value.profile.height,
            frame_id: frame_id.into(),
            distortion_model: value.distortion_model.clone(),
            k: [value.fx, 0., value.cx, 0., value.fy, value.cy, 0., 0., 1.],
            d: value.distortion.clone(),
            r: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            p: [
                value.fx, 0., value.cx, 0., 0., value.fy, value.cy, 0., 0., 0., 1., 0.,
            ],
        };
        result.validate()?;
        Ok(result)
    }
    /// Parse explicit observations. No defaults, synthetic fallback, binning,
    /// crop or invented projection matrix are applied.
    pub fn from_json(text: &str) -> Result<Self> {
        ensure!(text.len() <= 64 * 1024, "calibration input too large");
        let value: Value = serde_json::from_str(text).context("invalid calibration JSON")?;
        let object = value.as_object().context("calibration object required")?;
        let keys = [
            "version",
            "width",
            "height",
            "frame_id",
            "distortion_model",
            "k",
            "d",
            "r",
            "p",
        ];
        ensure!(
            object.len() == keys.len() && object.keys().all(|k| keys.contains(&k.as_str())),
            "unexpected or missing calibration field"
        );
        ensure!(
            value["version"].as_u64() == Some(1),
            "unsupported calibration version"
        );
        let dimension = |key| -> Result<u32> {
            Ok(u32::try_from(
                value[key]
                    .as_u64()
                    .context("integer calibration dimension required")?,
            )?)
        };
        let text_field = |key| -> Result<String> {
            Ok(value[key]
                .as_str()
                .context("calibration string required")?
                .to_owned())
        };
        let numbers = |key| -> Result<Vec<f64>> {
            value[key]
                .as_array()
                .context("calibration numeric array required")?
                .iter()
                .map(|v| v.as_f64().context("finite calibration number required"))
                .collect()
        };
        let calibration = Self {
            width: dimension("width")?,
            height: dimension("height")?,
            frame_id: text_field("frame_id")?,
            distortion_model: text_field("distortion_model")?,
            k: numbers("k")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("K must have 9 elements"))?,
            d: numbers("d")?,
            r: numbers("r")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("R must have 9 elements"))?,
            p: numbers("p")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("P must have 12 elements"))?,
        };
        calibration.validate()?;
        Ok(calibration)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16384).contains(&self.width)
                && (1..=16384).contains(&self.height)
                && token(&self.frame_id),
            "invalid calibration geometry or frame"
        );
        ensure!(
            self.k
                .iter()
                .chain(self.d.iter())
                .chain(self.r.iter())
                .chain(self.p.iter())
                .all(|v| v.is_finite()),
            "calibration must be finite"
        );
        let coefficients = match self.distortion_model.as_str() {
            "plumb_bob" => 5,
            "rational_polynomial" => 8,
            "equidistant" => 4,
            _ => bail!("unsupported distortion model"),
        };
        ensure!(
            self.d.len() == coefficients,
            "distortion coefficient count differs"
        );
        ensure!(
            self.k[0] > 0.0
                && self.k[4] > 0.0
                && near(self.k[6], 0.0, 1e-6)
                && near(self.k[7], 0.0, 1e-6)
                && near(self.k[8], 1.0, 1e-6),
            "invalid camera intrinsic matrix"
        );
        ensure!(
            self.p[0] > 0.0
                && self.p[5] > 0.0
                && near(self.p[8], 0.0, 1e-6)
                && near(self.p[9], 0.0, 1e-6)
                && near(self.p[10], 1.0, 1e-6)
                && near(self.p[11], 0.0, 1e-6),
            "invalid camera projection matrix"
        );
        for row in 0..3 {
            for col in 0..3 {
                let product: f64 = (0..3)
                    .map(|j| self.r[row * 3 + j] * self.r[col * 3 + j])
                    .sum();
                ensure!(
                    near(product, if row == col { 1.0 } else { 0.0 }, 1e-5),
                    "rectification matrix must be orthonormal"
                );
            }
        }
        let r = self.r;
        let determinant = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
            + r[2] * (r[3] * r[7] - r[4] * r[6]);
        ensure!(
            near(determinant, 1.0, 1e-5),
            "rectification must be a proper rotation"
        );
        Ok(())
    }
}

// Exact layout mirrored by ros/include/gdp_ros_bridge.h. u32 flags avoid C++
// bool layout assumptions; lengths are fixed width, no Rust usize crosses ABI.
#[repr(C)]
struct ConfigRaw {
    abi_version: u32,
    struct_size: u32,
    node_name: *const c_char,
    namespace_name: *const c_char,
    image_topic: *const c_char,
    camera_info_topic: *const c_char,
    metadata_topic: *const c_char,
    frame_id: *const c_char,
    width: u32,
    height: u32,
    distortion_model: *const c_char,
    k: [f64; 9],
    r: [f64; 9],
    p: [f64; 12],
    d: *const f64,
    d_len: u32,
}
#[repr(C)]
struct FrameRaw {
    abi_version: u32,
    struct_size: u32,
    encoding: u32,
    width: u32,
    height: u32,
    step: u32,
    is_bigendian: u32,
    stamp_sec: i32,
    stamp_nanosec: u32,
    sequence: u64,
    has_device_timestamp: u32,
    has_system_timestamp: u32,
    device_timestamp_us: u64,
    system_timestamp_us: u64,
    has_depth_scale: u32,
    normalized: u32,
    source_depth_scale_mm: f32,
    output_depth_scale_mm: f32,
    data: *const u8,
    data_len: u64,
}
type Create = unsafe extern "C" fn(*const ConfigRaw, *mut *mut c_void) -> i32;
type Publish = unsafe extern "C" fn(*mut c_void, *const FrameRaw) -> i32;
type Destroy = unsafe extern "C" fn(*mut c_void);
struct Library {
    handle: NonNull<c_void>,
    create: Create,
    publish: Publish,
    destroy: Destroy,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.handle.as_ptr());
        }
    }
}
fn loader_error() -> String {
    unsafe {
        let p = libc::dlerror();
        if p.is_null() {
            "native bridge loader failure".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
impl Library {
    fn load(path: &Path) -> Result<Rc<Self>> {
        ensure!(path.is_absolute(), "bridge path must be absolute");
        let path = CString::new(path.as_os_str().as_bytes())?;
        let raw = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        let handle = NonNull::new(raw).ok_or_else(|| anyhow::anyhow!(loader_error()))?;
        // Guard the dlopen reference even when a required symbol is absent.
        struct Close(NonNull<c_void>);
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe {
                    libc::dlclose(self.0.as_ptr());
                }
            }
        }
        let close = Close(handle);
        unsafe fn symbol<T: Copy>(handle: NonNull<c_void>, name: &'static [u8]) -> Result<T> {
            unsafe {
                libc::dlerror();
            }
            let raw = unsafe { libc::dlsym(handle.as_ptr(), name.as_ptr().cast()) };
            ensure!(
                !raw.is_null(),
                "missing bridge symbol: {}",
                String::from_utf8_lossy(name)
            );
            ensure!(
                mem::size_of::<T>() == mem::size_of_val(&raw),
                "unsupported function pointer ABI"
            );
            Ok(unsafe { mem::transmute_copy(&raw) })
        }
        let version: unsafe extern "C" fn() -> u32 =
            unsafe { symbol(handle, b"gdp_ros_abi_version\0")? };
        ensure!(
            unsafe { version() } == ABI_VERSION,
            "ROS bridge ABI version differs"
        );
        let library = Self {
            handle,
            create: unsafe { symbol(handle, b"gdp_ros_create\0")? },
            publish: unsafe { symbol(handle, b"gdp_ros_publish\0")? },
            destroy: unsafe { symbol(handle, b"gdp_ros_destroy\0")? },
            _thread: PhantomData,
        };
        mem::forget(close);
        Ok(Rc::new(library))
    }
}

/// A session distinguishes device reopen/reset from sequence progression.
fn validate_session_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "session id must be 128-bit lowercase hex"
    );
    Ok(())
}
unsafe fn set_native_session(
    library: &Library,
    handle: NonNull<c_void>,
    name: &'static [u8],
    value: &str,
) -> Result<()> {
    validate_session_id(value)?;
    let raw = unsafe { libc::dlsym(library.handle.as_ptr(), name.as_ptr().cast()) };
    ensure!(!raw.is_null(), "bridge session extension unavailable");
    let function: unsafe extern "C" fn(*mut c_void, *const c_char) -> i32 =
        unsafe { mem::transmute(raw) };
    let value = CString::new(value)?;
    ensure!(
        unsafe { function(handle.as_ptr(), value.as_ptr()) } == 0,
        "bridge session binding failed"
    );
    Ok(())
}

/// Thread-confined native publisher. It cannot be moved into a worker thread.
///
/// ```compile_fail
/// fn send_publisher(publisher: ros_output::Publisher) {
///     std::thread::spawn(move || drop(publisher));
/// }
/// ```
pub struct Publisher {
    library: Rc<Library>,
    handle: NonNull<c_void>,
    calibration: Calibration,
    stamps: StampGuard,
    sequence: Option<u64>,
    session_bound: bool,
}
impl Publisher {
    pub fn load(
        path: impl AsRef<Path>,
        binding: Binding,
        calibration: Calibration,
    ) -> Result<Self> {
        binding.validate()?;
        calibration.validate()?;
        ensure!(
            binding.frame_id == calibration.frame_id,
            "calibration optical frame differs from binding"
        );
        let strings = [
            &binding.node_name,
            &binding.namespace,
            &binding.image_topic,
            &binding.camera_info_topic,
            &binding.metadata_topic,
            &binding.frame_id,
            &calibration.distortion_model,
        ]
        .into_iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
        let config = ConfigRaw {
            abi_version: ABI_VERSION,
            struct_size: u32::try_from(mem::size_of::<ConfigRaw>())?,
            node_name: strings[0].as_ptr(),
            namespace_name: strings[1].as_ptr(),
            image_topic: strings[2].as_ptr(),
            camera_info_topic: strings[3].as_ptr(),
            metadata_topic: strings[4].as_ptr(),
            frame_id: strings[5].as_ptr(),
            width: calibration.width,
            height: calibration.height,
            distortion_model: strings[6].as_ptr(),
            k: calibration.k,
            r: calibration.r,
            p: calibration.p,
            d: calibration.d.as_ptr(),
            d_len: u32::try_from(calibration.d.len())?,
        };
        let library = Library::load(path.as_ref())?;
        let mut handle = ptr::null_mut();
        let code = unsafe { (library.create)(&config, &mut handle) };
        if code != 0 {
            if !handle.is_null() {
                unsafe {
                    (library.destroy)(handle);
                }
            }
            bail!("ROS bridge creation failed (code {code})");
        }
        let handle = NonNull::new(handle).context("ROS bridge returned null publisher")?;
        Ok(Self {
            library,
            handle,
            calibration,
            stamps: StampGuard::default(),
            sequence: None,
            session_bound: false,
        })
    }
    /// Bind once before any publication; recreating a publisher is required for a new session.
    pub fn set_session_id(&mut self, value: &str) -> Result<()> {
        ensure!(
            !self.session_bound && self.sequence.is_none(),
            "session already bound or publisher active"
        );
        unsafe {
            set_native_session(
                &self.library,
                self.handle,
                b"gdp_ros_set_session_id\0",
                value,
            )?;
        }
        self.session_bound = true;
        Ok(())
    }
    /// Validate and synchronously copy one frame into ROS-owned messages.
    /// Callers retain payload ownership; a failed call may have published only
    /// some of the three topics and should terminate the experimental session.
    pub fn publish(
        &mut self,
        metadata: &PacketMetadata,
        payload: &[u8],
        stamp_ns: u64,
    ) -> Result<()> {
        self.publish_with_timestamp_source(metadata, payload, stamp_ns, TimestampSource::System)
    }
    pub fn publish_with_timestamp_source(
        &mut self,
        metadata: &PacketMetadata,
        payload: &[u8],
        stamp_ns: u64,
        source: TimestampSource,
    ) -> Result<()> {
        metadata.validate(payload.len())?;
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "ROS image exceeds payload limit"
        );
        ensure!(
            metadata.layout.width == self.calibration.width
                && metadata.layout.height == self.calibration.height,
            "frame dimensions differ from calibration"
        );
        if metadata.encoding == FrameEncoding::Depth16 {
            ensure!(
                cfg!(target_endian = "little"),
                "only little-endian depth is supported"
            );
            ensure!(
                metadata.output_depth_scale_mm == Some(1.0),
                "ROS 16UC1 requires millimetre output"
            );
        }
        let system = metadata
            .system_timestamp_us
            .context("explicit SDK system timestamp required")?;
        let mapped = match source {
            TimestampSource::System => system,
            TimestampSource::Global => metadata
                .global_timestamp_us
                .context("SDK global timestamp unavailable")?,
        };
        ensure!(system > 0, "SDK system timestamp unavailable");
        ensure!(
            mapped > 0 && mapped.checked_mul(1000) == Some(stamp_ns),
            "stamp must exactly map selected SDK Unix time"
        );
        // Resolve before native publication: old bridges remain usable for legacy
        // system stamps, while explicit global requests cannot silently downgrade.
        let extension = if source == TimestampSource::Global
            || metadata.global_timestamp_us.is_some()
        {
            let name = b"gdp_ros_publish_timestamped\0";
            let raw = unsafe { libc::dlsym(self.library.handle.as_ptr(), name.as_ptr().cast()) };
            ensure!(!raw.is_null(), "bridge timestamp extension unavailable");
            Some(unsafe {
                mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *const FrameRaw, u64, u32) -> i32,
                >(raw)
            })
        } else {
            None
        };
        let observed = if let Some(returned) = metadata.sdk_return_unix_ns {
            ensure!(returned > 0, "SDK return Unix timestamp unavailable");
            let name = b"gdp_ros_publish_observed\0";
            let raw = unsafe { libc::dlsym(self.library.handle.as_ptr(), name.as_ptr().cast()) };
            ensure!(
                !raw.is_null(),
                "bridge pipeline observation extension unavailable"
            );
            Some((
                unsafe {
                    mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(
                            *mut c_void,
                            *const FrameRaw,
                            u64,
                            u32,
                            u64,
                            u32,
                        ) -> i32,
                    >(raw)
                },
                returned,
            ))
        } else {
            None
        };
        let stamp = UnixStamp {
            sec: i32::try_from(stamp_ns / 1_000_000_000)?,
            nanosec: (stamp_ns % 1_000_000_000) as u32,
        };
        self.stamps.check(stamp)?;
        ensure!(
            self.sequence.is_none_or(|last| metadata.sequence > last),
            "frame sequence did not advance"
        );
        let raw = FrameRaw {
            abi_version: ABI_VERSION,
            struct_size: u32::try_from(mem::size_of::<FrameRaw>())?,
            encoding: match metadata.encoding {
                FrameEncoding::Yuyv => 1,
                FrameEncoding::Depth16 => 2,
            },
            width: metadata.layout.width,
            height: metadata.layout.height,
            step: u32::try_from(metadata.layout.stride)?,
            is_bigendian: 0,
            stamp_sec: stamp.sec,
            stamp_nanosec: stamp.nanosec,
            sequence: metadata.sequence,
            has_device_timestamp: u32::from(metadata.device_timestamp_us.is_some()),
            has_system_timestamp: 1,
            device_timestamp_us: metadata.device_timestamp_us.unwrap_or(0),
            system_timestamp_us: system,
            has_depth_scale: u32::from(metadata.encoding == FrameEncoding::Depth16),
            normalized: u32::from(metadata.normalized),
            source_depth_scale_mm: metadata.source_depth_scale_mm.unwrap_or(0.0),
            output_depth_scale_mm: metadata.output_depth_scale_mm.unwrap_or(0.0),
            data: payload.as_ptr(),
            data_len: u64::try_from(payload.len())?,
        };
        let source_code = match source {
            TimestampSource::System => 1,
            TimestampSource::Global => 2,
        };
        let code = unsafe {
            if let Some((publish, returned)) = observed {
                publish(
                    self.handle.as_ptr(),
                    &raw,
                    metadata.global_timestamp_us.unwrap_or(0),
                    source_code,
                    returned,
                    u32::from(metadata.sdk_system_clock_verified),
                )
            } else {
                match extension {
                    Some(publish) => publish(
                        self.handle.as_ptr(),
                        &raw,
                        metadata.global_timestamp_us.unwrap_or(0),
                        match source {
                            TimestampSource::System => 1,
                            TimestampSource::Global => 2,
                        },
                    ),
                    None => (self.library.publish)(self.handle.as_ptr(), &raw),
                }
            }
        };
        ensure!(code == 0, "ROS bridge publication failed (code {code})");
        self.stamps.accept(stamp)?;
        self.sequence = Some(metadata.sequence);
        Ok(())
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        unsafe {
            (self.library.destroy)(self.handle.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn calibration() -> Calibration {
        Calibration {
            width: 2,
            height: 2,
            frame_id: "test_optical".into(),
            distortion_model: "plumb_bob".into(),
            k: [1., 0., 1., 0., 1., 1., 0., 0., 1.],
            d: vec![0.; 5],
            r: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            p: [1., 0., 1., 0., 0., 1., 1., 0., 0., 0., 1., 0.],
        }
    }
    #[test]
    fn calibration_rejects_invalid_matrices_and_models() {
        let good = calibration();
        good.validate().unwrap();
        let mut bad = good.clone();
        bad.k[0] = 0.;
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.d.push(0.);
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.r[0] = -1.;
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.p[5] = f64::NAN;
        assert!(bad.validate().is_err());
        let mut bad = good;
        bad.distortion_model = "unknown".into();
        assert!(bad.validate().is_err());
    }
    #[test]
    fn calibration_json_has_no_default_fallback() {
        assert!(Calibration::from_json("{}").is_err());
        let good = r#"{"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]}"#;
        Calibration::from_json(good).unwrap();
        assert!(Calibration::from_json(&good.replace("\"version\":1", "\"version\":2")).is_err());
        assert!(
            Calibration::from_json(
                &good.replace("\"version\":1", "\"version\":1,\"imaginary\":true")
            )
            .is_err()
        );
    }
    #[test]
    fn binding_requires_explicit_consistent_topics() {
        let mut b = Binding {
            node_name: "probe".into(),
            namespace: "/camera".into(),
            image_topic: "/camera/color/image_raw".into(),
            camera_info_topic: "/camera/color/camera_info".into(),
            metadata_topic: "/camera/color/frame_metadata".into(),
            frame_id: "test_optical".into(),
        };
        b.validate().unwrap();
        b.metadata_topic = "/other/frame_metadata".into();
        assert!(b.validate().is_err());
        b.namespace = "/camera//".into();
        assert!(b.validate().is_err());
    }
}
