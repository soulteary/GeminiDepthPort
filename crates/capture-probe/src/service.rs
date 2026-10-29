//! Supervised resident RGB-D worker. A separate process bounds native hangs.
use crate::rgbd::{self, Config, Metrics, QueuedPair};
use anyhow::{Context, Result, ensure};
use frame_core::{LatestQueue, PairGuard, PairProvenance};
use ring::digest;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
fn install_signals() -> Result<()> {
    // The handler performs only an atomic store; all SDK shutdown runs on worker thread.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = stop_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        ensure!(
            libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) == 0
                && libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == 0,
            "signal setup failed"
        );
    }
    Ok(())
}
struct Settings {
    status_path: PathBuf,
    recording_control_path: Option<PathBuf>,
    generation: String,
    idle_timeout_ms: u64,
    startup_timeout_ms: u64,
    heartbeat_ms: u64,
    heartbeat_timeout_ms: u64,
    max_restarts: u64,
    restart_backoff_ms: u64,
    shutdown_timeout_ms: u64,
    ready_min_pairs: u64,
}
struct ServiceConfig {
    usb_recovery: Option<crate::usb_recovery::Recovery>,
    capture: Config,
    settings: Settings,
    hash: String,
    runtime_contract: Option<Value>,
    quality_runtime: Option<quality_core::RuntimeQualityPolicy>,
    raw_settings: Option<crate::raw_recording::RawSettings>,
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
fn sha(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}
impl ServiceConfig {
    fn parse(raw: &str) -> Result<Self> {
        ensure!(raw.len() <= 64 * 1024, "config too large");
        let mut value: Value =
            serde_json::from_value(capture_contract::parse_strict(raw.as_bytes())?)?;
        let usb_recovery_value = value
            .as_object_mut()
            .context("config object required")?
            .remove("usb_recovery");
        let usb_recovery = usb_recovery_value
            .as_ref()
            .map(crate::usb_recovery::Recovery::parse)
            .transpose()?;
        let raw_value = value
            .as_object_mut()
            .context("config object required")?
            .remove("raw_recording");
        let raw_settings = raw_value
            .as_ref()
            .map(crate::raw_recording::RawSettings::parse)
            .transpose()?;
        let quality_runtime = value
            .as_object_mut()
            .context("config object required")?
            .remove("quality_runtime")
            .map(serde_json::from_value::<quality_core::RuntimeQualityPolicy>)
            .transpose()?;
        let runtime_contract = value
            .as_object_mut()
            .context("config object required")?
            .remove("runtime_contract");
        if let Some(contract) = &runtime_contract {
            let object = contract
                .as_object()
                .context("runtime contract object required")?;
            let keys = ["sdk_sha256", "bridge_sha256", "service_sha256"];
            ensure!(
                object.len() == 3
                    && keys
                        .iter()
                        .all(
                            |key| object
                                .get(*key)
                                .and_then(Value::as_str)
                                .is_some_and(|v| v.len() == 64
                                    && v.bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                        ),
                "invalid runtime contract"
            );
        }
        let service = value
            .as_object_mut()
            .context("config object required")?
            .remove("service")
            .context("explicit service policy required")?;
        let keys = [
            "status_path",
            "generation",
            "idle_timeout_ms",
            "startup_timeout_ms",
            "heartbeat_ms",
            "heartbeat_timeout_ms",
            "max_restarts",
            "restart_backoff_ms",
            "shutdown_timeout_ms",
            "ready_min_pairs",
        ];
        let object = service.as_object().context("service object required")?;
        ensure!(
            object.len() == keys.len() + usize::from(object.contains_key("recording_control_path"))
                && keys.iter().all(|k| object.contains_key(*k)),
            "service fields required"
        );
        ensure!(
            value.get("seconds").is_none(),
            "resident service refuses finite seconds"
        );
        value["seconds"] = json!(300);
        let mut capture = Config::parse(&value.to_string())?;
        if usb_recovery.is_some() {
            ensure!(
                capture.serial.len() <= 128
                    && capture.serial.bytes().all(|b| (33..=126).contains(&b)),
                "USB recovery requires an ASCII device serial"
            );
        }
        if let Some(policy) = usb_recovery_value {
            capture.configuration["usb_recovery"] = policy;
        }
        if let Some(policy) = &quality_runtime {
            policy.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                policy.quality.action == capture_contract::QualityAction::ReportOnly,
                "service runtime quality supports report-only"
            );
            ensure!(
                capture.quality_policy.as_ref() == Some(&policy.quality),
                "quality policies disagree"
            );
            capture.configuration["quality_runtime"] = serde_json::to_value(policy)?;
        }
        if let Some(raw) = raw_value {
            ensure!(
                quality_runtime.is_some(),
                "raw recording requires explicit runtime quality evidence"
            );
            capture.configuration["raw_recording"] = raw;
        }
        ensure!(
            capture.ros.is_some(),
            "resident service requires real ROS output"
        );
        let number = |key: &str| -> Result<u64> {
            service[key]
                .as_u64()
                .context("unsigned service limit required")
        };
        let status_path = PathBuf::from(
            service["status_path"]
                .as_str()
                .context("status path required")?,
        );
        ensure!(
            status_path.is_absolute()
                && status_path.file_name().is_some()
                && status_path
                    .parent()
                    .is_some_and(|p| p != Path::new("/") && p != Path::new("/tmp"))
                && !status_path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "private absolute status directory required"
        );
        let generation = service["generation"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .context("opaque generation required")?
            .to_owned();
        let recording_control_path = service
            .get("recording_control_path")
            .map(|v| -> Result<PathBuf> {
                let path = PathBuf::from(v.as_str().context("recording control path string")?);
                ensure!(
                    path.is_absolute()
                        && path.file_name().is_some_and(|p| p == "control.json")
                        && path
                            .parent()
                            .is_some_and(|p| p != Path::new("/") && p != Path::new("/tmp"))
                        && !path
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                    "private recording control path required"
                );
                Ok(path)
            })
            .transpose()?;
        let settings = Settings {
            status_path,
            recording_control_path,
            generation,
            idle_timeout_ms: number("idle_timeout_ms")?,
            startup_timeout_ms: number("startup_timeout_ms")?,
            heartbeat_ms: number("heartbeat_ms")?,
            heartbeat_timeout_ms: number("heartbeat_timeout_ms")?,
            max_restarts: number("max_restarts")?,
            restart_backoff_ms: number("restart_backoff_ms")?,
            shutdown_timeout_ms: number("shutdown_timeout_ms")?,
            ready_min_pairs: number("ready_min_pairs")?,
        };
        ensure!(
            (100..=60_000).contains(&settings.idle_timeout_ms)
                && settings.idle_timeout_ms >= u64::from(capture.poll_ms)
                && (1_000..=120_000).contains(&settings.startup_timeout_ms)
                && settings.startup_timeout_ms > settings.idle_timeout_ms
                && (50..=5_000).contains(&settings.heartbeat_ms)
                && (500..=120_000).contains(&settings.heartbeat_timeout_ms)
                && settings.heartbeat_timeout_ms >= settings.heartbeat_ms.saturating_mul(3)
                && (0..=20).contains(&settings.max_restarts)
                && (100..=30_000).contains(&settings.restart_backoff_ms)
                && (100..=30_000).contains(&settings.shutdown_timeout_ms)
                && (1..=10_000).contains(&settings.ready_min_pairs),
            "service policy bounds"
        );
        ensure!(
            capture.startup_discovery_seconds * 1000.0 < settings.startup_timeout_ms as f64,
            "startup allowance exceeds deadline"
        );
        Ok(Self {
            capture,
            settings,
            hash: sha(raw.as_bytes()),
            runtime_contract,
            quality_runtime,
            raw_settings,
            usb_recovery,
        })
    }
}
impl ServiceConfig {
    fn verify_runtime(&self) -> Result<()> {
        if let Some(recovery) = &self.usb_recovery {
            recovery.verify()?;
        }
        if let Some(contract) = &self.runtime_contract {
            for (key, path) in [
                ("sdk_sha256", PathBuf::from(&self.capture.sdk_library)),
                (
                    "bridge_sha256",
                    PathBuf::from(&self.capture.ros.as_ref().context("ROS required")?.bridge),
                ),
                ("service_sha256", std::env::current_exe()?),
            ] {
                ensure!(
                    sha(&fs::read(path)?)
                        == contract[key].as_str().context("runtime hash required")?,
                    "runtime artifact mismatch"
                );
            }
        }
        Ok(())
    }
}

fn random_session() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_millis().min(u64::MAX as u128) as u64)
}
fn atomic_status(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().context("status parent required")?;
    fs::create_dir_all(parent)?;
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "status directory cannot be symlink"
    );
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = parent.join(format!(
        ".status-{}-{}.tmp",
        std::process::id(),
        random_session()?
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
/// Runtime adapter owns observed receipts separately from snapshot-bound checks.
/// Before an actual SDK snapshot exists, it never invents calibration/context IDs.
struct ServiceQuality {
    engine: quality_core::QualityEngine,
    policy: quality_core::RuntimeQualityPolicy,
    session: capture_contract::SessionId,
    instance: String,
    origin: Instant,
    directory: PathBuf,
    snapshot: Option<capture_contract::snapshot::SnapshotEnvelope>,
    receipts: std::collections::BTreeMap<String, u64>,
    unbound: std::collections::BTreeMap<String, u64>,
    last_observed: std::collections::BTreeMap<String, Value>,
    finalized: bool,
    publication_calls: u64,
    publication_failures: u64,
    partial_publications: u64,
    progress: std::collections::BTreeMap<String, (u64, u64)>,
    source_progress_invalid: bool,
    binding_invalid: bool,
    contexts: std::collections::BTreeMap<String, capture_contract::FrameContext>,
}
impl ServiceQuality {
    fn new(
        policy: quality_core::RuntimeQualityPolicy,
        session: &str,
        instance: &str,
        origin: Instant,
        status: &Path,
    ) -> Result<Self> {
        let session = capture_contract::SessionId::parse(session)?;
        let engine = quality_core::QualityEngine::new(
            policy.clone(),
            session.clone(),
            vec!["color".into(), "depth".into()],
        )
        .map_err(anyhow::Error::msg)?;
        Ok(Self {
            engine,
            policy,
            session: session.clone(),
            instance: instance.into(),
            origin,
            directory: status
                .parent()
                .context("status parent")?
                .join("quality")
                .join(session.as_str()),
            snapshot: None,
            receipts: std::collections::BTreeMap::from([("color".into(), 0), ("depth".into(), 0)]),
            unbound: std::collections::BTreeMap::from([("color".into(), 0), ("depth".into(), 0)]),
            last_observed: std::collections::BTreeMap::new(),
            finalized: false,
            publication_calls: 0,
            publication_failures: 0,
            partial_publications: 0,
            progress: std::collections::BTreeMap::new(),
            source_progress_invalid: false,
            binding_invalid: false,
            contexts: std::collections::BTreeMap::new(),
        })
    }
    fn at(&self, instant: Instant) -> capture_contract::MonoTime {
        capture_contract::MonoTime {
            session_id: self.session.clone(),
            offset_ns: instant
                .saturating_duration_since(self.origin)
                .as_nanos()
                .min(u64::MAX as u128) as u64,
        }
    }
    fn evaluate(
        &mut self,
        violation: quality_core::FrameViolation,
        condition: quality_core::Condition,
    ) -> Result<()> {
        self.engine
            .evaluate(
                self.at(Instant::now()),
                quality_core::Scope::Group,
                violation,
                condition,
            )
            .map_err(anyhow::Error::msg)
    }
    fn tick(&mut self) -> Result<()> {
        self.engine
            .tick(self.at(Instant::now()))
            .map_err(anyhow::Error::msg)
    }
    fn bind(&mut self, snapshot: &capture_contract::snapshot::SnapshotEnvelope) -> Result<()> {
        snapshot.require_runtime()?;
        ensure!(
            snapshot.content.binding.session_id == self.session
                && snapshot.content.binding.instance_id.as_str() == self.instance
                && snapshot.content.quality_policy == self.policy.quality,
            "quality snapshot binding"
        );
        if self
            .snapshot
            .as_ref()
            .is_some_and(|old| old.snapshot_id != snapshot.snapshot_id)
        {
            self.diagnostic(quality_core::DiagnosticKind::BindingChanged)?;
            self.evaluate(
                quality_core::FrameViolation::FrameContractInvalid,
                quality_core::Condition::Violated,
            )?;
            anyhow::bail!("quality snapshot changed within session");
        }
        self.snapshot = Some(snapshot.clone());
        Ok(())
    }
    fn receipt(
        &mut self,
        stream: &str,
        metadata: Option<&orbbec_safe::FrameMetadata>,
        received: Instant,
        received_unix_ns: Option<u64>,
        contract_valid: bool,
    ) -> Result<()> {
        *self.receipts.entry(stream.into()).or_default() += 1;
        if self.snapshot.is_none() {
            *self.unbound.entry(stream.into()).or_default() += 1;
        }
        let Some(metadata) = metadata else {
            self.engine
                .record_diagnostic(
                    self.at(received),
                    quality_core::DiagnosticKind::MetadataInvalid,
                )
                .map_err(anyhow::Error::msg)?;
            return self
                .engine
                .evaluate(
                    self.at(received),
                    quality_core::Scope::Group,
                    quality_core::FrameViolation::FrameContractInvalid,
                    quality_core::Condition::Violated,
                )
                .map_err(anyhow::Error::msg);
        };
        let mut contract_valid = contract_valid;
        let scale_changed = stream == "depth"
            && self
                .snapshot
                .as_ref()
                .and_then(|s| s.content.configuration.get("source_depth_scale_mm"))
                .and_then(Value::as_f64)
                .is_some_and(|expected| metadata.depth_scale_mm != Some(expected as f32));
        let dimensions_changed = self
            .snapshot
            .as_ref()
            .and_then(|s| s.content.calibration.models.get(stream))
            .is_some_and(|m| (m.width, m.height) != (metadata.width, metadata.height));
        let binding_invalid = self.binding_invalid || scale_changed || dimensions_changed;
        if binding_invalid {
            self.binding_invalid = true;
            contract_valid = false;
            if scale_changed || dimensions_changed {
                self.engine
                    .record_diagnostic(
                        self.at(received),
                        if scale_changed {
                            quality_core::DiagnosticKind::DepthScaleChanged
                        } else {
                            quality_core::DiagnosticKind::BindingChanged
                        },
                    )
                    .map_err(anyhow::Error::msg)?;
            }
        }
        if !contract_valid && !binding_invalid {
            self.engine
                .record_diagnostic(
                    self.at(received),
                    quality_core::DiagnosticKind::MetadataInvalid,
                )
                .map_err(anyhow::Error::msg)?;
        }
        if !binding_invalid {
            if let Some(last) = self.progress.get(stream) {
                if metadata.index <= last.0 || metadata.timestamp_us <= last.1 {
                    self.source_progress_invalid = true;
                }
            }
            self.progress
                .entry(stream.into())
                .and_modify(|last| {
                    last.0 = last.0.max(metadata.index);
                    last.1 = last.1.max(metadata.timestamp_us);
                })
                .or_insert((metadata.index, metadata.timestamp_us));
        }
        self.last_observed.insert(stream.into(),json!({"sequence":metadata.index,"device_timestamp_us":metadata.timestamp_us,"system_timestamp_us":metadata.system_timestamp_us,"width":metadata.width,"height":metadata.height,"format":format!("{:?}",metadata.format),"payload_bytes":metadata.data_size,"depth_scale_mm":metadata.depth_scale_mm,"received_mono_offset_ns":self.at(received).offset_ns,"received_unix_ns":received_unix_ns,"contract_valid":contract_valid}));
        let Some(snapshot) = &self.snapshot else {
            if !contract_valid {
                self.engine
                    .evaluate(
                        self.at(received),
                        quality_core::Scope::Stream(stream.into()),
                        quality_core::FrameViolation::FrameContractInvalid,
                        quality_core::Condition::Violated,
                    )
                    .map_err(anyhow::Error::msg)?;
            }
            return Ok(());
        };
        let b = &snapshot.content.binding;
        let context = capture_contract::FrameContext {
            version: 1,
            session_id: self.session.clone(),
            generation_id: b.generation_id.clone(),
            configuration_id: b.configuration_id.clone(),
            calibration_id: b.calibration_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            stream_id: stream.into(),
            sequence: Some(metadata.index),
            device_timestamp_us: Some(metadata.timestamp_us),
            system_timestamp_us: Some(metadata.system_timestamp_us),
            received_mono_offset_ns: self.at(received).offset_ns,
        };
        self.contexts.insert(stream.into(), context.clone());
        let observation = quality_core::FrameObservation {
            context,
            contract_valid,
            valid_fraction: None,
            zero_fraction: None,
            source: (!binding_invalid).then_some(quality_core::SourceStamp {
                clock: quality_core::SourceClock::Capture(
                    capture_contract::ClockDomain::SdkSystemUnixMicroseconds,
                ),
                value: metadata.system_timestamp_us,
            }),
            mapper: quality_core::ClockMapper::DeclaredSdkSystemUnix,
            received_unix_ns,
        };
        if stream == "depth" {
            self.engine.record_frame_deferred_pixels(observation)
        } else {
            self.engine.record_frame(observation)
        }
        .map_err(anyhow::Error::msg)
    }
    fn diagnostic(&mut self, kind: quality_core::DiagnosticKind) -> Result<()> {
        self.engine
            .record_diagnostic(self.at(Instant::now()), kind)
            .map_err(anyhow::Error::msg)
    }
    fn pixels(
        &mut self,
        source: frame_core::DepthSampleStats,
        output: Option<frame_core::DepthSampleStats>,
        contract_valid: bool,
    ) -> Result<()> {
        let convert = |s: frame_core::DepthSampleStats| quality_core::DepthPixelMetrics {
            samples: s.samples,
            zero_samples: s.zero_samples,
            nonzero_samples: s.nonzero_samples,
            max_u16_samples: s.max_u16_samples,
        };
        let pixels = convert(source);
        pixels.validate().map_err(anyhow::Error::msg)?;
        if let Some(last) = self.last_observed.get_mut("depth") {
            last["pixel_measurement"] = json!({"kind":"numerical-u16-nonzero-not-physical-certification","source_payload":pixels,"output_payload":output.map(convert),"output_scale_mm":output.map(|_|1.0)});
        }
        if let Some(context) = self.contexts.get("depth") {
            self.engine
                .record_pixel_metrics(
                    context,
                    pixels,
                    self.at(Instant::now()),
                    contract_valid && !self.binding_invalid,
                )
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
    fn publication(&mut self, ok: bool, completed_calls: u64) -> Result<()> {
        self.publication_calls = self.publication_calls.saturating_add(completed_calls);
        if !ok {
            self.diagnostic(quality_core::DiagnosticKind::PublicationFailed)?;
            if completed_calls > 0 {
                self.diagnostic(quality_core::DiagnosticKind::PublicationPartial)?;
            }
            self.publication_failures = self.publication_failures.saturating_add(1);
            if completed_calls > 0 {
                self.partial_publications = self.partial_publications.saturating_add(1);
            }
        }
        self.evaluate(
            quality_core::FrameViolation::PublishPartialFailure,
            if ok {
                quality_core::Condition::Healthy
            } else {
                quality_core::Condition::Violated
            },
        )
    }
    fn references(&self) -> Value {
        json!({"version":1,"state":self.engine.snapshot().state,"latest_event_sequence":self.engine.snapshot().latest_event_sequence,"current_path":self.directory.join("current.json"),"events_path":self.directory.join("events.json"),"query_path":self.directory.join("query.json"),"summary_path":self.finalized.then(||self.directory.join("summary.json"))})
    }
    fn persist(&mut self) -> Result<()> {
        let query = self
            .engine
            .query(quality_core::QualityQueryRequest {
                version: 1,
                query: quality_core::QualityQuery::Snapshot,
            })
            .map_err(anyhow::Error::msg)?;
        atomic_status(
            &self.directory.join("query.json"),
            &serde_json::to_value(query)?,
        )?;
        let current = self.engine.snapshot();
        atomic_status(
            &self.directory.join("current.json"),
            &json!({"version":1,"kind":"capture-service-quality","instance_id":self.instance,"session_id":self.session,"snapshot_id":self.snapshot.as_ref().map(|s|&s.snapshot_id),"source_age_mapping":"declared-sdk-system-unix","quality":current,"received_counts":self.receipts,"pre_snapshot_received_counts":self.unbound,"publication_calls":self.publication_calls,"publication_failures":self.publication_failures,"partial_publications":self.partial_publications,"last_observed":self.last_observed}),
        )?;
        let events = self
            .engine
            .events(None, self.policy.event_capacity)
            .map_err(anyhow::Error::msg)?;
        atomic_status(
            &self.directory.join("events.json"),
            &serde_json::to_value(events)?,
        )
    }
    fn finalize(&mut self) -> Result<()> {
        if !self.finalized {
            self.tick()?;
            let summary = self.engine.finalize_session();
            self.finalized = true;
            atomic_status(
                &self.directory.join("summary.json"),
                &json!({"version":1,"instance_id":self.instance,"summary":summary,"received_counts":self.receipts,"pre_snapshot_received_counts":self.unbound}),
            )?;
        }
        Ok(())
    }
}

fn measure_sdk_depth(frame: &orbbec_safe::Frame) -> Result<frame_core::DepthSampleStats> {
    let m = frame.metadata()?;
    ensure!(
        m.format == orbbec_safe::Format::Depth16,
        "depth format required"
    );
    Ok(frame_core::measure_depth_samples(
        frame.data()?,
        frame_core::FrameLayout {
            width: m.width,
            height: m.height,
            stride: m.width as usize * 2,
            format: frame_core::PixelFormat::Depth16,
        },
        false,
    )?)
}
fn missing_streams(
    color_present: bool,
    depth_present: bool,
) -> Option<(&'static str, quality_core::DiagnosticKind)> {
    match (color_present, depth_present) {
        (false, false) => Some(("MISSING_BOTH", quality_core::DiagnosticKind::MissingBoth)),
        (false, true) => Some(("MISSING_COLOR", quality_core::DiagnosticKind::MissingColor)),
        (true, false) => Some(("MISSING_DEPTH", quality_core::DiagnosticKind::MissingDepth)),
        (true, true) => None,
    }
}
fn pair_rejection(error: &frame_core::PairError) -> (&'static str, quality_core::FrameViolation) {
    use frame_core::PairError::*;
    match error {
        SkewExceeded => (
            "PAIR_SKEW_EXCEEDED",
            quality_core::FrameViolation::PairSkewExceeded,
        ),
        PairSequenceNotAdvancing
        | ColorSequenceNotAdvancing
        | DepthSequenceNotAdvancing
        | ColorTimestampNotAdvancing
        | DepthTimestampNotAdvancing => (
            "SOURCE_NOT_ADVANCING",
            quality_core::FrameViolation::TimestampRegression,
        ),
        InvalidPolicy => (
            "PAIR_BINDING_OR_POLICY_INVALID",
            quality_core::FrameViolation::FrameContractInvalid,
        ),
        _ => (
            "PAIR_FRAME_CONTRACT_INVALID",
            quality_core::FrameViolation::FrameContractInvalid,
        ),
    }
}
fn drain_with_quality(
    quality: Option<&mut ServiceQuality>,
    metrics: &mut Metrics,
    drain: impl FnOnce(&mut Metrics) -> Result<()>,
) -> Result<()> {
    let calls_before = metrics.publish_calls;
    let result = drain(metrics);
    if let Some(quality) = quality {
        quality.publication(
            result.is_ok(),
            metrics.publish_calls.saturating_sub(calls_before),
        )?;
    }
    result
}

struct Status<'a> {
    config: &'a ServiceConfig,
    session: String,
    instance: String,
    snapshot: Option<(String, String)>,
    heartbeat: u64,
    started: Instant,
    restarts: u64,
    last_failure: Option<&'static str>,
    quality: Option<ServiceQuality>,
    raw: Option<crate::raw_recording::RawRecorder>,
    usb_recovery: Option<Value>,
}
impl Status<'_> {
    fn observe_raw_quality(&mut self) -> Result<()> {
        if let Some(reason) = self
            .raw
            .as_mut()
            .and_then(crate::raw_recording::RawRecorder::failure_to_report)
        {
            if let Some(q) = &mut self.quality {
                q.diagnostic(quality_core::DiagnosticKind::RawRecordingFailed)?;
                q.engine
                    .evaluate(
                        q.at(Instant::now()),
                        quality_core::Scope::Recorder,
                        if reason == "RAW_QUEUE_OVERFLOW" {
                            quality_core::FrameViolation::QueueOverflow
                        } else {
                            quality_core::FrameViolation::FrameContractInvalid
                        },
                        quality_core::Condition::Violated,
                    )
                    .map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }
    fn write(
        &mut self,
        state: &str,
        reason: Option<&'static str>,
        metrics: &Metrics,
        last_pair: Option<Instant>,
    ) -> Result<()> {
        self.observe_raw_quality()?;
        self.heartbeat += 1;
        if reason.is_some() && reason != Some("SESSION_FAILED") {
            self.last_failure = reason;
        }
        if let Some(quality) = &mut self.quality {
            quality.persist()?;
        }
        atomic_status(
            &self.config.settings.status_path,
            &json!({"version":1,"kind":"capture-service-status","state":state,"ready":state=="ready","reason":reason,"last_failure_reason":self.last_failure,"session_id":self.session,"instance_id":self.instance,"snapshot_id":self.snapshot.as_ref().map(|s|&s.0),"snapshot_path":self.snapshot.as_ref().map(|s|&s.1),"quality":self.quality.as_ref().map(ServiceQuality::references),"usb_recovery":self.usb_recovery,"raw_recording":self.raw.as_ref().map(crate::raw_recording::RawRecorder::status),"generation":self.config.settings.generation,"config_sha256":self.config.hash,"pid":std::process::id(),"heartbeat_sequence":self.heartbeat,"updated_unix_ms":unix_ms(),"uptime_ms":self.started.elapsed().as_millis(),"restart_attempt":self.restarts,"last_pair_age_ms":last_pair.map(|v|v.elapsed().as_millis()),"pairs_accepted":metrics.accepted,"pairs_rejected":metrics.rejected,"pairs_published":metrics.published_pairs,"individual_publish_calls":metrics.publish_calls,"publish_failures":metrics.publish_failures,"stamp_failures":metrics.stamp_failures,"framesets_observed":metrics.framesets,"missing_pairs":metrics.missing,"skew_rejections":metrics.skew,"monotonic_rejections":metrics.monotonic,"poll_timeouts":metrics.timeouts,"physical_sync_verified":false,"delivery_verified":false}),
        )
    }
}
fn arm_parent_death(expected_parent: u32) -> Result<()> {
    // Docker ENTRYPOINT supervisors legitimately own PID 1 in their namespace.
    ensure!(expected_parent >= 1, "supervisor PID required");
    #[cfg(target_os = "linux")]
    unsafe {
        // Register before any config/SDK work. Checking the caller-supplied PID
        // after registration closes the parent-died-before-prctl race.
        ensure!(
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == 0,
            "parent death signal setup failed"
        );
        ensure!(
            libc::getppid() == expected_parent as libc::pid_t,
            "supervisor already exited"
        );
    }
    Ok(())
}
fn worker(config: &ServiceConfig, restart: u64) -> Result<()> {
    let mut status = Status {
        config,
        session: random_session()?,
        instance: capture_contract::InstanceId::parse(
            std::env::var("GDP_CAPTURE_INSTANCE_ID").context("supervisor instance ID required")?,
        )?
        .to_string(),
        snapshot: None,
        heartbeat: 0,
        started: Instant::now(),
        restarts: restart,
        last_failure: None,
        quality: None,
        raw: None,
        usb_recovery: None,
    };
    status.quality = config
        .quality_runtime
        .as_ref()
        .map(|policy| {
            ServiceQuality::new(
                policy.clone(),
                &status.session,
                &status.instance,
                status.started,
                &config.settings.status_path,
            )
        })
        .transpose()?;
    status.raw = config
        .raw_settings
        .clone()
        .map(|settings| crate::raw_recording::RawRecorder::new(settings, &status.session));
    let mut metrics = Metrics::default();
    let mut last_pair = None;
    status.write("starting", None, &metrics, last_pair)?;
    let outcome = (|| -> Result<()> {
        ensure!(
            cfg!(target_os = "linux") && cfg!(target_endian = "little"),
            "supported SDK host required"
        );
        config.verify_runtime()?;
        if let Some(recovery) = &config.usb_recovery {
            ensure!(
                !recording_blocks_restart(config.settings.recording_control_path.as_deref()),
                "recording protection blocks USB recovery"
            );
            status.usb_recovery = Some(json!({"version":1,"state":"running"}));
            status.write("starting", None, &metrics, last_pair)?;
            match recovery.execute(&config.capture.serial) {
                Ok(result) => status.usb_recovery = Some(result),
                Err(error) => {
                    status.last_failure = Some("USB_STREAM_RECOVERY_FAILED");
                    status.usb_recovery = Some(json!({"version":1,"state":"failed"}));
                    return Err(error);
                }
            }
            status.write("starting", None, &metrics, last_pair)?;
            ensure!(
                !STOP.load(Ordering::Relaxed)
                    && !recording_blocks_restart(config.settings.recording_control_path.as_deref()),
                "capture stopped or protected after USB recovery"
            );
        }
        let sdk = match orbbec_safe::Sdk::load(&config.capture.sdk_library) {
            Ok(sdk) => sdk,
            Err(error) => {
                if let Some(quality) = &mut status.quality {
                    quality.evaluate(
                        quality_core::FrameViolation::DeviceDisconnected,
                        quality_core::Condition::Violated,
                    )?;
                }
                return Err(error.into());
            }
        };
        let mut pipeline = match sdk.open(
            &config.capture.serial,
            Some(config.capture.color.profile),
            Some(config.capture.depth.profile),
        ) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                if let Some(quality) = &mut status.quality {
                    quality.evaluate(
                        quality_core::FrameViolation::DeviceDisconnected,
                        quality_core::Condition::Violated,
                    )?;
                }
                return Err(error.into());
            }
        };
        let mut ros = None;
        let mut queue = LatestQueue::new(config.capture.capacity)?;
        let mut guard = PairGuard::default();
        let mut bound_guard: Option<frame_core::BoundPairGuard> = None;
        let opened = Instant::now();
        let warmup = Duration::from_secs_f64(config.capture.startup_discovery_seconds);
        let mut heartbeat = Instant::now();
        let mut consecutive = 0_u64;
        let mut had_ready = false;
        let mut sequence = 0_u64;
        let result = (|| -> Result<()> {
            while !STOP.load(Ordering::Relaxed) {
                if let Some(quality) = &mut status.quality {
                    quality.tick()?;
                }
                let startup = opened.elapsed() < warmup;
                let mut state = if had_ready && consecutive >= config.settings.ready_min_pairs {
                    "ready"
                } else if had_ready {
                    "degraded"
                } else {
                    "starting"
                };
                if heartbeat.elapsed() >= Duration::from_millis(config.settings.heartbeat_ms) {
                    status.write(state, None, &metrics, last_pair)?;
                    heartbeat = Instant::now();
                }
                ensure!(
                    had_ready
                        || status.started.elapsed()
                            < Duration::from_millis(config.settings.startup_timeout_ms),
                    "startup deadline"
                );
                if !startup
                    && last_pair.is_some_and(|v: Instant| {
                        v.elapsed() > Duration::from_millis(config.settings.idle_timeout_ms)
                    })
                {
                    status.write("degraded", Some("STREAM_TIMEOUT"), &metrics, last_pair)?;
                    anyhow::bail!("stream timeout");
                }
                let set = match pipeline.wait(config.capture.poll_ms) {
                    Ok(Some(set)) => set,
                    Ok(None) => {
                        metrics.timeouts += 1;
                        continue;
                    }
                    Err(error) => {
                        if let Some(quality) = &mut status.quality {
                            quality.evaluate(
                                quality_core::FrameViolation::DeviceDisconnected,
                                quality_core::Condition::Violated,
                            )?;
                        }
                        return Err(error.into());
                    }
                };
                sequence += 1;
                metrics.framesets += 1;
                let received = Instant::now();
                if startup {
                    if let Some(raw) = &status.raw {
                        raw.observe_excluded(true);
                    }
                }
                let color = set.color()?;
                let depth = set.depth()?;
                let received_unix_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|d| u64::try_from(d.as_nanos()).ok());
                let color_metadata = color.as_ref().and_then(|f| f.metadata().ok());
                let depth_metadata = depth.as_ref().and_then(|f| f.metadata().ok());
                if let Some(quality) = &mut status.quality {
                    quality.source_progress_invalid = false;
                    quality.binding_invalid = quality.snapshot.as_ref().is_some_and(|snapshot| {
                        let scale = snapshot
                            .content
                            .configuration
                            .get("source_depth_scale_mm")
                            .and_then(Value::as_f64);
                        depth_metadata.as_ref().is_some_and(|m| {
                            scale.is_some_and(|s| m.depth_scale_mm != Some(s as f32))
                        }) || [
                            ("color", color_metadata.as_ref()),
                            ("depth", depth_metadata.as_ref()),
                        ]
                        .iter()
                        .any(|(stream, m)| {
                            m.is_some_and(|m| {
                                snapshot
                                    .content
                                    .calibration
                                    .models
                                    .get(*stream)
                                    .is_some_and(|expected| {
                                        (expected.width, expected.height) != (m.width, m.height)
                                    })
                            })
                        })
                    });
                    for (stream, frame, metadata, profile, format) in [
                        (
                            "color",
                            color.as_ref(),
                            color_metadata.as_ref(),
                            config.capture.color.profile,
                            orbbec_safe::Format::Yuyv,
                        ),
                        (
                            "depth",
                            depth.as_ref(),
                            depth_metadata.as_ref(),
                            config.capture.depth.profile,
                            orbbec_safe::Format::Depth16,
                        ),
                    ] {
                        if frame.is_some() {
                            let valid = metadata.is_some_and(|m| {
                                m.format == format
                                    && m.width == profile.width
                                    && m.height == profile.height
                                    && (m.width as usize)
                                        .checked_mul(m.height as usize)
                                        .and_then(|n| n.checked_mul(2))
                                        == Some(m.data_size)
                                    && (stream != "depth"
                                        || m.depth_scale_mm
                                            .is_some_and(|v| v.is_finite() && v > 0.))
                            });
                            quality.receipt(stream, metadata, received, received_unix_ns, valid)?;
                        }
                    }
                    if quality.source_progress_invalid {
                        quality.evaluate(
                            quality_core::FrameViolation::TimestampRegression,
                            quality_core::Condition::Violated,
                        )?;
                    }
                    if let (Some(c), Some(d)) = (&color_metadata, &depth_metadata) {
                        let (c, d) = match config.capture.policy.clock {
                            frame_core::PairClock::Device => (c.timestamp_us, d.timestamp_us),
                            frame_core::PairClock::System => {
                                (c.system_timestamp_us, d.system_timestamp_us)
                            }
                        };
                        if c.abs_diff(d) > config.capture.policy.max_delta_us {
                            quality.diagnostic(quality_core::DiagnosticKind::PairSkew)?;
                        }
                        quality
                            .engine
                            .observe_pair_delta(quality.at(Instant::now()), c.abs_diff(d))
                            .map_err(anyhow::Error::msg)?;
                    }
                }
                if let Some((missing_reason, missing_kind)) =
                    missing_streams(color.is_some(), depth.is_some())
                {
                    metrics.missing += 1;
                    metrics.reject(missing_reason);
                    if !startup {
                        if let Some(raw) = &mut status.raw {
                            raw.observe_excluded(false);
                            if raw.active() {
                                raw.fail(missing_reason);
                            }
                        }
                    }
                    if let Some(quality) = &mut status.quality {
                        quality.diagnostic(missing_kind)?;
                        if let Some(frame) = depth.as_ref() {
                            match measure_sdk_depth(frame) {
                                Ok(stats) => quality.pixels(stats, None, true)?,
                                Err(_) => {
                                    quality.diagnostic(
                                        quality_core::DiagnosticKind::PixelPayloadInvalid,
                                    )?;
                                }
                            }
                        }
                        quality.evaluate(
                            quality_core::FrameViolation::FrameContractInvalid,
                            quality_core::Condition::Violated,
                        )?;
                    }
                    consecutive = 0;
                    if !startup {
                        status.write("degraded", Some(missing_reason), &metrics, last_pair)?;
                    }
                    continue;
                }
                if ros.is_none() {
                    ros = rgbd::setup_ros(&config.capture, &set.calibration()?)?;
                    if let Some(output) = &mut ros {
                        output.color.set_session_id(&status.session)?;
                        output.depth.set_session_id(&status.session)?;
                        output.aux.set_session_id(&status.session)?;
                    }
                }
                if status.raw.is_some()
                    && bound_guard.is_none()
                    && config.capture.quality_policy.is_some()
                    && color_metadata.as_ref().is_some_and(|m| {
                        m.format == orbbec_safe::Format::Yuyv
                            && (m.width, m.height)
                                == (
                                    config.capture.color.profile.width,
                                    config.capture.color.profile.height,
                                )
                    })
                    && depth_metadata.as_ref().is_some_and(|m| {
                        m.format == orbbec_safe::Format::Depth16
                            && (m.width, m.height)
                                == (
                                    config.capture.depth.profile.width,
                                    config.capture.depth.profile.height,
                                )
                            && m.depth_scale_mm.is_some_and(|v| v.is_finite() && v > 0.)
                    })
                {
                    let instance = std::env::var("GDP_CAPTURE_INSTANCE_ID")
                        .context("explicit runtime instance required for pair evidence")?;
                    let snapshot = crate::capture_snapshot::build(
                        &config.capture,
                        &set.calibration()?,
                        crate::capture_snapshot::RuntimeBinding {
                            generation: &config.settings.generation,
                            session: &status.session,
                            instance: &instance,
                            runtime_sha: &config.hash,
                            runtime_contract: config.runtime_contract.as_ref(),
                        },
                        depth_metadata
                            .as_ref()
                            .and_then(|m| m.depth_scale_mm)
                            .context("depth scale required")?,
                    )?;
                    let directory = config
                        .settings
                        .status_path
                        .parent()
                        .context("status directory")?
                        .join("snapshots");
                    let store = capture_contract::store::SnapshotStore::open(&directory)?;
                    let id = store.publish(&snapshot)?;
                    status.snapshot = Some((
                        id.to_string(),
                        directory
                            .join(format!("{id}.json"))
                            .to_string_lossy()
                            .into(),
                    ));
                    if let Some(quality) = &mut status.quality {
                        quality.bind(&snapshot)?;
                    }
                    if let Some(raw) = &mut status.raw {
                        if raw
                            .initialize(
                                &snapshot,
                                status.started,
                                depth_metadata
                                    .as_ref()
                                    .and_then(|m| m.depth_scale_mm)
                                    .context("raw source scale")?,
                            )
                            .is_err()
                        {
                            raw.fail("RAW_INITIALIZATION_FAILED");
                        }
                    }
                    bound_guard = Some(frame_core::BoundPairGuard::new(snapshot, status.started)?);
                }
                if !startup {
                    if let Some(raw) = &mut status.raw {
                        raw.observe_excluded(false);
                        if raw
                            .capture(
                                color.as_ref().context("color frame")?,
                                depth.as_ref().context("depth frame")?,
                                sequence,
                                received,
                                config.capture.policy,
                            )
                            .is_err()
                        {
                            raw.fail("RAW_NATIVE_PAIR_REJECTED");
                        }
                    }
                }
                status.observe_raw_quality()?;
                let packets = (|| -> Result<_> {
                    Ok((
                        rgbd::packet(
                            color.unwrap(),
                            &config.capture.color,
                            false,
                            false,
                            received,
                            &mut metrics,
                        )?,
                        rgbd::packet_measured(
                            depth.unwrap(),
                            &config.capture.depth,
                            true,
                            config.capture.normalize,
                            received,
                            &mut metrics,
                            status.quality.is_some(),
                        )?,
                    ))
                })();
                let (color, depth) = match packets {
                    Ok((color, measured)) => {
                        if let Some(quality) = &mut status.quality {
                            if let Some(source) = measured.source_pixels {
                                quality.pixels(source, measured.output_pixels, true)?;
                            }
                        }
                        (color, measured.packet)
                    }
                    Err(_) => {
                        metrics.reject("FRAME_CONTRACT_REJECTED");
                        if let Some(quality) = &mut status.quality {
                            quality
                                .diagnostic(quality_core::DiagnosticKind::PixelPayloadInvalid)?;
                            quality.evaluate(
                                quality_core::FrameViolation::FrameContractInvalid,
                                quality_core::Condition::Violated,
                            )?;
                        }
                        consecutive = 0;
                        if !startup {
                            status.write(
                                "degraded",
                                Some("FRAME_CONTRACT_REJECTED"),
                                &metrics,
                                last_pair,
                            )?;
                        }
                        continue;
                    }
                };
                if bound_guard.is_none() && config.capture.quality_policy.is_some() {
                    let instance = std::env::var("GDP_CAPTURE_INSTANCE_ID")
                        .context("explicit runtime instance required for pair evidence")?;
                    let snapshot = crate::capture_snapshot::build(
                        &config.capture,
                        &set.calibration()?,
                        crate::capture_snapshot::RuntimeBinding {
                            generation: &config.settings.generation,
                            session: &status.session,
                            instance: &instance,
                            runtime_sha: &config.hash,
                            runtime_contract: config.runtime_contract.as_ref(),
                        },
                        depth
                            .metadata
                            .source_depth_scale_mm
                            .context("depth scale required")?,
                    )?;
                    let directory = config
                        .settings
                        .status_path
                        .parent()
                        .context("status directory")?
                        .join("snapshots");
                    let store = capture_contract::store::SnapshotStore::open(&directory)?;
                    let id = store.publish(&snapshot)?;
                    status.snapshot = Some((
                        id.to_string(),
                        directory
                            .join(format!("{id}.json"))
                            .to_string_lossy()
                            .into(),
                    ));
                    if let Some(quality) = &mut status.quality {
                        quality.bind(&snapshot)?;
                    }
                    bound_guard = Some(frame_core::BoundPairGuard::new(snapshot, status.started)?);
                }
                let lengths = (color.payload.data()?.len(), depth.payload.data()?.len());
                let result = if let Some(guard) = bound_guard.as_mut() {
                    guard
                        .accept(
                            sequence,
                            Some(color),
                            Some(depth),
                            lengths,
                            config.capture.policy,
                            PairProvenance::CallerObservedSdkFrameset,
                        )
                        .map(|b| (b.pair, Some(b.evidence)))
                        .map_err(|e| match e {
                            frame_core::BoundPairError::Pair(p) => p,
                            _ => frame_core::PairError::InvalidPolicy,
                        })
                } else {
                    guard
                        .accept(
                            sequence,
                            Some(color),
                            Some(depth),
                            lengths,
                            config.capture.policy,
                            PairProvenance::CallerObservedSdkFrameset,
                        )
                        .map(|pair| (pair, None))
                };
                let (pair, bound_evidence) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        if matches!(error, frame_core::PairError::SkewExceeded) {
                            metrics.skew += 1;
                        } else if pair_rejection(&error).1
                            == quality_core::FrameViolation::TimestampRegression
                        {
                            metrics.monotonic += 1;
                        }
                        metrics.reject(pair_rejection(&error).0);
                        if let Some(quality) = &mut status.quality {
                            quality.diagnostic(quality_core::DiagnosticKind::PairRejected)?;
                            // Raw pair delta/progression were already observed once
                            // for this frameset; a guard rejection must not count a
                            // second observation or shorten configured hysteresis.
                            if !matches!(error, frame_core::PairError::SkewExceeded)
                                && !quality.source_progress_invalid
                            {
                                quality.evaluate(
                                    pair_rejection(&error).1,
                                    quality_core::Condition::Violated,
                                )?;
                            }
                        }
                        consecutive = 0;
                        if !startup {
                            status.write(
                                "degraded",
                                Some("PAIR_CONTRACT_REJECTED"),
                                &metrics,
                                last_pair,
                            )?;
                        }
                        continue;
                    }
                };
                if startup {
                    continue;
                }
                metrics.accepted += 1;
                if let Some(quality) = &mut status.quality {
                    quality.evaluate(
                        quality_core::FrameViolation::FrameContractInvalid,
                        quality_core::Condition::Healthy,
                    )?;
                    if !quality.source_progress_invalid {
                        quality.evaluate(
                            quality_core::FrameViolation::TimestampRegression,
                            quality_core::Condition::Healthy,
                        )?;
                    }
                }
                let evicted = queue.push(QueuedPair {
                    bound_evidence,
                    pair,
                    enqueued_at: Instant::now(),
                });
                if let Some(quality) = &mut status.quality {
                    if evicted.is_some() {
                        quality.diagnostic(quality_core::DiagnosticKind::QueueOverflow)?;
                    }
                    quality.evaluate(
                        quality_core::FrameViolation::QueueOverflow,
                        if evicted.is_some() {
                            quality_core::Condition::Violated
                        } else {
                            quality_core::Condition::Healthy
                        },
                    )?;
                }
                drop(evicted);
                drain_with_quality(status.quality.as_mut(), &mut metrics, |metrics| {
                    rgbd::drain(&mut queue, &mut ros, metrics)
                })?;
                last_pair = Some(Instant::now());
                consecutive += 1;
                if consecutive >= config.settings.ready_min_pairs {
                    state = "ready";
                    had_ready = true;
                }
                if state == "ready" && consecutive == config.settings.ready_min_pairs {
                    status.write(state, None, &metrics, last_pair)?;
                    heartbeat = Instant::now();
                }
            }
            Ok(())
        })();
        while queue.pop().is_some() {}
        // This native stop can hang; the parent supervisor enforces shutdown deadline.
        let stopped = pipeline.stop();
        result?;
        stopped?;
        Ok(())
    })();
    if let Err(error) = &outcome {
        // SDK details may contain device identities. This bounded record stays
        // in the owner-only status directory; public stderr remains redacted.
        let detail = format!("{error:#}");
        let truncated = detail.chars().count() > 4096;
        let detail: String = detail.chars().take(4096).collect();
        let path = config
            .settings
            .status_path
            .parent()
            .context("status directory")?
            .join(format!("failure-{}.json", status.session));
        // Diagnostics must not mask the original acquisition failure if storage
        // is unavailable. The usual status/finalization path still runs below.
        let _ = atomic_status(
            &path,
            &json!({"version":1,
            "kind":"capture-service-private-failure","session_id":status.session,
            "error_chain":detail,"details_truncated":truncated}),
        );
    }
    if let Some(raw) = &mut status.raw {
        raw.finish_session(outcome.is_ok())?;
    }
    status.observe_raw_quality()?;
    if let Some(quality) = &mut status.quality {
        if outcome.is_err() && quality.engine.snapshot().state == quality_core::QualityState::Normal
        {
            quality.evaluate(
                quality_core::FrameViolation::FrameContractInvalid,
                quality_core::Condition::Violated,
            )?;
        }
        quality.finalize()?;
    }
    if STOP.load(Ordering::Relaxed) && outcome.is_ok() {
        status.write("stopped", Some("SIGNAL"), &metrics, last_pair)?;
        Ok(())
    } else {
        status.write("degraded", Some("SESSION_FAILED"), &metrics, last_pair)?;
        outcome.and_then(|_| anyhow::bail!("unexpected session end"))
    }
}
fn signal_child(child: &Child, signal: libc::c_int) {
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}
#[cfg(target_os = "linux")]
fn adopt_worker_descendants() -> Result<()> {
    // PID 1 already adopts orphans; standalone supervisors need the same local
    // descendant ownership. This does not adopt unrelated host processes.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "worker subreaper setup failed"
    );
    Ok(())
}
#[cfg(target_os = "linux")]
fn reap_owned_group_descendants(
    group: libc::pid_t,
    leader: libc::pid_t,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let mut matching = false;
        let mut pending = false;
        // Linux may attach an adopted descendant to any live supervisor thread.
        // Enumerate only our own direct children, never arbitrary /proc PIDs.
        let mut children = std::collections::BTreeSet::new();
        for task in fs::read_dir("/proc/self/task")? {
            let path = task?.path().join("children");
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for pid in text.split_whitespace() {
                children.insert(pid.parse::<libc::pid_t>()?);
            }
        }
        for pid in children {
            if pid == leader || unsafe { libc::getpgid(pid) } != group {
                continue;
            }
            matching = true;
            let mut status = 0;
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == 0 {
                pending = true;
            }
            if result < 0 {
                pending = true;
                let error = std::io::Error::last_os_error();
                ensure!(
                    matches!(error.raw_os_error(), Some(libc::ECHILD) | Some(libc::EINTR)),
                    "owned descendant wait failed"
                );
            }
        }
        // The exited leader remains unreaped throughout the scan/cleanup. Its
        // PID/PGID cannot be recycled into an unrelated process group.
        if !matching {
            return Ok(());
        }
        if !pending {
            continue;
        }
        ensure!(
            started.elapsed() < timeout,
            "owned worker group cleanup deadline"
        );
        thread::sleep(Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())));
    }
}
fn reap_worker_exit(
    child: &mut Child,
    timeout: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    #[cfg(target_os = "linux")]
    {
        let leader = libc::pid_t::try_from(child.id())?;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                return Ok(None);
            }
            return Err(error.into());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        ensure!(
            unsafe { info.si_pid() } == leader && unsafe { libc::getpgid(leader) } == leader,
            "worker process group ownership changed"
        );
        let killed = unsafe { libc::kill(-leader, libc::SIGKILL) };
        if killed < 0 {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
                "owned worker group kill failed"
            )
        }
        reap_owned_group_descendants(leader, leader, timeout)?;
        // Reap the anchor only after all adopted same-group descendants are gone.
        Ok(Some(child.wait()?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = timeout;
        Ok(child.try_wait()?)
    }
}
fn terminate(child: &mut Child, timeout: Duration) -> Result<()> {
    signal_child(child, libc::SIGTERM);
    let started = Instant::now();
    loop {
        if reap_worker_exit(child, timeout.saturating_sub(started.elapsed()))?.is_some() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            signal_child(child, libc::SIGKILL);
            // Wait observation remains non-reaping so group ownership stays
            // anchored even when SIGTERM/SIGKILL race with normal worker exit.
            let killed_at = Instant::now();
            loop {
                if reap_worker_exit(child, timeout.saturating_sub(killed_at.elapsed()))?.is_some() {
                    return Ok(());
                }
                ensure!(killed_at.elapsed() < timeout, "killed worker exit deadline");
                thread::sleep(
                    Duration::from_millis(5).min(timeout.saturating_sub(killed_at.elapsed())),
                );
            }
        }
        thread::sleep(Duration::from_millis(25).min(timeout.saturating_sub(started.elapsed())));
    }
}
fn wait_interruptible(duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration && !STOP.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(25));
    }
}
fn supervisor_status(
    config: &ServiceConfig,
    state: &str,
    reason: &str,
    restarts: u64,
    instance_id: &str,
) -> Result<()> {
    let mut value = fs::read_to_string(&config.settings.status_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"version":1,"kind":"capture-service-status"}));
    if value["reason"]
        .as_str()
        .is_some_and(|v| v != "WORKER_EXITED" && v != "SESSION_FAILED")
    {
        value["worker_reason"] = value["reason"].clone();
    }
    value["ready"] = false.into();
    value["state"] = state.into();
    value["reason"] = reason.into();
    value["supervisor_pid"] = std::process::id().into();
    value["instance_id"] = instance_id.into();
    value["restart_attempt"] = restarts.into();
    value["updated_unix_ms"] = unix_ms().into();
    atomic_status(&config.settings.status_path, &value)
}
// Any present/unreadable gate is protection, including malformed/symlink input.
fn recording_blocks_restart(path: Option<&Path>) -> bool {
    path.is_some_and(
        |p| !matches!(fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
    )
}
fn acknowledge_recording(config: &ServiceConfig, worker_pid: u32) {
    let Some(path) = config.settings.recording_control_path.as_deref() else {
        return;
    };
    let operation = (|| -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 65536,
            "recording control file invalid"
        );
        let request = capture_contract::recording::GateRequest::parse(&fs::read(path)?)?;
        let status: Value =
            serde_json::from_str(&fs::read_to_string(&config.settings.status_path)?)?;
        let ack = request.acknowledge(&status, worker_pid, std::process::id(), &config.hash)?;
        let ack = serde_json::to_value(ack)?;
        let ack_path = path.with_file_name("ack.json");
        if fs::read(&ack_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .as_ref()
            == Some(&ack)
        {
            return Ok(());
        }
        atomic_status(&ack_path, &ack)
    })();
    // Missing, malformed or stale controls are never acknowledged. Presence
    // still prevents recovery; a management timeout keeps its durable guard.
    let _ = operation;
}
fn supervisor(config: &ServiceConfig, path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    adopt_worker_descendants()?;
    let parent = config
        .settings
        .status_path
        .parent()
        .context("status parent")?;
    fs::create_dir_all(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join("service.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "service already running"
    );
    let instance_id = format!("{}{}", random_session()?, random_session()?);
    let mut restarts = 0_u64;
    loop {
        ensure!(
            sha(&fs::read(path)?) == config.hash,
            "immutable config changed"
        );
        if STOP.load(Ordering::Relaxed) {
            supervisor_status(config, "stopped", "SIGNAL", restarts, &instance_id)?;
            return Ok(());
        }
        while recording_blocks_restart(config.settings.recording_control_path.as_deref()) {
            supervisor_status(
                config,
                "degraded",
                "RECORDING_PROTECTED_RECOVERY_BLOCKED",
                restarts,
                &instance_id,
            )?;
            if STOP.load(Ordering::Relaxed) {
                supervisor_status(config, "stopped", "SIGNAL", restarts, &instance_id)?;
                return Ok(());
            }
            wait_interruptible(Duration::from_millis(100));
        }
        let mut child = Command::new(std::env::current_exe()?)
            .env("GDP_CAPTURE_INSTANCE_ID", &instance_id)
            .args(["--worker", "--config"])
            .arg(path)
            .args([
                "--expected-config-sha",
                &config.hash,
                "--restart-attempt",
                &restarts.to_string(),
                "--supervisor-pid",
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let mut last_change = Instant::now();
        let mut marker = None;
        let reason = loop {
            acknowledge_recording(config, child.id());
            if STOP.load(Ordering::Relaxed) {
                terminate(
                    &mut child,
                    Duration::from_millis(config.settings.shutdown_timeout_ms),
                )?;
                break "SIGNAL";
            }
            if let Some(exit) = reap_worker_exit(
                &mut child,
                Duration::from_millis(config.settings.shutdown_timeout_ms),
            )? {
                break if exit.success() {
                    "WORKER_STOPPED"
                } else {
                    "WORKER_EXITED"
                };
            }
            if let Ok(raw) = fs::read_to_string(&config.settings.status_path) {
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    if value["pid"].as_u64() == Some(u64::from(child.id())) {
                        let next = (
                            value["session_id"].as_str().unwrap_or("").to_owned(),
                            value["heartbeat_sequence"].as_u64().unwrap_or(0),
                        );
                        if marker.as_ref() != Some(&next) {
                            marker = Some(next);
                            last_change = Instant::now();
                        }
                    }
                }
            }
            if last_change.elapsed() > Duration::from_millis(config.settings.heartbeat_timeout_ms) {
                terminate(
                    &mut child,
                    Duration::from_millis(config.settings.shutdown_timeout_ms),
                )?;
                break "NATIVE_HEARTBEAT_TIMEOUT";
            }
            thread::sleep(Duration::from_millis(50));
        };
        supervisor_status(
            config,
            if STOP.load(Ordering::Relaxed) {
                "stopped"
            } else {
                "degraded"
            },
            reason,
            restarts,
            &instance_id,
        )?;
        if STOP.load(Ordering::Relaxed) {
            // A signal can arrive after the degraded snapshot above was written.
            // A successful shutdown must commit its final state before returning.
            supervisor_status(config, "stopped", "SIGNAL", restarts, &instance_id)?;
            return Ok(());
        }
        ensure!(
            restarts < config.settings.max_restarts,
            "restart budget exhausted"
        );
        restarts += 1;
        wait_interruptible(Duration::from_millis(
            config
                .settings
                .restart_backoff_ms
                .saturating_mul(restarts.min(10)),
        ));
    }
}
pub fn main_entry() {
    let result = (|| -> Result<()> {
        install_signals()?;
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|s| s == "--validate-config") {
            ensure!(
                args.len() == 2 && Path::new(&args[1]).is_absolute(),
                "absolute config path required"
            );
            let config = ServiceConfig::parse(&fs::read_to_string(&args[1])?)?;
            config.verify_runtime()?;
            println!(
                "{}",
                json!({"version":1,"kind":"capture-service-config-validation","valid":true,"runtime_contract_verified":config.runtime_contract.is_some(),"usb_recovery_artifact_verified":config.usb_recovery.is_some(),"hardware_verified":false})
            );
            return Ok(());
        }
        let worker_mode = args.first().is_some_and(|s| s == "--worker");
        if worker_mode {
            ensure!(
                args.len() == 9 && args[7] == "--supervisor-pid",
                "invalid worker parent binding"
            );
            arm_parent_death(args[8].parse()?)?;
        }
        let offset = usize::from(worker_mode);
        ensure!(
            args.len() >= offset + 2 && args[offset] == "--config",
            "config required"
        );
        let path = PathBuf::from(&args[offset + 1]);
        ensure!(path.is_absolute(), "absolute private config required");
        let config = ServiceConfig::parse(&fs::read_to_string(&path)?)?;
        if worker_mode {
            ensure!(
                args.len() == 9
                    && args[3] == "--expected-config-sha"
                    && args[4] == config.hash
                    && args[5] == "--restart-attempt",
                "invalid worker invocation"
            );
            worker(&config, args[6].parse()?)
        } else {
            ensure!(args.len() == 2, "unexpected service argument");
            supervisor(&config, &path)
        }
    })();
    if result.is_err() {
        eprintln!(
            "{}",
            json!({"version":1,"kind":"capture-service","ok":false,"error":"CAPTURE_SERVICE_FAILED","scope":"private-diagnostics-redacted"})
        );
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Value {
        json!({"version":1,"sdk_library":"/private/missing-sdk.so","serial":"private-device-never-output","color":{"width":640,"height":480,"fps":30,"frame_id":"color_optical"},"depth":{"width":640,"height":480,"fps":30,"frame_id":"depth_optical","unit_policy":"normalize-mm"},"pairing":{"clock":"device","max_delta_us":20000},"poll_ms":100,"capacity":4,"startup_discovery_seconds":0,"ros":{"bridge":"/private/missing-bridge.so","namespace":"/test","stamp_source":"sdk-system-unix"},"service":{"status_path":"/tmp/private-capture/status.json","generation":"test-generation","idle_timeout_ms":2000,"startup_timeout_ms":15000,"heartbeat_ms":500,"heartbeat_timeout_ms":10000,"max_restarts":3,"restart_backoff_ms":1000,"shutdown_timeout_ms":1000,"ready_min_pairs":5}})
    }
    fn runtime_policy() -> quality_core::RuntimeQualityPolicy {
        quality_core::RuntimeQualityPolicy {
            version: 1,
            quality: capture_contract::QualityPolicy {
                version: 1,
                max_source_age_ns: 1_000_000_000,
                max_future_ns: 100_000_000,
                max_pair_delta_us: 20_000,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: capture_contract::QualityAction::ReportOnly,
            },
            stale_after_ns: 1_000_000,
            event_capacity: 8,
            window_capacity: 8,
            window_horizon_ns: 10_000_000_000,
            summary_capacity: 1,
            max_sessions: 1,
            rules: quality_core::FrameViolation::ALL
                .into_iter()
                .map(|v| {
                    (
                        v,
                        quality_core::RulePolicy {
                            open_after: 1,
                            recover_after: 2,
                            severity: quality_core::QualityState::Degraded,
                        },
                    )
                })
                .collect(),
        }
    }
    fn quality_snapshot(
        session: &str,
        instance: &str,
    ) -> capture_contract::snapshot::SnapshotEnvelope {
        use capture_contract::{snapshot::*, *};
        let session = SessionId::parse(session).unwrap();
        let instance = InstanceId::parse(instance).unwrap();
        let calibration = CalibrationRecord {
            version: 1,
            source: CalibrationSource::SdkActiveProfiles,
            actual_runtime_observed: true,
            source_session_id: Some(session.clone()),
            source_instance_id: Some(instance.clone()),
            observed_unix_ns: Some(1),
            models: [("color", "color_frame"), ("depth", "depth_frame")]
                .into_iter()
                .map(|(name, frame)| {
                    (
                        name.into(),
                        CameraModel {
                            frame_id: frame.into(),
                            width: 2,
                            height: 1,
                            k: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                            distortion_model: DistortionModel::None,
                            distortion_coefficients: vec![],
                        },
                    )
                })
                .collect(),
            transforms: vec![],
            physical_accuracy_verified: false,
            legacy_source_sha256: None,
        };
        let configuration = json!({"synthetic_test_fixture":true});
        CaptureSnapshot {
            version: 1,
            binding: SnapshotBinding {
                instance_id: instance,
                session_id: session,
                generation_id: GenerationId::parse("c".repeat(32)).unwrap(),
                configuration_id: ConfigurationId::parse(
                    content_digest(
                        ContentKind::Configuration,
                        &serde_json::to_vec(&configuration).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap(),
                calibration_id: calibration.id().unwrap(),
            },
            configuration,
            calibration,
            depth: DepthRepresentation::Millimetres {
                quantization:
                    QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
            },
            quality_policy: runtime_policy().quality,
        }
        .seal()
        .unwrap()
    }
    #[test]
    fn service_quality_policy_is_explicit_consistent_and_snapshot_configuration_bound() {
        let mut value = config();
        let policy = runtime_policy();
        value["quality_policy"] = serde_json::to_value(&policy.quality).unwrap();
        value["quality_runtime"] = serde_json::to_value(&policy).unwrap();
        let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
        assert_eq!(
            parsed.capture.configuration["quality_runtime"],
            value["quality_runtime"]
        );
        value["quality_runtime"]["quality"]["max_pair_delta_us"] = json!(1);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
        value["quality_runtime"] = serde_json::to_value(&policy).unwrap();
        value["quality_runtime"]["event_capacity"] = json!(0);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
    }
    #[test]
    fn service_adapter_receipts_staleness_partial_publication_and_private_summary_are_real() {
        let dir =
            std::env::temp_dir().join(format!("quality-adapter-{}", random_session().unwrap()));
        let session = "a".repeat(32);
        let instance = "b".repeat(64);
        let mut quality = ServiceQuality::new(
            runtime_policy(),
            &session,
            &instance,
            Instant::now(),
            &dir.join("status.json"),
        )
        .unwrap();
        let mut metadata = orbbec_safe::FrameMetadata {
            format: orbbec_safe::Format::Depth16,
            width: 2,
            height: 1,
            index: 1,
            timestamp_us: 10,
            system_timestamp_us: 1_000_000,
            data_size: 4,
            depth_scale_mm: Some(1.),
        };
        quality
            .receipt(
                "depth",
                Some(&metadata),
                Instant::now(),
                Some(1_000_000_000),
                true,
            )
            .unwrap();
        assert_eq!(quality.receipts["depth"], 1);
        assert_eq!(quality.unbound["depth"], 1);
        assert_eq!(quality.engine.snapshot().streams["depth"].frame_count, 0);
        quality
            .bind(&quality_snapshot(&session, &instance))
            .unwrap();
        metadata.index = 2;
        metadata.timestamp_us = 20;
        metadata.system_timestamp_us += 1;
        quality
            .receipt(
                "depth",
                Some(&metadata),
                Instant::now(),
                Some(1_000_001_000),
                true,
            )
            .unwrap();
        assert_eq!(quality.engine.snapshot().streams["depth"].frame_count, 1);
        let mut metrics = Metrics::default();
        assert!(
            drain_with_quality(Some(&mut quality), &mut metrics, |metrics| {
                metrics.publish_calls += 1;
                anyhow::bail!("synthetic second publisher failed")
            })
            .is_err()
        );
        assert!(
            drain_with_quality(Some(&mut quality), &mut metrics, |metrics| {
                metrics.publish_calls += 2;
                anyhow::bail!("synthetic metadata publisher failed")
            })
            .is_err()
        );
        assert_eq!(quality.partial_publications, 2);
        assert_eq!(quality.publication_calls, 3);
        let events = quality.engine.events(None, 8).unwrap();
        assert_eq!(
            events
                .events
                .iter()
                .filter(|e| e.violation == quality_core::FrameViolation::PublishPartialFailure)
                .count(),
            1
        );
        quality.publication(true, 4).unwrap();
        assert!(
            quality
                .engine
                .snapshot()
                .active_episodes
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::PublishPartialFailure)
        );
        quality.publication(true, 4).unwrap();
        assert!(
            !quality
                .engine
                .snapshot()
                .active_episodes
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::PublishPartialFailure)
        );
        thread::sleep(Duration::from_millis(2));
        quality.tick().unwrap();
        assert!(
            quality
                .engine
                .snapshot()
                .active_episodes
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::StreamStale)
        );
        quality.persist().unwrap();
        quality.finalize().unwrap();
        quality.persist().unwrap();
        let current: Value =
            serde_json::from_slice(&fs::read(quality.directory.join("current.json")).unwrap())
                .unwrap();
        assert_eq!(current["received_counts"]["depth"], 2);
        assert_eq!(current["quality"]["current_session_finalized"], true);
        assert_eq!(current["last_observed"]["depth"]["sequence"], 2);
        assert_eq!(current["partial_publications"], 2);
        assert!(quality.directory.join("summary.json").is_file());
        assert!(quality.directory.join("events.json").is_file());
        assert_eq!(
            fs::metadata(quality.directory.join("current.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runtime_pixels_are_numerical_raw_counts_once_and_scale_changes_are_not_timestamp_faults() {
        let directory =
            std::env::temp_dir().join(format!("quality-pixels-{}", random_session().unwrap()));
        let session = "a".repeat(32);
        let instance = "b".repeat(64);
        let mut policy = runtime_policy();
        policy.quality.min_valid_fraction = 0.75;
        policy.quality.max_zero_fraction = 0.25;
        let mut content = quality_snapshot(&session, &instance).content;
        content.quality_policy = policy.quality.clone();
        content.configuration["source_depth_scale_mm"] = json!(0.1f32);
        content.binding.configuration_id = capture_contract::ConfigurationId::parse(
            capture_contract::content_digest(
                capture_contract::ContentKind::Configuration,
                &serde_json::to_vec(&content.configuration).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let snapshot = content.seal().unwrap();
        let mut q = ServiceQuality::new(
            policy,
            &session,
            &instance,
            Instant::now(),
            &directory.join("status.json"),
        )
        .unwrap();
        q.bind(&snapshot).unwrap();
        let mut m = orbbec_safe::FrameMetadata {
            format: orbbec_safe::Format::Depth16,
            width: 2,
            height: 1,
            index: 5,
            timestamp_us: 50,
            system_timestamp_us: 1_000_000,
            data_size: 4,
            depth_scale_mm: Some(0.1),
        };
        q.receipt("depth", Some(&m), Instant::now(), Some(1_000_000_000), true)
            .unwrap();
        let layout = frame_core::FrameLayout {
            width: 2,
            height: 1,
            stride: 4,
            format: frame_core::PixelFormat::Depth16,
        };
        let raw = [0, 0, 255, 255];
        let normalized = frame_core::normalize_depth_mm(&raw, layout, false, 0.1).unwrap();
        q.pixels(
            normalized.stats.source_samples(),
            Some(normalized.stats.output_samples()),
            true,
        )
        .unwrap();
        let state = q.engine.snapshot();
        assert_eq!(state.streams["depth"].frame_count, 1);
        assert_eq!(q.receipts["depth"], 1);
        assert_eq!(
            state.streams["depth"].window_metrics.valid_fraction,
            Some(0.5)
        );
        assert_eq!(
            state.streams["depth"].window_metrics.max_u16_fraction,
            Some(0.5)
        );
        assert!(
            state
                .active_episodes
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::FrameContractInvalid)
        );
        assert_eq!(
            q.last_observed["depth"]["pixel_measurement"]["source_payload"]["max_u16_samples"],
            1
        );
        assert_eq!(
            q.last_observed["depth"]["pixel_measurement"]["output_payload"]["max_u16_samples"],
            0
        );
        assert_eq!(raw, [0, 0, 255, 255]);
        m.depth_scale_mm = Some(0.2);
        m.index = 1;
        m.timestamp_us = 1;
        m.system_timestamp_us = 1;
        q.receipt("depth", Some(&m), Instant::now(), Some(2_000), true)
            .unwrap();
        let state = q.engine.snapshot();
        assert_eq!(
            state.diagnostics.session_counts[&quality_core::DiagnosticKind::DepthScaleChanged],
            1
        );
        assert!(
            !state
                .active_episodes
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::TimestampRegression)
        );
        assert_eq!(
            pair_rejection(&frame_core::PairError::InvalidPolicy).1,
            quality_core::FrameViolation::FrameContractInvalid
        );
        assert_eq!(q.receipts["depth"], 2);
        assert_eq!(q.receipts["color"], 0);
        assert!(state.streams["color"].window_metrics.depth_pixels.is_none());
    }
    #[test]
    fn absent_stream_causes_are_stable_and_do_not_fabricate_other_route_frames() {
        assert_eq!(
            missing_streams(false, true),
            Some(("MISSING_COLOR", quality_core::DiagnosticKind::MissingColor))
        );
        assert_eq!(
            missing_streams(true, false),
            Some(("MISSING_DEPTH", quality_core::DiagnosticKind::MissingDepth))
        );
        assert_eq!(
            missing_streams(false, false),
            Some(("MISSING_BOTH", quality_core::DiagnosticKind::MissingBoth))
        );
        assert_eq!(missing_streams(true, true), None);
    }
    #[test]
    fn raw_recording_is_opt_in_requires_quality_and_keeps_configuration_content() {
        let mut value = config();
        assert!(
            ServiceConfig::parse(&value.to_string())
                .unwrap()
                .raw_settings
                .is_none()
        );
        value["raw_recording"] = json!({"directory":"/private/native","name":"capture","queue_capacity":2,"max_pairs":30,"max_bytes":1_000_000});
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
        let policy = runtime_policy();
        value["quality_policy"] = serde_json::to_value(&policy.quality).unwrap();
        value["quality_runtime"] = serde_json::to_value(policy).unwrap();
        let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
        assert!(parsed.raw_settings.is_some());
        assert_eq!(
            parsed.capture.configuration["raw_recording"],
            value["raw_recording"]
        );
        value["raw_recording"]["max_pairs"] = json!(0);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
    }
    #[test]
    fn service_config_requires_explicit_bounded_policy() {
        let mut value = config();
        assert!(ServiceConfig::parse(&value.to_string()).is_ok());
        value["service"]["max_restarts"] = json!(21);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
        value = config();
        value["seconds"] = json!(5);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
        value = config();
        value["service"]["heartbeat_timeout_ms"] = json!(500);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
    }
    #[test]
    fn service_policy_numeric_limits_include_endpoints_and_reject_neighbors() {
        for (field, minimum, maximum) in [
            ("idle_timeout_ms", 100_u64, 60_000_u64),
            ("startup_timeout_ms", 1_000, 120_000),
            ("heartbeat_ms", 50, 5_000),
            ("heartbeat_timeout_ms", 500, 120_000),
            ("max_restarts", 0, 20),
            ("restart_backoff_ms", 100, 30_000),
            ("shutdown_timeout_ms", 100, 30_000),
            ("ready_min_pairs", 1, 10_000),
        ] {
            // Keep related limits valid so each rejected value exercises its own bound.
            let mut value = config();
            value["service"]["idle_timeout_ms"] = json!(100);
            value["service"]["startup_timeout_ms"] = json!(120_000);
            value["service"]["heartbeat_ms"] = json!(50);
            value["service"]["heartbeat_timeout_ms"] = json!(120_000);
            for valid in [minimum, maximum] {
                value["service"][field] = json!(valid);
                let result = ServiceConfig::parse(&value.to_string());
                assert!(result.is_ok(), "{field}={valid}: {:?}", result.err());
            }
            for invalid in minimum.checked_sub(1).into_iter().chain([maximum + 1]) {
                value["service"][field] = json!(invalid);
                let error = ServiceConfig::parse(&value.to_string())
                    .err()
                    .unwrap_or_else(|| panic!("accepted {field}={invalid}"));
                assert_eq!(
                    error.to_string(),
                    "service policy bounds",
                    "{field}={invalid}"
                );
            }
        }
    }
    #[test]
    fn service_policy_limits_require_unsigned_integers() {
        for field in [
            "idle_timeout_ms",
            "startup_timeout_ms",
            "heartbeat_ms",
            "heartbeat_timeout_ms",
            "max_restarts",
            "restart_backoff_ms",
            "shutdown_timeout_ms",
            "ready_min_pairs",
        ] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!("1000"),
                json!(true),
                Value::Null,
            ] {
                let mut value = config();
                value["service"][field] = invalid.clone();
                let error = ServiceConfig::parse(&value.to_string())
                    .err()
                    .unwrap_or_else(|| panic!("accepted {field}={invalid}"));
                assert_eq!(error.to_string(), "unsigned service limit required");
            }
        }
    }
    #[test]
    fn service_idle_timeout_cannot_precede_poll_interval() {
        for (idle_ms, valid) in [(499, false), (500, true), (501, true)] {
            let mut value = config();
            value["poll_ms"] = json!(500);
            value["service"]["idle_timeout_ms"] = json!(idle_ms);
            assert_eq!(
                ServiceConfig::parse(&value.to_string()).is_ok(),
                valid,
                "idle timeout {idle_ms} ms with a 500 ms poll"
            );
        }
    }
    #[test]
    fn service_startup_deadline_must_follow_idle_timeout() {
        for (startup_ms, valid) in [(1_999, false), (2_000, false), (2_001, true)] {
            let mut value = config();
            value["service"]["startup_timeout_ms"] = json!(startup_ms);
            assert_eq!(
                ServiceConfig::parse(&value.to_string()).is_ok(),
                valid,
                "startup deadline {startup_ms} ms with a 2000 ms idle timeout"
            );
        }
    }
    #[test]
    fn service_heartbeat_deadline_allows_exactly_three_intervals() {
        for (timeout_ms, valid) in [(1_499, false), (1_500, true), (1_501, true)] {
            let mut value = config();
            value["service"]["heartbeat_timeout_ms"] = json!(timeout_ms);
            assert_eq!(
                ServiceConfig::parse(&value.to_string()).is_ok(),
                valid,
                "heartbeat timeout {timeout_ms} ms with a 500 ms interval"
            );
        }
    }
    #[test]
    fn service_startup_discovery_must_finish_before_deadline() {
        for (seconds, valid) in [(9.999, true), (10.0, false), (10.001, false)] {
            let mut value = config();
            value["service"]["startup_timeout_ms"] = json!(10_000);
            value["startup_discovery_seconds"] = json!(seconds);
            let result = ServiceConfig::parse(&value.to_string());
            assert_eq!(result.is_ok(), valid, "discovery allowance {seconds} s");
            if !valid {
                assert_eq!(
                    result.err().unwrap().to_string(),
                    "startup allowance exceeds deadline"
                );
            }
        }
    }
    #[test]
    fn service_status_path_requires_private_absolute_parent() {
        // Parsing must remain independent of whether these directories exist.
        for (path, valid) in [
            ("/tmp/private-capture/status.json", true),
            ("/var/lib/gdp/status.json", true),
            ("/private/gdp/status.json", true),
            ("", false),
            ("status.json", false),
            ("relative/status.json", false),
            ("/", false),
            ("/status.json", false),
            ("/tmp/status.json", false),
        ] {
            let mut value = config();
            value["service"]["status_path"] = json!(path);
            assert_eq!(
                ServiceConfig::parse(&value.to_string()).is_ok(),
                valid,
                "{path}"
            );
        }
    }
    #[test]
    fn service_status_path_rejects_parent_traversal_before_io() {
        for path in [
            "/tmp/private-capture/../status.json",
            "/var/private-capture/../../status.json",
            "/tmp/../private-capture/status.json",
        ] {
            let mut value = config();
            value["service"]["status_path"] = json!(path);
            // Never invoke atomic_status: these inputs could chmod a shared ancestor.
            let error = ServiceConfig::parse(&value.to_string())
                .err()
                .unwrap_or_else(|| panic!("accepted status path {path}"));
            assert_eq!(
                error.to_string(),
                "private absolute status directory required"
            );
        }
    }
    #[test]
    fn service_policy_requires_exact_fields_and_an_object() {
        let baseline = config();
        for field in baseline["service"].as_object().unwrap().keys() {
            let mut value = baseline.clone();
            value["service"].as_object_mut().unwrap().remove(field);
            let error = ServiceConfig::parse(&value.to_string())
                .err()
                .unwrap_or_else(|| panic!("accepted missing service field {field}"));
            assert_eq!(error.to_string(), "service fields required");
        }
        let mut value = baseline.clone();
        value["service"]["unexpected"] = json!(true);
        assert_eq!(
            ServiceConfig::parse(&value.to_string())
                .err()
                .unwrap()
                .to_string(),
            "service fields required"
        );
        for invalid in [Value::Null, json!([]), json!("policy"), json!(1)] {
            let mut value = baseline.clone();
            value["service"] = invalid;
            assert_eq!(
                ServiceConfig::parse(&value.to_string())
                    .err()
                    .unwrap()
                    .to_string(),
                "service object required"
            );
        }
    }
    #[test]
    fn service_generation_requires_bounded_ascii_token() {
        for generation in ["generation_01-abc".to_owned(), "a".repeat(128)] {
            let mut value = config();
            value["service"]["generation"] = json!(generation);
            let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
            assert_eq!(parsed.settings.generation, generation);
        }
        for generation in [
            String::new(),
            "a".repeat(129),
            "generation/name".into(),
            "generation name".into(),
            "generation\nname".into(),
            "generation\0name".into(),
            "生成标识".into(),
        ] {
            let mut value = config();
            value["service"]["generation"] = json!(generation);
            assert_eq!(
                ServiceConfig::parse(&value.to_string())
                    .err()
                    .unwrap()
                    .to_string(),
                "opaque generation required"
            );
        }
    }
    #[test]
    fn service_configuration_size_limit_includes_exact_boundary() {
        let mut raw = config().to_string();
        raw.push_str(&" ".repeat(64 * 1024 - raw.len()));
        assert_eq!(raw.len(), 64 * 1024);
        assert!(ServiceConfig::parse(&raw).is_ok());
        raw.push(' ');
        assert_eq!(
            ServiceConfig::parse(&raw).err().unwrap().to_string(),
            "config too large"
        );
    }
    #[test]
    fn service_configuration_requires_ros_output() {
        let mut value = config();
        value.as_object_mut().unwrap().remove("ros");
        assert_eq!(
            ServiceConfig::parse(&value.to_string())
                .err()
                .unwrap()
                .to_string(),
            "resident service requires real ROS output"
        );
    }
    #[test]
    fn usb_recovery_configuration_is_preserved_and_rejects_invalid_identity() {
        let mut value = config();
        value["usb_recovery"] = json!({"helper":"/private/helper","sha256":"a".repeat(64)});
        let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
        assert_eq!(
            parsed.capture.configuration["usb_recovery"],
            value["usb_recovery"]
        );
        value["serial"] = json!("bad\nidentity");
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
    }
    #[test]
    fn runtime_contract_requires_exact_hash_fields_and_actual_bytes() {
        let mut value = config();
        value["runtime_contract"] = json!({"sdk_sha256":"a".repeat(64),"bridge_sha256":"b".repeat(64),"service_sha256":"c".repeat(64)});
        let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
        assert!(parsed.verify_runtime().is_err());
        value["runtime_contract"]["unexpected"] = json!(true);
        assert!(ServiceConfig::parse(&value.to_string()).is_err());
    }
    #[test]
    fn zero_parent_pid_is_rejected() {
        assert!(arm_parent_death(0).is_err());
    }
    #[test]
    fn sessions_are_independent_128bit_lowercase_hex() {
        assert_eq!(hex(&[0, 1, 15, 16, 128, 255]), "00010f1080ff");
        assert_eq!(
            sha(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let a = random_session().unwrap();
        let b = random_session().unwrap();
        assert_eq!(a.len(), 32);
        assert!(
            a.bytes()
                .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
        );
        assert_ne!(a, b);
    }
    #[test]
    fn any_present_recording_control_blocks_restart_including_corrupt_or_symlink() {
        let directory =
            std::env::temp_dir().join(format!("capture-gate-test-{}", random_session().unwrap()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("control.json");
        assert!(!recording_blocks_restart(Some(&path)));
        fs::write(&path, b"malformed").unwrap();
        assert!(recording_blocks_restart(Some(&path)));
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(directory.join("missing"), &path).unwrap();
        assert!(recording_blocks_restart(Some(&path)));
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn status_is_atomic_private_and_redacted() {
        let dir =
            std::env::temp_dir().join(format!("capture-status-test-{}", random_session().unwrap()));
        let mut value = config();
        value["service"]["status_path"] = json!(dir.join("status.json").to_str().unwrap());
        let parsed = ServiceConfig::parse(&value.to_string()).unwrap();
        let mut status = Status {
            config: &parsed,
            session: random_session().unwrap(),
            instance: "c".repeat(64),
            snapshot: None,
            heartbeat: 0,
            started: Instant::now(),
            restarts: 0,
            last_failure: None,
            quality: None,
            raw: None,
            usb_recovery: None,
        };
        status
            .write(
                "degraded",
                Some("STREAM_TIMEOUT"),
                &Metrics::default(),
                None,
            )
            .unwrap();
        status
            .write(
                "degraded",
                Some("SESSION_FAILED"),
                &Metrics::default(),
                None,
            )
            .unwrap();
        let raw = fs::read_to_string(&parsed.settings.status_path).unwrap();
        assert!(!raw.contains("private-device"));
        let actual: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(actual["heartbeat_sequence"], 2);
        assert_eq!(actual["last_failure_reason"], "STREAM_TIMEOUT");
        assert_eq!(
            fs::metadata(&parsed.settings.status_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn group_ownership_mismatch_is_rejected_before_reaping_or_signalling_other_group() {
        let mut control = Command::new("sleep")
            .arg("10")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut worker = Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(control.id() as i32)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match reap_worker_exit(&mut worker, Duration::from_millis(100)) {
                Ok(None) => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
                Ok(Some(_)) => {
                    panic!("foreign group must not be reaped through supervised cleanup")
                }
            }
        }
        assert!(control.try_wait().unwrap().is_none());
        assert!(
            worker.wait().unwrap().success(),
            "WNOWAIT must preserve the rejected leader for its real owner"
        );
        control.kill().unwrap();
        control.wait().unwrap();
    }
    #[test]
    fn native_process_is_killed_when_graceful_deadline_expires() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        terminate(&mut child, Duration::from_millis(50)).unwrap();
        assert!(child.try_wait().unwrap().is_some());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn quality_adapter_rejects_rebinding_and_tracks_invalid_unbound_receipts() {
        let dir =
            std::env::temp_dir().join(format!("quality-boundaries-{}", random_session().unwrap()));
        let session = "a".repeat(32);
        let instance = "b".repeat(64);
        let mut q = ServiceQuality::new(
            runtime_policy(),
            &session,
            &instance,
            Instant::now(),
            &dir.join("status.json"),
        )
        .unwrap();
        q.receipt("depth", None, Instant::now(), None, false)
            .unwrap();
        let m = orbbec_safe::FrameMetadata {
            format: orbbec_safe::Format::Depth16,
            width: 2,
            height: 1,
            index: 1,
            timestamp_us: 10,
            system_timestamp_us: 1_000_000,
            data_size: 4,
            depth_scale_mm: Some(1.),
        };
        q.receipt(
            "depth",
            Some(&m),
            Instant::now(),
            Some(1_000_000_000),
            false,
        )
        .unwrap();
        assert_eq!(q.receipts["depth"], 2);
        assert_eq!(q.engine.snapshot().streams["depth"].frame_count, 0);
        let mut snapshot = quality_snapshot(&session, &instance);
        q.bind(&snapshot).unwrap();
        snapshot.content.calibration.observed_unix_ns = Some(2);
        snapshot.content.binding.calibration_id = snapshot.content.calibration.id().unwrap();
        let changed = snapshot.content.seal().unwrap();
        assert!(q.bind(&changed).is_err());
        let mut wrong = quality_snapshot(&"c".repeat(32), &instance);
        assert!(q.bind(&wrong).is_err());
        wrong = quality_snapshot(&session, &instance);
        wrong.content.quality_policy.max_pair_delta_us += 1;
        assert!(q.bind(&wrong).is_err());
        q.finalize().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pair_rejections_preserve_stable_quality_classifications() {
        use frame_core::PairError::*;
        for error in [
            PairSequenceNotAdvancing,
            ColorSequenceNotAdvancing,
            DepthSequenceNotAdvancing,
            ColorTimestampNotAdvancing,
            DepthTimestampNotAdvancing,
        ] {
            assert_eq!(
                pair_rejection(&error),
                (
                    "SOURCE_NOT_ADVANCING",
                    quality_core::FrameViolation::TimestampRegression
                )
            );
        }
        assert_eq!(
            pair_rejection(&SkewExceeded).1,
            quality_core::FrameViolation::PairSkewExceeded
        );
        assert_eq!(
            pair_rejection(&InvalidPolicy).0,
            "PAIR_BINDING_OR_POLICY_INVALID"
        );
        assert_eq!(
            pair_rejection(&MissingColor).0,
            "PAIR_FRAME_CONTRACT_INVALID"
        );
    }
    #[test]
    fn quality_adapter_propagates_storage_obstacles_and_rejects_rebinding_after_finalization() {
        for file in ["query.json", "current.json", "events.json", "summary.json"] {
            let dir = std::env::temp_dir().join(format!(
                "gdp-quality-obstacle-{}",
                random_session().unwrap()
            ));
            let mut q = ServiceQuality::new(
                runtime_policy(),
                &"a".repeat(32),
                &"b".repeat(64),
                Instant::now(),
                &dir.join("status.json"),
            )
            .unwrap();
            q.persist().unwrap();
            let path = q.directory.join(file);
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            fs::create_dir(&path).unwrap();
            assert!(
                if file == "summary.json" {
                    q.finalize()
                } else {
                    q.persist()
                }
                .is_err(),
                "{file}"
            );
            fs::remove_dir_all(dir).unwrap();
        }
        let dir =
            std::env::temp_dir().join(format!("gdp-quality-binding-{}", random_session().unwrap()));
        let mut q = ServiceQuality::new(
            runtime_policy(),
            &"a".repeat(32),
            &"b".repeat(64),
            Instant::now(),
            &dir.join("status.json"),
        )
        .unwrap();
        let snapshot = quality_snapshot(&"a".repeat(32), &"b".repeat(64));
        q.bind(&snapshot).unwrap();
        let meta = orbbec_safe::FrameMetadata {
            format: orbbec_safe::Format::Depth16,
            width: 3,
            height: 1,
            index: 1,
            timestamp_us: 100,
            system_timestamp_us: 100,
            data_size: 6,
            depth_scale_mm: Some(1.),
        };
        q.receipt("depth", Some(&meta), Instant::now(), Some(100000), true)
            .unwrap();
        assert!(q.binding_invalid);
        q.persist().unwrap();
        q.engine.finalize_session();
        let mut content = snapshot.content;
        content.calibration.observed_unix_ns = Some(2);
        content.binding.calibration_id = content.calibration.id().unwrap();
        assert!(q.bind(&content.seal().unwrap()).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn service_policy_rejects_unsafe_recording_control_paths_and_fail_closed_runtime_quality() {
        for path in [
            "relative/control.json",
            "/tmp/control.json",
            "/private/x/../control.json",
            "/private/x/not-control.json",
        ] {
            let mut v = config();
            v["service"]["recording_control_path"] = json!(path);
            assert!(ServiceConfig::parse(&v.to_string()).is_err());
        }
        let mut v = config();
        let mut policy = runtime_policy();
        policy.quality.action = capture_contract::QualityAction::FailClosed;
        v["quality_policy"] = serde_json::to_value(&policy.quality).unwrap();
        v["quality_runtime"] = serde_json::to_value(policy).unwrap();
        assert!(
            ServiceConfig::parse(&v.to_string())
                .err()
                .unwrap()
                .to_string()
                .contains("report-only")
        );
    }
    #[test]
    fn unbound_pixels_zero_publication_and_repeated_finalization_remain_explicit() {
        let dir =
            std::env::temp_dir().join(format!("gdp-quality-empty-{}", random_session().unwrap()));
        let mut q = ServiceQuality::new(
            runtime_policy(),
            &"a".repeat(32),
            &"b".repeat(64),
            Instant::now(),
            &dir.join("status.json"),
        )
        .unwrap();
        let samples = frame_core::DepthSampleStats {
            samples: 2,
            zero_samples: 1,
            nonzero_samples: 1,
            max_u16_samples: 0,
        };
        q.pixels(samples, None, true).unwrap();
        assert!(q.contexts.is_empty());
        q.publication(false, 0).unwrap();
        assert_eq!(q.partial_publications, 0);
        assert_eq!(q.publication_failures, 1);
        q.finalize().unwrap();
        q.finalize().unwrap();
        assert!(q.finalized);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rebinding_an_active_quality_session_is_reported_before_it_can_change_identity() {
        let dir = std::env::temp_dir().join(format!("gdp-rebinding-{}", random_session().unwrap()));
        let mut q = ServiceQuality::new(
            runtime_policy(),
            &"a".repeat(32),
            &"b".repeat(64),
            Instant::now(),
            &dir.join("status.json"),
        )
        .unwrap();
        let first = quality_snapshot(&"a".repeat(32), &"b".repeat(64));
        q.bind(&first).unwrap();
        let mut changed = first.content.clone();
        changed.calibration.observed_unix_ns = Some(2);
        changed.binding.calibration_id = changed.calibration.id().unwrap();
        assert!(
            q.bind(&changed.seal().unwrap())
                .unwrap_err()
                .to_string()
                .contains("quality snapshot changed")
        );
        assert_eq!(q.snapshot.as_ref().unwrap().snapshot_id, first.snapshot_id);
        assert!(
            q.engine
                .events(None, 8)
                .unwrap()
                .events
                .iter()
                .any(|e| e.violation == quality_core::FrameViolation::FrameContractInvalid)
        );
        q.finalize().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn already_reaped_worker_returns_a_real_child_ownership_error() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sh")
            .args(["-c", ":"])
            .process_group(0)
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(reap_worker_exit(&mut child, Duration::from_millis(100)).is_err());
    }
}
