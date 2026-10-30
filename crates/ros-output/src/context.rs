//! Explicit, thread-confined context lease. No process-wide or namespace cache.
use super::*;
pub(super) type ImageCreateShared =
    unsafe extern "C" fn(*mut c_void, *const ConfigRaw, *mut *mut c_void) -> i32;
pub(super) type ImuCreateShared = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut *mut c_void,
) -> i32;
pub(super) struct ContextLease {
    pub(super) library: Rc<Library>,
    pub(super) handle: NonNull<c_void>,
    pub(super) image_create: ImageCreateShared,
    pub(super) imu_create: ImuCreateShared,
    destroy: Destroy,
}
impl Drop for ContextLease {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.handle.as_ptr()) };
    }
}
/// Publishers retain their own lease, so dropping the caller cannot shut down peers.
#[derive(Clone)]
pub struct SharedRosContext(pub(super) Rc<ContextLease>);
impl SharedRosContext {
    /// Old bridges without this entire extension retain their isolated behavior.
    /// Partial or incompatible extensions fail closed rather than mixing handles.
    pub fn try_load(path: impl AsRef<Path>) -> Result<Option<Self>> {
        let library = Library::load(path.as_ref())?;
        let names: [&[u8]; 5] = [
            b"gdp_ros_context_abi_version\0",
            b"gdp_ros_context_create\0",
            b"gdp_ros_context_destroy\0",
            b"gdp_ros_create_in_context\0",
            b"gdp_ros_imu_create_in_context\0",
        ];
        let pointers: Vec<_> = names
            .iter()
            .map(|n| unsafe { libc::dlsym(library.handle.as_ptr(), n.as_ptr().cast()) })
            .collect();
        if pointers.iter().all(|p| p.is_null()) {
            return Ok(None);
        }
        ensure!(
            pointers.iter().all(|p| !p.is_null()),
            "partial ROS context extension"
        );
        let version: unsafe extern "C" fn() -> u32 = unsafe { mem::transmute(pointers[0]) };
        ensure!(unsafe { version() } == 1, "ROS context ABI differs");
        let create: unsafe extern "C" fn(*mut *mut c_void) -> i32 =
            unsafe { mem::transmute(pointers[1]) };
        let destroy: Destroy = unsafe { mem::transmute(pointers[2]) };
        let mut handle = ptr::null_mut();
        let code = unsafe { create(&mut handle) };
        if code != 0 {
            if !handle.is_null() {
                unsafe { destroy(handle) };
            }
            bail!("ROS context creation failed");
        }
        Ok(Some(Self(Rc::new(ContextLease {
            library,
            handle: NonNull::new(handle).context("null ROS context")?,
            destroy,
            image_create: unsafe { mem::transmute::<*mut c_void, ImageCreateShared>(pointers[3]) },
            imu_create: unsafe { mem::transmute::<*mut c_void, ImuCreateShared>(pointers[4]) },
        }))))
    }
}

pub(super) enum BridgeSource<'a> {
    Isolated(&'a Path),
    Shared(&'a SharedRosContext),
}
impl<'a> BridgeSource<'a> {
    pub(super) fn context(&self) -> Option<&'a SharedRosContext> {
        match self {
            Self::Isolated(_) => None,
            Self::Shared(c) => Some(c),
        }
    }
    pub(super) fn library(&self) -> Result<Rc<Library>> {
        match self {
            Self::Isolated(path) => Library::load(path),
            Self::Shared(c) => Ok(c.0.library.clone()),
        }
    }
}
