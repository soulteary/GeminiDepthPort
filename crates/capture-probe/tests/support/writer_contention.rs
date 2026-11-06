//! Test-only adapter: real service quality batches and persistence on the same
//! filesystem as the common raw writer, without an SDK/ROS claim.
use super::{ServiceQuality, persistence};
use capture_contract::snapshot::SnapshotEnvelope;
use offline_depth::recording::RawRecordingDiagnostics;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "../../../offline-depth/tests/writer_contention_benchmark.rs"]
mod common;

const SAMPLE_BYTES: usize = 64 * 1024 * 1024;
const HEARTBEAT: Duration = Duration::from_millis(500);
enum Control {
    Checkpoint(mpsc::SyncSender<Value>),
    Draining(mpsc::SyncSender<Value>),
    Finish(bool),
}
struct Background {
    enabled: bool,
    sender: mpsc::SyncSender<Control>,
    worker: Option<JoinHandle<Value>>,
}
impl Background {
    fn start(
        root: &Path,
        snapshot: &SnapshotEnvelope,
        diagnostics: RawRecordingDiagnostics,
        progress: Arc<common::BackgroundProgress>,
    ) -> Box<dyn common::BackgroundLoad> {
        let enabled = match std::env::var("GDP_CONTENTION_STATUS_LOAD").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => panic!("explicit GDP_CONTENTION_STATUS_LOAD=0|1 required"),
        };
        Self::start_mode(root, snapshot, diagnostics, progress, enabled)
    }
    fn start_mode(
        root: &Path,
        snapshot: &SnapshotEnvelope,
        diagnostics: RawRecordingDiagnostics,
        progress: Arc<common::BackgroundProgress>,
        enabled: bool,
    ) -> Box<dyn common::BackgroundLoad> {
        let root = root.to_path_buf();
        let snapshot = snapshot.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ready, started) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("contention-status".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| background_loop(&root, &snapshot, &diagnostics, &progress, enabled, receiver, &ready))).unwrap_or_else(|_| Err(anyhow::anyhow!("background worker panicked")));
                match result {
                    Ok(report) => report,
                    Err(error) => {
                        progress.failed.store(true, Ordering::Release);
                        let _ = ready.try_send(Err(format!("{error:#}")));
                        json!({"errors":1,"error":format!("{error:#}"),"final_critical_ack":false,"sampling":{"ok":false}})
                    }
                }
            })
            .unwrap();
        let load = Box::new(Self {
            enabled,
            sender,
            worker: Some(worker),
        });
        started
            .recv()
            .unwrap()
            .expect("real status background startup");
        load
    }
    fn finish_worker(&mut self, successful: bool) -> Value {
        let _ = self.sender.send(Control::Finish(successful));
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("background worker panic")
    }
}
impl common::BackgroundLoad for Background {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn checkpoint_ready(&self) -> Value {
        let (send, receive) = mpsc::sync_channel(1);
        if self.sender.send(Control::Checkpoint(send)).is_err() {
            return json!({"errors":1,"checkpoint_critical_ack":false,"control_error":"background-control-closed; final worker report retains original failure"});
        }
        receive.recv().unwrap_or_else(|_| json!({"errors":1,"checkpoint_critical_ack":false,"control_error":"background-control-closed; final worker report retains original failure"}))
    }
    fn draining(&self) -> Value {
        let (send, receive) = mpsc::sync_channel(1);
        if self.sender.send(Control::Draining(send)).is_err() {
            return json!({"errors":1,"draining_critical_ack":false,"control_error":"background-control-closed; final worker report retains original failure"});
        }
        receive.recv().unwrap_or_else(|_| json!({"errors":1,"draining_critical_ack":false,"control_error":"background-control-closed; final worker report retains original failure"}))
    }
    fn finish(mut self: Box<Self>, successful: bool) -> Value {
        self.finish_worker(successful)
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.finish_worker(false);
        }
    }
}

fn runtime_policy(snapshot: &SnapshotEnvelope) -> quality_core::RuntimeQualityPolicy {
    quality_core::RuntimeQualityPolicy {
        version: 1,
        quality: snapshot.content.quality_policy.clone(),
        stale_after_ns: 1_000_000_000,
        event_capacity: 64,
        window_capacity: 128,
        window_horizon_ns: 10_000_000_000,
        summary_capacity: 1,
        max_sessions: 1,
        rules: quality_core::FrameViolation::ALL
            .into_iter()
            .map(|rule| {
                (
                    rule,
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

struct StateLoad {
    quality: ServiceQuality,
    persistence: persistence::Persistence,
    status: PathBuf,
    heartbeat: u64,
    seen: u64,
}
impl StateLoad {
    fn new(root: &Path, snapshot: &SnapshotEnvelope) -> anyhow::Result<Self> {
        let status = root.join("state/status.json");
        let mut quality = ServiceQuality::new(
            runtime_policy(snapshot),
            snapshot.content.binding.session_id.as_str(),
            snapshot.content.binding.instance_id.as_str(),
            Instant::now(),
            &status,
        )?;
        quality.bind(snapshot)?;
        Ok(Self {
            quality,
            persistence: persistence::Persistence::new()?,
            status,
            heartbeat: 0,
            seen: 0,
        })
    }
    fn emit(
        &mut self,
        snapshot: &SnapshotEnvelope,
        diagnostics: &RawRecordingDiagnostics,
        progress: &common::BackgroundProgress,
        state: &str,
        critical: bool,
    ) -> anyhow::Result<()> {
        let submitted = progress.submitted.load(Ordering::Acquire);
        // Modeled receipts preserve per-frame progress while the actual
        // production quality/persistence methods construct and write the batch.
        for sequence in self.seen + 1..=submitted {
            for (stream, width, height, format) in [
                ("color", 640, 480, orbbec_safe::Format::Yuyv),
                ("depth", 640, 400, orbbec_safe::Format::Depth16),
            ] {
                let now = super::unix_ms() * 1_000_000;
                let metadata = orbbec_safe::FrameMetadata {
                    format,
                    width,
                    height,
                    index: sequence,
                    timestamp_us: sequence * 1_000_000 / 30,
                    system_timestamp_us: now / 1000,
                    data_size: width as usize * height as usize * 2,
                    depth_scale_mm: (stream == "depth").then_some(0.1),
                };
                self.quality
                    .receipt(stream, Some(&metadata), Instant::now(), Some(now), true)?;
                if stream == "depth" {
                    self.quality.pixels(
                        frame_core::DepthSampleStats {
                            samples: 640 * 400,
                            zero_samples: 0,
                            nonzero_samples: 640 * 400,
                            max_u16_samples: 0,
                        },
                        None,
                        true,
                    )?;
                }
            }
        }
        self.seen = submitted;
        if !self.quality.finalized {
            self.quality.tick()?;
        }
        self.heartbeat += 1;
        let mut batch = self.quality.persistence_batch()?;
        batch.push((self.status.clone(), json!({"version":1,"kind":"capture-service-status",
            "session_id":snapshot.content.binding.session_id,"instance_id":snapshot.content.binding.instance_id,
            "state":state,"pid":std::process::id(),"heartbeat_sequence":self.heartbeat,
            "updated_unix_ms":super::unix_ms(),"quality":self.quality.references(),
            "raw_recording":{"state":state,"submitted_pairs":submitted,
                "written_pairs":progress.written.load(Ordering::Acquire),"writer_queue":progress.queue.observation(),
                "disk_stages":diagnostics.report()},
            "status_persistence":self.persistence.report(),"scope":"synthetic storage isolation; no SDK/ROS"})));
        self.persistence.submit(batch, critical)
    }
}

fn background_loop(
    root: &Path,
    snapshot: &SnapshotEnvelope,
    diagnostics: &RawRecordingDiagnostics,
    progress: &common::BackgroundProgress,
    enabled: bool,
    receiver: mpsc::Receiver<Control>,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> anyhow::Result<Value> {
    let mut state = enabled
        .then(|| StateLoad::new(root, snapshot))
        .transpose()?;
    if let Some(state) = &mut state {
        state.emit(snapshot, diagnostics, progress, "starting", true)?;
    }
    ready
        .send(Ok(()))
        .map_err(|_| anyhow::anyhow!("start receiver gone"))?;
    // Test-only fault: exercise a real production rename error after startup
    // has been durably acknowledged, without mocking Persistence.
    if enabled
        && std::env::var("GDP_CONTENTION_TEST_STATUS_FAILURE_AFTER_STARTUP").as_deref() == Ok("1")
    {
        let status = root.join("state/status.json");
        fs::remove_file(&status)?;
        fs::create_dir(&status)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("status-load-samples.jsonl"))?;
    let requested: u64 = std::env::var("GDP_CONTENTION_PAIRS")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(30);
    let row_limit = (requested / 30 * 4 + 1000).min(20_000);
    let mut rows = 0u64;
    let mut bytes = 0usize;
    let mut periodic = 0u64;
    let mut previous = Instant::now();
    let mut max_gap = 0u64;
    let mut next = previous + HEARTBEAT;
    let mut old_phase = 0;
    let mut draining_ack = false;
    let success = loop {
        match receiver.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Control::Finish(success)) => break success,
            Ok(Control::Draining(reply)) => {
                if let Some(state) = &mut state {
                    state.emit(snapshot, diagnostics, progress, "draining", true)?;
                    draining_ack = true;
                }
                old_phase = 2;
                let report = state
                    .as_ref()
                    .map(|s| s.persistence.report())
                    .unwrap_or(Value::Null);
                let _ = reply.send(report);
                continue;
            }
            Ok(Control::Checkpoint(reply)) => {
                if let Some(state) = &mut state {
                    state.emit(snapshot, diagnostics, progress, "checkpoint-ready", true)?;
                }
                let mut report = state
                    .as_ref()
                    .map(|s| s.persistence.report())
                    .unwrap_or(Value::Null);
                if enabled {
                    report["checkpoint_critical_ack"] = json!(true);
                }
                let _ = reply.send(report);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break false,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        max_gap = max_gap.max(now.duration_since(previous).as_nanos() as u64);
        previous = now;
        let phase = progress.phase.load(Ordering::Acquire);
        let name = match phase {
            0 => "waiting-barrier",
            1 => "recording",
            2 => "draining",
            3 => "independent-readback",
            _ => "readers-finished",
        };
        if let Some(state) = &mut state {
            state.emit(
                snapshot,
                diagnostics,
                progress,
                name,
                phase == 2 && old_phase != 2,
            )?;
        }
        old_phase = phase;
        periodic += 1;
        let sample = json!({"kind":"writer-status-load-sample-v1","phase":name,
            "monotonic_ns":common::monotonic_ns(),"pid":std::process::id(),
            "submitted_pairs":progress.submitted.load(Ordering::Acquire),"written_pairs":progress.written.load(Ordering::Acquire),
            "queue":progress.queue.observation(),"writer_context":diagnostics.failure_context(),
            "persistence":state.as_ref().map(|s|s.persistence.report()),"status_background_load":enabled});
        let mut raw = serde_json::to_vec(&sample)?;
        raw.push(b'\n');
        append_sample(
            &mut file,
            &raw,
            &mut rows,
            &mut bytes,
            row_limit,
            SAMPLE_BYTES,
        )?;
        // Never compensate scheduling or filesystem delay with a write burst.
        next = now + HEARTBEAT;
    };
    if let Some(state) = &mut state {
        state.quality.finalize()?;
        state.emit(
            snapshot,
            diagnostics,
            progress,
            if success { "complete" } else { "failed" },
            true,
        )?;
    }
    file.flush()?;
    let mut report = state
        .as_ref()
        .map(|s| s.persistence.report())
        .unwrap_or_else(|| json!({}));
    report["state_directory"] = json!(enabled.then(|| root.join("state")));
    report["heartbeat_count"] = json!(state.as_ref().map_or(0, |s| s.heartbeat));
    report["periodic_submissions"] = json!(if enabled { periodic } else { 0 });
    report["start_critical_ack"] = json!(enabled);
    report["final_critical_ack"] = json!(enabled);
    report["draining_critical_ack"] = json!(draining_ack);
    report["submission_errors"] = json!(0);
    report["heartbeat_max_gap_ns"] = json!(max_gap);
    report["sampling"] = json!({"ok":true,"rows":rows,"bytes":bytes,"max_rows":row_limit,"max_bytes":SAMPLE_BYTES,
        "relative_path":"status-load-samples.jsonl","scope":"500ms sampled queue/writer context; not atomic across components, not continuous syscall timing"});
    Ok(report)
}

fn append_sample(
    file: &mut fs::File,
    raw: &[u8],
    rows: &mut u64,
    bytes: &mut usize,
    row_limit: u64,
    byte_limit: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        *rows < row_limit && raw.len() <= byte_limit.saturating_sub(*bytes),
        "BACKGROUND_SAMPLE_LIMIT"
    );
    file.write_all(raw)?;
    *rows += 1;
    *bytes += raw.len();
    Ok(())
}

#[test]
#[ignore = "only controlled same-disk writer experiment with a fresh root"]
fn writer_contention_with_status_child() {
    let adapter = offline_depth::sha256(include_bytes!("writer_contention.rs"));
    common::run_child_with_background(Some(Background::start), Some(&adapter));
}

#[test]
fn production_state_load_critical_ack_and_sampler_are_real_and_bounded() {
    for enabled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (snapshot, _) = common::fixture();
        let progress = Arc::new(common::BackgroundProgress::default());
        let diagnostics = RawRecordingDiagnostics::with_checkpoint_observation(true);
        let load = Background::start_mode(
            root.path(),
            &snapshot,
            diagnostics,
            progress.clone(),
            enabled,
        );
        progress.submitted.store(2, Ordering::Release);
        progress.written.store(2, Ordering::Release);
        progress.phase.store(2, Ordering::Release);
        let started = Instant::now();
        while fs::metadata(root.path().join("status-load-samples.jsonl"))
            .map_or(true, |m| m.len() == 0)
            && started.elapsed() < Duration::from_secs(4)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = load.draining();
        let checkpoint = load.checkpoint_ready();
        if enabled {
            assert_eq!(
                checkpoint["submitted_sequence"],
                checkpoint["completed_sequence"]
            );
        }
        let report = load.finish(true);
        assert_eq!(report["sampling"]["ok"], true, "{report}");
        assert!(report["sampling"]["rows"].as_u64().unwrap() > 0);
        assert!(report["sampling"]["bytes"].as_u64().unwrap() < SAMPLE_BYTES as u64);
        assert_eq!(root.path().join("state/status.json").exists(), enabled);
        if enabled {
            assert_eq!(report["errors"], 0);
            assert_eq!(report["pending_snapshots"], 0);
            let saved: Value =
                serde_json::from_slice(&fs::read(root.path().join("state/status.json")).unwrap())
                    .unwrap();
            assert_eq!(saved["state"], "complete");
            assert_eq!(saved["raw_recording"]["written_pairs"], 2);
            assert!(Path::new(saved["quality"]["events_path"].as_str().unwrap()).is_file());
        }
    }
}

#[test]
fn sampler_limits_reject_before_writing_an_extra_byte_or_row() {
    for (rows_limit, bytes_limit) in [(1, 100), (10, 3)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut rows = 0;
        let mut bytes = 0;
        append_sample(
            &mut file,
            b"abc",
            &mut rows,
            &mut bytes,
            rows_limit,
            bytes_limit,
        )
        .unwrap();
        assert!(
            append_sample(
                &mut file,
                b"d",
                &mut rows,
                &mut bytes,
                rows_limit,
                bytes_limit
            )
            .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"abc");
        assert_eq!((rows, bytes), (1, 3));
    }
}
#[test]
fn real_status_rename_failure_sets_shared_fatal_and_cannot_ack_completion() {
    let root = tempfile::tempdir().unwrap();
    let (snapshot, _) = common::fixture();
    let progress = Arc::new(common::BackgroundProgress::default());
    let load = Background::start_mode(
        root.path(),
        &snapshot,
        RawRecordingDiagnostics::default(),
        progress.clone(),
        true,
    );
    let path = root.path().join("state/status.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let start = Instant::now();
    while !progress.failed.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(4) {
        thread::sleep(Duration::from_millis(10));
    }
    let report = load.finish(false);
    assert!(progress.failed.load(Ordering::Acquire), "{report}");
    assert_eq!(report["errors"], 1);
    assert_eq!(report["final_critical_ack"], false);
    assert_eq!(report["sampling"]["ok"], false);
    assert!(!root.path().join("capture/COMPLETE").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn real_ignored_child_finishes_on_and_off_without_report_lock_recursion() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    for (enabled, fault) in [(false, false), (true, false), (true, true)] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let log_path = root.join("child.log");
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&log_path)
            .unwrap();
        let mut child = OwnedChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "service::writer_contention::writer_contention_with_status_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("GDP_CONTENTION_ROOT", root)
                .env("GDP_CONTENTION_MODE", "run")
                .env("GDP_CONTENTION_PAIRS", "30")
                .env(
                    "GDP_CONTENTION_STATUS_LOAD",
                    if enabled { "1" } else { "0" },
                )
                .env(
                    "GDP_CONTENTION_TEST_STATUS_FAILURE_AFTER_STARTUP",
                    if fault { "1" } else { "0" },
                )
                .env("GDP_CONTENTION_BARRIER", root.join("start.json"))
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        loop {
            if let Some(ready) = fs::read(root.join("ready.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                assert_eq!(ready["pid"], child.0.id());
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before ready: {}",
                fs::read_to_string(&log_path).unwrap()
            );
            assert!(started.elapsed() < Duration::from_secs(20), "ready timeout");
            thread::sleep(Duration::from_millis(5));
        }
        let at = common::monotonic_ns() + 200_000_000;
        fs::write(root.join("start.tmp"),serde_json::to_vec(&json!({"start_monotonic_ns":at,"start_unix_ns":super::unix_ms()*1_000_000+200_000_000})).unwrap()).unwrap();
        fs::rename(root.join("start.tmp"), root.join("start.json")).unwrap();
        let result = loop {
            if let Some(exit) = child.0.try_wait().unwrap() {
                break exit;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "completion/report timeout: {}",
                fs::read_to_string(&log_path).unwrap()
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            result.success(),
            "{}",
            fs::read_to_string(&log_path).unwrap()
        );
        let report: Value =
            serde_json::from_slice(&fs::read(root.join("result.json")).unwrap()).unwrap();
        if fault {
            assert_eq!(report["ok"], false, "{report}");
            assert_eq!(report["status_persistence"]["errors"], 1);
            assert_eq!(report["status_persistence"]["final_critical_ack"], false);
            assert!(
                report["status_persistence"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("status persistence"),
                "{report}"
            );
            assert!(!fs::read_to_string(&log_path).unwrap().contains("SendError"));
            assert!(!root.join("capture/COMPLETE").exists());
            assert!(root.join("capture.partial").is_dir());
            continue;
        }
        assert_eq!(report["ok"], true, "{report}");
        assert_eq!(report["written_pairs"], 30);
        assert!(report["capture_elapsed_ns"].as_u64().unwrap() >= 1_000_000_000);
        assert_eq!(report["status_background_load"], enabled);
        assert_eq!(report["queue_final"]["queued_pairs"], 0);
        assert_eq!(report["queue_final"]["inflight_pairs"], 0);
        assert_eq!(report["independent_readback"]["passes"], 2);
        assert_eq!(
            report["independent_readback"]["pairs_per_pass"],
            json!([30, 30])
        );
        assert_eq!(report["background_sampling"]["ok"], true);
        assert!(root.join("capture/COMPLETE").is_file());
        if enabled {
            assert_eq!(report["status_persistence"]["draining_critical_ack"], true);
            assert_eq!(report["status_persistence"]["final_critical_ack"], true);
        } else {
            assert!(report["status_persistence"].is_null());
        }
    }
}
