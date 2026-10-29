//! Thread-confined, polling-only SDK ownership adapter.
//! SDK objects and frames retain their loaded library. No Send/Sync is asserted.
//! Software aggregation and global timestamp mapping are explicit opt-ins. This
//! adapter does not configure exposure, hardware synchronization, or depth filters.
use orbbec_sys as sys;
mod calibration;
pub use calibration::{Extrinsics, SessionCalibration, StreamCalibration};
use std::{
    cell::Cell,
    ffi::{CStr, CString},
    fmt,
    path::Path,
    ptr::{self, NonNull},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, Clone)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}
/// SDK aggregation output policy; values match ob_frame_aggregate_output_mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum FrameAggregateOutput {
    All = 0,
    Any = 2,
    Disabled = 3,
}
/// `None` leaves the SDK default unchanged. Explicit requests fail on unsupported
/// SDK APIs rather than silently reverting to another capture policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    pub host_clock: Option<HostClock>,
    pub software_sync: Option<bool>,
    pub aggregate: Option<FrameAggregateOutput>,
    pub global_timestamp: Option<bool>,
}

/// Clock shared by every Context in the SDK runtime. Realtime is epoch-based;
/// monotonic values must not be published as Unix/ROS timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum HostClock {
    Realtime = 0,
    Monotonic = 1,
}

// Serialize open/configure/start so a clock change cannot race another adapter
// stream start. This tracks streams opened through this adapter, not foreign
// consumers using the same SDK runtime directly.
static SDK_OPEN_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE_PIPELINES: AtomicUsize = AtomicUsize::new(0);
struct Registration(Cell<bool>);
impl Registration {
    fn begin(&self) {
        self.0.set(true);
        ACTIVE_PIPELINES.fetch_add(1, Ordering::SeqCst);
    }
    fn end(&self) {
        if self.0.replace(false) {
            ACTIVE_PIPELINES.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.end();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yuyv,
    Depth16,
    Other(i32),
}
#[derive(Debug, Clone, Copy)]
pub struct FrameMetadata {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub index: u64,
    pub timestamp_us: u64,
    pub system_timestamp_us: u64,
    pub data_size: usize,
    /// Multiply raw depth samples by this factor to obtain millimeters.
    pub depth_scale_mm: Option<f32>,
}

fn check(api: &sys::Api, error: *mut sys::Error) -> Result<()> {
    if error.is_null() {
        return Ok(());
    }
    // SDK owns the error and its message until ob_delete_error.
    let message = unsafe {
        let p = (api.ob_error_get_message)(error);
        if p.is_null() {
            "SDK error without message".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    unsafe { (api.ob_delete_error)(error) };
    Err(Error(message))
}
type Delete = unsafe extern "C" fn(*mut sys::Handle, sys::ErrorOut);
struct Handle {
    ptr: NonNull<sys::Handle>,
    delete: Delete,
    api: Rc<sys::Api>,
}
impl Handle {
    fn new(
        api: &Rc<sys::Api>,
        delete: Delete,
        f: impl FnOnce(sys::ErrorOut) -> *mut sys::Handle,
    ) -> Result<Self> {
        let mut error = ptr::null_mut();
        let raw = f(&mut error);
        // Even nonconforming error+object returns are reclaimed.
        let handle = NonNull::new(raw).map(|ptr| Self {
            ptr,
            delete,
            api: api.clone(),
        });
        check(api, error)?;
        handle.ok_or_else(|| Error("SDK returned a null object without error".into()))
    }
    fn raw(&self) -> *mut sys::Handle {
        self.ptr.as_ptr()
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        let mut e = ptr::null_mut();
        unsafe { (self.delete)(self.raw(), &mut e) };
        let _ = check(&self.api, e);
    }
}

#[derive(Clone)]
pub struct Sdk {
    api: Rc<sys::Api>,
}
impl Sdk {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let api = Rc::new(sys::Api::load(path).map_err(Error)?);
        let version = unsafe { (api.ob_get_version)() };
        // This ABI subset was transcribed from v2 headers. Do not accept v1.
        if !(20000..30000).contains(&version) {
            return Err(Error(format!(
                "unsupported SDK version {version}; expected v2"
            )));
        }
        Ok(Self { api })
    }
    pub fn version(&self) -> i32 {
        unsafe { (self.api.ob_get_version)() }
    }
    pub fn open(
        &self,
        serial: &str,
        color: Option<VideoProfile>,
        depth: Option<VideoProfile>,
    ) -> Result<Pipeline> {
        self.open_with_options(serial, color, depth, CaptureOptions::default())
    }
    pub fn open_with_options(
        &self,
        serial: &str,
        color: Option<VideoProfile>,
        depth: Option<VideoProfile>,
        options: CaptureOptions,
    ) -> Result<Pipeline> {
        if serial.trim().is_empty() {
            return Err(Error("explicit device serial is required".into()));
        }
        let serial = CString::new(serial).map_err(|_| Error("serial contains NUL".into()))?;
        if color.is_none() && depth.is_none() {
            return Err(Error("at least one stream is required".into()));
        }
        for p in [color, depth].into_iter().flatten() {
            if p.width == 0 || p.height == 0 || p.fps == 0 {
                return Err(Error("profile dimensions and FPS must be positive".into()));
            }
        }
        if color.is_some_and(|p| p.width % 2 != 0) {
            return Err(Error("YUYV width must be even".into()));
        }
        let _open_guard = SDK_OPEN_LOCK
            .lock()
            .map_err(|_| Error("SDK configuration lock poisoned".into()))?;
        let a = &self.api;
        let context = Handle::new(a, a.ob_delete_context, |e| unsafe {
            (a.ob_create_context)(e)
        })?;
        if let Some(clock) = options.host_clock {
            let (get, set) = a.host_clock_api().map_err(Error)?;
            let mut e = ptr::null_mut();
            let current = unsafe { get(context.raw(), &mut e) };
            check(a, e)?;
            if current != clock as i32 {
                if ACTIVE_PIPELINES.load(Ordering::SeqCst) != 0 {
                    return Err(Error(
                        "cannot change SDK host clock while a pipeline is active".into(),
                    ));
                }
                let mut e = ptr::null_mut();
                unsafe { set(context.raw(), clock as i32, &mut e) };
                check(a, e)?;
                let mut e = ptr::null_mut();
                let actual = unsafe { get(context.raw(), &mut e) };
                check(a, e)?;
                if actual != clock as i32 {
                    return Err(Error("SDK host clock setting was not applied".into()));
                }
            }
        }
        let list = Handle::new(a, a.ob_delete_device_list, |e| unsafe {
            (a.ob_query_device_list)(context.raw(), e)
        })?;
        let device = Handle::new(a, a.ob_delete_device, |e| unsafe {
            (a.ob_device_list_get_device_by_serial_number)(list.raw(), serial.as_ptr(), e)
        })?;
        let pipeline = Handle::new(a, a.ob_delete_pipeline, |e| unsafe {
            (a.ob_create_pipeline_with_device)(device.raw(), e)
        })?;
        let config = Handle::new(a, a.ob_delete_config, |e| unsafe {
            (a.ob_create_config)(e)
        })?;
        for (p, stream, format) in [
            (color, sys::STREAM_COLOR, sys::FORMAT_YUYV),
            (depth, sys::STREAM_DEPTH, sys::FORMAT_Y16),
        ] {
            if let Some(p) = p {
                let mut e = ptr::null_mut();
                unsafe {
                    (a.ob_config_enable_video_stream)(
                        config.raw(),
                        stream,
                        p.width,
                        p.height,
                        p.fps,
                        format,
                        &mut e,
                    )
                };
                check(a, e)?;
            }
        }
        // Configure before starting streams: changing sync on a running SDK clears
        // its frame queues. Resolve only the APIs required by explicit options.
        if let Some(enabled) = options.global_timestamp {
            let (supported, enable) = a.global_device_api().map_err(Error)?;
            let mut e = ptr::null_mut();
            let has_global = unsafe { supported(device.raw(), &mut e) };
            check(a, e)?;
            if !has_global {
                return Err(Error("device does not support global timestamps".into()));
            }
            if enabled {
                // Refuse a partial global API before mutating device state.
                a.global_timestamp_api().map_err(Error)?;
            }
            let mut e = ptr::null_mut();
            unsafe { enable(device.raw(), enabled, &mut e) };
            check(a, e)?;
        }
        if let Some(mode) = options.aggregate {
            let apply = a.aggregate_api().map_err(Error)?;
            let mut e = ptr::null_mut();
            unsafe { apply(config.raw(), mode as i32, &mut e) };
            check(a, e)?;
        }
        if let Some(enabled) = options.software_sync {
            let apply = a.frame_sync_api(enabled).map_err(Error)?;
            let mut e = ptr::null_mut();
            unsafe { apply(pipeline.raw(), &mut e) };
            check(a, e)?;
        }
        let session = Rc::new(Session {
            pipeline,
            config,
            device,
            list,
            context,
            active: Cell::new(false),
            color_requested: color,
            depth_requested: depth,
            registration: Registration(Cell::new(false)),
        });
        let mut e = ptr::null_mut();
        session.registration.begin();
        unsafe {
            (a.ob_pipeline_start_with_config)(session.pipeline.raw(), session.config.raw(), &mut e)
        };
        // Stop even a partially started pipeline when start reports an error.
        session.active.set(true);
        check(a, e)?;
        Ok(Pipeline { session })
    }
}
// Field order retains all parent resources until child handles are deleted.
#[allow(dead_code)]
struct Session {
    pipeline: Handle,
    config: Handle,
    device: Handle,
    list: Handle,
    context: Handle,
    active: Cell<bool>,
    color_requested: Option<VideoProfile>,
    depth_requested: Option<VideoProfile>,
    // Last field releases registration only after SDK parent handles on an
    // unsuccessful stop; successful stop releases it immediately.
    registration: Registration,
}
impl Session {
    fn api(&self) -> &Rc<sys::Api> {
        &self.pipeline.api
    }
    fn stop(&self) -> Result<()> {
        if !self.active.get() {
            return Ok(());
        }
        let mut e = ptr::null_mut();
        unsafe { (self.api().ob_pipeline_stop)(self.pipeline.raw(), &mut e) };
        check(self.api(), e)?;
        self.active.set(false);
        self.registration.end();
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub struct Pipeline {
    session: Rc<Session>,
}
impl Pipeline {
    pub fn wait(&mut self, timeout_ms: u32) -> Result<Option<FrameSet>> {
        if timeout_ms == 0 || timeout_ms > 60_000 {
            return Err(Error("poll timeout must be 1..60000 ms".into()));
        }
        if !self.session.active.get() {
            return Err(Error("pipeline is stopped".into()));
        }
        let mut e = ptr::null_mut();
        let raw = unsafe {
            (self.session.api().ob_pipeline_wait_for_frameset)(
                self.session.pipeline.raw(),
                timeout_ms,
                &mut e,
            )
        };
        let frame = Frame::from_raw(self.session.clone(), raw, false);
        check(self.session.api(), e)?;
        Ok(frame.map(|frame| FrameSet { frame }))
    }
    pub fn stop(&mut self) -> Result<()> {
        self.session.stop()
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub struct FrameSet {
    frame: Frame,
}
impl FrameSet {
    pub fn color(&self) -> Result<Option<Frame>> {
        self.get(false)
    }
    pub fn depth(&self) -> Result<Option<Frame>> {
        self.get(true)
    }
    fn get(&self, depth: bool) -> Result<Option<Frame>> {
        let a = self.frame.session.api();
        let mut e = ptr::null_mut();
        let f = if depth {
            a.ob_frameset_get_depth_frame
        } else {
            a.ob_frameset_get_color_frame
        };
        let raw = unsafe { f(self.frame.ptr.as_ptr(), &mut e) };
        let frame = Frame::from_raw(self.frame.session.clone(), raw, depth);
        check(a, e)?;
        Ok(frame)
    }
}
pub struct Frame {
    ptr: NonNull<sys::Handle>,
    session: Rc<Session>,
    depth: bool,
}
impl Frame {
    fn from_raw(session: Rc<Session>, raw: *mut sys::Handle, depth: bool) -> Option<Self> {
        NonNull::new(raw).map(|ptr| Self {
            ptr,
            session,
            depth,
        })
    }
    fn read<T>(
        &self,
        f: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> T,
    ) -> Result<T> {
        let mut e = ptr::null_mut();
        let value = unsafe { f(self.ptr.as_ptr(), &mut e) };
        check(self.session.api(), e)?;
        Ok(value)
    }
    /// Capture timestamp mapped into the SDK host clock domain. Zero means
    /// unavailable; callers must not silently replace it with receipt time.
    pub fn global_timestamp_us(&self) -> Result<Option<u64>> {
        let read = self.session.api().global_timestamp_api().map_err(Error)?;
        let timestamp = self.read(read)?;
        Ok((timestamp != 0).then_some(timestamp))
    }
    pub fn metadata(&self) -> Result<FrameMetadata> {
        let a = self.session.api();
        let format = match self.read(a.ob_frame_get_format)? {
            sys::FORMAT_YUYV => Format::Yuyv,
            sys::FORMAT_Y16 => Format::Depth16,
            n => Format::Other(n),
        };
        let depth_scale_mm = if self.depth {
            let s = self.read(a.ob_depth_frame_get_value_scale)?;
            if !s.is_finite() || s <= 0.0 {
                return Err(Error("invalid SDK depth scale".into()));
            }
            Some(s)
        } else {
            None
        };
        Ok(FrameMetadata {
            format,
            width: self.read(a.ob_video_frame_get_width)?,
            height: self.read(a.ob_video_frame_get_height)?,
            index: self.read(a.ob_frame_get_index)?,
            timestamp_us: self.read(a.ob_frame_get_timestamp_us)?,
            system_timestamp_us: self.read(a.ob_frame_get_system_timestamp_us)?,
            data_size: self.read(a.ob_frame_get_data_size)? as usize,
            depth_scale_mm,
        })
    }
    /// Borrowed payload cannot escape its owning SDK frame.
    ///
    /// ```compile_fail
    /// fn escape(frame: orbbec_safe::Frame) -> &'static [u8] {
    ///     frame.data().unwrap()
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// fn drop_while_borrowed(frame: orbbec_safe::Frame) {
    ///     let bytes = frame.data().unwrap();
    ///     drop(frame);
    ///     println!("{}", bytes.len());
    /// }
    /// ```
    pub fn data(&self) -> Result<&[u8]> {
        let a = self.session.api();
        let len = self.read(a.ob_frame_get_data_size)? as usize;
        if len == 0 {
            return Ok(&[]);
        }
        if len > isize::MAX as usize {
            return Err(Error("SDK frame exceeds slice limit".into()));
        }
        let data = self.read(a.ob_frame_get_data)?;
        if data.is_null() {
            return Err(Error("SDK returned null frame data".into()));
        }
        // SDK frame reference owns an immutable payload until deleted. The
        // returned slice cannot outlive self and the adapter exposes no mutation.
        Ok(unsafe { std::slice::from_raw_parts(data, len) })
    }
    pub fn try_clone(&self) -> Result<Self> {
        let mut e = ptr::null_mut();
        unsafe { (self.session.api().ob_frame_add_ref)(self.ptr.as_ptr(), &mut e) };
        check(self.session.api(), e)?;
        Ok(Self {
            ptr: self.ptr,
            session: self.session.clone(),
            depth: self.depth,
        })
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        let mut e = ptr::null_mut();
        unsafe { (self.session.api().ob_delete_frame)(self.ptr.as_ptr(), &mut e) };
        let _ = check(self.session.api(), e);
    }
}
