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

#[test]
fn pipeline_round_trip_explicit_checkpoint_drop_and_start_constraints() {
    let (snapshot, pair, pixels) = fixture();
    let root = tempfile::tempdir().unwrap();
    let mut legacy = RawRecordingWriter::begin(root.path(), "legacy", snapshot.clone()).unwrap();
    assert!(legacy.enable_checkpoint_pipeline().is_err());
    drop(legacy);
    let mut writer =
        RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot.clone(), 1_000_000)
            .unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    let diagnostics = writer.diagnostics();
    for n in 1..=65 {
        append(&mut writer, &pair, &pixels, n);
    }
    assert!(writer.enable_checkpoint_pipeline().is_err());
    writer.checkpoint().unwrap();
    assert_eq!(diagnostics.report()["durable_pairs"], 65);
    let path = writer.finish().unwrap();
    assert_eq!(read_all(&path, false).len(), 65);
    assert_eq!(
        diagnostics.report()["validation_image_bytes"],
        65 * (4 + pixels.len()) * 2
    );
    assert!(
        diagnostics.report()["uncommitted_pairs_peak"]
            .as_u64()
            .unwrap()
            <= 32
    );
    assert!(
        diagnostics.report()["uncommitted_payload_bytes_peak"]
            .as_u64()
            .unwrap()
            <= 16 * 1024 * 1024
    );
    let mut writer =
        RawRecordingWriter::begin_chunked(root.path(), "partial", snapshot, 1_000_000).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    for n in 1..=17 {
        append(&mut writer, &pair, &pixels, n);
    }
    drop(writer);
    assert_eq!(
        read_all(&root.path().join("partial.partial"), true).len(),
        17
    );
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "isolated pipeline readback fault child"]
fn pipeline_readback_child() {
    let root = std::path::PathBuf::from(std::env::var_os("GDP_PIPELINE_ROOT").unwrap());
    let mode = std::env::var("GDP_PIPELINE_MODE").unwrap();
    let (snapshot, pair, pixels) = fixture();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 1_000_000).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    for n in 1..=16 {
        append(&mut writer, &pair, &pixels, n);
    }
    writer.checkpoint().unwrap();
    let diagnostics = writer.diagnostics();
    fs::write(root.join("armed"), b"1").unwrap();
    for n in 17..=32 {
        append(&mut writer, &pair, &pixels, n);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while !root.join("injected").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // The verifier is blocked reading chunk 2; chunk 3 must still accept 15 pairs.
    for n in 33..=47 {
        append(&mut writer, &pair, &pixels, n);
    }
    assert_eq!(diagnostics.report()["durable_pairs"], 16);
    assert_eq!(diagnostics.report()["uncommitted_pairs_peak"], 31);
    assert_eq!(read_all(&root.join("capture.partial"), true).len(), 16);
    fs::write(root.join("ready"), b"1").unwrap();
    if mode == "kill" {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    if mode == "corrupt" {
        let path = root.join("capture.partial/payload-00000000000000000002.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
    }
    fs::write(root.join("release"), b"1").unwrap();
    if mode == "success" {
        writer.checkpoint().unwrap();
        assert_eq!(diagnostics.report()["durable_pairs"], 47);
        let path = writer.finish().unwrap();
        assert_eq!(read_all(&path, false).len(), 47);
    } else {
        assert!(writer.checkpoint().is_err());
        assert!(writer.finish().is_err());
        assert_eq!(read_all(&root.join("capture.partial"), true).len(), 16);
        assert!(!root.join("capture/COMPLETE").exists());
    }
}
#[cfg(target_os = "linux")]
#[test]
fn pipeline_overlaps_reads_with_writes_and_retains_prefix_on_read_error_corruption_and_sigkill() {
    for mode in ["success", "error", "corrupt", "kill"] {
        let root = tempfile::tempdir().unwrap();
        let library = io_faults::compile(root.path());
        let marker = root.path().join("injected");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "pipeline_readback_child",
                "--ignored",
                "--nocapture",
            ])
            .env("GDP_PIPELINE_ROOT", root.path())
            .env("GDP_PIPELINE_MODE", mode)
            .env("GDP_IO_ARMED", root.path().join("armed"))
            .env("GDP_IO_RELEASE", root.path().join("release"));
        if mode != "error" {
            command.env("GDP_IO_DELAY_MS", "0");
        }
        io_faults::inject(
            &mut command,
            &library,
            "read",
            "/payload-00000000000000000002.bin",
            &marker,
        );
        if mode == "kill" {
            let mut child = command.spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !root.path().join("ready").exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            child.kill().unwrap();
            assert!(!child.wait().unwrap().success());
            assert_eq!(
                read_all(&root.path().join("capture.partial"), true).len(),
                16
            );
        } else {
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        io_faults::assert_injected(&marker, "read");
    }
}

#[test]
fn pipeline_byte_credits_wait_before_an_indivisible_oversized_pair() {
    let (snapshot, mut pair, _) = fixture();
    let mut content = snapshot.content;
    let width = 2048;
    let height = 4097;
    let model = content.calibration.models.get_mut("depth").unwrap();
    model.width = width;
    model.height = height;
    content.binding.calibration_id = content.calibration.id().unwrap();
    let snapshot = content.seal().unwrap();
    for f in [&mut pair.color, &mut pair.depth] {
        f.context.snapshot_id = snapshot.snapshot_id.clone();
        f.context.calibration_id = snapshot.content.binding.calibration_id.clone();
    }
    pair.depth.width = width;
    pair.depth.height = height;
    pair.depth.stride = width as usize * 2;
    let pixels = vec![0; pair.depth.stride * height as usize];
    assert!(pixels.len() > 16 * 1024 * 1024);
    let root = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot, 80 * 1024 * 1024)
            .unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    let diagnostics = writer.diagnostics();
    for n in 1..=3 {
        append(&mut writer, &pair, &pixels, n);
    }
    let path = writer.finish().unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    for _ in 0..3 {
        assert!(source.validate_next().unwrap());
    }
    assert!(!source.validate_next().unwrap());
    // Peak can exceed 16 MiB only for this one whole pair, never two pairs.
    assert_eq!(diagnostics.report()["uncommitted_pairs_peak"], 1);
    assert!(
        diagnostics.report()["uncommitted_payload_bytes_peak"]
            .as_u64()
            .unwrap()
            < (pixels.len() + 65540) as u64
    );
}
#[test]
fn pipeline_budget_failure_never_publishes_complete() {
    let (snapshot, pair, pixels) = fixture();
    let root = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot, 100_000).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    let mut failed = false;
    for n in 1..=100 {
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
    assert!(!root.path().join("capture/COMPLETE").exists());
    let mut source =
        RecordingDirectorySource::open_recovered(&root.path().join("capture.partial")).unwrap();
    let mut count = 0;
    while source.validate_next().unwrap() {
        count += 1;
    }
    assert!(count >= 16);
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "isolated pending checkpoint commit failure child"]
fn pipeline_commit_failure_child() {
    let root = std::path::PathBuf::from(std::env::var_os("GDP_PIPELINE_ROOT").unwrap());
    let (snapshot, pair, pixels) = fixture();
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 1_000_000).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    for n in 1..=16 {
        append(&mut writer, &pair, &pixels, n);
    }
    writer.checkpoint().unwrap();
    let diagnostics = writer.diagnostics();
    for n in 17..=32 {
        append(&mut writer, &pair, &pixels, n);
    }
    fs::write(root.join("armed"), b"1").unwrap();
    assert!(writer.checkpoint().is_err());
    assert_eq!(diagnostics.report()["durable_pairs"], 16);
    assert!(writer.finish().is_err());
    let recovered = read_all(&root.join("capture.partial"), true);
    assert!(recovered.len() == 16 || recovered.len() == 32);
    assert!(!root.join("capture/COMPLETE").exists());
}
#[cfg(target_os = "linux")]
#[test]
fn pending_checkpoint_write_sync_and_rename_failures_never_acknowledge_new_prefix() {
    for (operation, suffix) in [
        ("write", "/.checkpoint.tmp"),
        ("fsync", "/.checkpoint.tmp"),
        ("rename", "/checkpoint.json"),
        ("fsync", "/capture.partial"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let library = io_faults::compile(root.path());
        let marker = root.path().join("injected");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "pipeline_commit_failure_child",
                "--ignored",
                "--nocapture",
            ])
            .env("GDP_PIPELINE_ROOT", root.path())
            .env("GDP_IO_ARMED", root.path().join("armed"));
        io_faults::inject(&mut command, &library, operation, suffix, &marker);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{operation} {suffix}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        io_faults::assert_injected(&marker, operation);
    }
}

fn imu_fixture() -> (
    SnapshotEnvelope,
    BoundPairEvidence,
    Vec<u8>,
    capture_contract::imu::ImuConfig,
) {
    let (snapshot, mut pair, pixels) = fixture();
    let mut content = snapshot.content;
    let c:capture_contract::imu::ImuConfig=serde_json::from_value(json!({"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"})).unwrap();
    content.configuration["imu"] = serde_json::to_value(&c).unwrap();
    content.configuration["acquisition"] =
        json!({"preset":"strict-rgbd","advanced":{"message_timestamp":"system"}});
    content.binding.configuration_id = capture_contract::ConfigurationId::parse(
        capture_contract::content_digest(
            capture_contract::ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    for f in [&mut pair.color, &mut pair.depth] {
        f.context.configuration_id = snapshot.content.binding.configuration_id.clone();
        f.context.snapshot_id = snapshot.snapshot_id.clone();
    }
    (snapshot, pair, pixels, c)
}
fn imu_samples(n: u64) -> Vec<capture_contract::imu::ImuSample> {
    use capture_contract::imu::*;
    [ImuKind::Accel, ImuKind::Gyro]
        .into_iter()
        .map(|kind| ImuSample {
            kind,
            sequence: n,
            device_timestamp_us: n * 5000,
            system_timestamp_us: 1_700_000_000_000_000 + n * 5000,
            global_timestamp_us: None,
            xyz: [1., 2., 3.],
            frame_id: if kind == ImuKind::Accel {
                "accel_frame"
            } else {
                "gyro_frame"
            }
            .into(),
        })
        .collect()
}
#[test]
fn imu_batches_are_hashed_verified_twice_and_recovered_with_both_v3_checkpoint_modes() {
    for pipeline in [false, true] {
        let (snapshot, pair, pixels, _) = imu_fixture();
        let root = tempfile::tempdir().unwrap();
        let mut w =
            RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot, 2_000_000).unwrap();
        if pipeline {
            w.enable_checkpoint_pipeline().unwrap();
        }
        let diagnostics = w.diagnostics();
        for n in 1..=35 {
            w.append_with_imu(
                &pair_at(&pair, n),
                &[1, 2, 3, 4],
                &pixels,
                StorageEndian::Little,
                &imu_samples(n),
            )
            .unwrap();
        }
        w.checkpoint().unwrap();
        let recovered = read_all(&root.path().join("capture.partial"), true);
        assert_eq!(recovered.len(), 35);
        for (i, f) in recovered.iter().enumerate() {
            assert_eq!(f.record.imu, imu_samples(i as u64 + 1));
        }
        let path = w.finish().unwrap();
        assert_eq!(diagnostics.report()["validation_imu_samples"], 140);
        assert_eq!(read_all(&path, false).len(), 35);
        let index = path.join("records.jsonl");
        let mut bytes = fs::read(&index).unwrap();
        let p = bytes.windows(3).position(|b| b == b"1.0").unwrap();
        bytes[p] = b'2';
        fs::write(index, bytes).unwrap();
        let mut source = RecordingDirectorySource::open(&path).unwrap();
        let result = (|| -> offline_depth::Result<()> {
            while source.validate_next()? {}
            Ok(())
        })();
        assert!(result.is_err());
    }
}
#[test]
fn imu_recording_rejects_unbound_missing_and_regressing_samples_and_preserves_legacy_v2() {
    let (snapshot, pair, pixels, _) = imu_fixture();
    let root = tempfile::tempdir().unwrap();
    let mut w = RawRecordingWriter::begin(root.path(), "legacy", snapshot.clone()).unwrap();
    w.append_with_imu(
        &pair_at(&pair, 1),
        &[1, 2, 3, 4],
        &pixels,
        StorageEndian::Little,
        &imu_samples(1),
    )
    .unwrap();
    let path = w.finish().unwrap();
    assert_eq!(read_all(&path, false)[0].record.imu, imu_samples(1));
    let mut w =
        RawRecordingWriter::begin_chunked(root.path(), "bad", snapshot.clone(), 1_000_000).unwrap();
    w.append_with_imu(
        &pair_at(&pair, 1),
        &[1, 2, 3, 4],
        &pixels,
        StorageEndian::Little,
        &imu_samples(1),
    )
    .unwrap();
    assert!(
        w.append_with_imu(
            &pair_at(&pair, 2),
            &[1, 2, 3, 4],
            &pixels,
            StorageEndian::Little,
            &imu_samples(1)
        )
        .is_err()
    );
    assert!(w.finish().is_err());
    let mut w =
        RawRecordingWriter::begin_chunked(root.path(), "missing", snapshot, 1_000_000).unwrap();
    append(&mut w, &pair, &pixels, 1);
    assert!(w.finish().is_err());
    let (snapshot, pair, pixels) = fixture();
    let mut w =
        RawRecordingWriter::begin_chunked(root.path(), "unbound", snapshot, 1_000_000).unwrap();
    assert!(
        w.append_with_imu(
            &pair,
            &[1, 2, 3, 4],
            &pixels,
            StorageEndian::Little,
            &imu_samples(1)
        )
        .is_err()
    );
}

#[test]
fn cached_imu_bindings_are_validated_at_writer_and_independent_reader_open() {
    for fault in ["profile", "acquisition"] {
        let (snapshot, pair, pixels, _) = imu_fixture();
        let root = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(root.path(), "valid", snapshot.clone(), 1_000_000)
                .unwrap();
        writer
            .append_with_imu(
                &pair_at(&pair, 1),
                &[1, 2, 3, 4],
                &pixels,
                StorageEndian::Little,
                &imu_samples(1),
            )
            .unwrap();
        let path = writer.finish().unwrap();
        let mut content = snapshot.content;
        if fault == "profile" {
            content.configuration["imu"]["accel_rate_hz"] = json!(333);
        } else {
            content.configuration["acquisition"] = json!({"preset":"invalid"});
        }
        content.binding.configuration_id = capture_contract::ConfigurationId::parse(
            capture_contract::content_digest(
                capture_contract::ContentKind::Configuration,
                &serde_json::to_vec(&content.configuration).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let invalid = content.seal().unwrap();
        assert!(RawRecordingWriter::begin(root.path(), "invalid", invalid.clone()).is_err());
        assert!(!root.path().join("invalid.partial").exists());
        // A new reader must rebuild its own binding even if the snapshot and
        // manifest have valid content hashes. The former writer's cache is irrelevant.
        fs::write(path.join("snapshot.json"), invalid.to_bytes().unwrap()).unwrap();
        let mut manifest: RecordingManifest =
            serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
        manifest.binding = invalid.content.binding;
        fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(RecordingDirectorySource::open(&path).is_err(), "{fault}");
    }
}

#[test]
fn cached_recording_binding_keeps_disabled_and_global_clock_policies() {
    for policy in ["disabled", "global"] {
        let (snapshot, mut pair, pixels, _) = imu_fixture();
        let mut content = snapshot.content;
        if policy == "disabled" {
            content.configuration["imu"]["enabled"] = json!(false);
            content
                .configuration
                .as_object_mut()
                .unwrap()
                .remove("acquisition");
        } else {
            content.configuration["acquisition"]["advanced"]["message_timestamp"] = json!("global");
        }
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
            frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
            frame.context.snapshot_id = snapshot.snapshot_id.clone();
        }
        let root = tempfile::tempdir().unwrap();
        if policy == "global" {
            let mut bad =
                RawRecordingWriter::begin(root.path(), "missing-global", snapshot.clone()).unwrap();
            assert!(
                bad.append_with_imu(
                    &pair_at(&pair, 1),
                    &[1, 2, 3, 4],
                    &pixels,
                    StorageEndian::Little,
                    &imu_samples(1)
                )
                .is_err()
            );
            assert!(bad.finish().is_err());
        }
        let mut samples = if policy == "disabled" {
            Vec::new()
        } else {
            imu_samples(1)
        };
        for sample in &mut samples {
            sample.global_timestamp_us = Some(sample.system_timestamp_us + 1);
        }
        let mut writer =
            RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot, 1_000_000).unwrap();
        let diagnostics = writer.diagnostics();
        writer
            .append_with_imu(
                &pair_at(&pair, 1),
                &[1, 2, 3, 4],
                &pixels,
                StorageEndian::Little,
                &samples,
            )
            .unwrap();
        let path = writer.finish().unwrap();
        assert_eq!(
            diagnostics.report()["validation_imu_samples"],
            samples.len() * 2
        );
        assert_eq!(read_all(&path, false)[0].record.imu, samples);
    }
}

#[test]
fn borrowed_shared_imu_batches_keep_index_bytes_and_readback_hashes_identical() {
    use capture_contract::imu::{SharedImuSample, ValidatedImuConfig};
    let (snapshot, pair, pixels, config) = imu_fixture();
    let binding = ValidatedImuConfig::new(config).unwrap();
    let root = tempfile::tempdir().unwrap();
    for chunked in [false, true] {
        let mut paths = Vec::new();
        for shared in [false, true] {
            let name = format!("capture-{chunked}-{shared}");
            let mut w = if chunked {
                RawRecordingWriter::begin_chunked(root.path(), &name, snapshot.clone(), 2_000_000)
                    .unwrap()
            } else {
                RawRecordingWriter::begin(root.path(), &name, snapshot.clone()).unwrap()
            };
            let stats = w.diagnostics();
            for n in 1..=35 {
                let samples = imu_samples(n);
                if shared {
                    let shared: Vec<SharedImuSample> = samples
                        .into_iter()
                        .map(|s| SharedImuSample {
                            frame_id: binding.frame_id(s.kind).clone(),
                            kind: s.kind,
                            sequence: s.sequence,
                            device_timestamp_us: s.device_timestamp_us,
                            system_timestamp_us: s.system_timestamp_us,
                            global_timestamp_us: s.global_timestamp_us,
                            xyz: s.xyz,
                        })
                        .collect();
                    w.append_shared_imu(
                        &pair_at(&pair, n),
                        &[1, 2, 3, 4],
                        &pixels,
                        StorageEndian::Little,
                        &shared,
                    )
                    .unwrap();
                } else {
                    w.append_with_imu(
                        &pair_at(&pair, n),
                        &[1, 2, 3, 4],
                        &pixels,
                        StorageEndian::Little,
                        &samples,
                    )
                    .unwrap();
                }
            }
            let path = w.finish().unwrap();
            // V3 retains checkpoint + final full scan; legacy V2 retains its
            // existing final scan. Buffer changes do not alter either policy.
            assert_eq!(
                stats.report()["validation_imu_samples"],
                if chunked { 140 } else { 70 }
            );
            let frames = read_all(&path, false);
            let bytes = fs::read(path.join("records.jsonl")).unwrap();
            let expected = frames
                .iter()
                .flat_map(|frame| {
                    let mut b = serde_json::to_vec(&frame.record).unwrap();
                    b.push(b'\n');
                    b
                })
                .collect::<Vec<_>>();
            assert_eq!(bytes, expected);
            paths.push(path);
        }
        assert_eq!(
            fs::read(paths[0].join("records.jsonl")).unwrap(),
            fs::read(paths[1].join("records.jsonl")).unwrap()
        );
    }
}
