use crate::{ClockMapper, RollingWindow, SourceStamp, validate_source_age};
use capture_contract::{FrameContext, MonoTime, QualityAction, QualityPolicy, SessionId, token};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const QUALITY_VERSION: u32 = 1;
pub type QualityResult<T> = Result<T, &'static str>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FrameViolation {
    StreamStale,
    TimestampRegression,
    PairSkewExceeded,
    FrameContractInvalid,
    QueueOverflow,
    DeviceDisconnected,
    SessionChanged,
    PublishPartialFailure,
    TimeMappingInvalid,
    /// Explicit observer lifecycle fact, not a configurable frame rule.
    ObserverInterrupted,
}
impl FrameViolation {
    // Existing configurable rules stay compatible; lifecycle commands have no
    // synthetic threshold/recovery rule.
    pub const ALL: [Self; 9] = [
        Self::StreamStale,
        Self::TimestampRegression,
        Self::PairSkewExceeded,
        Self::FrameContractInvalid,
        Self::QueueOverflow,
        Self::DeviceDisconnected,
        Self::SessionChanged,
        Self::PublishPartialFailure,
        Self::TimeMappingInvalid,
    ];
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QualityState {
    Normal,
    Degraded,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Condition {
    Violated,
    Healthy,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "stream_id",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum Scope {
    Group,
    Recorder,
    Stream(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulePolicy {
    pub open_after: u32,
    pub recover_after: u32,
    pub severity: QualityState,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeQualityPolicy {
    pub version: u32,
    pub quality: QualityPolicy,
    pub stale_after_ns: u64,
    pub event_capacity: usize,
    pub window_capacity: usize,
    pub window_horizon_ns: u64,
    pub summary_capacity: usize,
    /// Total distinct sessions admitted by this engine. Exhaustion is explicit;
    /// old IDs are never forgotten and accidentally accepted as a fresh epoch.
    pub max_sessions: usize,
    pub rules: BTreeMap<FrameViolation, RulePolicy>,
}
impl RuntimeQualityPolicy {
    pub fn validate(&self) -> QualityResult<()> {
        self.quality
            .validate()
            .map_err(|_| "QUALITY_POLICY_INVALID")?;
        if self.version != QUALITY_VERSION
            || self.stale_after_ns == 0
            || self.window_horizon_ns == 0
            || !(1..=100_000).contains(&self.event_capacity)
            || !(1..=100_000).contains(&self.window_capacity)
            || !(1..=4096).contains(&self.max_sessions)
            || self.summary_capacity == 0
            || self.summary_capacity > self.max_sessions
            || self.rules.len() != FrameViolation::ALL.len()
            || FrameViolation::ALL
                .iter()
                .any(|code| !self.rules.contains_key(code))
            || self.rules.values().any(|rule| {
                rule.open_after == 0
                    || rule.recover_after == 0
                    || rule.open_after > 1_000_000
                    || rule.recover_after > 1_000_000
                    || rule.severity == QualityState::Normal
            })
        {
            return Err("RUNTIME_QUALITY_POLICY_INVALID");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameObservation {
    pub context: FrameContext,
    pub contract_valid: bool,
    /// None is unobserved, never an invented pixel metric.
    pub valid_fraction: Option<f64>,
    pub zero_fraction: Option<f64>,
    pub source: Option<SourceStamp>,
    pub mapper: ClockMapper,
    pub received_unix_ns: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventTransition {
    Opened,
    Recovered,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InterruptionReason {
    DriverExited,
    ObserverInterrupted,
    CoreUnavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption_reason: Option<InterruptionReason>,
    pub version: u32,
    pub sequence: u64,
    /// The opening event sequence identifies this episode within this feed.
    pub episode_sequence: u64,
    pub violation: FrameViolation,
    pub scope: Scope,
    pub transition: EventTransition,
    pub severity: QualityState,
    pub at: MonoTime,
    /// Remains in its original session domain across later session changes.
    pub origin: MonoTime,
    pub bad_observations: u64,
    pub recovery_observations: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveEpisode {
    pub episode_sequence: u64,
    pub violation: FrameViolation,
    pub scope: Scope,
    pub severity: QualityState,
    pub origin: MonoTime,
    pub last_observed: MonoTime,
    pub bad_observations: u64,
    pub recovery_observations: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub origin_session_id: SessionId,
    pub after_sequence: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventGap {
    pub code: String,
    pub requested_after: u64,
    pub first_retained_sequence: u64,
    pub last_lost_sequence: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    pub version: u32,
    pub origin_session_id: SessionId,
    pub latest_sequence: u64,
    pub gap: Option<EventGap>,
    pub events: Vec<QualityEvent>,
    pub next_cursor: EventCursor,
    pub has_more: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    #[serde(default)]
    pub interruption_reason: Option<InterruptionReason>,
    pub version: u32,
    pub session_id: SessionId,
    pub finalized: bool,
    pub last_mono_offset_ns: u64,
    pub frame_counts: BTreeMap<String, u64>,
    pub open_episode_sequences: Vec<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamQuality {
    #[serde(default)]
    pub window_metrics: crate::StreamWindowMetrics,
    pub frame_count: u64,
    pub recent_frames: usize,
    pub recent_checks_passed_fraction: Option<f64>,
    pub last_receive_mono_offset_ns: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySnapshot {
    #[serde(default)]
    pub interruption_reason: Option<InterruptionReason>,
    #[serde(default)]
    pub pair_delta_us: Option<crate::ScalarSummary>,
    #[serde(default)]
    pub diagnostics: crate::DiagnosticSnapshot,
    pub version: u32,
    pub origin_session_id: SessionId,
    pub session_id: SessionId,
    pub at: MonoTime,
    pub state: QualityState,
    pub action: QualityAction,
    pub continue_allowed: bool,
    pub active_episodes: Vec<ActiveEpisode>,
    pub streams: BTreeMap<String, StreamQuality>,
    pub finalized_sessions: Vec<SessionSummary>,
    pub summaries_evicted: u64,
    pub latest_event_sequence: u64,
    pub current_session_finalized: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum QualityQuery {
    Snapshot,
    Events {
        cursor: Option<EventCursor>,
        limit: usize,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityQueryRequest {
    pub version: u32,
    pub query: QualityQuery,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum QualityQueryResult {
    Snapshot { snapshot: QualitySnapshot },
    Events { page: EventPage },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityQueryResponse {
    pub version: u32,
    pub result: QualityQueryResult,
}
#[derive(Default)]
struct Streak {
    bad: u32,
    good: u32,
}
struct StreamState {
    window: RollingWindow<bool>,
    metrics: RollingWindow<crate::metrics::MetricSample>,
    latest_context: Option<FrameContext>,
    frames: u64,
    last_received: Option<u64>,
    source_last: BTreeMap<u8, u64>,
    source_clock: Option<crate::SourceClock>,
    mapper_binding: Option<ClockMapper>,
    binding: Option<(
        capture_contract::GenerationId,
        capture_contract::ConfigurationId,
        capture_contract::CalibrationId,
        capture_contract::SnapshotId,
    )>,
}
pub struct QualityEngine {
    policy: RuntimeQualityPolicy,
    origin_session: SessionId,
    current_session: SessionId,
    last_ns: u64,
    seen: BTreeSet<SessionId>,
    streams: BTreeMap<String, StreamState>,
    streaks: BTreeMap<(FrameViolation, Scope), Streak>,
    active: BTreeMap<(FrameViolation, Scope), ActiveEpisode>,
    events: VecDeque<QualityEvent>,
    event_sequence: u64,
    summaries: VecDeque<SessionSummary>,
    summaries_evicted: u64,
    finalized: bool,
    interruption_reason: Option<InterruptionReason>,
    pair_delta: RollingWindow<u64>,
    diagnostic_window: RollingWindow<crate::DiagnosticKind>,
    diagnostic_counts: BTreeMap<crate::DiagnosticKind, u64>,
}
impl QualityEngine {
    pub fn new(
        policy: RuntimeQualityPolicy,
        session: SessionId,
        streams: Vec<String>,
    ) -> QualityResult<Self> {
        policy.validate()?;
        if streams.is_empty()
            || streams.len() > 64
            || streams.iter().any(|s| !token(s))
            || streams.iter().collect::<BTreeSet<_>>().len() != streams.len()
        {
            return Err("STREAM_SET_INVALID");
        }
        let mut states = BTreeMap::new();
        for name in streams {
            states.insert(name, Self::new_stream(&policy, &session)?);
        }
        let pair_delta = RollingWindow::new(
            session.clone(),
            policy.window_capacity,
            policy.window_horizon_ns,
        )?;
        let diagnostic_window = RollingWindow::new(
            session.clone(),
            policy.window_capacity,
            policy.window_horizon_ns,
        )?;
        Ok(Self {
            pair_delta,
            diagnostic_window,
            diagnostic_counts: BTreeMap::new(),
            policy,
            origin_session: session.clone(),
            current_session: session.clone(),
            last_ns: 0,
            seen: BTreeSet::from([session]),
            streams: states,
            streaks: BTreeMap::new(),
            active: BTreeMap::new(),
            events: VecDeque::new(),
            event_sequence: 0,
            summaries: VecDeque::new(),
            summaries_evicted: 0,
            finalized: false,
            interruption_reason: None,
        })
    }
    fn new_stream(
        policy: &RuntimeQualityPolicy,
        session: &SessionId,
    ) -> QualityResult<StreamState> {
        Ok(StreamState {
            metrics: RollingWindow::new(
                session.clone(),
                policy.window_capacity,
                policy.window_horizon_ns,
            )?,
            latest_context: None,
            window: RollingWindow::new(
                session.clone(),
                policy.window_capacity,
                policy.window_horizon_ns,
            )?,
            frames: 0,
            last_received: None,
            source_last: BTreeMap::new(),
            source_clock: None,
            mapper_binding: None,
            binding: None,
        })
    }
    fn check_time(&self, at: &MonoTime) -> QualityResult<()> {
        if self.finalized {
            return Err("SESSION_FINALIZED");
        }
        if at.session_id != self.current_session {
            return Err("MONOTONIC_DOMAIN_MISMATCH");
        }
        if at.offset_ns < self.last_ns {
            return Err("MONOTONIC_REGRESSION");
        }
        Ok(())
    }
    fn check_scope(&self, scope: &Scope) -> QualityResult<()> {
        if let Scope::Stream(name) = scope {
            if !self.streams.contains_key(name) {
                return Err("STREAM_UNREGISTERED");
            }
        }
        Ok(())
    }
    fn emit(&mut self, mut event: QualityEvent) -> QualityResult<()> {
        self.event_sequence = self
            .event_sequence
            .checked_add(1)
            .ok_or("EVENT_SEQUENCE_EXHAUSTED")?;
        event.sequence = self.event_sequence;
        if event.episode_sequence == 0 {
            event.episode_sequence = event.sequence;
        }
        if self.events.len() == self.policy.event_capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
        Ok(())
    }
    /// Only explicit Healthy evidence counts toward recovery; Unknown never
    /// clears an episode. A new session is not implicitly Healthy evidence.
    pub fn evaluate(
        &mut self,
        at: MonoTime,
        scope: Scope,
        violation: FrameViolation,
        condition: Condition,
    ) -> QualityResult<()> {
        self.check_time(&at)?;
        if violation == FrameViolation::ObserverInterrupted {
            return Err("LIFECYCLE_COMMAND_REQUIRED");
        }
        self.check_scope(&scope)?;
        if self.event_sequence == u64::MAX {
            return Err("EVENT_SEQUENCE_EXHAUSTED");
        }
        self.last_ns = at.offset_ns;
        let key = (violation, scope.clone());
        if condition == Condition::Unknown {
            if let Some(streak) = self.streaks.get_mut(&key) {
                streak.bad = 0;
                streak.good = 0;
            }
            if let Some(active) = self.active.get_mut(&key) {
                active.recovery_observations = 0;
            }
            return Ok(());
        }
        let rule = &self.policy.rules[&violation];
        let streak = self.streaks.entry(key.clone()).or_default();
        match condition {
            Condition::Violated => {
                streak.bad = streak.bad.saturating_add(1);
                streak.good = 0;
                if let Some(active) = self.active.get_mut(&key) {
                    active.bad_observations = active
                        .bad_observations
                        .checked_add(1)
                        .ok_or("OBSERVATION_COUNT_EXHAUSTED")?;
                    active.recovery_observations = 0;
                    active.last_observed = at.clone();
                } else if streak.bad >= rule.open_after {
                    let sequence = self.event_sequence + 1;
                    let active = ActiveEpisode {
                        episode_sequence: sequence,
                        violation,
                        scope: scope.clone(),
                        severity: rule.severity,
                        origin: at.clone(),
                        last_observed: at.clone(),
                        bad_observations: u64::from(streak.bad),
                        recovery_observations: 0,
                    };
                    let event = QualityEvent {
                        interruption_reason: None,
                        version: QUALITY_VERSION,
                        sequence: 0,
                        episode_sequence: sequence,
                        violation,
                        scope,
                        transition: EventTransition::Opened,
                        severity: active.severity,
                        at: at.clone(),
                        origin: at,
                        bad_observations: active.bad_observations,
                        recovery_observations: 0,
                    };
                    self.active.insert(key, active);
                    self.emit(event)?;
                }
            }
            Condition::Healthy => {
                streak.bad = 0;
                streak.good = streak.good.saturating_add(1);
                if let Some(active) = self.active.get_mut(&key) {
                    active.recovery_observations = streak.good;
                    active.last_observed = at.clone();
                    if streak.good >= rule.recover_after {
                        let recovery_observations = streak.good;
                        let active = self.active.remove(&key).expect("active episode exists");
                        self.emit(QualityEvent {
                            interruption_reason: None,
                            version: QUALITY_VERSION,
                            sequence: 0,
                            episode_sequence: active.episode_sequence,
                            violation,
                            scope,
                            transition: EventTransition::Recovered,
                            severity: active.severity,
                            at,
                            origin: active.origin,
                            bad_observations: active.bad_observations,
                            recovery_observations,
                        })?;
                    }
                }
            }
            Condition::Unknown => {}
        }
        Ok(())
    }
    pub fn record_frame(&mut self, frame: FrameObservation) -> QualityResult<()> {
        self.record_frame_evidence(frame, None, false)
    }
    /// Decoder/association boundaries can explicitly mark a valid layout's
    /// overall contract Unknown while required CameraInfo is still pending.
    /// Invalid layout/bindings cannot be promoted to Healthy by this override.
    pub fn record_frame_with_evidence(
        &mut self,
        frame: FrameObservation,
        evidence: Condition,
    ) -> QualityResult<()> {
        self.record_frame_evidence(frame, Some(evidence), false)
    }
    /// A receipt is counted once. A later exact-context pixel observation supplies
    /// the contract verdict, without resetting the pixel violation hysteresis.
    pub fn record_frame_deferred_pixels(&mut self, frame: FrameObservation) -> QualityResult<()> {
        self.record_frame_evidence(frame, None, true)
    }
    fn record_frame_evidence(
        &mut self,
        frame: FrameObservation,
        evidence: Option<Condition>,
        deferred_pixels: bool,
    ) -> QualityResult<()> {
        frame
            .context
            .validate()
            .map_err(|_| "FRAME_CONTEXT_INVALID")?;
        let at = frame.context.received_at();
        self.check_time(&at)?;
        let scope = Scope::Stream(frame.context.stream_id.clone());
        self.check_scope(&scope)?;
        let state = self
            .streams
            .get_mut(&frame.context.stream_id)
            .expect("registered");
        let binding = (
            frame.context.generation_id.clone(),
            frame.context.configuration_id.clone(),
            frame.context.calibration_id.clone(),
            frame.context.snapshot_id.clone(),
        );
        let bound = state.binding.as_ref().is_none_or(|old| old == &binding);
        if state.binding.is_none() {
            state.binding = Some(binding);
        }
        let fractions = [frame.valid_fraction, frame.zero_fraction]
            .iter()
            .flatten()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v));
        let mut source_bound = true;
        if let Some(source) = frame.source {
            source_bound = state.source_clock.is_none_or(|clock| clock == source.clock)
                && state
                    .mapper_binding
                    .is_none_or(|mapper| mapper == frame.mapper);
            if state.source_clock.is_none() {
                state.source_clock = Some(source.clock);
            }
            if source_bound
                && state.mapper_binding.is_none()
                && crate::map_source_time(source, frame.mapper).is_ok()
            {
                state.mapper_binding = Some(frame.mapper);
            }
        }
        let contract_valid = frame.contract_valid
            && bound
            && source_bound
            && fractions
            && frame
                .valid_fraction
                .is_none_or(|v| v >= self.policy.quality.min_valid_fraction)
            && frame
                .zero_fraction
                .is_none_or(|v| v <= self.policy.quality.max_zero_fraction);
        let contract_evidence = if !contract_valid {
            Condition::Violated
        } else {
            evidence.unwrap_or(Condition::Healthy)
        };
        let regression = frame
            .source
            .map(|stamp| {
                if !source_bound || !bound {
                    return Condition::Unknown;
                }
                let key = stamp.clock.key();
                let condition = state
                    .source_last
                    .get(&key)
                    .map_or(Condition::Unknown, |last| {
                        if stamp.value <= *last {
                            Condition::Violated
                        } else {
                            Condition::Healthy
                        }
                    });
                // Never lower a source high-water mark after a regression.
                state
                    .source_last
                    .entry(key)
                    .and_modify(|last| *last = (*last).max(stamp.value))
                    .or_insert(stamp.value);
                condition
            })
            .unwrap_or(Condition::Unknown);
        let mapping = frame.source.filter(|_| bound).map(|source| {
            if !source_bound {
                return Err(crate::MappingError::ClockMismatch);
            }
            validate_source_age(
                source,
                frame.mapper,
                frame.received_unix_ns,
                &self.policy.quality,
            )
        });
        state.frames = state
            .frames
            .checked_add(1)
            .ok_or("OBSERVATION_COUNT_EXHAUSTED")?;
        state.last_received = Some(at.offset_ns);
        let base_passed = contract_evidence == Condition::Healthy
            && regression != Condition::Violated
            && mapping.is_some_and(|v| v.is_ok());
        let age_ns = frame
            .source
            .and_then(|source| crate::map_source_time(source, frame.mapper).ok())
            .zip(frame.received_unix_ns)
            .filter(|_| source_bound && bound)
            .map(|(source, received)| {
                if received >= source {
                    (received - source) as f64
                } else {
                    -((source - received) as f64)
                }
            });
        state.metrics.push(
            &at,
            crate::metrics::MetricSample {
                age_ns,
                pixels: None,
                base_passed,
                contract_valid,
            },
        )?;
        state.latest_context = Some(frame.context.clone());
        state.window.push(&at, base_passed && !deferred_pixels)?;
        if !bound {
            self.record_diagnostic(at.clone(), crate::DiagnosticKind::BindingChanged)?;
        }
        self.evaluate(
            at.clone(),
            scope.clone(),
            FrameViolation::StreamStale,
            Condition::Healthy,
        )?;
        if !deferred_pixels || !contract_valid {
            self.evaluate(
                at.clone(),
                scope.clone(),
                FrameViolation::FrameContractInvalid,
                contract_evidence,
            )?;
        }
        self.evaluate(
            at.clone(),
            scope.clone(),
            FrameViolation::TimestampRegression,
            regression,
        )?;
        self.evaluate(
            at,
            scope,
            FrameViolation::TimeMappingInvalid,
            match mapping {
                Some(Ok(_)) => Condition::Healthy,
                Some(Err(_)) => Condition::Violated,
                None => Condition::Unknown,
            },
        )
    }
    /// Receipt staleness is driven by a caller's Tick even when no frames arrive.
    /// Non-stale ticks are not frame evidence and do not satisfy recovery counts.
    pub fn tick(&mut self, at: MonoTime) -> QualityResult<()> {
        self.check_time(&at)?;
        let names = self.streams.keys().cloned().collect::<Vec<_>>();
        for name in names {
            let stream = self.streams.get_mut(&name).expect("registered");
            stream.window.tick(&at)?;
            stream.metrics.tick(&at)?;
            let stale =
                at.offset_ns - stream.last_received.unwrap_or(0) >= self.policy.stale_after_ns;
            if stale {
                self.evaluate(
                    at.clone(),
                    Scope::Stream(name),
                    FrameViolation::StreamStale,
                    Condition::Violated,
                )?;
            }
        }
        self.pair_delta.tick(&at)?;
        self.diagnostic_window.tick(&at)?;
        self.last_ns = at.offset_ns;
        Ok(())
    }
    /// Fixed enum keys keep classification counters bounded. This records facts
    /// only; it never decides an episode from the configured action.
    pub fn record_diagnostic(
        &mut self,
        at: MonoTime,
        kind: crate::DiagnosticKind,
    ) -> QualityResult<()> {
        self.check_time(&at)?;
        self.diagnostic_window.push(&at, kind)?;
        let count = self.diagnostic_counts.entry(kind).or_default();
        *count = count.checked_add(1).ok_or("OBSERVATION_COUNT_EXHAUSTED")?;
        self.last_ns = at.offset_ns;
        Ok(())
    }
    pub fn record_pixel_metrics(
        &mut self,
        context: &FrameContext,
        pixels: crate::DepthPixelMetrics,
        at: MonoTime,
        pixel_contract_valid: bool,
    ) -> QualityResult<()> {
        self.check_time(&at)?;
        pixels.validate()?;
        let state = self
            .streams
            .get_mut(&context.stream_id)
            .ok_or("STREAM_UNKNOWN")?;
        if state.latest_context.as_ref() != Some(context) {
            return Err("PIXEL_CONTEXT_MISMATCH");
        }
        let sample = state.metrics.latest_mut(context.received_mono_offset_ns)?;
        if sample.pixels.is_some() {
            return Err("PIXEL_OBSERVATION_DUPLICATE");
        }
        let passed = sample.contract_valid
            && pixel_contract_valid
            && pixels.valid_fraction() >= self.policy.quality.min_valid_fraction
            && pixels.zero_fraction() <= self.policy.quality.max_zero_fraction;
        let evaluate = sample.contract_valid;
        let base_passed = sample.base_passed;
        // The original receipt's bool sample is retained in the same window.
        let window = state.window.latest_mut(context.received_mono_offset_ns)?;
        *window = base_passed && passed;
        state
            .metrics
            .latest_mut(context.received_mono_offset_ns)?
            .pixels = Some(pixels);
        if evaluate {
            self.evaluate(
                at.clone(),
                Scope::Stream(context.stream_id.clone()),
                FrameViolation::FrameContractInvalid,
                if passed {
                    Condition::Healthy
                } else {
                    Condition::Violated
                },
            )?;
        }
        self.last_ns = at.offset_ns;
        Ok(())
    }
    pub fn observe_pair_delta(&mut self, at: MonoTime, delta_us: u64) -> QualityResult<()> {
        self.check_time(&at)?;
        self.pair_delta.push(&at, delta_us)?;
        let condition = if delta_us > self.policy.quality.max_pair_delta_us {
            Condition::Violated
        } else {
            Condition::Healthy
        };
        self.evaluate(
            at,
            Scope::Group,
            FrameViolation::PairSkewExceeded,
            condition,
        )
    }
    /// One real lifecycle interruption, independent of frame thresholds/action.
    /// No receipt/pixel count or SDK-disconnection provenance is fabricated.
    pub fn interrupt(&mut self, at: MonoTime, reason: InterruptionReason) -> QualityResult<()> {
        self.check_time(&at)?;
        if let Some(existing) = self.interruption_reason {
            return if existing == reason {
                Ok(())
            } else {
                Err("SESSION_ALREADY_INTERRUPTED")
            };
        }
        let sequence = self
            .event_sequence
            .checked_add(1)
            .ok_or("EVENT_SEQUENCE_EXHAUSTED")?;
        let active = ActiveEpisode {
            episode_sequence: sequence,
            violation: FrameViolation::ObserverInterrupted,
            scope: Scope::Group,
            severity: QualityState::Failed,
            origin: at.clone(),
            last_observed: at.clone(),
            bad_observations: 1,
            recovery_observations: 0,
        };
        self.emit(QualityEvent {
            version: QUALITY_VERSION,
            sequence: 0,
            episode_sequence: sequence,
            violation: FrameViolation::ObserverInterrupted,
            scope: Scope::Group,
            transition: EventTransition::Opened,
            severity: QualityState::Failed,
            at: at.clone(),
            origin: at.clone(),
            bad_observations: 1,
            recovery_observations: 0,
            interruption_reason: Some(reason),
        })?;
        self.active
            .insert((FrameViolation::ObserverInterrupted, Scope::Group), active);
        self.interruption_reason = Some(reason);
        self.last_ns = at.offset_ns;
        Ok(())
    }
    pub fn finalize_session(&mut self) -> SessionSummary {
        let summary = SessionSummary {
            interruption_reason: self.interruption_reason,
            version: QUALITY_VERSION,
            session_id: self.current_session.clone(),
            finalized: true,
            last_mono_offset_ns: self.last_ns,
            frame_counts: self
                .streams
                .iter()
                .map(|(k, v)| (k.clone(), v.frames))
                .collect(),
            open_episode_sequences: self.active.values().map(|v| v.episode_sequence).collect(),
        };
        if !self.finalized {
            if self.summaries.len() == self.policy.summary_capacity {
                self.summaries.pop_front();
                self.summaries_evicted += 1;
            }
            self.summaries.push_back(summary.clone());
            self.finalized = true;
        }
        summary
    }
    pub fn begin_session(&mut self, session: SessionId) -> QualityResult<()> {
        if self.seen.contains(&session) {
            return Err("SESSION_ID_REUSED");
        }
        if self.seen.len() == self.policy.max_sessions {
            return Err("SESSION_LIMIT_EXHAUSTED");
        }
        self.finalize_session();
        self.current_session = session.clone();
        self.seen.insert(session.clone());
        self.last_ns = 0;
        self.finalized = false;
        self.interruption_reason = None;
        self.pair_delta = RollingWindow::new(
            session.clone(),
            self.policy.window_capacity,
            self.policy.window_horizon_ns,
        )?;
        self.diagnostic_window = RollingWindow::new(
            session.clone(),
            self.policy.window_capacity,
            self.policy.window_horizon_ns,
        )?;
        self.diagnostic_counts.clear();
        for state in self.streams.values_mut() {
            *state = Self::new_stream(&self.policy, &session)?;
        }
        // Pending/recovery streaks are not proof of health in a new domain.
        for streak in self.streaks.values_mut() {
            streak.bad = 0;
            streak.good = 0;
        }
        self.evaluate(
            MonoTime {
                session_id: session,
                offset_ns: 0,
            },
            Scope::Group,
            FrameViolation::SessionChanged,
            Condition::Violated,
        )
    }
    pub fn events(&self, cursor: Option<EventCursor>, limit: usize) -> QualityResult<EventPage> {
        if !(1..=self.policy.event_capacity).contains(&limit) {
            return Err("EVENT_PAGE_LIMIT_INVALID");
        }
        let cursor = cursor.unwrap_or(EventCursor {
            origin_session_id: self.origin_session.clone(),
            after_sequence: 0,
        });
        if cursor.origin_session_id != self.origin_session {
            return Err("CURSOR_DOMAIN_MISMATCH");
        }
        if cursor.after_sequence > self.event_sequence {
            return Err("CURSOR_AHEAD");
        }
        let first = self.events.front().map_or(1, |v| v.sequence);
        let gap = if cursor.after_sequence < first.saturating_sub(1) {
            Some(EventGap {
                code: "EVENT_GAP".into(),
                requested_after: cursor.after_sequence,
                first_retained_sequence: first,
                last_lost_sequence: first - 1,
            })
        } else {
            None
        };
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence > cursor.after_sequence)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let after = events
            .last()
            .map_or(cursor.after_sequence, |event| event.sequence);
        Ok(EventPage {
            version: QUALITY_VERSION,
            origin_session_id: self.origin_session.clone(),
            latest_sequence: self.event_sequence,
            gap,
            events,
            next_cursor: EventCursor {
                origin_session_id: self.origin_session.clone(),
                after_sequence: after,
            },
            has_more: after < self.event_sequence,
        })
    }
    pub fn snapshot(&self) -> QualitySnapshot {
        let state = self
            .active
            .values()
            .map(|v| v.severity)
            .max()
            .unwrap_or(QualityState::Normal);
        let coverage = !self.finalized
            && self.streams.values().all(|stream| {
                !stream.window.is_empty()
                    && stream
                        .window
                        .iter()
                        .last()
                        .is_some_and(|(_, passed)| *passed)
                    && stream
                        .last_received
                        .is_some_and(|last| self.last_ns - last < self.policy.stale_after_ns)
                    && stream.window.iter().filter(|(_, passed)| *passed).count() as f64
                        / stream.window.len() as f64
                        >= self.policy.quality.min_valid_fraction
            });
        let mut window_counts = BTreeMap::new();
        for (_, kind) in self.diagnostic_window.iter() {
            *window_counts.entry(*kind).or_insert(0) += 1;
        }
        QualitySnapshot {
            interruption_reason: self.interruption_reason,
            pair_delta_us: crate::metrics::summarize(
                self.pair_delta.iter().map(|(_, v)| *v as f64),
            ),
            diagnostics: crate::DiagnosticSnapshot {
                session_counts: self.diagnostic_counts.clone(),
                window_counts,
            },
            version: QUALITY_VERSION,
            origin_session_id: self.origin_session.clone(),
            session_id: self.current_session.clone(),
            at: MonoTime {
                session_id: self.current_session.clone(),
                offset_ns: self.last_ns,
            },
            state,
            action: self.policy.quality.action,
            continue_allowed: self.policy.quality.action == QualityAction::ReportOnly
                || (state == QualityState::Normal && coverage),
            active_episodes: self.active.values().cloned().collect(),
            streams: self
                .streams
                .iter()
                .map(|(name, state)| {
                    (
                        name.clone(),
                        StreamQuality {
                            window_metrics: stream_metrics(state),
                            frame_count: state.frames,
                            recent_frames: state.window.len(),
                            recent_checks_passed_fraction: if state.window.is_empty() {
                                None
                            } else {
                                Some(
                                    state.window.iter().filter(|(_, v)| *v).count() as f64
                                        / state.window.len() as f64,
                                )
                            },
                            last_receive_mono_offset_ns: state.last_received,
                        },
                    )
                })
                .collect(),
            finalized_sessions: self.summaries.iter().cloned().collect(),
            summaries_evicted: self.summaries_evicted,
            latest_event_sequence: self.event_sequence,
            current_session_finalized: self.finalized,
        }
    }
    pub fn query(&self, request: QualityQueryRequest) -> QualityResult<QualityQueryResponse> {
        if request.version != QUALITY_VERSION {
            return Err("QUALITY_QUERY_VERSION");
        }
        let result = match request.query {
            QualityQuery::Snapshot => QualityQueryResult::Snapshot {
                snapshot: self.snapshot(),
            },
            QualityQuery::Events { cursor, limit } => QualityQueryResult::Events {
                page: self.events(cursor, limit)?,
            },
        };
        Ok(QualityQueryResponse {
            version: QUALITY_VERSION,
            result,
        })
    }
}

fn stream_metrics(state: &StreamState) -> crate::StreamWindowMetrics {
    let mut result = crate::StreamWindowMetrics::default();
    let receipts: Vec<_> = state.metrics.iter().map(|(ns, _)| *ns).collect();
    if let (Some(first), Some(last)) = (receipts.first(), receipts.last()) {
        if receipts.len() > 1 && last > first {
            result.active_window_fps =
                Some((receipts.len() - 1) as f64 * 1e9 / (last - first) as f64);
        }
    }
    result.max_receive_gap_ns = receipts.windows(2).map(|p| p[1] - p[0]).max();
    result.source_age_ns =
        crate::metrics::summarize(state.metrics.iter().filter_map(|(_, sample)| sample.age_ns));
    let mut totals: Option<crate::DepthPixelMetrics> = None;
    for (_, sample) in state.metrics.iter() {
        if let Some(p) = sample.pixels {
            let total = totals.get_or_insert(crate::DepthPixelMetrics {
                samples: 0,
                zero_samples: 0,
                nonzero_samples: 0,
                max_u16_samples: 0,
            });
            total.samples = total.samples.saturating_add(p.samples);
            total.zero_samples = total.zero_samples.saturating_add(p.zero_samples);
            total.nonzero_samples = total.nonzero_samples.saturating_add(p.nonzero_samples);
            total.max_u16_samples = total.max_u16_samples.saturating_add(p.max_u16_samples);
        }
    }
    result.depth_pixels = totals;
    result.valid_fraction = totals.map(|p| p.valid_fraction());
    result.zero_fraction = totals.map(|p| p.zero_fraction());
    result.max_u16_fraction = totals.map(|p| p.max_u16_fraction());
    result
}

#[cfg(test)]
mod sequence_boundaries {
    use super::*;
    fn engine() -> QualityEngine {
        let policy = RuntimeQualityPolicy {
            version: 1,
            quality: QualityPolicy {
                version: 1,
                max_source_age_ns: 100,
                max_future_ns: 10,
                max_pair_delta_us: 10,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: QualityAction::ReportOnly,
            },
            stale_after_ns: 10,
            event_capacity: 8,
            window_capacity: 8,
            window_horizon_ns: 100,
            summary_capacity: 1,
            max_sessions: 2,
            rules: FrameViolation::ALL
                .into_iter()
                .map(|v| {
                    (
                        v,
                        RulePolicy {
                            open_after: 1,
                            recover_after: 1,
                            severity: QualityState::Degraded,
                        },
                    )
                })
                .collect(),
        };
        QualityEngine::new(
            policy,
            SessionId::parse("a".repeat(32)).unwrap(),
            vec!["color".into(), "depth".into()],
        )
        .unwrap()
    }
    #[test]
    fn multi_stream_ticks_and_receipts_cannot_wrap_the_public_event_sequence() {
        let mut e = engine();
        e.event_sequence = u64::MAX - 1;
        assert_eq!(
            e.tick(MonoTime {
                session_id: e.current_session.clone(),
                offset_ns: 10
            }),
            Err("EVENT_SEQUENCE_EXHAUSTED")
        );
        assert_eq!(e.event_sequence, u64::MAX);
        assert_eq!(e.events.back().unwrap().sequence, u64::MAX);
        let mut e = engine();
        e.event_sequence = u64::MAX - 1;
        let frame = FrameObservation {
            context: FrameContext {
                version: 1,
                session_id: e.current_session.clone(),
                generation_id: capture_contract::GenerationId::parse("b".repeat(32)).unwrap(),
                configuration_id: capture_contract::ConfigurationId::parse("c".repeat(64)).unwrap(),
                calibration_id: capture_contract::CalibrationId::parse("d".repeat(64)).unwrap(),
                snapshot_id: capture_contract::SnapshotId::parse("e".repeat(64)).unwrap(),
                stream_id: "color".into(),
                sequence: Some(1),
                device_timestamp_us: Some(1),
                system_timestamp_us: Some(1),
                global_timestamp_us: None,
                received_mono_offset_ns: 1,
            },
            contract_valid: false,
            valid_fraction: None,
            zero_fraction: None,
            source: None,
            mapper: crate::ClockMapper::Unknown,
            received_unix_ns: None,
        };
        assert_eq!(
            e.record_frame(frame.clone()),
            Err("EVENT_SEQUENCE_EXHAUSTED")
        );
        assert_eq!(e.event_sequence, u64::MAX);
        let mut e = engine();
        e.event_sequence = u64::MAX;
        assert_eq!(
            e.record_frame(frame.clone()),
            Err("EVENT_SEQUENCE_EXHAUSTED")
        );
        let mut good = frame.clone();
        good.contract_valid = true;
        let mut e = engine();
        e.evaluate(
            MonoTime {
                session_id: e.current_session.clone(),
                offset_ns: 0,
            },
            Scope::Stream("color".into()),
            FrameViolation::StreamStale,
            Condition::Violated,
        )
        .unwrap();
        e.event_sequence = u64::MAX - 1;
        assert_eq!(
            e.record_frame(good.clone()),
            Err("EVENT_SEQUENCE_EXHAUSTED")
        );
        let pixels = crate::DepthPixelMetrics {
            samples: 1,
            zero_samples: 0,
            nonzero_samples: 1,
            max_u16_samples: 0,
        };
        let mut e = engine();
        e.record_frame_deferred_pixels(good.clone()).unwrap();
        e.event_sequence = u64::MAX;
        assert_eq!(
            e.record_pixel_metrics(&good.context, pixels, good.context.received_at(), true),
            Err("EVENT_SEQUENCE_EXHAUSTED")
        );
        let mut e = engine();
        e.record_frame_deferred_pixels(frame.clone()).unwrap();
        e.record_pixel_metrics(&frame.context, pixels, frame.context.received_at(), true)
            .unwrap();
        assert!(
            e.snapshot()
                .active_episodes
                .iter()
                .any(|v| v.violation == FrameViolation::FrameContractInvalid)
        );
    }
}
