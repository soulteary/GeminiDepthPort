//! Opt-in bounded owned native SDK capture. SDK handles never cross threads.
use anyhow::{Context, Result, ensure};
use capture_contract::{DepthRepresentation, snapshot::SnapshotEnvelope};
use frame_core::{
    BoundPairEvidence, BoundPairGuard, FrameEncoding, FrameLayout, FramePacket, PacketMetadata,
    PairPolicy, PairProvenance, PixelFormat,
};
use offline_depth::{StorageEndian, recording::RawRecordingWriter};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Instant,
};
#[derive(Clone)]
pub(crate) struct RawSettings {
    pub directory: PathBuf,
    pub name: String,
    pub queue_capacity: usize,
    pub max_pairs: u64,
    pub max_bytes: u64,
}
impl RawSettings {
    pub fn parse(value: &Value) -> Result<Self> {
        let m = value.as_object().context("raw recording object")?;
        ensure!(
            m.len() == 5
                && [
                    "directory",
                    "name",
                    "queue_capacity",
                    "max_pairs",
                    "max_bytes"
                ]
                .iter()
                .all(|k| m.contains_key(*k)),
            "explicit raw recording fields required"
        );
        let directory = PathBuf::from(value["directory"].as_str().context("raw directory")?);
        ensure!(
            directory.is_absolute()
                && directory != Path::new("/")
                && directory != Path::new("/tmp")
                && !directory
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "private absolute raw directory required"
        );
        let name = value["name"].as_str().context("raw name")?.to_owned();
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "raw name invalid"
        );
        let queue_capacity = usize::try_from(
            value["queue_capacity"]
                .as_u64()
                .context("raw queue capacity")?,
        )?;
        ensure!(
            (1..=16).contains(&queue_capacity),
            "raw queue capacity invalid"
        );
        let max_pairs = value["max_pairs"]
            .as_u64()
            .filter(|v| *v > 0)
            .context("raw pair limit")?;
        let max_bytes = value["max_bytes"]
            .as_u64()
            .filter(|v| *v > 0)
            .context("raw byte limit")?;
        Ok(Self {
            directory,
            name,
            queue_capacity,
            max_pairs,
            max_bytes,
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    Recording,
    Draining,
    Complete,
    Committing,
    Failed,
}
impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting-for-snapshot",
            Self::Recording => "recording",
            Self::Draining => "draining",
            Self::Complete => "complete",
            Self::Committing => "committing",
            Self::Failed => "failed",
        }
    }
}
struct Shared {
    state: State,
    reason: Option<&'static str>,
    submitted: u64,
    written: u64,
    payload_bytes: u64,
    overflows: u64,
    excluded_startup: u64,
    waiting_framesets: u64,
    native_snapshot: Option<String>,
    ros_snapshot: Option<String>,
}
struct OwnedPair {
    evidence: BoundPairEvidence,
    color: Vec<u8>,
    depth: Vec<u8>,
}
pub(crate) struct RawRecorder {
    settings: RawSettings,
    session: String,
    name: String,
    shared: Arc<Mutex<Shared>>,
    abort: Arc<AtomicBool>,
    sender: Option<SyncSender<OwnedPair>>,
    worker: Option<JoinHandle<()>>,
    guard: Option<BoundPairGuard>,
    reported_failure: bool,
    #[cfg(test)]
    test_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    test_commit_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    test_panic_worker: bool,
}
#[cfg(test)]
struct TestGate {
    started: std::sync::Barrier,
    resume: std::sync::Barrier,
}
impl RawRecorder {
    pub fn new(settings: RawSettings, session: &str) -> Self {
        Self {
            name: format!("{}-{session}", settings.name),
            settings,
            session: session.into(),
            shared: Arc::new(Mutex::new(Shared {
                state: State::Waiting,
                reason: None,
                submitted: 0,
                written: 0,
                payload_bytes: 0,
                overflows: 0,
                excluded_startup: 0,
                waiting_framesets: 0,
                native_snapshot: None,
                ros_snapshot: None,
            })),
            abort: Arc::new(AtomicBool::new(false)),
            sender: None,
            worker: None,
            guard: None,
            reported_failure: false,
            #[cfg(test)]
            test_gate: None,
            #[cfg(test)]
            test_commit_gate: None,
            #[cfg(test)]
            test_panic_worker: false,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn active(&self) -> bool {
        self.lock().state == State::Recording
    }
    pub fn fail(&mut self, reason: &'static str) {
        let mut shared = self.lock();
        if matches!(
            shared.state,
            State::Complete | State::Committing | State::Failed
        ) {
            return;
        }
        shared.state = State::Failed;
        shared.reason = Some(reason);
        drop(shared);
        self.abort.store(true, Ordering::Release);
        self.sender.take();
    }
    pub fn observe_excluded(&self, startup: bool) {
        let mut s = self.lock();
        if matches!(s.state, State::Waiting | State::Recording) {
            if startup {
                s.excluded_startup = s.excluded_startup.saturating_add(1)
            } else if s.state == State::Waiting {
                s.waiting_framesets = s.waiting_framesets.saturating_add(1)
            }
        }
    }
    pub fn initialize(
        &mut self,
        ros: &SnapshotEnvelope,
        origin: Instant,
        scale: f32,
    ) -> Result<()> {
        if self.lock().state != State::Waiting {
            return Ok(());
        }
        ros.require_runtime()?;
        ensure!(
            matches!(ros.content.depth, DepthRepresentation::Millimetres { .. }),
            "explicit source ROS millimetre snapshot required"
        );
        ensure!(
            ros.content
                .configuration
                .get("source_depth_scale_mm")
                .and_then(Value::as_f64)
                == Some(f64::from(scale)),
            "native scale differs from actual capture source"
        );
        ensure!(
            ros.content.binding.session_id.as_str() == self.session,
            "raw session binding"
        );
        ensure!(scale.is_finite() && scale > 0., "native scale invalid");
        let mut content = ros.content.clone();
        content.depth = DepthRepresentation::SdkCounts {
            scale_mm: f64::from(scale),
        };
        let native = content.seal()?;
        self.guard = Some(BoundPairGuard::new(native.clone(), origin)?);
        let (tx, rx) = mpsc::sync_channel::<OwnedPair>(self.settings.queue_capacity);
        let shared = Arc::clone(&self.shared);
        let abort = Arc::clone(&self.abort);
        let settings = self.settings.clone();
        let source_ros_snapshot = ros.clone();
        let name = self.name.clone();
        {
            let mut s = self.lock();
            s.state = State::Recording;
            s.native_snapshot = Some(native.snapshot_id.to_string());
            s.ros_snapshot = Some(ros.snapshot_id.to_string());
        }
        #[cfg(test)]
        let test_gate = self.test_gate.clone();
        #[cfg(test)]
        let test_commit_gate = self.test_commit_gate.clone();
        #[cfg(test)]
        let test_panic_worker = self.test_panic_worker;
        let worker = thread::Builder::new()
            .name("native-depth-recording".into())
            .spawn(move || {
                #[cfg(test)]
                if test_panic_worker {
                    panic!("injected raw writer panic");
                }
                let outcome = (|| -> Result<()> {
                    std::fs::create_dir_all(&settings.directory)?;
                    let mut writer = RawRecordingWriter::begin_with_limit(
                        &settings.directory,
                        &name,
                        native,
                        settings.max_bytes,
                    )?;
                    #[cfg(test)]
                    if let Some(gate) = test_gate {
                        gate.started.wait();
                        gate.resume.wait();
                    }
                    writer.attach_ros_snapshot(&source_ros_snapshot)?;
                    let mut written = 0u64;
                    while let Ok(pair) = rx.recv() {
                        ensure!(!abort.load(Ordering::Acquire), "raw aborted");
                        writer.append(
                            &pair.evidence,
                            &pair.color,
                            &pair.depth,
                            StorageEndian::Little,
                        )?;
                        written += 1;
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.written = written;
                        drop(state);
                    }
                    ensure!(
                        !abort.load(Ordering::Acquire) && written == settings.max_pairs,
                        "raw incomplete"
                    );
                    #[cfg(test)]
                    if let Some(gate) = test_commit_gate {
                        gate.started.wait();
                        gate.resume.wait();
                    }
                    writer.finish_guarded(|| {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if abort.load(Ordering::Acquire) || state.state == State::Failed {
                            return Err(offline_depth::OfflineError::Invalid(
                                "RAW_ABORTED_BEFORE_COMMIT",
                            ));
                        }
                        // This gate linearizes the completed capture transaction.
                        // A later SDK failure cannot undo an already committed job.
                        state.state = State::Committing;
                        Ok(state)
                    })?;
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    ensure!(s.state != State::Failed, "raw failed before commit");
                    s.state = State::Complete;
                    Ok(())
                })();
                if let Err(error) = outcome {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if s.state != State::Failed {
                        s.state = State::Failed;
                        s.reason = Some(
                            if error
                                .downcast_ref::<offline_depth::OfflineError>()
                                .is_some_and(|e| {
                                    matches!(
                                        e,
                                        offline_depth::OfflineError::Invalid(
                                            "RECORDING_BYTE_LIMIT"
                                        )
                                    )
                                })
                            {
                                "RAW_BYTE_LIMIT"
                            } else {
                                "RAW_STORAGE_OR_INCOMPLETE"
                            },
                        );
                    }
                    abort.store(true, Ordering::Release);
                }
            })?;
        self.sender = Some(tx);
        self.worker = Some(worker);
        Ok(())
    }
    pub fn capture(
        &mut self,
        color: &orbbec_safe::Frame,
        depth: &orbbec_safe::Frame,
        sequence: u64,
        received: Instant,
        policy: PairPolicy,
    ) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        let next_bytes = color
            .metadata()?
            .data_size
            .checked_add(depth.metadata()?.data_size)
            .context("raw payload overflow")? as u64;
        if self
            .lock()
            .payload_bytes
            .checked_add(next_bytes)
            .is_none_or(|bytes| bytes > self.settings.max_bytes)
        {
            self.fail("RAW_BYTE_LIMIT");
            anyhow::bail!("raw byte limit");
        }
        let color = copy_packet(color, false, received)?;
        let depth = copy_packet(depth, true, received)?;
        self.submit_owned(color, depth, sequence, policy)
    }
    fn submit_owned(
        &mut self,
        color: FramePacket<Vec<u8>>,
        depth: FramePacket<Vec<u8>>,
        sequence: u64,
        policy: PairPolicy,
    ) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        let lengths = (color.payload.len(), depth.payload.len());
        let pair = self.guard.as_mut().context("raw guard missing")?.accept(
            sequence,
            Some(color),
            Some(depth),
            lengths,
            policy,
            PairProvenance::CallerObservedSdkFrameset,
        )?;
        let payload_bytes = lengths
            .0
            .checked_add(lengths.1)
            .context("raw length overflow")? as u64;
        let message = OwnedPair {
            evidence: pair.evidence,
            color: pair.pair.color.payload,
            depth: pair.pair.depth.payload,
        };
        let result = self
            .sender
            .as_ref()
            .context("raw queue missing")?
            .try_send(message);
        match result {
            Ok(()) => {
                let target = self.settings.max_pairs;
                let mut s = self.lock();
                s.submitted += 1;
                s.payload_bytes = s.payload_bytes.saturating_add(payload_bytes);
                if s.state == State::Failed {
                    drop(s);
                    self.sender.take();
                    anyhow::bail!("raw writer failed while submitting")
                }
                let done = s.submitted == target;
                if done {
                    s.state = State::Draining
                }
                drop(s);
                if done {
                    self.sender.take();
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.lock().overflows += 1;
                self.fail("RAW_QUEUE_OVERFLOW");
                anyhow::bail!("raw queue overflow")
            }
            Err(TrySendError::Disconnected(_)) => {
                self.fail("RAW_STORAGE_OR_QUEUE_CLOSED");
                anyhow::bail!("raw queue closed")
            }
        }
    }
    pub fn finish_session(&mut self, successful: bool) -> Result<()> {
        let state = self.lock().state;
        if !matches!(state, State::Complete | State::Committing | State::Failed)
            && (!successful || state != State::Draining)
        {
            self.fail(if successful {
                "RAW_TARGET_INCOMPLETE"
            } else {
                "RAW_SDK_OR_SESSION_ERROR"
            });
        }
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.fail("RAW_WORKER_FAILED");
            }
        }
        Ok(())
    }
    pub fn failure_to_report(&mut self) -> Option<&'static str> {
        let reason = {
            let s = self.lock();
            if s.state == State::Failed {
                s.reason
            } else {
                None
            }
        };
        if reason.is_some() && !self.reported_failure {
            self.reported_failure = true;
            reason
        } else {
            None
        }
    }
    pub fn status(&self) -> Value {
        let s = self.lock();
        json!({"version":1,"state":s.state.as_str(),"reason":s.reason,"session_id":self.session,"max_pairs":self.settings.max_pairs,"max_bytes":self.settings.max_bytes,"queue_capacity":self.settings.queue_capacity,"submitted_pairs":s.submitted,"written_pairs":s.written,"payload_bytes_submitted":s.payload_bytes,"whole_pair_queue_overflows":s.overflows,"startup_framesets_excluded":s.excluded_startup,"waiting_framesets_excluded":s.waiting_framesets,"native_snapshot_id":s.native_snapshot,"ros_snapshot_id":s.ros_snapshot,"job_path":self.settings.directory.join(&self.name),"partial_path":self.settings.directory.join(format!("{}.partial",self.name)),"business_validity":null,"recording_protection_released":false})
    }
}
impl Drop for RawRecorder {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Release);
        self.sender.take();
    }
}
fn copy_packet(
    frame: &orbbec_safe::Frame,
    depth: bool,
    received: Instant,
) -> Result<FramePacket<Vec<u8>>> {
    let source = frame.metadata()?;
    let expected = if depth {
        orbbec_safe::Format::Depth16
    } else {
        orbbec_safe::Format::Yuyv
    };
    ensure!(
        source.format == expected
            && source.width > 0
            && source.height > 0
            && source.width <= 16384
            && source.height <= 16384,
        "raw stream format or dimensions"
    );
    let data = frame.data()?;
    ensure!(
        data.len() == source.data_size && data.len() <= 64 * 1024 * 1024,
        "raw payload length"
    );
    let scale = if depth { source.depth_scale_mm } else { None };
    let metadata = PacketMetadata {
        layout: FrameLayout {
            width: source.width,
            height: source.height,
            stride: source.width as usize * 2,
            format: if depth {
                PixelFormat::Depth16
            } else {
                PixelFormat::Yuyv
            },
        },
        encoding: if depth {
            FrameEncoding::Depth16
        } else {
            FrameEncoding::Yuyv
        },
        sequence: source.index,
        device_timestamp_us: Some(source.timestamp_us),
        system_timestamp_us: Some(source.system_timestamp_us),
        source_depth_scale_mm: scale,
        output_depth_scale_mm: scale,
        normalized: false,
        received_at: received,
    };
    metadata.validate(data.len())?;
    let mut payload = Vec::new();
    payload.try_reserve_exact(data.len())?;
    payload.extend_from_slice(data);
    Ok(FramePacket { metadata, payload })
}
#[cfg(test)]
mod tests {
    use super::*;
    use capture_contract::{snapshot::*, *};
    fn snapshot() -> SnapshotEnvelope {
        let session = SessionId::parse("a".repeat(32)).unwrap();
        let instance = InstanceId::parse("b".repeat(64)).unwrap();
        let calibration = CalibrationRecord {
            version: 1,
            source: CalibrationSource::SdkActiveProfiles,
            actual_runtime_observed: true,
            source_session_id: Some(session.clone()),
            source_instance_id: Some(instance.clone()),
            observed_unix_ns: Some(1000),
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
        let configuration = json!({"source_depth_scale_mm":f64::from(0.1f32),"synthetic":true});
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
            quality_policy: QualityPolicy {
                version: 1,
                max_source_age_ns: 1_000_000_000,
                max_future_ns: 100_000_000,
                max_pair_delta_us: 20_000,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: QualityAction::ReportOnly,
            },
        }
        .seal()
        .unwrap()
    }
    static FIXTURE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "raw-recorder-{}-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }
    fn settings(directory: &Path, max_pairs: u64, max_bytes: u64) -> RawSettings {
        RawSettings {
            directory: directory.into(),
            name: "native".into(),
            queue_capacity: 2,
            max_pairs,
            max_bytes,
        }
    }
    fn packet(depth: bool, n: u64, received: Instant) -> FramePacket<Vec<u8>> {
        let scale = depth.then_some(0.1);
        FramePacket {
            metadata: PacketMetadata {
                layout: FrameLayout {
                    width: 2,
                    height: 1,
                    stride: 4,
                    format: if depth {
                        PixelFormat::Depth16
                    } else {
                        PixelFormat::Yuyv
                    },
                },
                encoding: if depth {
                    FrameEncoding::Depth16
                } else {
                    FrameEncoding::Yuyv
                },
                sequence: if depth { 200 + n } else { 17 + n * 3 },
                device_timestamp_us: Some(n * 1000),
                system_timestamp_us: Some(1_000_000 + n * 1000),
                source_depth_scale_mm: scale,
                output_depth_scale_mm: scale,
                normalized: false,
                received_at: received,
            },
            payload: if depth {
                vec![0, 0, 255, 255]
            } else {
                vec![10, 20, 30, 40]
            },
        }
    }
    fn policy() -> PairPolicy {
        PairPolicy {
            clock: frame_core::PairClock::Device,
            max_delta_us: 20_000,
        }
    }
    #[test]
    fn bounded_native_target_finishes_independently_with_original_counts_and_distinct_snapshot() {
        let dir = directory();
        let ros = snapshot();
        let origin = Instant::now();
        let mut sink = RawRecorder::new(
            settings(&dir, 2, 1_000_000),
            ros.content.binding.session_id.as_str(),
        );
        sink.initialize(&ros, origin, 0.1).unwrap();
        for n in 1..=2 {
            sink.submit_owned(
                packet(false, n, Instant::now()),
                packet(true, n, Instant::now()),
                n,
                policy(),
            )
            .unwrap();
        }
        assert!(!sink.active());
        sink.finish_session(true).unwrap();
        let status = sink.status();
        assert_eq!(status["state"], "complete");
        assert_eq!(status["written_pairs"], 2);
        assert_ne!(status["native_snapshot_id"], status["ros_snapshot_id"]);
        assert_eq!(status["recording_protection_released"], false);
        let path = dir.join(&sink.name);
        assert!(path.join("ros-snapshot.json").is_file());
        let association: Value =
            serde_json::from_slice(&std::fs::read(path.join("snapshot-association.json")).unwrap())
                .unwrap();
        assert_eq!(association["ros_snapshot_id"], ros.snapshot_id.as_str());
        let mut recorded = offline_depth::recording::RecordingDirectorySource::open(&path).unwrap();
        assert_eq!(recorded.snapshot().content.binding, ros.content.binding);
        assert!(
            matches!(recorded.snapshot().content.depth,DepthRepresentation::SdkCounts{scale_mm} if scale_mm==f64::from(0.1f32))
        );
        for n in 1..=2 {
            let frame = recorded.next_frame().unwrap().unwrap();
            assert_eq!(frame.depth, [0, 0, 255, 255]);
            match frame.evidence {
                offline_depth::recording::TaggedEvidence::SdkBoundPair { pair, .. } => {
                    assert_eq!(pair.pair_sequence, n);
                    assert_eq!(pair.depth.context.sequence, Some(200 + n));
                    assert_eq!(pair.color.context.sequence, Some(17 + n * 3));
                    assert_eq!(
                        pair.depth.context.system_timestamp_us,
                        Some(1_000_000 + n * 1000)
                    );
                    assert!(!pair.depth.normalized);
                    assert_eq!(pair.depth.output_depth_scale_mm, Some(0.1));
                }
                _ => panic!("native evidence required"),
            }
        }
        assert!(recorded.next_frame().unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn raw_byte_budget_is_terminal_and_cannot_create_complete() {
        let dir = directory();
        let ros = snapshot();
        let mut sink = RawRecorder::new(
            settings(&dir, 2, 1),
            ros.content.binding.session_id.as_str(),
        );
        sink.initialize(&ros, Instant::now(), 0.1).unwrap();
        let started = Instant::now();
        while sink.status()["state"] != "failed" {
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        sink.finish_session(true).unwrap();
        assert_eq!(sink.status()["state"], "failed");
        assert_eq!(sink.status()["reason"], "RAW_BYTE_LIMIT");
        let stage = dir.join(format!("{}.partial", sink.name));
        assert!(stage.is_dir());
        assert!(!stage.join("COMPLETE").exists());
        assert!(!dir.join(&sink.name).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn actual_bounded_channel_overflow_and_sdk_abort_keep_partial_without_silent_completion() {
        for overflow in [true, false] {
            let dir = directory();
            let ros = snapshot();
            let mut cfg = settings(&dir, 3, 1_000_000);
            cfg.queue_capacity = 1;
            let mut sink = RawRecorder::new(cfg, ros.content.binding.session_id.as_str());
            let gate = Arc::new(TestGate {
                started: std::sync::Barrier::new(2),
                resume: std::sync::Barrier::new(2),
            });
            sink.test_gate = Some(Arc::clone(&gate));
            sink.initialize(&ros, Instant::now(), 0.1).unwrap();
            gate.started.wait();
            sink.submit_owned(
                packet(false, 1, Instant::now()),
                packet(true, 1, Instant::now()),
                1,
                policy(),
            )
            .unwrap();
            if overflow {
                assert!(
                    sink.submit_owned(
                        packet(false, 2, Instant::now()),
                        packet(true, 2, Instant::now()),
                        2,
                        policy()
                    )
                    .is_err()
                );
                assert_eq!(sink.status()["whole_pair_queue_overflows"], 1);
            } else {
                sink.fail("RAW_SDK_OR_SESSION_ERROR");
            }
            gate.resume.wait();
            sink.finish_session(false).unwrap();
            assert_eq!(sink.status()["state"], "failed");
            assert!(!dir.join(&sink.name).exists());
            let stage = dir.join(format!("{}.partial", sink.name));
            assert!(stage.is_dir());
            assert!(!stage.join("COMPLETE").exists());
            assert!(stage.join("INTERRUPTED.json").exists());
            assert!(sink.failure_to_report().is_some());
            assert!(sink.failure_to_report().is_none());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn explicit_raw_configuration_refuses_ambiguous_or_unbounded_settings() {
        let base = json!({"directory":"/private/raw","name":"sample","queue_capacity":2,"max_pairs":30,"max_bytes":1_000_000});
        assert!(RawSettings::parse(&base).is_ok());
        for (field, bad) in [
            ("queue_capacity", json!(0)),
            ("max_pairs", json!(0)),
            ("max_bytes", json!(0)),
            ("name", json!("../escape")),
            ("directory", json!("/tmp/../escape")),
        ] {
            let mut v = base.clone();
            v[field] = bad;
            assert!(RawSettings::parse(&v).is_err());
        }
        let mut v = base;
        v["unexpected"] = json!(true);
        assert!(RawSettings::parse(&v).is_err());
    }
    #[test]
    fn raw_initialization_refuses_wrong_provenance_and_finish_marks_unmet_target() {
        let dir = directory();
        let ros = snapshot();
        let mut sink = RawRecorder::new(
            settings(&dir, 2, 1_000_000),
            ros.content.binding.session_id.as_str(),
        );
        sink.observe_excluded(false);
        sink.observe_excluded(true);
        assert_eq!(sink.status()["waiting_framesets_excluded"], 1);
        assert!(
            sink.initialize(&ros, Instant::now(), 1.)
                .unwrap_err()
                .to_string()
                .contains("scale")
        );
        let mut counts = ros.content.clone();
        counts.depth = DepthRepresentation::SdkCounts {
            scale_mm: f64::from(0.1f32),
        };
        assert!(
            sink.initialize(&counts.seal().unwrap(), Instant::now(), 0.1)
                .unwrap_err()
                .to_string()
                .contains("millimetre")
        );
        let mut wrong = RawRecorder::new(settings(&dir, 2, 1_000_000), &"b".repeat(32));
        assert!(
            wrong
                .initialize(&ros, Instant::now(), 0.1)
                .unwrap_err()
                .to_string()
                .contains("session binding")
        );
        sink.finish_session(true).unwrap();
        assert_eq!(sink.status()["state"], "failed");
        assert_eq!(sink.failure_to_report(), Some("RAW_TARGET_INCOMPLETE"));
        assert!(sink.initialize(&ros, Instant::now(), 0.1).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn draining_is_observable_and_abort_at_the_commit_gate_never_creates_complete() {
        let dir = directory();
        let ros = snapshot();
        let gate = Arc::new(TestGate {
            started: std::sync::Barrier::new(2),
            resume: std::sync::Barrier::new(2),
        });
        let mut sink = RawRecorder::new(
            settings(&dir, 1, 1_000_000),
            ros.content.binding.session_id.as_str(),
        );
        sink.test_commit_gate = Some(gate.clone());
        sink.initialize(&ros, Instant::now(), 0.1).unwrap();
        sink.submit_owned(
            packet(false, 1, Instant::now()),
            packet(true, 1, Instant::now()),
            1,
            policy(),
        )
        .unwrap();
        gate.started.wait();
        assert_eq!(sink.status()["state"], "draining");
        sink.submit_owned(
            packet(false, 2, Instant::now()),
            packet(true, 2, Instant::now()),
            2,
            policy(),
        )
        .unwrap();
        assert_eq!(sink.status()["submitted_pairs"], 1);
        sink.fail("RAW_ABORTED_AT_COMMIT");
        gate.resume.wait();
        sink.finish_session(false).unwrap();
        assert_eq!(sink.status()["reason"], "RAW_ABORTED_AT_COMMIT");
        assert!(!dir.join(&sink.name).exists());
        assert!(!dir.join(format!("{}.partial/COMPLETE", sink.name)).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn writer_startup_failures_and_panics_close_the_pair_queue_without_a_complete_job() {
        for panic in [false, true] {
            let dir = directory();
            let ros = snapshot();
            let mut sink = RawRecorder::new(
                settings(&dir, 1, 1_000_000),
                ros.content.binding.session_id.as_str(),
            );
            sink.test_panic_worker = panic;
            if !panic {
                std::fs::create_dir(dir.join(format!("{}.partial", sink.name))).unwrap();
            }
            sink.initialize(&ros, Instant::now(), 0.1).unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while !sink.worker.as_ref().unwrap().is_finished() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            if panic {
                assert!(
                    sink.submit_owned(
                        packet(false, 1, Instant::now()),
                        packet(true, 1, Instant::now()),
                        1,
                        policy()
                    )
                    .is_err()
                );
                assert_eq!(sink.status()["reason"], "RAW_STORAGE_OR_QUEUE_CLOSED");
            } else {
                assert_eq!(sink.status()["reason"], "RAW_STORAGE_OR_INCOMPLETE");
            }
            sink.finish_session(false).unwrap();
            assert!(!dir.join(&sink.name).exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
