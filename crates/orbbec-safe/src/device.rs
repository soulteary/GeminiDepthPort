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
    pub range_current: Option<i32>,
    pub observed_before: Option<i32>,
    pub getter_attempted: bool,
    pub getter_returned: Option<i32>,
    pub getter_error: Option<String>,
    pub getter_error_truncated: bool,
    pub getter_started_monotonic_ns: Option<u64>,
    pub getter_finished_monotonic_ns: Option<u64>,
    pub range_uvc: UvcObservation,
    pub getter_uvc: UvcObservation,
    pub readable_queried: bool,
    pub writable_queried: bool,
    pub stage: &'static str,
    pub started_monotonic_ns: Option<u64>,
    pub finished_monotonic_ns: Option<u64>,
    pub failure_monotonic_ns: Option<u64>,
    pub error_truncated: bool,
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
fn within_bounds(value: i32, range: &PropertyRange) -> bool {
    (range.min..=range.max).contains(&value)
}
fn monotonic_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    u64::try_from(ts.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(ts.tv_nsec).ok()?)
}
fn classified<T>(code: &str, result: Result<T>) -> Result<T> {
    result.map_err(|error| Error(format!("{code}: {error}")))
}
fn remember_error(p: &mut PropertyReport, error: &Error) {
    p.error_truncated = error.0.chars().count() > 256;
    p.error = Some(error.0.chars().take(256).collect());
}
fn failure(
    code: &str,
    p: &PropertyReport,
    properties: &[PropertyReport],
    color: Option<VideoProfile>,
    depth: Option<VideoProfile>,
    rollback: Option<bool>,
) -> Error {
    let ae = |name| {
        properties
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.observed_before)
    };
    // Only one property, two AE observations and fixed-size profiles. SDK text
    // is bounded above; no serial, frame payload or caller-controlled path.
    Error(format!(
        "{code}: {}",
        serde_json::json!({"version":1,"kind":"device-property-failure","property":p,
        "auto_exposure_before":{"color":ae("enable_color_auto_exposure"),"ir":ae("enable_ir_auto_exposure")},
        "auto_exposure_observed_monotonic_ns":{
            "color":properties.iter().find(|p|p.name=="enable_color_auto_exposure").and_then(|p|p.finished_monotonic_ns),
            "ir":properties.iter().find(|p|p.name=="enable_ir_auto_exposure").and_then(|p|p.finished_monotonic_ns)},
        "auto_exposure_scope":"earlier preflight reads, not an atomic SDK snapshot",
        "profiles":{"color":color,"depth":depth},"rollback_verified":rollback})
    ))
}
pub(super) fn configure(
    a: &Rc<sys::Api>,
    device: *mut sys::Handle,
    c: &DeviceControls,
    color: Option<VideoProfile>,
    depth: Option<VideoProfile>,
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
    let uvc = a.uvc_observation_api();
    let mut properties = Vec::with_capacity(6);
    for (name, id, boolean, requested) in specs {
        let mut p = PropertyReport {
            name,
            id,
            kind: if boolean { "bool" } else { "int" },
            readable: false,
            writable: false,
            range: None,
            range_current: None,
            observed_before: None,
            getter_attempted: false,
            getter_returned: None,
            getter_error: None,
            getter_error_truncated: false,
            getter_started_monotonic_ns: None,
            getter_finished_monotonic_ns: None,
            range_uvc: Default::default(),
            getter_uvc: Default::default(),
            readable_queried: false,
            writable_queried: false,
            stage: "read_support",
            started_monotonic_ns: monotonic_ns(),
            finished_monotonic_ns: None,
            failure_monotonic_ns: None,
            error_truncated: false,
            observed: None,
            requested,
            readback_verified: false,
            error: None,
        };
        let discovery = (|| -> Result<()> {
            p.readable = classified(
                "DEVICE_PROPERTY_SUPPORT_QUERY",
                call(a, |e| unsafe { (d.supported)(device, id, 1, e) }),
            )?;
            p.readable_queried = true;
            p.stage = "write_support";
            let write_support = classified(
                "DEVICE_PROPERTY_SUPPORT_QUERY",
                call(a, |e| unsafe { (d.supported)(device, id, 2, e) }),
            );
            p.writable_queried = write_support.is_ok();
            p.writable = *write_support.as_ref().unwrap_or(&false);
            if !p.readable {
                write_support?;
                return Ok(());
            }
            p.stage = "range_query";
            let (range_result, range_trace) = crate::uvc_observation::observe(uvc.as_ref(), || {
                let value = if boolean {
                    let r = classified(
                        "DEVICE_PROPERTY_RANGE_QUERY",
                        call(a, |e| unsafe { (d.bool_range)(device, id, e) }),
                    )?;
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
                    let r = classified(
                        "DEVICE_PROPERTY_RANGE_QUERY",
                        call(a, |e| unsafe { (d.int_range)(device, id, e) }),
                    )?;
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
                Ok(value)
            });
            p.range_uvc = range_trace;
            if let Ok((range, current)) = range_result.as_ref() {
                p.range_current = Some(*current);
                p.range = Some(range.clone());
            }
            // Always attempt the independent read when READ permission is known.
            // Its evidence cannot erase a range/query/schema anomaly or authorize
            // a write; keep the primary failure and any secondary read failure.
            p.getter_attempted = true;
            p.getter_started_monotonic_ns = monotonic_ns();
            let (getter_result, getter_trace) =
                crate::uvc_observation::observe(uvc.as_ref(), || {
                    classified(
                        "DEVICE_PROPERTY_VALUE_READ",
                        read(a, &d, device, id, boolean),
                    )
                });
            p.getter_finished_monotonic_ns = monotonic_ns();
            p.getter_uvc = getter_trace;
            if let Ok(current) = getter_result.as_ref() {
                p.getter_returned = Some(*current);
            }
            let getter_result = getter_result.and_then(|current| {
                if p.getter_uvc.invalid || p.getter_uvc.truncated {
                    return Err(Error("DEVICE_PROPERTY_UVC_TRACE_INVALID".into()));
                }
                if p.getter_uvc.failed() {
                    return Err(Error("DEVICE_PROPERTY_UVC_READ_FAILED".into()));
                }
                Ok(current)
            });
            match &getter_result {
                Ok(current) => {
                    p.observed_before = Some(*current);
                    p.observed = Some(*current);
                }
                Err(error) => {
                    p.getter_error_truncated = error.0.chars().count() > 256;
                    p.getter_error = Some(error.0.chars().take(256).collect());
                }
            }
            p.stage = "write_support";
            write_support?;
            p.stage = "range_query";
            let (range, current) = range_result?;
            p.stage = "range_uvc";
            if p.range_uvc.invalid || p.range_uvc.truncated {
                return Err(Error("DEVICE_PROPERTY_UVC_TRACE_INVALID".into()));
            }
            if p.range_uvc.failed() {
                return Err(Error("DEVICE_PROPERTY_UVC_READ_FAILED".into()));
            }
            p.stage = "range_schema";
            if range.min > range.max || range.step < 0 || !within_bounds(range.default, &range) {
                return Err(Error("DEVICE_PROPERTY_RANGE_SCHEMA".into()));
            }
            // cur is an observation, not a manual write request. AE can return
            // intermediate/non-grid values; range and get are separate SDK calls.
            p.stage = "range_current";
            if !within_bounds(current, &range) {
                return Err(Error("DEVICE_PROPERTY_VALUE_RANGE".into()));
            }
            p.stage = "value_read";
            let current = getter_result?;
            p.stage = "value_bounds";
            if !within_bounds(current, &range) {
                return Err(Error("DEVICE_PROPERTY_VALUE_RANGE".into()));
            }
            p.stage = "preflight_complete";
            Ok(())
        })();
        p.finished_monotonic_ns = monotonic_ns();
        if let Err(error) = discovery {
            p.failure_monotonic_ns = monotonic_ns();
            p.finished_monotonic_ns = p.failure_monotonic_ns;
            remember_error(&mut p, &error);
            if requested.is_some() {
                let code = error
                    .0
                    .split(':')
                    .next()
                    .unwrap_or("DEVICE_PROPERTY_DISCOVERY");
                return Err(failure(code, &p, &properties, color, depth, None));
            }
        }
        if let Some(value) = requested {
            if !p.readable || !p.writable {
                p.failure_monotonic_ns = monotonic_ns();
                p.finished_monotonic_ns = p.failure_monotonic_ns;
                return Err(failure(
                    "DEVICE_PROPERTY_UNSUPPORTED",
                    &p,
                    &properties,
                    color,
                    depth,
                    None,
                ));
            }
            p.stage = "requested_range";
            if !p.range.as_ref().is_some_and(|r| in_range(value, r)) {
                p.failure_monotonic_ns = monotonic_ns();
                p.finished_monotonic_ns = p.failure_monotonic_ns;
                return Err(failure(
                    "DEVICE_PROPERTY_OUT_OF_RANGE",
                    &p,
                    &properties,
                    color,
                    depth,
                    None,
                ));
            }
        }
        properties.push(p);
    }
    let previous: Vec<_> = properties.iter().map(|p| p.observed).collect();
    let mut attempted = Vec::with_capacity(6);
    let mut failed_index = 0;
    let applied = (|| -> Result<()> {
        for (i, p) in properties.iter_mut().enumerate() {
            if let Some(value) = p.requested {
                failed_index = i;
                attempted.push(i);
                p.stage = "write";
                classified("DEVICE_PROPERTY_WRITE", write(a, &d, device, p, value))?;
                p.stage = "write_readback";
                let observed = classified(
                    "DEVICE_PROPERTY_VALUE_READ",
                    read(a, &d, device, p.id, p.kind == "bool"),
                )?;
                p.observed = Some(observed);
                if observed != value {
                    return Err(Error(format!(
                        "DEVICE_PROPERTY_READBACK_MISMATCH: {}",
                        p.name
                    )));
                }
            }
        }
        // Recheck earlier requests too: later writes must not silently undo them.
        for (i, p) in properties.iter_mut().enumerate() {
            if let Some(value) = p.requested {
                failed_index = i;
                p.stage = "final_readback";
                let observed = classified(
                    "DEVICE_PROPERTY_VALUE_READ",
                    read(a, &d, device, p.id, p.kind == "bool"),
                )?;
                p.observed = Some(observed);
                if observed != value {
                    return Err(Error(format!(
                        "DEVICE_PROPERTY_READBACK_MISMATCH: {}",
                        p.name
                    )));
                }
                p.observed = Some(observed);
                p.readback_verified = true;
                p.stage = "complete";
                p.finished_monotonic_ns = monotonic_ns();
            }
        }
        Ok(())
    })();
    if let Err(error) = applied {
        properties[failed_index].failure_monotonic_ns = monotonic_ns();
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
        let p = &mut properties[failed_index];
        remember_error(p, &error);
        p.finished_monotonic_ns = monotonic_ns();
        let code = error.0.split(':').next().unwrap_or("DEVICE_PROPERTY_APPLY");
        let detail = failure(
            code,
            &properties[failed_index],
            &properties,
            color,
            depth,
            Some(!rollback_failed),
        );
        return Err(Error(format!(
            "{detail}; rollback_verified={}",
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
