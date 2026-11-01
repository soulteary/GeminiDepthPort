//! Common driver compiled against both source revisions. Only the implementation
//! of the recording writer/reader changes; fixtures and RSS measurement do not.
#![cfg(target_os = "linux")]
mod support;
use capture_contract::{ConfigurationId, ContentKind, content_digest, snapshot::SnapshotEnvelope};
use frame_core::BoundPairEvidence;
use offline_depth::{StorageEndian, recording::*};
use serde_json::json;
use std::{fs, path::PathBuf, time::Instant};
fn fixture(width: u32, height: u32) -> (SnapshotEnvelope, BoundPairEvidence, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    support::fixture(root.path(), true, 0.);
    let snapshot: SnapshotEnvelope =
        serde_json::from_slice(&fs::read(root.path().join("snapshot.json")).unwrap()).unwrap();
    let mut pair: BoundPairEvidence =
        serde_json::from_slice(&fs::read(root.path().join("pair.json")).unwrap()).unwrap();
    let mut content = snapshot.content;
    for m in content.calibration.models.values_mut() {
        m.width = width;
        m.height = height;
    }
    content.binding.calibration_id = content.calibration.id().unwrap();
    content.configuration =
        json!({"verification_benchmark":true,"source_depth_scale_mm":f64::from(0.1f32)});
    content.binding.configuration_id = ConfigurationId::parse(
        content_digest(
            ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    for frame in [&mut pair.color, &mut pair.depth] {
        frame.width = width;
        frame.height = height;
        frame.stride = width as usize * 2;
        frame.context.snapshot_id = snapshot.snapshot_id.clone();
        frame.context.calibration_id = snapshot.content.binding.calibration_id.clone();
        frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
    }
    let mut pixels = vec![0; width as usize * height as usize * 2];
    for p in pixels.chunks_exact_mut(2) {
        p[0] = 10;
    }
    (snapshot, pair, pixels)
}
fn peak_rss_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
#[test]
#[ignore = "common release child driver for before/after validation benchmark"]
fn checkpoint_sync_benchmark_child() {
    let (width, height, pairs) = match std::env::var("GDP_VERIFY_PROFILE").unwrap().as_str() {
        "burst" => (640, 400, 128u64),
        "paced" => (640, 400, 64u64),
        _ => panic!("unknown benchmark profile"),
    };
    let root = PathBuf::from(std::env::var_os("GDP_VERIFY_ROOT").unwrap());
    let (snapshot, mut pair, pixels) = fixture(width, height);
    let cpu = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) },
            0
        );
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    };
    let started = Instant::now();
    let cpu_start = cpu();
    let mut append_ns = Vec::new();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 512 * 1024 * 1024).unwrap();
    if std::env::var("GDP_CHECKPOINT_PIPELINE").as_deref() == Ok("1") {
        writer.enable_checkpoint_pipeline().unwrap();
    }
    let diagnostics = writer.diagnostics();
    for n in 1..=pairs {
        pair.pair_sequence = n;
        for f in [&mut pair.color, &mut pair.depth] {
            f.context.sequence = Some(n);
            f.context.device_timestamp_us = Some(n * 33333);
            f.context.system_timestamp_us = Some(1_000_000 + n * 33333);
            f.context.received_mono_offset_ns = n * 33333000;
        }
        pair.color_stamp_us = n * 33333;
        pair.depth_stamp_us = n * 33333;
        let append_started = Instant::now();
        writer
            .append(&pair, &pixels, &pixels, StorageEndian::Little)
            .unwrap();
        append_ns.push(append_started.elapsed().as_nanos() as u64);
        if std::env::var("GDP_VERIFY_PROFILE").as_deref() == Ok("paced") {
            let due = started + std::time::Duration::from_micros(n * 33333);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    }
    let path = writer.finish().unwrap();
    let seconds = started.elapsed().as_secs_f64();
    let cpu_seconds = cpu() - cpu_start;
    append_ns.sort_unstable();
    let append_p99_ns = append_ns[(append_ns.len() * 99).div_ceil(100) - 1];
    // Capture HWM before the independent materialized readback below. Otherwise
    // that verification would hide the streaming path's memory benefit.
    let peak_rss_kib = peak_rss_kib();
    let report = diagnostics.report();
    let mut artifact_hashes = serde_json::Map::new();
    for name in [
        "snapshot.json",
        "records.jsonl",
        "checkpoint.json",
        "manifest.json",
        "COMPLETE",
    ] {
        use std::hash::{Hash, Hasher};
        let mut checksum = std::collections::hash_map::DefaultHasher::new();
        fs::read(path.join(name)).unwrap().hash(&mut checksum);
        artifact_hashes.insert(name.into(), json!(checksum.finish()));
    }
    let payload_bytes = pairs * 2 * pixels.len() as u64;
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    let mut count = 0;
    while let Some(frame) = source.next_frame().unwrap() {
        assert_eq!(frame.depth, pixels);
        count += 1;
    }
    assert_eq!(count, pairs);
    fs::write(root.join("result.json"),serde_json::to_vec_pretty(&json!({
        "cpu_seconds":cpu_seconds,"cpu_cores":cpu_seconds/seconds,"append_p99_ns":append_p99_ns,
        "width":width,"height":height,"pairs":pairs,"payload_bytes":payload_bytes,
        "total_seconds_including_commit_verification":seconds,"payload_mib_per_second":payload_bytes as f64/1048576./seconds,
        "peak_rss_kib":peak_rss_kib,"peak_rss_source":"/proc/self/status VmHWM; entire isolated child, sampled before post-timer materialized comparison; not anonymous-only RSS",
        "artifact_hashes":artifact_hashes,"files":fs::read_dir(&path).unwrap().count(),"pairs_read_back":count,"diagnostics":report})).unwrap()).unwrap();
}
