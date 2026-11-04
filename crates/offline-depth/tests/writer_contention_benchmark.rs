//! Common Linux release driver for a bounded 30 FPS single/three writer experiment.
//! It uses the real writer, fsyncs, checkpoint credits and readers, but models the
//! owned whole-pair queue: it does not exercise SDK/ROS or capture-service state.
mod support;
use capture_contract::{
    ConfigurationId, ContentKind, content_digest,
    imu::{ImuKind, ImuSample},
    snapshot::SnapshotEnvelope,
};
use frame_core::BoundPairEvidence;
use offline_depth::{StorageEndian, recording::*, sha256};
use ring::digest::{Context, SHA256};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const CAPACITY: usize = 16;
const COLOR_BYTES: usize = 640 * 480 * 2;
const DEPTH_BYTES: usize = 640 * 400 * 2;
const PAIR_BYTES: usize = COLOR_BYTES + DEPTH_BYTES;
fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
fn monotonic_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn cpu_seconds() -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) },
        0
    );
    t.tv_sec as f64 + t.tv_nsec as f64 / 1e9
}
fn hwm_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|s| s.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
fn fixture() -> (SnapshotEnvelope, BoundPairEvidence) {
    let root = tempfile::tempdir().unwrap();
    support::fixture(root.path(), true, 0.);
    let snapshot: SnapshotEnvelope =
        serde_json::from_slice(&fs::read(root.path().join("snapshot.json")).unwrap()).unwrap();
    let mut pair: BoundPairEvidence =
        serde_json::from_slice(&fs::read(root.path().join("pair.json")).unwrap()).unwrap();
    let mut content = snapshot.content;
    content.calibration.models.get_mut("color").unwrap().width = 640;
    content.calibration.models.get_mut("color").unwrap().height = 480;
    content.calibration.models.get_mut("depth").unwrap().width = 640;
    content.calibration.models.get_mut("depth").unwrap().height = 400;
    content.binding.calibration_id = content.calibration.id().unwrap();
    content.configuration = json!({"writer_contention_fixture":1,
        "source_depth_scale_mm":f64::from(0.1f32),
        "acquisition":{"preset":"strict-rgbd","advanced":{"message_timestamp":"global"}},
        "imu":{"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,
        "accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,
        "stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"}});
    content.binding.configuration_id = ConfigurationId::parse(
        content_digest(
            ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    pair.color.width = 640;
    pair.color.height = 480;
    pair.color.stride = 1280;
    pair.depth.width = 640;
    pair.depth.height = 400;
    pair.depth.stride = 1280;
    for f in [&mut pair.color, &mut pair.depth] {
        f.context.snapshot_id = snapshot.snapshot_id.clone();
        f.context.calibration_id = snapshot.content.binding.calibration_id.clone();
        f.context.configuration_id = snapshot.content.binding.configuration_id.clone();
    }
    (snapshot, pair)
}
fn pair_at(template: &BoundPairEvidence, n: u64) -> BoundPairEvidence {
    let mut pair = template.clone();
    let stamp = n * 1_000_000 / 30;
    pair.pair_sequence = n;
    pair.color_stamp_us = stamp;
    pair.depth_stamp_us = stamp;
    for f in [&mut pair.color, &mut pair.depth] {
        f.context.sequence = Some(n);
        f.context.device_timestamp_us = Some(stamp);
        f.context.system_timestamp_us = Some(1_700_000_000_000_000 + stamp);
        f.context.global_timestamp_us = Some(1_700_000_000_100_000 + stamp);
        f.context.received_mono_offset_ns = stamp * 1000;
    }
    pair
}
fn pixels(n: u64, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    for (i, p) in bytes.chunks_exact_mut(2).enumerate() {
        p.copy_from_slice(&(((i as u64 + n) % 60000 + 1) as u16).to_le_bytes());
    }
    bytes
}
fn imu(n: u64) -> Vec<ImuSample> {
    (((n - 1) * 200 / 30 + 1)..=(n * 200 / 30))
        .flat_map(|sequence| {
            [ImuKind::Accel, ImuKind::Gyro].map(move |kind| ImuSample {
                kind,
                sequence,
                device_timestamp_us: sequence * 5000,
                system_timestamp_us: 1_700_000_000_000_000 + sequence * 5000,
                global_timestamp_us: Some(1_700_000_000_100_000 + sequence * 5000),
                xyz: [1., 2., 3.],
                frame_id: if kind == ImuKind::Accel {
                    "accel_frame"
                } else {
                    "gyro_frame"
                }
                .into(),
            })
        })
        .collect()
}
struct Message {
    pair: BoundPairEvidence,
    color: Vec<u8>,
    depth: Vec<u8>,
    imu: Vec<ImuSample>,
    enqueued: Instant,
}
#[derive(Default)]
struct QueueState {
    items: VecDeque<Message>,
    closed: bool,
    abort: bool,
    peak: usize,
    discarded: u64,
}
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}
impl Queue {
    fn push(&self, message: Message) -> Result<(), &'static str> {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return Err("RAW_STORAGE_OR_QUEUE_CLOSED");
        }
        if s.items.len() == CAPACITY {
            s.abort = true;
            s.closed = true;
            s.discarded += s.items.len() as u64;
            s.items.clear();
            self.ready.notify_all();
            return Err("RAW_QUEUE_OVERFLOW");
        }
        s.items.push_back(message);
        s.peak = s.peak.max(s.items.len());
        self.ready.notify_one();
        Ok(())
    }
    fn pop(&self) -> Option<Message> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.abort {
                return None;
            }
            if let Some(v) = s.items.pop_front() {
                return Some(v);
            }
            if s.closed {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }
}
fn percentile(samples: &mut [u64]) -> Option<u64> {
    samples.sort_unstable();
    (!samples.is_empty()).then(|| samples[(samples.len() * 99).div_ceil(100) - 1])
}
fn verify(path: &Path, recovered: bool, expected: u64) -> Value {
    let before = Instant::now();
    let mut counts = Vec::new();
    for _ in 0..2 {
        let mut reader = if recovered {
            RecordingDirectorySource::open_recovered(path)
        } else {
            RecordingDirectorySource::open(path)
        }
        .unwrap();
        let mut count = 0;
        while let Some(frame) = reader.next_frame().unwrap() {
            count += 1;
            assert_eq!(frame.record.sequence, count);
            assert_eq!(frame.record.imu, imu(count));
            assert_eq!(frame.depth, pixels(count, DEPTH_BYTES));
            assert_eq!(
                frame.record.color.file.sha256,
                sha256(&pixels(count, COLOR_BYTES))
            );
        }
        assert_eq!(count, expected);
        counts.push(count);
    }
    json!({"passes":2,"pairs_per_pass":counts,"wall_ns":before.elapsed().as_nanos() as u64,
        "per_frame_color_depth_evidence_hashes":true,"expected_imu_samples":expected * 200 / 30 * 2})
}
fn barrier(root: &Path) -> (u64, u64) {
    let path = PathBuf::from(std::env::var_os("GDP_CONTENTION_BARRIER").unwrap());
    fs::write(
        root.join("ready.json"),
        serde_json::to_vec(&json!({"pid":std::process::id(),"ready_unix_ns":unix_ns()})).unwrap(),
    )
    .unwrap();
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(30), "barrier timeout");
        match fs::read(&path) {
            Ok(bytes) => {
                let target: Value = serde_json::from_slice(&bytes).unwrap();
                let display_unix = target["start_unix_ns"].as_u64().unwrap();
                let target = target["start_monotonic_ns"].as_u64().unwrap();
                while monotonic_ns() < target {
                    thread::sleep(Duration::from_micros(100));
                }
                return (target, display_unix);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) => panic!("barrier read: {e}"),
        }
    }
}
fn metadata_hashes(path: &Path) -> Value {
    let mut out = serde_json::Map::new();
    for name in [
        "snapshot.json",
        "records.jsonl",
        "checkpoint.json",
        "manifest.json",
        "COMPLETE",
    ] {
        out.insert(
            name.into(),
            json!(sha256(&fs::read(path.join(name)).unwrap())),
        );
    }
    Value::Object(out)
}
#[test]
#[ignore = "run only via writer_contention_ab.py with a new output directory"]
fn writer_contention_child() {
    assert!(cfg!(target_os = "linux"), "Linux release child required");
    let root = PathBuf::from(std::env::var_os("GDP_CONTENTION_ROOT").unwrap());
    let mode = std::env::var("GDP_CONTENTION_MODE").unwrap_or_else(|_| "run".into());
    let pairs: u64 = std::env::var("GDP_CONTENTION_PAIRS")
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=108000).contains(&pairs));
    if mode == "recover" {
        let partial = root.join("capture.partial");
        assert!(!partial.join("COMPLETE").exists());
        assert!(RecordingDirectorySource::open(&partial).is_err());
        let mut report = verify(&partial, true, pairs);
        report["ordinary_reader_rejected"] = json!(true);
        assert!(!root.join("capture/COMPLETE").exists());
        fs::write(
            root.join("recovery-result.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        return;
    }
    let (snapshot, template) = fixture();
    let max_bytes = pairs * (PAIR_BYTES as u64 + 32768) + 16 * 1024 * 1024;
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, max_bytes).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    let diagnostics = writer.diagnostics();
    if mode == "crash" {
        for n in 1..=pairs {
            writer
                .append_with_imu(
                    &pair_at(&template, n),
                    &pixels(n, COLOR_BYTES),
                    &pixels(n, DEPTH_BYTES),
                    StorageEndian::Little,
                    &imu(n),
                )
                .unwrap();
        }
        writer.checkpoint().unwrap();
        fs::write(
            root.join("recovery-ready.json"),
            serde_json::to_vec(&json!({"pairs":pairs,"pid":std::process::id()})).unwrap(),
        )
        .unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    assert_eq!(mode, "run");
    let queue = Arc::new(Queue::default());
    let sink_queue = queue.clone();
    let sink_root = root.clone();
    let delay_ms: u64 = std::env::var("GDP_CONTENTION_TEST_SINK_DELAY_MS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    let sink = thread::Builder::new()
        .name("native-depth-recording".into())
        .spawn(move || {
            let mut append = Vec::new();
            let mut wait = Vec::new();
            let mut written = 0;
            let result = (|| -> Result<Option<PathBuf>, String> {
                while let Some(message) = sink_queue.pop() {
                    wait.push(message.enqueued.elapsed().as_nanos() as u64);
                    if delay_ms > 0 {
                        thread::sleep(Duration::from_millis(delay_ms));
                    }
                    if sink_queue.state.lock().unwrap().abort {
                        break;
                    }
                    let at = Instant::now();
                    writer
                        .append_with_imu(
                            &message.pair,
                            &message.color,
                            &message.depth,
                            StorageEndian::Little,
                            &message.imu,
                        )
                        .map_err(|e| e.to_string())?;
                    append.push(at.elapsed().as_nanos() as u64);
                    written += 1;
                }
                if sink_queue.state.lock().unwrap().abort {
                    drop(writer);
                    Ok(None)
                } else {
                    writer.finish().map(Some).map_err(|e| e.to_string())
                }
            })();
            if result.is_err() {
                let mut s = sink_queue.state.lock().unwrap();
                s.abort = true;
                s.closed = true;
            }
            let path = result.as_ref().ok().and_then(|p| p.as_ref());
            let verification = path.map(|p| verify(p, false, written));
            let artifacts = path.map(|p| metadata_hashes(p));
            if path.is_none() && sink_root.join("capture/COMPLETE").exists() {
                panic!("failed run published COMPLETE");
            }
            (written, append, wait, result.err(), verification, artifacts)
        })
        .unwrap();
    let (barrier_target, barrier_display_unix) = barrier(&root);
    let actual_monotonic_start = monotonic_ns();
    let actual_start = unix_ns();
    let started = Instant::now();
    let cpu_start = cpu_seconds();
    let mut input = Context::new(&SHA256);
    let mut reason = None;
    let mut submitted = 0;
    let mut producer_late_max_ns = 0u64;
    for n in 1..=pairs {
        let due = started + Duration::from_nanos((n - 1) * 1_000_000_000 / 30);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        producer_late_max_ns = producer_late_max_ns
            .max(Instant::now().saturating_duration_since(due).as_nanos() as u64);
        let pair = pair_at(&template, n);
        let color = pixels(n, COLOR_BYTES);
        let depth = pixels(n, DEPTH_BYTES);
        let imu = imu(n);
        input.update(&color);
        input.update(&depth);
        input.update(&serde_json::to_vec(&pair).unwrap());
        input.update(&serde_json::to_vec(&imu).unwrap());
        let message = Message {
            pair,
            color,
            depth,
            imu,
            enqueued: Instant::now(),
        };
        if let Err(e) = queue.push(message) {
            reason = Some(e);
            break;
        }
        submitted += 1;
    }
    queue.close();
    let (written, mut append, mut wait, error, verification, artifacts) = sink.join().unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu_start;
    let state = queue.state.lock().unwrap();
    let complete = reason.is_none() && error.is_none() && written == pairs;
    let digest = input.finish();
    let input_sha256: String = digest.as_ref().iter().map(|n| format!("{n:02x}")).collect();
    let report = json!({"kind":"writer-contention-child-v1","common_driver_sha256":sha256(include_bytes!("writer_contention_benchmark.rs")),"ok":complete,"pairs":pairs,
        "submitted_pairs":submitted,"written_pairs":written,"reason":reason,"writer_error":error,
        "queue_capacity":CAPACITY,"queue_peak":state.peak,"overflow":u64::from(reason == Some("RAW_QUEUE_OVERFLOW")),"discarded_pairs":state.discarded,
        "queue_scope":"modeled nonblocking owned-whole-pair FIFO; 16 queued plus one in flight; overflow aborts rather than blocking capture",
        "checkpoint_pipeline":true,"credit_pairs":32,"credit_bytes":16*1024*1024,
        "fps":30,"imu_hz_per_stream":200,"color_bytes":COLOR_BYTES,"depth_bytes":DEPTH_BYTES,
        "input_sha256":input_sha256,"artifact_sha256":artifacts,"barrier_target_monotonic_ns":barrier_target,
        "barrier_target_unix_ns":barrier_display_unix,"actual_start_monotonic_ns":actual_monotonic_start,
        "actual_start_unix_ns":actual_start,"producer_late_max_ns":producer_late_max_ns,
        "append_p99_ns":percentile(&mut append),"append_max_ns":append.last(),
        "queue_wait_p99_ns":percentile(&mut wait),"queue_wait_max_ns":wait.last(),
        "cpu_seconds":cpu,"cpu_cores":cpu/elapsed,"wall_seconds_including_independent_readers":elapsed,
        "peak_rss_kib":hwm_kib(),"peak_rss_scope":"isolated child including producer, writer and two extra materialized readers",
        "independent_readback":verification,"diagnostics":diagnostics.report(),
        "status_background_load":false,"test_sink_delay_ms":delay_ms});
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
#[test]
fn imu_rate_and_timestamp_fixture_is_identical_across_runs() {
    let a: Vec<_> = (1..=30).flat_map(imu).collect();
    assert_eq!(a.len(), 400);
    for kind in [ImuKind::Accel, ImuKind::Gyro] {
        let samples: Vec<_> = a.iter().filter(|s| s.kind == kind).collect();
        assert_eq!(samples.len(), 200);
        assert!(
            samples
                .windows(2)
                .all(|s| s[1].sequence == s[0].sequence + 1
                    && s[1].device_timestamp_us > s[0].device_timestamp_us
                    && s[1].global_timestamp_us > s[0].global_timestamp_us)
        );
    }
}
#[test]
fn queue_overflow_stays_at_sixteen_and_discards_pending_pairs() {
    let (_, template) = fixture();
    let queue = Queue::default();
    for n in 1..=17 {
        let result = queue.push(Message {
            pair: pair_at(&template, n),
            color: vec![],
            depth: vec![],
            imu: imu(n),
            enqueued: Instant::now(),
        });
        assert_eq!(
            result,
            if n == 17 {
                Err("RAW_QUEUE_OVERFLOW")
            } else {
                Ok(())
            }
        );
    }
    let s = queue.state.lock().unwrap();
    assert_eq!(s.peak, 16);
    assert_eq!(s.discarded, 16);
    assert!(s.abort);
    drop(s);
    assert!(queue.pop().is_none());
}
