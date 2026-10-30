//! Bounded, runtime-only checkpoint observations. Never a durability decision.
use super::*;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};
const HISTORY: usize = 64;
const ACTIVE: usize = 4;
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    IndexFlush,
    PayloadSync,
    IndexSync,
    DirectorySync,
    Prepare,
    VerifyDispatch,
    VerifyWait,
    Readback,
    CheckpointWrite,
    CheckpointRename,
    CheckpointDirectorySync,
    FinalReadback,
}
#[derive(Clone, Copy, Serialize)]
pub(super) struct CheckpointInfo {
    pub checkpoint_id: u64,
    pub prefix_pairs: u64,
    pub payload_bytes: Option<u64>,
    pub asynchronous: bool,
}
#[derive(Clone, Serialize)]
struct Span {
    span_id: u64,
    phase: Phase,
    info: CheckpointInfo,
    pid: u32,
    tid: Option<u64>,
    start_monotonic_ns: Option<u64>,
}
#[derive(Clone, Serialize)]
struct Event {
    event_id: u64,
    #[serde(flatten)]
    span: Span,
    wall_ns: u64,
    thread_cpu_ns: Option<u64>,
    ok: bool,
}
#[derive(Clone, Default)]
struct Times {
    wall: StageStats,
    cpu: StageStats,
    errors: u64,
}
#[derive(Clone)]
struct State {
    next_span: u64,
    next_event: u64,
    overwritten: u64,
    active_limit_hits: u64,
    active: Vec<Span>,
    events: VecDeque<Event>,
    stages: BTreeMap<Phase, Times>,
}
#[derive(Clone)]
pub(super) struct Observation {
    enabled: bool,
    anchor: Option<(Option<u64>, Option<u64>)>,
    state: Arc<Mutex<State>>,
}
#[cfg(unix)]
fn clock(id: libc::clockid_t) -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
        return None;
    }
    u64::try_from(ts.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(ts.tv_nsec).ok()?)
}
fn mono() -> Option<u64> {
    #[cfg(unix)]
    {
        clock(libc::CLOCK_MONOTONIC)
    }
    #[cfg(not(unix))]
    {
        None
    }
}
fn cpu() -> Option<u64> {
    #[cfg(unix)]
    {
        clock(libc::CLOCK_THREAD_CPUTIME_ID)
    }
    #[cfg(not(unix))]
    {
        None
    }
}
fn tid() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        u64::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
impl Observation {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            anchor: enabled.then(|| {
                (
                    mono(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .and_then(|d| u64::try_from(d.as_nanos()).ok()),
                )
            }),
            state: Arc::new(Mutex::new(State {
                next_span: 0,
                next_event: 0,
                overwritten: 0,
                active_limit_hits: 0,
                active: Vec::with_capacity(if enabled { ACTIVE } else { 0 }),
                events: VecDeque::with_capacity(if enabled { HISTORY } else { 0 }),
                stages: BTreeMap::new(),
            })),
        }
    }
    pub fn measure<T, E>(
        &self,
        info: CheckpointInfo,
        phase: Phase,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if !self.enabled {
            return run();
        }
        let start_monotonic_ns = mono();
        let start_cpu = cpu();
        let started = Instant::now();
        let span = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.next_span += 1;
            let span = Span {
                span_id: s.next_span,
                phase,
                info,
                pid: std::process::id(),
                tid: tid(),
                start_monotonic_ns,
            };
            if s.active.len() < ACTIVE {
                s.active.push(span.clone());
            } else {
                s.active_limit_hits += 1;
            }
            span
        };
        let result = run();
        let wall_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let thread_cpu_ns = start_cpu.zip(cpu()).and_then(|(a, b)| b.checked_sub(a));
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.active.retain(|v| v.span_id != span.span_id);
        let times = s.stages.entry(phase).or_default();
        times.wall.observe_ns(wall_ns);
        if let Some(n) = thread_cpu_ns {
            times.cpu.observe_ns(n);
        }
        if result.is_err() {
            times.errors += 1;
        }
        s.next_event += 1;
        let event_id = s.next_event;
        if s.events.len() == HISTORY {
            s.events.pop_front();
            s.overwritten += 1;
        }
        s.events.push_back(Event {
            event_id,
            span,
            wall_ns,
            thread_cpu_ns,
            ok: result.is_ok(),
        });
        result
    }
    pub fn report(&self) -> Value {
        // Bound the copy, then serialize outside the writer's observation lock.
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let stages=s.stages.iter().map(|(p,t)|(serde_json::to_value(p).unwrap().as_str().unwrap().to_owned(),serde_json::json!({"wall":t.wall.report(),"thread_cpu":t.cpu.report(),"errors":t.errors}))).collect::<BTreeMap<_,_>>();
        serde_json::json!({"version":1,"enabled":self.enabled,"clock":"CLOCK_MONOTONIC","anchor_monotonic_ns":self.anchor.and_then(|v|v.0),"anchor_unix_ns":self.anchor.and_then(|v|v.1),"scope":"completed spans; sync wall includes filesystem/scheduling wait, not physical device latency; overlapping stages must not be summed","history_capacity":HISTORY,"active_capacity":ACTIVE,"oldest_event_id":s.events.front().map(|e|e.event_id),"latest_event_id":s.next_event,"overwritten_events":s.overwritten,"active_limit_hits":s.active_limit_hits,"stages":stages,"active":s.active,"events":s.events})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn info() -> CheckpointInfo {
        CheckpointInfo {
            checkpoint_id: 1,
            prefix_pairs: 32,
            payload_bytes: Some(128),
            asynchronous: false,
        }
    }
    #[test]
    fn bounded_history_active_and_errors_never_change_results() {
        let d = Observation::new(true);
        let failed = d.measure(info(), Phase::PayloadSync, || {
            assert_eq!(d.report()["active"].as_array().unwrap().len(), 1);
            Err::<(), _>("disk-failure")
        });
        assert_eq!(failed, Err("disk-failure"));
        for _ in 0..70 {
            assert_eq!(d.measure(info(), Phase::Prepare, || Ok::<_, ()>(7)), Ok(7));
        }
        let r = d.report();
        assert_eq!(r["events"].as_array().unwrap().len(), HISTORY);
        assert_eq!(r["overwritten_events"], 7);
        assert_eq!(r["latest_event_id"], 71);
        assert_eq!(r["stages"]["payload_sync"]["errors"], 1);
        assert!(r["active"].as_array().unwrap().is_empty());
        let off = Observation::new(false);
        assert_eq!(
            off.measure(info(), Phase::Prepare, || Err::<(), _>(9)),
            Err(9)
        );
        assert!(off.report()["events"].as_array().unwrap().is_empty());
        assert_eq!(off.report()["enabled"], false);
    }
    #[cfg(unix)]
    #[test]
    fn unavailable_clock_is_explicit() {
        assert_eq!(clock(-1), None);
    }
    #[test]
    fn concurrent_completion_ids_preserve_late_events_and_report_during_wait() {
        let d = Observation::new(true);
        let other = d.clone();
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            other.measure(info(), Phase::Readback, || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok::<_, ()>(())
            })
        });
        ready.recv().unwrap();
        assert_eq!(d.report()["active"][0]["phase"], "readback");
        d.measure(info(), Phase::IndexFlush, || Ok::<_, ()>(()))
            .unwrap();
        let first = d.report();
        assert_eq!(first["events"][0]["event_id"], 1);
        assert_eq!(first["events"][0]["span_id"], 2);
        release.send(()).unwrap();
        task.join().unwrap().unwrap();
        let last = d.report();
        assert_eq!(last["events"][1]["event_id"], 2);
        assert_eq!(last["events"][1]["span_id"], 1);
        assert!(last["active"].as_array().unwrap().is_empty());
    }
    #[test]
    fn active_overflow_is_explicit_and_wait_wall_is_distinct_from_cpu() {
        fn nested(d: &Observation, n: usize) {
            d.measure(info(), Phase::VerifyWait, || {
                if n > 0 {
                    nested(d, n - 1);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        let d = Observation::new(true);
        nested(&d, 4);
        let r = d.report();
        assert_eq!(r["active_limit_hits"], 1);
        assert!(
            r["stages"]["verify_wait"]["wall"]["minimum_ns"]
                .as_u64()
                .unwrap()
                >= 9_000_000
        );
        assert!(r["active"].as_array().unwrap().is_empty());
    }
}
