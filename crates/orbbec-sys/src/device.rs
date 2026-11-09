//! Optional startup device/property ABI from SDK v2.10.6 Device.h/ObTypes.h.
use super::*;
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct IntRange {
    pub cur: i32,
    pub max: i32,
    pub min: i32,
    pub step: i32,
    pub def: i32,
}
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct BoolRange {
    pub cur: bool,
    pub max: bool,
    pub min: bool,
    pub step: bool,
    pub def: bool,
}
pub struct DeviceApi {
    pub info: unsafe extern "C" fn(*const Handle, ErrorOut) -> *mut Handle,
    pub delete_info: unsafe extern "C" fn(*mut Handle, ErrorOut),
    pub name: unsafe extern "C" fn(*const Handle, ErrorOut) -> *const c_char,
    pub firmware: unsafe extern "C" fn(*const Handle, ErrorOut) -> *const c_char,
    pub hardware: unsafe extern "C" fn(*const Handle, ErrorOut) -> *const c_char,
    pub pid: unsafe extern "C" fn(*const Handle, ErrorOut) -> c_int,
    pub vid: unsafe extern "C" fn(*const Handle, ErrorOut) -> c_int,
    pub supported: unsafe extern "C" fn(*const Handle, c_int, c_int, ErrorOut) -> bool,
    pub int_range: unsafe extern "C" fn(*const Handle, c_int, ErrorOut) -> IntRange,
    pub bool_range: unsafe extern "C" fn(*const Handle, c_int, ErrorOut) -> BoolRange,
    pub get_int: unsafe extern "C" fn(*const Handle, c_int, ErrorOut) -> i32,
    pub get_bool: unsafe extern "C" fn(*const Handle, c_int, ErrorOut) -> bool,
    pub set_int: unsafe extern "C" fn(*mut Handle, c_int, i32, ErrorOut),
    pub set_bool: unsafe extern "C" fn(*mut Handle, c_int, bool, ErrorOut),
}
impl Api {
    pub fn device_api(&self) -> Result<DeviceApi, String> {
        macro_rules! get {
            ($s:literal) => {
                unsafe { self._library.symbol(concat!($s, "\0").as_bytes())? }
            };
        }
        Ok(DeviceApi {
            info: get!("ob_device_get_device_info"),
            delete_info: get!("ob_delete_device_info"),
            name: get!("ob_device_info_get_name"),
            firmware: get!("ob_device_info_get_firmware_version"),
            hardware: get!("ob_device_info_get_hardware_version"),
            pid: get!("ob_device_info_get_pid"),
            vid: get!("ob_device_info_get_vid"),
            supported: get!("ob_device_is_property_supported"),
            int_range: get!("ob_device_get_int_property_range"),
            bool_range: get!("ob_device_get_bool_property_range"),
            get_int: get!("ob_device_get_int_property"),
            get_bool: get!("ob_device_get_bool_property"),
            set_int: get!("ob_device_set_int_property"),
            set_bool: get!("ob_device_set_bool_property"),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_property_range_abi() {
        assert_eq!(std::mem::size_of::<IntRange>(), 20);
        assert_eq!(std::mem::align_of::<IntRange>(), 1);
        assert_eq!(std::mem::size_of::<BoolRange>(), 5);
        assert_eq!(std::mem::align_of::<BoolRange>(), 1);
    }
}
