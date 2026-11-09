//! Bounded startup discovery and allowlisted controls; never called per frame.
use super::*;
use capture_contract::device::DeviceControls;
use serde::Serialize;
use sys::device::DeviceApi;

#[derive(Clone, Debug, Serialize)]
pub struct PropertyRange {
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
}
#[derive(Clone, Debug, Serialize)]
pub struct PropertyReport {
    pub name: &'static str,
    pub id: i32,
    pub kind: &'static str,
    pub readable: bool,
    pub writable: bool,
    pub range: Option<PropertyRange>,
    pub observed: Option<i32>,
    pub requested: Option<i32>,
    pub readback_verified: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub firmware: String,
    pub hardware: String,
    pub pid: i32,
    pub vid: i32,
}
#[derive(Clone, Debug, Serialize)]
pub struct DeviceReport {
    pub version: u32,
    pub available: bool,
    pub info: Option<DeviceInfo>,
    pub properties: Vec<PropertyReport>,
    pub error: Option<String>,
    pub scope: &'static str,
}
const SCOPE: &str = "six allowlisted RGB/IR exposure-gain properties, active profiles before first frame; observed values are startup snapshots, not continuous AE telemetry; no firmware update";
fn call<T>(a: &sys::Api, f: impl FnOnce(sys::ErrorOut) -> T) -> Result<T> {
    let mut e = ptr::null_mut();
    let value = f(&mut e);
    check(a, e)?;
    Ok(value)
}
fn info_string(
    a: &sys::Api,
    info: &Handle,
    f: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> *const std::ffi::c_char,
) -> Result<String> {
    let p = call(a, |e| unsafe { f(info.raw(), e) })?;
    if p.is_null() {
        return Err(Error("DEVICE_INFO_NULL".into()));
    }
    // Trusted SDK owns this string until info deletion. Bound traversal as well
    // as serialization; do not use unbounded CStr scanning on device text.
    for len in 0..=256 {
        if unsafe { *p.add(len) } == 0 {
            let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
            let text = std::str::from_utf8(bytes).map_err(|_| Error("DEVICE_INFO_UTF8".into()))?;
            if text.is_empty() || text.chars().any(char::is_control) {
                return Err(Error("DEVICE_INFO_TEXT".into()));
            }
            return Ok(text.into());
        }
    }
    Err(Error("DEVICE_INFO_TEXT_LIMIT".into()))
}
fn read(
    a: &sys::Api,
    d: &DeviceApi,
    device: *mut sys::Handle,
    id: i32,
    boolean: bool,
) -> Result<i32> {
    if boolean {
        call(a, |e| unsafe { (d.get_bool)(device, id, e) }).map(i32::from)
    } else {
        call(a, |e| unsafe { (d.get_int)(device, id, e) })
    }
}
fn write(
    a: &sys::Api,
    d: &DeviceApi,
    device: *mut sys::Handle,
    p: &PropertyReport,
    value: i32,
) -> Result<()> {
    if p.kind == "bool" {
        call(a, |e| unsafe { (d.set_bool)(device, p.id, value != 0, e) })
    } else {
        call(a, |e| unsafe { (d.set_int)(device, p.id, value, e) })
    }
}
fn in_range(value: i32, r: &PropertyRange) -> bool {
    value >= r.min
        && value <= r.max
        && (r.step == 0 || (i64::from(value) - i64::from(r.min)) % i64::from(r.step) == 0)
}
pub(super) fn configure(
    a: &Rc<sys::Api>,
    device: *mut sys::Handle,
    c: &DeviceControls,
) -> Result<DeviceReport> {
    c.validate().map_err(|e| Error(e.to_string()))?;
    let d = match a.device_api() {
        Ok(d) => d,
        Err(error) if !c.requested() => {
            return Ok(DeviceReport {
                version: 1,
                available: false,
                info: None,
                properties: vec![],
                error: Some(error),
                scope: SCOPE,
            });
        }
        Err(error) => return Err(Error(format!("DEVICE_CONTROL_API_UNAVAILABLE: {error}"))),
    };
    let info = Handle::new(a, d.delete_info, |e| unsafe { (d.info)(device, e) })?;
    let info = DeviceInfo {
        name: info_string(a, &info, d.name)?,
        firmware: info_string(a, &info, d.firmware)?,
        hardware: info_string(a, &info, d.hardware)?,
        pid: call(a, |e| unsafe { (d.pid)(info.raw(), e) })?,
        vid: call(a, |e| unsafe { (d.vid)(info.raw(), e) })?,
    };
    if !(0..=65535).contains(&info.pid) || !(0..=65535).contains(&info.vid) {
        return Err(Error("DEVICE_INFO_USB_ID".into()));
    }
    // Auto controls first. Preflight ALL requested operations before ANY write.
    let specs = [
        (
            "enable_color_auto_exposure",
            2000,
            true,
            c.enable_color_auto_exposure.map(i32::from),
        ),
        (
            "enable_ir_auto_exposure",
            2025,
            true,
            c.enable_ir_auto_exposure.map(i32::from),
        ),
        (
            "color_exposure",
            2001,
            false,
            c.color_exposure.filter(|v| *v >= 0),
        ),
        ("color_gain", 2002, false, c.color_gain.filter(|v| *v >= 0)),
        (
            "ir_exposure",
            2026,
            false,
            c.ir_exposure.filter(|v| *v >= 0),
        ),
        ("ir_gain", 2027, false, c.ir_gain.filter(|v| *v >= 0)),
    ];
    let mut properties = Vec::with_capacity(6);
    for (name, id, boolean, requested) in specs {
        let mut p = PropertyReport {
            name,
            id,
            kind: if boolean { "bool" } else { "int" },
            readable: false,
            writable: false,
            range: None,
            observed: None,
            requested,
            readback_verified: false,
            error: None,
        };
        let discovery = (|| -> Result<()> {
            p.readable = call(a, |e| unsafe { (d.supported)(device, id, 1, e) })?;
            p.writable = call(a, |e| unsafe { (d.supported)(device, id, 2, e) })?;
            if !p.readable {
                return Ok(());
            }
            let (range, current) = if boolean {
                let r = call(a, |e| unsafe { (d.bool_range)(device, id, e) })?;
                (
                    PropertyRange {
                        min: i32::from(r.min),
                        max: i32::from(r.max),
                        step: i32::from(r.step),
                        default: i32::from(r.def),
                    },
                    i32::from(r.cur),
                )
            } else {
                let r = call(a, |e| unsafe { (d.int_range)(device, id, e) })?;
                (
                    PropertyRange {
                        min: r.min,
                        max: r.max,
                        step: r.step,
                        default: r.def,
                    },
                    r.cur,
                )
            };
            if range.min > range.max
                || range.step < 0
                || !in_range(current, &range)
                || !in_range(range.default, &range)
            {
                return Err(Error("DEVICE_PROPERTY_RANGE_SCHEMA".into()));
            }
            let current = read(a, &d, device, id, boolean)?;
            if !in_range(current, &range) {
                return Err(Error("DEVICE_PROPERTY_VALUE_RANGE".into()));
            }
            p.range = Some(range);
            p.observed = Some(current);
            Ok(())
        })();
        if let Err(error) = discovery {
            p.error = Some(error.to_string())
        }
        if let Some(value) = requested {
            if !p.readable || !p.writable || p.error.is_some() {
                return Err(Error(format!("DEVICE_PROPERTY_UNSUPPORTED: {name}")));
            }
            if !p.range.as_ref().is_some_and(|r| in_range(value, r)) {
                return Err(Error(format!("DEVICE_PROPERTY_OUT_OF_RANGE: {name}")));
            }
        }
        properties.push(p);
    }
    let previous: Vec<_> = properties.iter().map(|p| p.observed).collect();
    let mut attempted = Vec::with_capacity(6);
    let applied = (|| -> Result<()> {
        for (i, p) in properties.iter().enumerate() {
            if let Some(value) = p.requested {
                attempted.push(i);
                write(a, &d, device, p, value)?;
                if read(a, &d, device, p.id, p.kind == "bool")? != value {
                    return Err(Error(format!(
                        "DEVICE_PROPERTY_READBACK_MISMATCH: {}",
                        p.name
                    )));
                }
            }
        }
        // Recheck earlier requests too: later writes must not silently undo them.
        for p in &mut properties {
            if let Some(value) = p.requested {
                let observed = read(a, &d, device, p.id, p.kind == "bool")?;
                if observed != value {
                    return Err(Error(format!(
                        "DEVICE_PROPERTY_READBACK_MISMATCH: {}",
                        p.name
                    )));
                }
                p.observed = Some(observed);
                p.readback_verified = true;
            }
        }
        Ok(())
    })();
    if let Err(error) = applied {
        let mut rollback_failed = false;
        for i in attempted.into_iter().rev() {
            let p = &properties[i];
            let old = previous[i].expect("preflight snapshot");
            if write(a, &d, device, p, old).is_err()
                || read(a, &d, device, p.id, p.kind == "bool").ok() != Some(old)
            {
                rollback_failed = true
            }
        }
        return Err(Error(format!(
            "{error}; rollback_verified={}",
            !rollback_failed
        )));
    }
    Ok(DeviceReport {
        version: 1,
        available: true,
        info: Some(info),
        properties,
        error: None,
        scope: SCOPE,
    })
}
