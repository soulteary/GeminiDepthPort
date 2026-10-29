//! Minimal dynamically loaded Orbbec C ABI, initially based on SDK 2.9.3 headers.
//! Initial header checks do not prove ABI equivalence with the deployed SDK.
//! No library is searched implicitly: callers must supply a trusted absolute path.
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    marker::PhantomData,
    os::unix::ffi::OsStrExt,
    path::Path,
    rc::Rc,
};
pub type Handle = c_void;
pub type Error = c_void;
pub type ErrorOut = *mut *mut Error;
pub const STREAM_COLOR: c_int = 2;
pub const STREAM_DEPTH: c_int = 3;
pub const FORMAT_YUYV: c_int = 0;
pub const FORMAT_Y16: c_int = 8;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CameraIntrinsic {
    pub fx: f32,
    pub fy: f32,
    pub cx: f32,
    pub cy: f32,
    pub width: i16,
    pub height: i16,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CameraDistortion {
    pub k1: f32,
    pub k2: f32,
    pub k3: f32,
    pub k4: f32,
    pub k5: f32,
    pub k6: f32,
    pub p1: f32,
    pub p2: f32,
    pub model: c_int,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Extrinsic {
    pub rot: [f32; 9],
    pub trans: [f32; 3],
}

/// Optional calibration symbols are resolved only when calibration is requested.
/// Existing acquisition-only binaries need not expose this additional subset.
#[derive(Clone, Copy)]
pub struct CalibrationApi {
    pub frame_profile: unsafe extern "C" fn(*const Handle, ErrorOut) -> *mut Handle,
    pub pipeline_config: unsafe extern "C" fn(*const Handle, ErrorOut) -> *mut Handle,
    pub config_profiles: unsafe extern "C" fn(*const Handle, ErrorOut) -> *mut Handle,
    pub delete_profiles: unsafe extern "C" fn(*const Handle, ErrorOut),
    pub profile_count: unsafe extern "C" fn(*const Handle, ErrorOut) -> u32,
    pub profile_get: unsafe extern "C" fn(*const Handle, c_int, ErrorOut) -> *mut Handle,
    pub delete_profile: unsafe extern "C" fn(*const Handle, ErrorOut),
    pub profile_type: unsafe extern "C" fn(*const Handle, ErrorOut) -> c_int,
    pub profile_format: unsafe extern "C" fn(*const Handle, ErrorOut) -> c_int,
    pub width: unsafe extern "C" fn(*const Handle, ErrorOut) -> u32,
    pub height: unsafe extern "C" fn(*const Handle, ErrorOut) -> u32,
    pub fps: unsafe extern "C" fn(*const Handle, ErrorOut) -> u32,
    pub intrinsic: unsafe extern "C" fn(*const Handle, ErrorOut) -> CameraIntrinsic,
    pub distortion: unsafe extern "C" fn(*const Handle, ErrorOut) -> CameraDistortion,
    pub extrinsic_to: unsafe extern "C" fn(*const Handle, *mut Handle, ErrorOut) -> Extrinsic,
}

struct Library(*mut c_void, PhantomData<Rc<()>>);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.0);
        }
    }
}
fn dl_error() -> String {
    unsafe {
        let p = libc::dlerror();
        if p.is_null() {
            "dynamic loader failure".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
impl Library {
    fn open(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("SDK library path must be absolute".into());
        }
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| "SDK path contains NUL")?;
        let p = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if p.is_null() {
            Err(dl_error())
        } else {
            Ok(Self(p, PhantomData))
        }
    }
    unsafe fn symbol<T: Copy>(&self, name: &'static [u8]) -> Result<T, String> {
        unsafe {
            libc::dlerror();
        }
        let p = unsafe { libc::dlsym(self.0, name.as_ptr().cast()) };
        if p.is_null() {
            return Err(format!(
                "missing SDK symbol {}: {}",
                String::from_utf8_lossy(name),
                dl_error()
            ));
        }
        // POSIX dlsym returns callable function addresses; every T here is the
        // exact C function pointer declaration transcribed from SDK headers.
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&p));
        Ok(unsafe { std::mem::transmute_copy(&p) })
    }
}
macro_rules! api {
    ($($name:ident: $ty:ty),* $(,)?) => {
        pub struct Api { _library: Library, $(pub $name: $ty,)* }
        impl Api {
            /// Load a trusted SDK binary. A foreign library must honor its C ABI.
            pub fn load(path: impl AsRef<Path>) -> Result<Self,String> {
                let library=Library::open(path.as_ref())?;
                $(let $name=unsafe {library.symbol(concat!(stringify!($name),"\0").as_bytes())?};)*
                Ok(Self {_library:library,$($name,)*})
            }
        }
    }
}
api! {
    ob_get_version: unsafe extern "C" fn()->c_int,
    ob_create_context: unsafe extern "C" fn(ErrorOut)->*mut Handle,
    ob_delete_context: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_query_device_list: unsafe extern "C" fn(*mut Handle,ErrorOut)->*mut Handle,
    ob_delete_device_list: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_device_list_get_device_by_serial_number: unsafe extern "C" fn(*const Handle,*const c_char,ErrorOut)->*mut Handle,
    ob_delete_device: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_create_pipeline_with_device: unsafe extern "C" fn(*const Handle,ErrorOut)->*mut Handle,
    ob_delete_pipeline: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_create_config: unsafe extern "C" fn(ErrorOut)->*mut Handle,
    ob_delete_config: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_config_enable_video_stream: unsafe extern "C" fn(*mut Handle,c_int,u32,u32,u32,c_int,ErrorOut),
    ob_pipeline_start_with_config: unsafe extern "C" fn(*mut Handle,*const Handle,ErrorOut),
    ob_pipeline_stop: unsafe extern "C" fn(*mut Handle,ErrorOut),
    ob_pipeline_wait_for_frameset: unsafe extern "C" fn(*mut Handle,u32,ErrorOut)->*mut Handle,
    ob_frameset_get_color_frame: unsafe extern "C" fn(*const Handle,ErrorOut)->*mut Handle,
    ob_frameset_get_depth_frame: unsafe extern "C" fn(*const Handle,ErrorOut)->*mut Handle,
    ob_delete_frame: unsafe extern "C" fn(*const Handle,ErrorOut),
    ob_frame_add_ref: unsafe extern "C" fn(*const Handle,ErrorOut),
    ob_frame_get_format: unsafe extern "C" fn(*const Handle,ErrorOut)->c_int,
    ob_video_frame_get_width: unsafe extern "C" fn(*const Handle,ErrorOut)->u32,
    ob_video_frame_get_height: unsafe extern "C" fn(*const Handle,ErrorOut)->u32,
    ob_frame_get_index: unsafe extern "C" fn(*const Handle,ErrorOut)->u64,
    ob_frame_get_timestamp_us: unsafe extern "C" fn(*const Handle,ErrorOut)->u64,
    ob_frame_get_system_timestamp_us: unsafe extern "C" fn(*const Handle,ErrorOut)->u64,
    ob_frame_get_data: unsafe extern "C" fn(*const Handle,ErrorOut)->*mut u8,
    ob_frame_get_data_size: unsafe extern "C" fn(*const Handle,ErrorOut)->u32,
    ob_depth_frame_get_value_scale: unsafe extern "C" fn(*const Handle,ErrorOut)->f32,
    ob_error_get_message: unsafe extern "C" fn(*const Error)->*const c_char,
    ob_delete_error: unsafe extern "C" fn(*mut Error),
}

/// Optional synchronization and clock APIs are resolved only on explicit request.
/// This preserves acquisition compatibility with SDKs exposing the original subset.
pub type HostClockGetFn = unsafe extern "C" fn(*const Handle, ErrorOut) -> c_int;
pub type HostClockSetFn = unsafe extern "C" fn(*mut Handle, c_int, ErrorOut);
pub type PipelineSyncFn = unsafe extern "C" fn(*mut Handle, ErrorOut);
pub type AggregateFn = unsafe extern "C" fn(*mut Handle, c_int, ErrorOut);
pub type GlobalSupportedFn = unsafe extern "C" fn(*const Handle, ErrorOut) -> bool;
pub type GlobalEnableFn = unsafe extern "C" fn(*mut Handle, bool, ErrorOut);
pub type GlobalTimestampFn = unsafe extern "C" fn(*const Handle, ErrorOut) -> u64;

impl Api {
    pub fn host_clock_api(&self) -> Result<(HostClockGetFn, HostClockSetFn), String> {
        unsafe {
            Ok((
                self._library
                    .symbol(b"ob_context_get_timestamp_clock_type\0")?,
                self._library
                    .symbol(b"ob_context_set_timestamp_clock_type\0")?,
            ))
        }
    }
    pub fn frame_sync_api(&self, enabled: bool) -> Result<PipelineSyncFn, String> {
        unsafe {
            self._library.symbol(if enabled {
                b"ob_pipeline_enable_frame_sync\0"
            } else {
                b"ob_pipeline_disable_frame_sync\0"
            })
        }
    }
    pub fn aggregate_api(&self) -> Result<AggregateFn, String> {
        unsafe {
            self._library
                .symbol(b"ob_config_set_frame_aggregate_output_mode\0")
        }
    }
    pub fn global_timestamp_api(&self) -> Result<GlobalTimestampFn, String> {
        unsafe { self._library.symbol(b"ob_frame_get_global_timestamp_us\0") }
    }
    pub fn global_device_api(&self) -> Result<(GlobalSupportedFn, GlobalEnableFn), String> {
        unsafe {
            Ok((
                self._library
                    .symbol(b"ob_device_is_global_timestamp_supported\0")?,
                self._library
                    .symbol(b"ob_device_enable_global_timestamp\0")?,
            ))
        }
    }
    pub fn calibration_api(&self) -> Result<CalibrationApi, String> {
        macro_rules! get {
            ($name:literal) => {
                unsafe { self._library.symbol(concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(CalibrationApi {
            frame_profile: get!("ob_frame_get_stream_profile"),
            pipeline_config: get!("ob_pipeline_get_config"),
            config_profiles: get!("ob_config_get_enabled_stream_profile_list"),
            delete_profiles: get!("ob_delete_stream_profile_list"),
            profile_count: get!("ob_stream_profile_list_get_count"),
            profile_get: get!("ob_stream_profile_list_get_profile"),
            delete_profile: get!("ob_delete_stream_profile"),
            profile_type: get!("ob_stream_profile_get_type"),
            profile_format: get!("ob_stream_profile_get_format"),
            width: get!("ob_video_stream_profile_get_width"),
            height: get!("ob_video_stream_profile_get_height"),
            fps: get!("ob_video_stream_profile_get_fps"),
            intrinsic: get!("ob_video_stream_profile_get_intrinsic"),
            distortion: get!("ob_video_stream_profile_get_distortion"),
            extrinsic_to: get!("ob_stream_profile_get_extrinsic_to"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibration_struct_layout_matches_packed_sdk_headers() {
        assert_eq!(std::mem::size_of::<CameraIntrinsic>(), 20);
        assert_eq!(std::mem::size_of::<CameraDistortion>(), 36);
        assert_eq!(std::mem::size_of::<Extrinsic>(), 48);
        assert_eq!(std::mem::align_of::<CameraIntrinsic>(), 1);
        assert_eq!(std::mem::align_of::<CameraDistortion>(), 1);
        assert_eq!(std::mem::align_of::<Extrinsic>(), 1);
    }
    #[test]
    fn rejects_relative_path() {
        assert!(
            Api::load("libOrbbecSDK.so")
                .err()
                .unwrap()
                .contains("absolute")
        );
    }
    #[test]
    fn missing_library_is_error() {
        assert!(Api::load("/does-not-exist/libOrbbecSDK.so").is_err());
    }
}
