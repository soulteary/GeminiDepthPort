//! Common driver compiled against both source revisions. Only the implementation
//! of the recording writer/reader changes; fixtures and RSS measurement do not.
#![cfg(target_os = "linux")]
#[path = "../../../tests/support/io_faults.rs"]
mod io_faults;
mod support;
use capture_contract::{ConfigurationId, ContentKind, content_digest, snapshot::SnapshotEnvelope};
use frame_core::BoundPairEvidence;
use offline_depth::{StorageEndian, recording::*};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command, time::Instant};
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
fn write_benchmark_child() {
    let (width, height, pairs) = match std::env::var("GDP_VERIFY_PROFILE").unwrap().as_str() {
        "compact" => (640, 400, 128u64),
        "wide" => (1920, 1080, 16u64),
        _ => panic!("unknown benchmark profile"),
    };
    let root = PathBuf::from(std::env::var_os("GDP_VERIFY_ROOT").unwrap());
    let (snapshot, mut pair, pixels) = fixture(width, height);
    let started = Instant::now();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 512 * 1024 * 1024).unwrap();
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
        writer
            .append(&pair, &pixels, &pixels, StorageEndian::Little)
            .unwrap();
    }
    let path = writer.finish().unwrap();
    let seconds = started.elapsed().as_secs_f64();
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
        "width":width,"height":height,"pairs":pairs,"payload_bytes":payload_bytes,
        "total_seconds_including_commit_verification":seconds,"payload_mib_per_second":payload_bytes as f64/1048576./seconds,
        "peak_rss_kib":peak_rss_kib,"peak_rss_source":"/proc/self/status VmHWM; entire isolated child, sampled before post-timer materialized comparison; not anonymous-only RSS",
        "artifact_hashes":artifact_hashes,"files":fs::read_dir(&path).unwrap().count(),"pairs_read_back":count,"diagnostics":report})).unwrap()).unwrap();
}
#[test]
#[ignore = "explicit release A/B experiment; no noisy performance assertions in normal CI"]
fn write_ab_benchmark() {
    let before = std::env::var_os("GDP_VERIFY_BEFORE").unwrap();
    let after = std::env::var_os("GDP_VERIFY_AFTER").unwrap();
    let mut rows = Vec::new();
    for repeat in 0..3 {
        for profile in ["compact", "wide"] {
            for delay in [0, 2] {
                let variants = if repeat % 2 == 0 {
                    [("before", &before), ("after", &after)]
                } else {
                    [("after", &after), ("before", &before)]
                };
                for (variant, binary) in variants {
                    let root = tempfile::tempdir().unwrap();
                    let marker = root.path().join("injected");
                    let mut command = Command::new(binary);
                    command
                        .args([
                            "--exact",
                            "write_benchmark_child",
                            "--ignored",
                            "--nocapture",
                        ])
                        .env("GDP_VERIFY_ROOT", root.path())
                        .env("GDP_VERIFY_PROFILE", profile)
                        .env_remove("LD_PRELOAD");
                    if delay > 0 {
                        let library = io_faults::compile(root.path());
                        command
                            .env("GDP_IO_DELAY_MS", delay.to_string())
                            .env("GDP_IO_REPEAT", "1");
                        io_faults::inject(&mut command, &library, "fsync", "", &marker);
                    }
                    let output = command.output().unwrap();
                    assert!(
                        output.status.success(),
                        "{variant} {profile}: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    if delay > 0 {
                        io_faults::assert_injected(&marker, "fsync");
                    }
                    let mut row: Value =
                        serde_json::from_slice(&fs::read(root.path().join("result.json")).unwrap())
                            .unwrap();
                    assert_eq!(
                        row["diagnostics"]["validation_image_bytes"]
                            .as_u64()
                            .unwrap(),
                        row["payload_bytes"].as_u64().unwrap() * 2
                    );
                    if variant == "after" {
                        assert!(row["diagnostics"]["payload_write_calls"].as_u64().unwrap() > 0);
                        assert_eq!(
                            row["diagnostics"]["validation_image_bytes"]
                                .as_u64()
                                .unwrap(),
                            row["payload_bytes"].as_u64().unwrap() * 2
                        );
                        assert!(
                            row["diagnostics"]["validation_buffer_peak_bytes"]
                                .as_u64()
                                .unwrap()
                                <= 65536
                        );
                    } else {
                        assert!(row["diagnostics"]["payload_write_calls"].is_null());
                    }
                    row["variant"] = json!(variant);
                    row["profile"] = json!(profile);
                    row["fsync_delay_ms"] = json!(delay);
                    row["repeat"] = json!(repeat);
                    rows.push(row);
                }
            }
        }
    }
    fs::write(std::env::var_os("GDP_VERIFY_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({
        "kind":"native-write-before-after","build":"Rust 1.90 release + thin LTO",
        "repeats":3,"runs":24,"rows":rows,
        "limits":"synthetic V3 recording only; no capture CPU / ROSbag2 claim; whole-process peak RSS is not anonymous RSS; cached filesystem throughput is not physical device throughput"})).unwrap()).unwrap();
}
