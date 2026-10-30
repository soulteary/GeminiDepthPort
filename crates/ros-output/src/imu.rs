//! Optional IMU bridge extension; separate raw accel/gyro messages, no fusion.
use super::*;
use frame_core::imu::{ImuConfig, ImuKind, ImuProgress, ImuSample};
#[repr(C)]
pub struct ImuFrameRaw {
    pub abi_version: u32,
    pub struct_size: u32,
    pub kind: u32,
    pub timestamp_source: u32,
    pub stamp_sec: i32,
    pub stamp_nanosec: u32,
    pub sequence: u64,
    pub device_timestamp_us: u64,
    pub system_timestamp_us: u64,
    pub global_timestamp_us: u64,
    pub xyz: [f64; 3],
}
type ImuCreate = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut *mut c_void,
) -> i32;
type ImuPublish = unsafe extern "C" fn(*mut c_void, *const ImuFrameRaw) -> i32;
unsafe fn symbol<T: Copy>(library: &Library, name: &'static [u8]) -> Result<T> {
    let raw = unsafe { libc::dlsym(library.handle.as_ptr(), name.as_ptr().cast()) };
    ensure!(
        !raw.is_null() && mem::size_of::<T>() == mem::size_of_val(&raw),
        "IMU bridge extension missing"
    );
    Ok(unsafe { mem::transmute_copy(&raw) })
}
pub struct ImuPublisher {
    library: Rc<Library>,
    handle: NonNull<c_void>,
    publish: ImuPublish,
    destroy: Destroy,
    config: ImuConfig,
    progress: ImuProgress,
    global: bool,
}
impl ImuPublisher {
    pub fn load(
        path: impl AsRef<Path>,
        namespace: &str,
        config: ImuConfig,
        global: bool,
    ) -> Result<Self> {
        config.validate()?;
        ensure!(
            config.enabled && absolute_name(namespace, true),
            "invalid IMU binding"
        );
        let library = Library::load(path.as_ref())?;
        let create: ImuCreate = unsafe { symbol(&library, b"gdp_ros_imu_create\0")? };
        let publish: ImuPublish = unsafe { symbol(&library, b"gdp_ros_imu_publish\0")? };
        let destroy: Destroy = unsafe { symbol(&library, b"gdp_ros_imu_destroy\0")? };
        let prefix = namespace.trim_end_matches('/');
        let strings = [
            "gdp_imu".into(),
            namespace.into(),
            format!("{prefix}/accel/sample"),
            format!("{prefix}/gyro/sample"),
            config.accel_frame_id.clone(),
            config.gyro_frame_id.clone(),
        ]
        .into_iter()
        .map(CString::new)
        .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut handle = ptr::null_mut();
        let code = unsafe {
            create(
                strings[0].as_ptr(),
                strings[1].as_ptr(),
                strings[2].as_ptr(),
                strings[3].as_ptr(),
                strings[4].as_ptr(),
                strings[5].as_ptr(),
                &mut handle,
            )
        };
        if code != 0 {
            if !handle.is_null() {
                unsafe { destroy(handle) }
            }
            bail!("IMU publisher creation failed");
        }
        Ok(Self {
            library,
            handle: NonNull::new(handle).context("null IMU publisher")?,
            publish,
            destroy,
            config,
            progress: ImuProgress::default(),
            global,
        })
    }
    pub fn publish(&mut self, sample: &ImuSample) -> Result<()> {
        let mut next = self.progress.clone();
        next.accept(sample, &self.config)?;
        let us = if self.global {
            sample
                .global_timestamp_us
                .context("IMU global clock unavailable")?
        } else {
            sample.system_timestamp_us
        };
        let stamp = frame_core::sdk_system_unix_stamp(Some(us))?;
        let frame = ImuFrameRaw {
            abi_version: 1,
            struct_size: mem::size_of::<ImuFrameRaw>() as u32,
            timestamp_source: if self.global { 2 } else { 1 },
            kind: if sample.kind == ImuKind::Accel { 1 } else { 2 },
            stamp_sec: stamp.sec,
            stamp_nanosec: stamp.nanosec,
            sequence: sample.sequence,
            device_timestamp_us: sample.device_timestamp_us,
            system_timestamp_us: sample.system_timestamp_us,
            global_timestamp_us: sample.global_timestamp_us.unwrap_or(0),
            xyz: sample.xyz,
        };
        ensure!(
            unsafe { (self.publish)(self.handle.as_ptr(), &frame) } == 0,
            "IMU publish failed"
        );
        self.progress = next;
        Ok(())
    }
}
impl Drop for ImuPublisher {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.handle.as_ptr()) };
        let _ = &self.library;
    }
}
