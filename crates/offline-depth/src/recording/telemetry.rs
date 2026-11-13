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
/// Why the append thread reaps the single pending checkpoint. Labels only
/// describe existing control flow; they do not change its admission policy.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum WaitReason {
    FinishedReap,
    GlobalPayloadCredit,
    GlobalPairCredit,
    GlobalPayloadAndPairCredit,
    ChunkRotation,
    ExplicitCheckpoint,
}
impl WaitReason {
    pub(super) fn credits(payload: bool, pairs: bool) -> Option<Self> {
        match (payload, pairs) {
            (true, true) => Some(Self::GlobalPayloadAndPairCredit),
            (true, false) => Some(Self::GlobalPayloadCredit),
            (false, true) => Some(Self::GlobalPairCredit),
            (false, false) => None,
        }
    }
}
/// Per-writer admission credits, copied before joining its own pending task.
#[derive(Clone, Copy, Serialize)]
pub(super) struct CreditAtWait {
    pub pending_payload_bytes: u64,
    pub pending_pairs: u64,
    pub active_payload_bytes: u64,
    pub active_pairs: u64,
    pub payload_limit_bytes: u64,
    pub pair_limit: u64,
}
#[derive(Clone, Serialize)]
struct Span {
    span_id: u64,
    phase: Phase,
    info: CheckpointInfo,
    pid: u32,
    tid: Option<u64>,
    start_monotonic_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_reason: Option<WaitReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_at_wait: Option<CreditAtWait>,
}
#[derive(Clone, Serialize)]
struct AppendSpan {
    append_id: u64,
    pair_sequence: u64,
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
    next_append: u64,
    active_append: Option<AppendSpan>,
    append_limit_hits: u64,
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
pub(super) struct AppendGuard<'a> {
    observation: &'a Observation,
    append_id: Option<u64>,
}
impl Drop for AppendGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.append_id {
            let mut state = self
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if state
                .active_append
                .as_ref()
                .is_some_and(|span| span.append_id == id)
            {
                state.active_append = None;
            }
        }
    }
}
struct SpanGuard<'a> {
    observation: &'a Observation,
    span_id: Option<u64>,
}
impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.span_id {
            self.observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .retain(|span| span.span_id != id);
        }
    }
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
pub(super) fn cpu() -> Option<u64> {
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
                next_append: 0,
                active_append: None,
                append_limit_hits: 0,
                active: Vec::with_capacity(if enabled { ACTIVE } else { 0 }),
                events: VecDeque::with_capacity(if enabled { HISTORY } else { 0 }),
                stages: BTreeMap::new(),
            })),
        }
    }
    pub fn begin_append(&self, pair_sequence: u64) -> AppendGuard<'_> {
        let mut guard = AppendGuard {
            observation: self,
            append_id: None,
        };
        if self.enabled {
            let start_monotonic_ns = mono();
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.active_append.is_none() {
                state.next_append += 1;
                let append_id = state.next_append;
                state.active_append = Some(AppendSpan {
                    append_id,
                    pair_sequence,
                    pid: std::process::id(),
                    tid: tid(),
                    start_monotonic_ns,
                });
                guard.append_id = Some(append_id);
            } else {
                // Shared diagnostic handles cannot grow the active context
                // without bound or overwrite the first owner's identity.
                state.append_limit_hits += 1;
            }
        }
        guard
    }
    pub fn failure_context(&self) -> Value {
        let capture_started_monotonic_ns = mono();
        // Copy only bounded active context. History/histograms and the IO
        // diagnostics mutex are deliberately absent from this failure path.
        let (append, active, active_limit_hits, append_limit_hits) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.active_append.clone(),
                state.active.clone(),
                state.active_limit_hits,
                state.append_limit_hits,
            )
        };
        let capture_finished_monotonic_ns = mono();
        serde_json::json!({
            "version":1,"observation_enabled":self.enabled,"clock":"CLOCK_MONOTONIC",
            "unavailable_reason":(!self.enabled).then_some("checkpoint_observation_disabled"),
            "capture_started_monotonic_ns":capture_started_monotonic_ns,
            "capture_finished_monotonic_ns":capture_finished_monotonic_ns,
            "active_append":append,"active_checkpoint_spans":active,
            "active_checkpoint_capacity":ACTIVE,"active_append_capacity":1,
            "active_checkpoint_limit_hits":active_limit_hits,"active_append_limit_hits":append_limit_hits,
            "scope":"bounded active-writer snapshot; not atomic with caller queue/status observations; verify_wait includes the entire pending checkpoint task, not only readback or physical disk latency"
        })
    }
    pub fn measure<T, E>(
        &self,
        info: CheckpointInfo,
        phase: Phase,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.measure_context(info, phase, None, None, run)
    }
    #[cfg(test)]
    pub fn measure_wait<T, E>(
        &self,
        info: CheckpointInfo,
        reason: WaitReason,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.measure_context(info, Phase::VerifyWait, Some(reason), None, run)
    }
    pub fn measure_credit_wait<T, E>(
        &self,
        info: CheckpointInfo,
        reason: WaitReason,
        credit: CreditAtWait,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.measure_context(info, Phase::VerifyWait, Some(reason), Some(credit), run)
    }
    fn measure_context<T, E>(
        &self,
        info: CheckpointInfo,
        phase: Phase,
        wait_reason: Option<WaitReason>,
        credit_at_wait: Option<CreditAtWait>,
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
                wait_reason,
                credit_at_wait,
            };
            if s.active.len() < ACTIVE {
                s.active.push(span.clone());
            } else {
                s.active_limit_hits += 1;
            }
            span
        };
        // Active spans must disappear on unwinding too. A panic remains
        // distinct from a completed IO result in the existing counters.
        let mut guard = SpanGuard {
            observation: self,
            span_id: Some(span.span_id),
        };
        let result = run();
        let wall_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let thread_cpu_ns = start_cpu.zip(cpu()).and_then(|(a, b)| b.checked_sub(a));
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.active.retain(|v| v.span_id != span.span_id);
        guard.span_id = None;
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
        serde_json::json!({"version":1,"enabled":self.enabled,"clock":"CLOCK_MONOTONIC","anchor_monotonic_ns":self.anchor.and_then(|v|v.0),"anchor_unix_ns":self.anchor.and_then(|v|v.1),"scope":"completed spans; sync wall includes filesystem/scheduling wait, not physical device latency; overlapping stages must not be summed","history_capacity":HISTORY,"active_capacity":ACTIVE,"oldest_event_id":s.events.front().map(|e|e.event_id),"latest_event_id":s.next_event,"overwritten_events":s.overwritten,"active_limit_hits":s.active_limit_hits,"active_append":s.active_append,"active_append_limit_hits":s.append_limit_hits,"stages":stages,"active":s.active,"events":s.events})
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
    fn credit_wait_links_the_pending_task_without_changing_the_result() {
        for enabled in [true, false] {
            let d = Observation::new(enabled);
            let credit = CreditAtWait {
                pending_payload_bytes: 8,
                pending_pairs: 16,
                active_payload_bytes: 7,
                active_pairs: 16,
                payload_limit_bytes: 16,
                pair_limit: 32,
            };
            let result =
                d.measure_credit_wait(info(), WaitReason::GlobalPairCredit, credit, || {
                    let active = d.failure_context();
                    if enabled {
                        assert_eq!(
                            active["active_checkpoint_spans"][0]["credit_at_wait"]["pending_pairs"],
                            16
                        );
                        assert_eq!(
                            active["active_checkpoint_spans"][0]["info"]["checkpoint_id"],
                            1
                        );
                    }
                    Err::<(), _>("join-error")
                });
            assert_eq!(result, Err("join-error"));
            let report = d.report();
            assert!(report["active"].as_array().unwrap().is_empty());
            if enabled {
                assert_eq!(report["events"][0]["credit_at_wait"]["pair_limit"], 32);
                assert_eq!(report["events"][0]["ok"], false);
            }
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
    #[test]
    fn failure_context_contains_only_bounded_active_work_and_cleans_every_exit() {
        let d = Observation::new(true);
        let append = d.begin_append(17);
        let result = d.measure_wait(info(), WaitReason::ChunkRotation, || {
            let snapshot = d.failure_context();
            assert_eq!(snapshot["observation_enabled"], true);
            assert_eq!(snapshot["active_append"]["pair_sequence"], 17);
            assert_eq!(snapshot["active_append"]["pid"], std::process::id());
            assert_eq!(
                snapshot["active_checkpoint_spans"][0]["phase"],
                "verify_wait"
            );
            assert_eq!(
                snapshot["active_checkpoint_spans"][0]["wait_reason"],
                "chunk_rotation"
            );
            assert_eq!(
                snapshot["active_checkpoint_spans"][0]["info"]["checkpoint_id"],
                1
            );
            assert!(snapshot.get("events").is_none());
            assert!(snapshot.get("stages").is_none());
            #[cfg(unix)]
            {
                let start = snapshot["capture_started_monotonic_ns"].as_u64().unwrap();
                let end = snapshot["capture_finished_monotonic_ns"].as_u64().unwrap();
                assert!(start <= end);
                assert!(
                    snapshot["active_append"]["start_monotonic_ns"]
                        .as_u64()
                        .unwrap()
                        <= start
                );
            }
            Err::<(), _>("unchanged-error")
        });
        assert_eq!(result, Err("unchanged-error"));
        assert!(
            d.failure_context()["active_checkpoint_spans"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        drop(append);
        assert!(d.failure_context()["active_append"].is_null());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _append = d.begin_append(18);
            d.measure_wait(
                info(),
                WaitReason::ExplicitCheckpoint,
                || -> std::result::Result<(), ()> {
                    panic!("injected observer unwind");
                },
            )
        }));
        assert!(panic.is_err());
        let snapshot = d.failure_context();
        assert!(snapshot["active_append"].is_null());
        assert!(
            snapshot["active_checkpoint_spans"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // A panic is not a completed call and does not forge an IO result.
        assert_eq!(d.report()["latest_event_id"], 1);
        assert_eq!(d.report()["stages"]["verify_wait"]["errors"], 1);
    }
    #[test]
    fn disabled_and_overlapping_append_contexts_are_explicit_and_bounded() {
        let off = Observation::new(false);
        let _off_append = off.begin_append(1);
        off.measure_wait(info(), WaitReason::FinishedReap, || Ok::<_, ()>(()))
            .unwrap();
        let snapshot = off.failure_context();
        assert_eq!(snapshot["observation_enabled"], false);
        assert_eq!(
            snapshot["unavailable_reason"],
            "checkpoint_observation_disabled"
        );
        assert!(snapshot["active_append"].is_null());
        assert!(
            snapshot["active_checkpoint_spans"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let d = Observation::new(true);
        let first = d.begin_append(1);
        let other = d.begin_append(2);
        assert_eq!(d.failure_context()["active_append"]["pair_sequence"], 1);
        assert_eq!(d.failure_context()["active_append_limit_hits"], 1);
        drop(other);
        assert_eq!(d.failure_context()["active_append"]["pair_sequence"], 1);
        drop(first);
        let next = d.begin_append(3);
        assert_eq!(d.failure_context()["active_append"]["pair_sequence"], 3);
        drop(next);
        assert!(d.report()["active_append"].is_null());
    }
    #[test]
    fn each_wait_reason_is_preserved_and_credit_predicates_do_not_change() {
        assert_eq!(WaitReason::credits(false, false), None);
        assert_eq!(
            WaitReason::credits(true, false),
            Some(WaitReason::GlobalPayloadCredit)
        );
        assert_eq!(
            WaitReason::credits(false, true),
            Some(WaitReason::GlobalPairCredit)
        );
        assert_eq!(
            WaitReason::credits(true, true),
            Some(WaitReason::GlobalPayloadAndPairCredit)
        );
        let d = Observation::new(true);
        for (reason, label) in [
            (WaitReason::FinishedReap, "finished_reap"),
            (WaitReason::GlobalPayloadCredit, "global_payload_credit"),
            (WaitReason::GlobalPairCredit, "global_pair_credit"),
            (
                WaitReason::GlobalPayloadAndPairCredit,
                "global_payload_and_pair_credit",
            ),
            (WaitReason::ChunkRotation, "chunk_rotation"),
            (WaitReason::ExplicitCheckpoint, "explicit_checkpoint"),
        ] {
            d.measure_wait(info(), reason, || {
                assert_eq!(
                    d.failure_context()["active_checkpoint_spans"][0]["wait_reason"],
                    label
                );
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(
                d.report()["events"].as_array().unwrap().last().unwrap()["wait_reason"],
                label
            );
        }
        d.measure(info(), Phase::PayloadSync, || {
            assert!(
                d.failure_context()["active_checkpoint_spans"][0]
                    .get("wait_reason")
                    .is_none()
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn unavailable_clock_is_explicit() {
        assert_eq!(clock(!0), None);
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
                    let context = d.failure_context();
                    assert_eq!(
                        context["active_checkpoint_spans"].as_array().unwrap().len(),
                        ACTIVE
                    );
                    assert_eq!(context["active_checkpoint_limit_hits"], 1);
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
