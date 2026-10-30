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
    if std::env::var_os("GDP_READER_IMU").is_some() {
        content.configuration["imu"] = json!({"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"});
        content.configuration["acquisition"] =
            json!({"preset":"strict-rgbd","advanced":{"message_timestamp":"system"}});
    }
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
fn reader_benchmark_child() {
    let (width, height, pairs) = match std::env::var("GDP_VERIFY_PROFILE").unwrap().as_str() {
        "compact" => (640, 400, 512u64),
        "wide" => (1920, 1080, 64u64),
        _ => panic!("unknown benchmark profile"),
    };
    let root = PathBuf::from(std::env::var_os("GDP_VERIFY_ROOT").unwrap());
    let (snapshot, mut pair, pixels) = fixture(width, height);
    let cpu_started = cpu_seconds();
    let started = Instant::now();
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
        let mut samples = Vec::new();
        if std::env::var_os("GDP_READER_IMU").is_some() {
            use capture_contract::imu::*;
            for seq in (n - 1) * 20 / 3 + 1..=n * 20 / 3 {
                for kind in [ImuKind::Accel, ImuKind::Gyro] {
                    samples.push(ImuSample {
                        kind,
                        sequence: seq,
                        device_timestamp_us: seq * 5000,
                        system_timestamp_us: 1_700_000_000_000_000 + seq * 5000,
                        global_timestamp_us: None,
                        xyz: [1., 2., 3.],
                        frame_id: if kind == ImuKind::Accel {
                            "accel_frame"
                        } else {
                            "gyro_frame"
                        }
                        .into(),
                    });
                }
            }
        }
        writer
            .append_with_imu(&pair, &pixels, &pixels, StorageEndian::Little, &samples)
            .unwrap();
    }
    let path = writer.finish().unwrap();
    let seconds = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu_started;
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
    // PROFILE_BEGIN
    if std::env::var_os("GDP_READER_PROFILE").is_some() {
        let mut runs = Vec::new();
        let enabled_first = std::env::var_os("GDP_PROFILE_FIRST").is_some();
        for enabled in if enabled_first {
            [true, false]
        } else {
            [false, true]
        } {
            let mut reader = RecordingDirectorySource::open(&path).unwrap();
            if enabled {
                reader.enable_validation_cpu_profiling().unwrap();
            }
            let cpu = cpu_seconds();
            let started = Instant::now();
            let mut read = 0;
            while reader.validate_next().unwrap() {
                read += 1;
            }
            let wall = started.elapsed().as_secs_f64();
            let cpu = cpu_seconds() - cpu;
            assert_eq!(read, pairs);
            runs.push(json!({"enabled":enabled,"cpu_seconds":cpu,"wall_seconds":wall,"profile":reader.validation_cpu_profile()}));
        }
        fs::write(
            root.join("reader-profile.json"),
            serde_json::to_vec_pretty(&runs).unwrap(),
        )
        .unwrap();
    }
    // PROFILE_END
    let mut count = 0;
    while let Some(frame) = source.next_frame().unwrap() {
        assert_eq!(frame.depth, pixels);
        count += 1;
    }
    assert_eq!(count, pairs);
    fs::write(root.join("result.json"),serde_json::to_vec_pretty(&json!({
        "cpu_seconds":cpu,"cpu_cores":cpu/seconds,"width":width,"height":height,"pairs":pairs,"payload_bytes":payload_bytes,
        "total_seconds_including_commit_verification":seconds,"payload_mib_per_second":payload_bytes as f64/1048576./seconds,
        "peak_rss_kib":peak_rss_kib,"peak_rss_source":"/proc/self/status VmHWM; entire isolated child, sampled before post-timer materialized comparison; not anonymous-only RSS",
        "artifact_hashes":artifact_hashes,"files":fs::read_dir(&path).unwrap().count(),"pairs_read_back":count,"diagnostics":report})).unwrap()).unwrap();
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let u = unsafe { usage.assume_init() };
    (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64
        + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
}

#[test]
#[ignore = "explicit release reader A/B; no noisy CI performance threshold"]
fn reader_ab_benchmark() {
    let before = std::env::var_os("GDP_READER_BEFORE").unwrap();
    let after = std::env::var_os("GDP_READER_AFTER").unwrap();
    let mut rows = Vec::new();
    for repeat in 0..7 {
        for (profile, imu) in [("compact", false), ("compact", true), ("wide", false)] {
            let mut hashes = None;
            for (variant, binary) in if repeat % 2 == 0 {
                [("before", &before), ("after", &after)]
            } else {
                [("after", &after), ("before", &before)]
            } {
                let root = tempfile::tempdir().unwrap();
                let mut command = std::process::Command::new(binary);
                command
                    .args([
                        "--exact",
                        "reader_benchmark_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("GDP_VERIFY_ROOT", root.path())
                    .env("GDP_VERIFY_PROFILE", profile)
                    .env("GDP_CHECKPOINT_PIPELINE", "1")
                    .env_remove("GDP_READER_PROFILE")
                    .env_remove("LD_PRELOAD")
                    .env_remove("GDP_READER_IMU");
                if imu {
                    command.env("GDP_READER_IMU", "1");
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{variant} {profile}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let mut row: serde_json::Value =
                    serde_json::from_slice(&fs::read(root.path().join("result.json")).unwrap())
                        .unwrap();
                assert_eq!(
                    row["diagnostics"]["validation_image_bytes"]
                        .as_u64()
                        .unwrap(),
                    row["payload_bytes"].as_u64().unwrap() * 2
                );
                if imu {
                    assert_eq!(
                        row["diagnostics"]["validation_imu_samples"]
                            .as_u64()
                            .unwrap(),
                        row["pairs"].as_u64().unwrap() * 20 / 3 * 4
                    );
                }
                if let Some(ref previous) = hashes {
                    assert_eq!(&row["artifact_hashes"], previous);
                } else {
                    hashes = Some(row["artifact_hashes"].clone());
                }
                row["variant"] = json!(variant);
                row["profile"] = json!(profile);
                row["imu"] = json!(imu);
                row["repeat"] = json!(repeat);
                rows.push(row);
            }
        }
    }
    fs::write(std::env::var_os("GDP_READER_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({"kind":"reader-cpu-and-allocation-optimization","repeats":7,"runs":42,"rows":rows,"limits":"synthetic native writer; Linux ARM64 release, cached filesystem; CPU whole isolated writer process; no SDK/USB/DDS; profiling disabled in A/B; HWM before materialized comparison"})).unwrap()).unwrap();
}
