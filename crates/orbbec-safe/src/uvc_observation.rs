//! Bounded optional observations from the caller thread, never inferred by model.
use super::*;
use serde::Serialize;
use sys::device::{UvcEvent, UvcObservationApi};
#[derive(Clone, Debug, Serialize)]
pub struct UvcReadEvent {
    pub backend: u32,
    pub operation: u32,
    pub selector: u32,
    pub request: u32,
    pub result: i32,
    pub error_number: i32,
    pub value: i32,
    pub value_valid: u32,
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
    pub started_ns: u64,
    pub finished_ns: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct UvcObservation {
    pub available: bool,
    pub backend: Option<&'static str>,
    pub truncated: bool,
    pub invalid: bool,
    pub events: Vec<UvcReadEvent>,
}
impl UvcObservation {
    pub fn failed(&self) -> bool {
        self.events
            .iter()
            .any(|e| e.result < 0 || e.value_valid == 0)
    }
}
pub(crate) fn observe<T>(
    api: Option<&UvcObservationApi>,
    read: impl FnOnce() -> Result<T>,
) -> (Result<T>, UvcObservation) {
    if let Some(api) = api {
        unsafe { (api.begin)() };
    }
    let result = read();
    let Some(api) = api else {
        return (result, UvcObservation::default());
    };
    let mut raw = [UvcEvent::default(); 8];
    let count = unsafe { (api.snapshot)(raw.as_mut_ptr(), raw.len() as u32) };
    let mut trace = UvcObservation {
        available: true,
        truncated: count > 8,
        ..Default::default()
    };
    for e in raw.into_iter().take((count as usize).min(8)) {
        let backend = match e.backend {
            1 => "v4l2",
            2 => "libuvc",
            _ => {
                trace.invalid = true;
                "unknown"
            }
        };
        trace.backend = Some(match trace.backend {
            None => backend,
            Some(old) if old == backend => old,
            _ => "mixed",
        });
        trace.invalid |= !(1..=5).contains(&e.operation)
            || e.value_valid > 1
            || (e.started_ns != 0 && e.finished_ns < e.started_ns);
        trace.events.push(UvcReadEvent {
            backend: e.backend,
            operation: e.operation,
            selector: e.selector,
            request: e.request,
            result: e.result,
            error_number: e.error_number,
            value: e.value,
            value_valid: e.value_valid,
            minimum: e.minimum,
            maximum: e.maximum,
            step: e.step,
            default_value: e.default_value,
            started_ns: e.started_ns,
            finished_ns: e.finished_ns,
        });
    }
    (result, trace)
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn begin() {}
    unsafe extern "C" fn snapshot(out: *mut UvcEvent, _: u32) -> u32 {
        for (index, backend) in [1, 2, 99].into_iter().enumerate() {
            unsafe {
                out.add(index).write(UvcEvent {
                    backend,
                    operation: if index == 2 { 99 } else { 1 },
                    value_valid: if index == 2 { 2 } else { 1 },
                    started_ns: 100,
                    finished_ns: 50,
                    ..Default::default()
                })
            };
        }
        3
    }
    #[test]
    fn optional_trace_preserves_read_errors_and_rejects_invalid_events() {
        let (value, absent) = observe(None, || Ok(42));
        assert_eq!(value.unwrap(), 42);
        assert!(!absent.available);
        assert!(absent.backend.is_none());
        let api = UvcObservationApi { begin, snapshot };
        let (value, trace) = observe(Some(&api), || Err::<(), _>(Error("original".into())));
        assert_eq!(value.unwrap_err().0, "original");
        assert_eq!(trace.backend, Some("mixed"));
        assert!(trace.invalid);
        assert_eq!(trace.events.len(), 3);
        assert!(!trace.truncated);
    }
}
