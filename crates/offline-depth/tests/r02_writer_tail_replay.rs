//! R02-shaped synthetic multi-writer faults; no change to the frozen benchmark driver.
#![cfg(target_os = "linux")]
#[path = "writer_contention_benchmark.rs"]
mod common;
use capture_contract::imu::ImuKind;
use offline_depth::{recording::*, sha256};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
fn monotonic_ns() -> u64 {
    common::monotonic_ns()
}
fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
fn verify(path: &Path, recovered: bool, expected: u64) -> Value {
    assert!(recovered);
    for _ in 0..2 {
        let mut reader = RecordingDirectorySource::open_recovered(path).unwrap();
        let mut count = 0u64;
        while let Some(frame) = reader.next_frame().unwrap() {
            count += 1;
            assert_eq!(frame.record.sequence, count);
            assert_eq!(frame.depth.len(), 640 * 400 * 2);
            for (i, pixel) in frame.depth.chunks_exact(2).enumerate() {
                assert_eq!(
                    u16::from_le_bytes([pixel[0], pixel[1]]),
                    ((i as u64 + count) % 60000 + 1) as u16
                );
            }
            let color: Vec<u8> = (0..640 * 480)
                .flat_map(|i| (((i as u64 + count) % 60000 + 1) as u16).to_le_bytes())
                .collect();
            assert_eq!(frame.record.color.file.sha256, sha256(&color));
            let first = (count - 1) * 200 / 30 + 1;
            let last = count * 200 / 30;
            assert_eq!(frame.record.imu.len(), ((last - first + 1) * 2) as usize);
            for (i, sample) in frame.record.imu.iter().enumerate() {
                let sequence = first + i as u64 / 2;
                assert_eq!(sample.sequence, sequence);
                assert_eq!(
                    sample.kind,
                    if i % 2 == 0 {
                        ImuKind::Accel
                    } else {
                        ImuKind::Gyro
                    }
                );
                assert_eq!(sample.device_timestamp_us, sequence * 5000);
                assert_eq!(
                    sample.system_timestamp_us,
                    1_700_000_000_000_000 + sequence * 5000
                );
                assert_eq!(
                    sample.global_timestamp_us,
                    Some(1_700_000_000_100_000 + sequence * 5000)
                );
                assert_eq!(sample.xyz, [1., 2., 3.]);
                assert_eq!(
                    sample.frame_id,
                    if i % 2 == 0 {
                        "accel_frame"
                    } else {
                        "gyro_frame"
                    }
                );
            }
        }
        assert_eq!(count, expected);
    }
    json!({"passes":2})
}

#[cfg(target_os = "linux")]
#[test]
fn r02_three_writer_multistage_tails_keep_limits_and_recover_confirmed_prefixes() {
    let root = tempfile::tempdir().unwrap();
    let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !source.join("tests/support/io_faults.c").exists() {
        source = source.parent().unwrap().to_path_buf();
    }
    let library = root.path().join("r02-tail.so");
    let built = std::process::Command::new("cc")
        .args([
            "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(source.join("tests/support/io_faults.c"))
        .args(["-ldl", "-o"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let barrier = root.path().join("start.json");
    let child_test = String::from("common::writer_contention_child");
    let mut children = Vec::new();
    for role in ["r02-a", "r02-b", "r02-c"] {
        let path = root.path().join(role);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("tail-armed"), b"1").unwrap();
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &child_test, "--ignored", "--nocapture"])
                .env("GDP_CONTENTION_ROOT", &path)
                .env("GDP_CONTENTION_MODE", "run")
                .env("GDP_CONTENTION_PAIRS", "120")
                .env("GDP_CONTENTION_BATCH_POLICY", "half-credit")
                .env("GDP_CONTENTION_BARRIER", &barrier)
                .env("LD_PRELOAD", &library)
                .env("GDP_IO_SYNC_TAIL_MODEL", role)
                .env("GDP_IO_SYNC_TAIL_ROOT", &path)
                .env("GDP_IO_SYNC_TAIL_ARM", path.join("tail-armed"))
                .env("GDP_IO_SYNC_TAIL_LOG", path.join("tail-log.jsonl"))
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !["r02-a", "r02-b", "r02-c"]
        .iter()
        .all(|r| root.path().join(r).join("ready.json").exists())
    {
        if Instant::now() >= deadline {
            for child in &mut children {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("owned R02 readiness deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(&barrier,serde_json::to_vec(&json!({"start_monotonic_ns":monotonic_ns()+200_000_000,"start_unix_ns":unix_ns()+200_000_000})).unwrap()).unwrap();
    loop {
        if children.iter_mut().all(|c| c.try_wait().unwrap().is_some()) {
            break;
        }
        if Instant::now() >= deadline {
            for child in &mut children {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("owned R02 multistage replay deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    for role in ["r02-a", "r02-b", "r02-c"] {
        let path = root.path().join(role);
        let result: Value =
            serde_json::from_slice(&fs::read(path.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["reason"], "RAW_QUEUE_OVERFLOW");
        assert_eq!(result["ok"], false);
        assert_eq!(result["fps"], 30);
        assert_eq!(result["imu_hz_per_stream"], 200);
        let shared_barrier: Value = serde_json::from_slice(&fs::read(&barrier).unwrap()).unwrap();
        assert_eq!(
            result["barrier_target_monotonic_ns"],
            shared_barrier["start_monotonic_ns"]
        );
        assert_eq!(result["queue_peak"], 16);
        assert_eq!(result["queue_capacity"], 16);
        assert_eq!(result["credit_pairs"], 32);
        assert_eq!(result["credit_bytes"], 16 * 1024 * 1024);
        assert!(
            result["diagnostics"]["uncommitted_payload_bytes_peak"]
                .as_u64()
                .unwrap()
                <= 16 * 1024 * 1024
        );
        assert!(
            result["diagnostics"]["uncommitted_pairs_peak"]
                .as_u64()
                .unwrap()
                <= 32
        );
        assert!(!path.join("capture/COMPLETE").exists());
        let partial = path.join("capture.partial");
        let checkpoint: Value =
            serde_json::from_slice(&fs::read(partial.join("checkpoint.json")).unwrap()).unwrap();
        let confirmed = checkpoint["record_count"].as_u64().unwrap();
        assert!(confirmed > 0);
        assert!(RecordingDirectorySource::open(&partial).is_err());
        let digest_files = || {
            fs::read_dir(&partial)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), sha256(&fs::read(entry.path()).unwrap()))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = digest_files();
        assert_eq!(verify(&partial, true, confirmed)["passes"], 2);
        assert_eq!(before, digest_files());
        let log = fs::read_to_string(path.join("tail-log.jsonl")).unwrap();
        for phase in [
            "payload_sync",
            "index_sync",
            "checkpoint_write",
            "checkpoint_directory_sync",
        ] {
            assert!(log.contains(phase), "{role}: {phase}");
        }
        if role == "r02-b" {
            assert!(log.contains("index_flush"));
            assert!(log.contains("checkpoint_rename"));
        }
    }
}
