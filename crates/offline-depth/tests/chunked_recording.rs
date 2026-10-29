mod support;
use capture_contract::snapshot::SnapshotEnvelope;
use frame_core::BoundPairEvidence;
use offline_depth::{StorageEndian, recording::*};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, process::Command};

fn fixture() -> (SnapshotEnvelope, BoundPairEvidence, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    support::fixture(dir.path(), true, 0.);
    (
        serde_json::from_slice(&fs::read(dir.path().join("snapshot.json")).unwrap()).unwrap(),
        serde_json::from_slice(&fs::read(dir.path().join("pair.json")).unwrap()).unwrap(),
        fs::read(dir.path().join("depth.raw16")).unwrap(),
    )
}
fn pair_at(pair: &BoundPairEvidence, n: u64) -> BoundPairEvidence {
    let mut p = pair.clone();
    p.pair_sequence = n;
    for frame in [&mut p.color, &mut p.depth] {
        frame.context.sequence = Some(n);
        frame.context.device_timestamp_us = Some(n * 33333);
        frame.context.system_timestamp_us = Some(n * 33333 + 1000000);
        frame.context.received_mono_offset_ns = n * 33333000;
    }
    p.color_stamp_us = n * 33333;
    p.depth_stamp_us = n * 33333;
    p
}
fn append(writer: &mut RawRecordingWriter, pair: &BoundPairEvidence, pixels: &[u8], n: u64) {
    writer
        .append(
            &pair_at(pair, n),
            &[1, 2, 3, 4],
            pixels,
            StorageEndian::Little,
        )
        .unwrap();
}
fn read_all(path: &Path, recovered: bool) -> Vec<RecordedFrame> {
    let mut source = if recovered {
        RecordingDirectorySource::open_recovered(path)
    } else {
        RecordingDirectorySource::open(path)
    }
    .unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = source.next_frame().unwrap() {
        frames.push(frame);
    }
    frames
}
#[test]
fn chunked_frames_round_trip_cross_chunk_boundaries_and_keep_original_evidence() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    let diagnostics = writer.diagnostics();
    for n in 1..=65 {
        append(&mut writer, &pair, &pixels, n);
    }
    assert_eq!(diagnostics.report()["durable_pairs"], 64);
    let path = writer.finish().unwrap();
    assert_eq!(diagnostics.report()["durable_pairs"], 65);
    assert_eq!(
        diagnostics.report()["validation_image_bytes"],
        65 * (4 + pixels.len()) * 2
    );
    assert!(
        diagnostics.report()["validation_buffer_peak_bytes"]
            .as_u64()
            .unwrap()
            <= 65536
    );
    assert_eq!(diagnostics.report()["payload_files"], 3);
    assert!(RecordingDirectorySource::open_recovered(&path).is_err());
    let frames = read_all(&path, false);
    assert_eq!(frames.len(), 65);
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(frame.depth, pixels);
        assert_eq!(frame.record.sequence, i as u64 + 1);
        let TaggedEvidence::SdkBoundPair { pair: actual, .. } = &frame.evidence else {
            panic!()
        };
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(pair_at(&pair, i as u64 + 1)).unwrap()
        );
    }
    assert!(!path.join("color-00000000000000000001.raw").exists());
    let result = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .args([
            "--input",
            path.to_str().unwrap(),
            "--recording",
            "--validate-only",
            "--preserve-counts",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
}
#[test]
fn recovery_ignores_uncommitted_truncated_tail_and_cannot_export_it_as_complete() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    for n in 1..=33 {
        append(&mut writer, &pair, &pixels, n);
    }
    let stage = dir.path().join("capture.partial");
    // Simulate abrupt loss, without running the graceful Drop checkpoint.
    std::mem::forget(writer);
    fs::OpenOptions::new()
        .append(true)
        .open(stage.join("records.jsonl"))
        .unwrap()
        .write_all(b"{torn tail")
        .unwrap();
    fs::write(stage.join("payload-00000000000000000002.bin"), b"torn").unwrap();
    let frames = read_all(&stage, true);
    assert_eq!(frames.len(), 32);
    assert!(frames.iter().all(|f| f.depth == pixels));
    assert!(!stage.join("COMPLETE").exists());
    let mut source = RecordingDirectorySource::open_recovered(&stage).unwrap();
    let result = offline_depth::recording::process(
        &mut source,
        Some((dir.path(), "export", depth_output::Mode::Directory)),
        offline_depth::Operation::PreserveCounts,
    );
    assert_eq!(result.unwrap_err().code(), "INCOMPLETE_RECORDING_INPUT");
    let result = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .args(["--recover-recording-prefix", stage.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(result.status.success());
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["complete"], false);
    assert_eq!(value["validated_pairs"], 32);
    assert_eq!(value["source_modified"], false);
    assert!(stage.join("payload-00000000000000000002.bin").exists());
}
#[test]
fn graceful_interruption_seals_only_valid_pairs_without_a_complete_marker() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    append(&mut writer, &pair, &pixels, 1);
    drop(writer);
    let stage = dir.path().join("capture.partial");
    assert_eq!(read_all(&stage, true).len(), 1);
    assert!(stage.join("INTERRUPTED.json").exists());
    assert!(!stage.join("COMPLETE").exists());
    assert!(!dir.path().join("capture").exists());
}
#[test]
fn corrupt_payload_checkpoint_index_ranges_and_chunk_tails_fail_closed() {
    for fault in [
        "pixels",
        "index",
        "offset",
        "overlap",
        "length",
        "path",
        "trailing",
        "snapshot",
        "checkpoint",
        "missing",
        "not-file",
        "metadata",
        "evidence-limit",
        "chunk-count",
        "chunk-limit",
        "checkpoint-version",
    ] {
        let (snapshot, pair, pixels) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
        append(&mut writer, &pair, &pixels, 1);
        writer.checkpoint().unwrap();
        std::mem::forget(writer);
        let stage = dir.path().join("capture.partial");
        let payload = stage.join("payload-00000000000000000001.bin");
        match fault {
            "pixels" => {
                let mut b = fs::read(&payload).unwrap();
                b[4] ^= 1;
                fs::write(&payload, b).unwrap();
            }
            "index" => {
                fs::write(stage.join("records.jsonl"), b"{}").unwrap();
            }
            "trailing" => {
                fs::OpenOptions::new()
                    .append(true)
                    .open(payload)
                    .unwrap()
                    .write_all(b"extra")
                    .unwrap();
            }
            "snapshot" => {
                fs::write(stage.join("snapshot.json"), b"{}").unwrap();
            }
            "checkpoint" => {
                fs::write(stage.join("checkpoint.json"), b"{}").unwrap();
            }
            "missing" => {
                fs::remove_file(payload).unwrap();
            }
            "not-file" => {
                fs::remove_file(&payload).unwrap();
                fs::create_dir(payload).unwrap();
            }
            "chunk-limit" => {
                fs::OpenOptions::new()
                    .write(true)
                    .open(payload)
                    .unwrap()
                    .set_len(129 * 1024 * 1024)
                    .unwrap();
            }
            "chunk-count" | "checkpoint-version" => {
                let mut c: Value =
                    serde_json::from_slice(&fs::read(stage.join("checkpoint.json")).unwrap())
                        .unwrap();
                c[if fault == "chunk-count" {
                    "chunks"
                } else {
                    "version"
                }] = json!(99);
                fs::write(stage.join("checkpoint.json"), c.to_string()).unwrap();
            }
            _ => {
                let mut record: Value =
                    serde_json::from_slice(&fs::read(stage.join("records.jsonl")).unwrap())
                        .unwrap();
                match fault {
                    "offset" => record["color"]["file"]["range"]["offset"] = json!(u64::MAX),
                    "overlap" => record["depth"]["file"]["range"]["offset"] = json!(0),
                    "length" => record["depth"]["file"]["range"]["length"] = json!(u64::MAX),
                    "path" => record["depth"]["file"]["path"] = json!("../escape"),
                    "metadata" => record["depth"]["metadata"] = record["depth"]["file"].clone(),
                    "evidence-limit" => record["evidence"]["range"]["length"] = json!(65537),
                    _ => unreachable!(),
                }
                let mut raw = serde_json::to_vec(&record).unwrap();
                raw.push(b'\n');
                fs::write(stage.join("records.jsonl"), &raw).unwrap();
                let mut checkpoint: Value =
                    serde_json::from_slice(&fs::read(stage.join("checkpoint.json")).unwrap())
                        .unwrap();
                checkpoint["index_bytes"] = json!(raw.len());
                checkpoint["records_sha256"] = json!(offline_depth::sha256(&raw));
                fs::write(stage.join("checkpoint.json"), checkpoint.to_string()).unwrap();
            }
        }
        let run = |materialize: bool| -> offline_depth::Result<()> {
            let mut source = RecordingDirectorySource::open_recovered(&stage)?;
            if materialize {
                while source.next_frame()?.is_some() {}
            } else {
                while source.validate_next()? {}
            }
            Ok(())
        };
        let expected = run(true).map_err(|e| e.code());
        let actual = run(false).map_err(|e| e.code());
        assert!(actual.is_err(), "{fault}");
        assert_eq!(actual, expected, "{fault}");
        assert!(!stage.join("COMPLETE").exists());
    }
}
#[test]
fn invalid_pair_budget_and_commit_cancellation_never_publish_complete() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "bad", snapshot.clone(), 1_000_000).unwrap();
    append(&mut writer, &pair, &pixels, 1);
    writer.checkpoint().unwrap();
    assert!(
        writer
            .append(&pair_at(&pair, 2), &[1], &pixels, StorageEndian::Little)
            .is_err()
    );
    assert!(writer.finish().is_err());
    assert_eq!(read_all(&dir.path().join("bad.partial"), true).len(), 1);
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "cancel", snapshot.clone(), 1_000_000)
            .unwrap();
    append(&mut writer, &pair, &pixels, 1);
    let result =
        writer.finish_guarded(|| Err::<(), _>(offline_depth::OfflineError::Invalid("CANCELLED")));
    assert_eq!(result.unwrap_err().code(), "CANCELLED");
    assert!(!dir.path().join("cancel").exists());
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "budget", snapshot, 10_000).unwrap();
    let mut failed = false;
    for n in 1..100 {
        if writer
            .append(
                &pair_at(&pair, n),
                &[1, 2, 3, 4],
                &pixels,
                StorageEndian::Little,
            )
            .is_err()
        {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(writer.finish().is_err());
    assert!(!dir.path().join("budget/COMPLETE").exists());
}

#[cfg(target_os = "linux")]
#[path = "../../../tests/support/io_faults.rs"]
mod io_faults;
#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess fixture for abrupt death and checkpoint I/O failures"]
fn checkpoint_child() {
    let root = std::path::PathBuf::from(std::env::var_os("GDP_CHUNK_ROOT").unwrap());
    let (snapshot, pair, pixels) = fixture();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 1_000_000).unwrap();
    for n in 1..=32 {
        append(&mut writer, &pair, &pixels, n);
    }
    assert_eq!(writer.diagnostics().report()["durable_pairs"], 32);
    fs::write(root.join("armed"), b"1").unwrap();
    let result = writer.append(
        &pair_at(&pair, 33),
        &[1, 2, 3, 4],
        &pixels,
        StorageEndian::Little,
    );
    if std::env::var_os("GDP_CHUNK_KILL").is_some() {
        result.unwrap();
        fs::write(root.join("ready"), b"1").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    assert!(result.and_then(|_| writer.checkpoint()).is_err());
    assert_eq!(writer.diagnostics().report()["durable_pairs"], 32);
    drop(writer);
}
#[cfg(target_os = "linux")]
#[test]
fn sigkill_preserves_the_last_checkpoint_without_running_drop() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "checkpoint_child", "--ignored"])
        .env("GDP_CHUNK_ROOT", root.path())
        .env("GDP_CHUNK_KILL", "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !root.path().join("ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let stage = root.path().join("capture.partial");
    assert_eq!(read_all(&stage, true).len(), 32);
    assert!(!stage.join("INTERRUPTED.json").exists());
    assert!(!stage.join("COMPLETE").exists());
}
#[cfg(target_os = "linux")]
#[test]
fn checkpoint_write_sync_and_rename_failures_never_promote_an_unverified_prefix() {
    for (op, suffix, nth, expected) in [
        ("writev", "/payload-00000000000000000002.bin", "1", 32),
        ("writev", "/payload-00000000000000000002.bin", "2", 32),
        ("write", "/records.jsonl", "1", 32),
        ("write", "/records.jsonl", "2", 32),
        ("fsync", "/payload-00000000000000000002.bin", "1", 32),
        ("fsync", "/records.jsonl", "1", 32),
        ("fsync", "/.checkpoint.tmp", "1", 32),
        ("fsync", "/capture.partial", "1", 32),
        ("rename", "/checkpoint.json", "1", 32),
        ("fsync", "/capture.partial", "2", 33),
    ] {
        let root = tempfile::tempdir().unwrap();
        let library = io_faults::compile(root.path());
        let marker = root.path().join("injected");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "checkpoint_child", "--ignored", "--nocapture"])
            .env("GDP_CHUNK_ROOT", root.path())
            .env("GDP_IO_ARMED", root.path().join("armed"))
            .env("GDP_IO_FAIL_NTH", nth);
        if op == "writev" && nth == "2" {
            command.env("GDP_IO_WRITEV_MAX_BYTES", "7");
        }
        if op == "write" && nth == "2" {
            command.env("GDP_IO_WRITE_MAX_BYTES", "7");
        }
        io_faults::inject(&mut command, &library, op, suffix, &marker);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{op} {suffix}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        io_faults::assert_injected(&marker, op);
        let stage = root.path().join("capture.partial");
        assert_eq!(read_all(&stage, true).len(), expected, "{op} {suffix}");
        assert!(!stage.join("COMPLETE").exists());
        assert!(!root.path().join("capture").exists());
    }
}

#[cfg(target_os = "linux")]
fn large_fixture() -> (SnapshotEnvelope, BoundPairEvidence, Vec<u8>) {
    let (snapshot, mut pair, _) = fixture();
    let mut content = snapshot.content;
    for model in content.calibration.models.values_mut() {
        model.width = 640;
        model.height = 400;
    }
    content.binding.calibration_id = content.calibration.id().unwrap();
    content.configuration =
        json!({"synthetic_native_benchmark":true,"source_depth_scale_mm":f64::from(0.1f32)});
    content.binding.configuration_id = capture_contract::ConfigurationId::parse(
        capture_contract::content_digest(
            capture_contract::ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    for frame in [&mut pair.color, &mut pair.depth] {
        frame.width = 640;
        frame.height = 400;
        frame.stride = 1280;
        frame.context.snapshot_id = snapshot.snapshot_id.clone();
        frame.context.calibration_id = snapshot.content.binding.calibration_id.clone();
        frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
    }
    let mut pixels = vec![0u8; 640 * 400 * 2];
    for p in pixels.chunks_exact_mut(2) {
        p[0] = 10;
    }
    (snapshot, pair, pixels)
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "release benchmark subprocess"]
fn recording_benchmark_child() {
    use std::time::Instant;
    let root = std::path::PathBuf::from(std::env::var_os("GDP_CHUNK_ROOT").unwrap());
    let (snapshot, pair, pixels) = large_fixture();
    let chunked = std::env::var("GDP_CHUNK_VARIANT").unwrap() == "v3";
    let started = Instant::now();
    let mut writer = if chunked {
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 512 * 1024 * 1024)
    } else {
        RawRecordingWriter::begin_with_limit(&root, "capture", snapshot, 512 * 1024 * 1024)
    }
    .unwrap();
    let diagnostics = writer.diagnostics();
    for n in 1..=128 {
        writer
            .append(&pair_at(&pair, n), &pixels, &pixels, StorageEndian::Little)
            .unwrap();
    }
    let append_seconds = started.elapsed().as_secs_f64();
    let path = writer.finish().unwrap();
    let total_seconds = started.elapsed().as_secs_f64();
    let file_count = fs::read_dir(&path).unwrap().count();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    let mut count = 0;
    while let Some(frame) = source.next_frame().unwrap() {
        assert_eq!(frame.depth, pixels);
        count += 1;
    }
    assert_eq!(count, 128);
    let payload_bytes = 128u64 * 2 * pixels.len() as u64;
    let result = json!({"append_seconds":append_seconds,"total_seconds_including_commit_verification":total_seconds,
        "payload_bytes":payload_bytes,"payload_mib_per_second":payload_bytes as f64/(1024.*1024.)/total_seconds,
        "files":file_count,"pairs_read_back":count,"diagnostics":diagnostics.report()});
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "explicit Linux release disk A/B benchmark; no CI performance thresholds"]
fn recording_chunk_ab_benchmark() {
    let mut rows = Vec::new();
    for repeat in 0..3 {
        for delay in [0, 2] {
            let variants = if repeat % 2 == 0 {
                ["v2", "v3"]
            } else {
                ["v3", "v2"]
            };
            for variant in variants {
                let root = tempfile::tempdir().unwrap();
                let library = io_faults::compile(root.path());
                let marker = root.path().join("injected");
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "recording_benchmark_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("GDP_CHUNK_ROOT", root.path())
                    .env("GDP_CHUNK_VARIANT", variant)
                    .env_remove("LD_PRELOAD");
                if delay > 0 {
                    command
                        .env("GDP_IO_DELAY_MS", delay.to_string())
                        .env("GDP_IO_REPEAT", "1");
                    io_faults::inject(&mut command, &library, "fsync", "", &marker);
                }
                let result = command.output().unwrap();
                assert!(
                    result.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                if delay > 0 {
                    io_faults::assert_injected(&marker, "fsync");
                }
                let mut row: Value =
                    serde_json::from_slice(&fs::read(root.path().join("result.json")).unwrap())
                        .unwrap();
                row["variant"] = json!(variant);
                row["repeat"] = json!(repeat);
                row["fsync_delay_ms"] = json!(delay);
                rows.push(row);
            }
        }
    }
    fs::write(std::env::var_os("GDP_CHUNK_BENCH_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({
        "kind":"native-recording-v2-v3-release-ab","pairs":128,"image_width":640,"image_height":400,
        "bytes_per_image":512000,"repeats":3,"rows":rows,"verification":"all target pairs read back and depth bytes compared; complete writer verification hashes both image streams and evidence",
        "limits":"synthetic owned buffers; filesystem/cache dependent; no SDK/USB/ROS/physical power-cut claim"})).unwrap()).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn byte_limit_rotates_chunks_without_splitting_a_large_pair() {
    let (snapshot, pair, pixels) = large_fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "large", snapshot, 64 * 1024 * 1024).unwrap();
    let diagnostics = writer.diagnostics();
    for n in 1..=17 {
        writer
            .append(&pair_at(&pair, n), &pixels, &pixels, StorageEndian::Little)
            .unwrap();
    }
    assert_eq!(diagnostics.report()["durable_pairs"], 16);
    assert_eq!(diagnostics.report()["payload_files"], 2);
    let path = writer.finish().unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    let mut count = 0;
    while let Some(frame) = source.next_frame().unwrap() {
        assert_eq!(frame.depth, pixels);
        assert_eq!(frame.record.color.file.path, frame.record.depth.file.path);
        count += 1;
    }
    assert_eq!(count, 17);
}
#[test]
fn recovered_prefix_validation_never_reports_complete_and_cli_rejects_ambiguous_arguments() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "prefix", snapshot, 1_000_000).unwrap();
    append(&mut writer, &pair, &pixels, 1);
    drop(writer);
    let mut source =
        RecordingDirectorySource::open_recovered(&dir.path().join("prefix.partial")).unwrap();
    let report = offline_depth::recording::process(
        &mut source,
        None,
        offline_depth::Operation::PreserveCounts,
    )
    .unwrap();
    assert_eq!(report.validated_pairs, 1);
    assert!(!report.complete);
    for args in [
        vec!["--recover-recording-prefix"],
        vec!["--recover-recording-prefix", "relative"],
        vec!["--recover-recording-prefix", "/tmp", "--output"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["complete"], false);
    }
}

#[test]
fn checkpoints_validate_new_pairs_before_superseding_the_last_valid_prefix() {
    for mode in ["duplicate", "geometry", "time"] {
        let (snapshot, pair, pixels) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
        append(&mut writer, &pair, &pixels, 1);
        writer.checkpoint().unwrap();
        let mut bad = pair_at(&pair, 2);
        let mut bytes = pixels.clone();
        match mode {
            "duplicate" => {
                bad.color.context.sequence = Some(1);
            }
            "geometry" => {
                bad.depth.width = 4;
                bad.depth.stride = 8;
                bytes.extend_from_slice(&pixels);
            }
            _ => {
                bad.color_stamp_us = 33333;
                bad.depth_stamp_us = 33333;
                bad.color.context.device_timestamp_us = Some(33333);
                bad.depth.context.device_timestamp_us = Some(33333);
            }
        }
        writer
            .append(&bad, &[1, 2, 3, 4], &bytes, StorageEndian::Little)
            .unwrap();
        assert!(writer.checkpoint().is_err(), "{mode}");
        assert_eq!(writer.diagnostics().report()["durable_pairs"], 1);
        drop(writer);
        assert_eq!(read_all(&dir.path().join("capture.partial"), true).len(), 1);
        assert!(!dir.path().join("capture/COMPLETE").exists());
    }
}

#[test]
fn missing_or_corrupt_v3_complete_marker_cannot_be_exported_as_a_finished_capture() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    append(&mut writer, &pair, &pixels, 1);
    let path = writer.finish().unwrap();
    fs::remove_file(path.join("COMPLETE")).unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    let report = offline_depth::recording::process(
        &mut source,
        None,
        offline_depth::Operation::PreserveCounts,
    )
    .unwrap();
    assert!(!report.complete);
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    let result = offline_depth::recording::process(
        &mut source,
        Some((dir.path(), "export", depth_output::Mode::Directory)),
        offline_depth::Operation::PreserveCounts,
    );
    assert_eq!(result.unwrap_err().code(), "INCOMPLETE_RECORDING_INPUT");
    fs::write(path.join("COMPLETE"), b"not a committed recording").unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    assert!(
        offline_depth::recording::process(
            &mut source,
            None,
            offline_depth::Operation::PreserveCounts
        )
        .is_err()
    );
}

#[test]
fn committed_chunked_recording_exports_original_depth_counts_and_source_blobs() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    append(&mut writer, &pair, &pixels, 1);
    let path = writer.finish().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .args([
            "--input",
            path.to_str().unwrap(),
            "--recording",
            "--output",
            dir.path().to_str().unwrap(),
            "--job",
            "exported",
            "--preserve-counts",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let decoded = support::decoded(&dir.path().join("exported/frames/00000000000000000000.png"));
    let expected: Vec<u16> = pixels
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    assert_eq!(decoded, expected);
    assert!(dir.path().join("exported/COMPLETE").exists());
}

#[test]
fn cached_chunk_reads_still_reject_truncation_or_growth_during_validation() {
    for grow in [false, true] {
        let (snapshot, pair, pixels) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
        append(&mut writer, &pair, &pixels, 1);
        append(&mut writer, &pair, &pixels, 2);
        let path = writer.finish().unwrap();
        let mut source = RecordingDirectorySource::open(&path).unwrap();
        source.next_frame().unwrap().unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path.join("payload-00000000000000000001.bin"))
            .unwrap();
        let length = file.metadata().unwrap().len();
        file.set_len(if grow { length + 1 } else { 4 }).unwrap();
        let result = (|| -> offline_depth::Result<()> {
            while source.next_frame()?.is_some() {}
            Ok(())
        })();
        assert!(result.is_err());
    }
}

#[test]
fn streaming_and_materialized_calls_share_progress_without_skipping_records() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    for n in 1..=3 {
        append(&mut writer, &pair, &pixels, n);
    }
    let path = writer.finish().unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    assert!(source.validate_next().unwrap());
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!(frame.record.sequence, 2);
    assert_eq!(frame.depth, pixels);
    assert!(source.validate_next().unwrap());
    assert!(!source.validate_next().unwrap());
    assert!(source.next_frame().unwrap().is_none());
    assert!(!source.validate_next().unwrap());
}
#[test]
fn checkpoint_and_final_streaming_verification_both_read_actual_payload_bytes() {
    for checkpointed in [false, true] {
        let (snapshot, pair, pixels) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
        append(&mut writer, &pair, &pixels, 1);
        if checkpointed {
            writer.checkpoint().unwrap();
        }
        let path = dir
            .path()
            .join("capture.partial/payload-00000000000000000001.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
        if checkpointed {
            append(&mut writer, &pair, &pixels, 2);
            assert_eq!(writer.finish().unwrap_err().code(), "RECORDING_DIGEST");
        } else {
            assert_eq!(writer.checkpoint().unwrap_err().code(), "RECORDING_DIGEST");
            assert_eq!(writer.diagnostics().report()["durable_pairs"], 0);
            drop(writer);
            assert!(!dir.path().join("capture.partial/checkpoint.json").exists());
        }
        assert!(!dir.path().join("capture/COMPLETE").exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "child proving actual writev short writes and EINTR remain lossless"]
fn short_writev_child() {
    let root = std::path::PathBuf::from(std::env::var_os("GDP_CHUNK_ROOT").unwrap());
    let (snapshot, pair, pixels) = fixture();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 1_000_000).unwrap();
    let diagnostics = writer.diagnostics();
    append(&mut writer, &pair, &pixels, 1);
    let path = writer.finish().unwrap();
    assert_eq!(read_all(&path, false)[0].depth, pixels);
    assert!(
        diagnostics.report()["payload_write_calls"]
            .as_u64()
            .unwrap()
            > 2
    );
    assert_eq!(diagnostics.report()["index_flushes"], 1);
    assert_eq!(diagnostics.report()["durable_pairs"], 1);
}
#[cfg(target_os = "linux")]
#[test]
fn real_short_writev_and_interrupted_call_preserve_all_payload_extents() {
    let root = tempfile::tempdir().unwrap();
    let library = io_faults::compile(root.path());
    let marker = root.path().join("injected");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "short_writev_child", "--ignored", "--nocapture"])
        .env("GDP_CHUNK_ROOT", root.path())
        .env("GDP_IO_WRITEV_MAX_BYTES", "7")
        .env("GDP_IO_ERRNO", "EINTR");
    io_faults::inject(
        &mut command,
        &library,
        "writev",
        "/payload-00000000000000000001.bin",
        &marker,
    );
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    io_faults::assert_injected(&marker, "writev");
}

#[test]
fn index_staging_is_bounded_and_flushed_before_checkpoint_acknowledgement() {
    let (snapshot, pair, pixels) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(dir.path(), "capture", snapshot, 1_000_000).unwrap();
    let diagnostics = writer.diagnostics();
    append(&mut writer, &pair, &pixels, 1);
    let index = dir.path().join("capture.partial/records.jsonl");
    assert_eq!(fs::metadata(&index).unwrap().len(), 0);
    assert_eq!(diagnostics.report()["index_flushes"], 0);
    writer.checkpoint().unwrap();
    assert!(fs::metadata(index).unwrap().len() > 0);
    assert_eq!(diagnostics.report()["index_flushes"], 1);
    assert_eq!(diagnostics.report()["index_write_calls"], 1);
    assert_eq!(diagnostics.report()["payload_write_calls"], 1);
    assert_eq!(read_all(&dir.path().join("capture.partial"), true).len(), 1);
    append(&mut writer, &pair, &pixels, 2);
    let path = writer.finish().unwrap();
    assert_eq!(read_all(&path, false).len(), 2);
    assert_eq!(diagnostics.report()["index_flushes"], 2);
    assert_eq!(
        diagnostics.report()["validation_image_bytes"],
        2 * 2 * (4 + pixels.len())
    );
}
