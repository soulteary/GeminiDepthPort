use capture_contract::imu::ImuConfig;
use orbbec_safe::{CaptureOptions, Sdk, VideoProfile};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
struct CountAlloc;
static COUNT_ALLOC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ALLOCATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
unsafe impl std::alloc::GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if COUNT_ALLOC.load(std::sync::atomic::Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        if COUNT_ALLOC.load(std::sync::atomic::Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { std::alloc::System.realloc(p, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;
fn config() -> ImuConfig {
    serde_json::from_str(r#"{"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":100,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"}"#).unwrap()
}
#[test]
#[ignore = "isolated optional IMU callback ABI child"]
fn imu_child() {
    let path = std::env::var_os("GDP_IMU_LIBRARY").unwrap();
    let mode = std::env::var("GDP_IMU_MODE").unwrap();
    let sdk = Sdk::load(path).unwrap();
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    let mut pipeline = sdk
        .open_with_options(
            "test-device",
            Some(p),
            Some(p),
            CaptureOptions {
                global_timestamp: Some(true),
                ..CaptureOptions::default()
            },
        )
        .unwrap();
    let mut disabled = config();
    disabled.enabled = false;
    assert!(pipeline.start_imu(disabled, true).is_err());
    let mut invalid = config();
    invalid.queue_capacity = 0;
    assert!(pipeline.start_imu(invalid, true).is_err());
    let mut requested = config();
    if mode == "slow" {
        requested.accel_rate_hz = 25;
        requested.gyro_rate_hz = 25;
    }
    if mode != "stale" && mode != "gyro-stale" {
        requested.stale_timeout_ms = 5000;
    }
    let opened = pipeline.start_imu(requested, true);
    if ["missing", "unsupported", "profile", "start"].contains(&mode.as_str()) {
        assert!(opened.is_err());
        pipeline.stop().unwrap();
        return;
    }
    let mut imu = opened.unwrap();
    if mode == "stop" || mode == "partial-stop" {
        assert!(imu.stop().is_err());
        drop(imu);
        let log =
            fs::read_to_string(std::env::var_os("GEMINIDEPTHPORT_MOCK_EVENTS").unwrap()).unwrap();
        assert!(!log.lines().any(|l| l == "imu-sensor-delete"));
        pipeline.stop().unwrap();
        return;
    }
    if mode == "reuse" {
        let mut samples = Vec::with_capacity(128);
        let first = samples.as_ptr();
        let mut second = None;
        let mut accel_frame = None;
        let mut seen = 0;
        let mut received = [0; 2];
        let mut turns = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        // Warm lazy platform mutex/thread state before measuring steady state.
        while !imu.counters().0.iter().all(|n| *n >= 2) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        ALLOCATIONS.store(0, std::sync::atomic::Ordering::Relaxed);
        COUNT_ALLOC.store(true, std::sync::atomic::Ordering::Relaxed);
        while seen < 32 || turns < 100 {
            assert!(Instant::now() < deadline);
            let counters = imu.drain_into(&mut samples).unwrap();
            assert!(counters.0[0] >= received[0] && counters.0[1] >= received[1]);
            received = counters.0;
            assert!(counters.1 <= 128);
            assert!(samples.capacity() >= 128);
            if turns == 0 {
                second = Some(samples.as_ptr());
            }
            assert_eq!(
                samples.as_ptr(),
                if turns % 2 == 0 {
                    second.unwrap()
                } else {
                    first
                }
            );
            for sample in &samples {
                if sample.kind == capture_contract::imu::ImuKind::Accel {
                    if let Some(frame) = &accel_frame {
                        assert!(std::sync::Arc::ptr_eq(frame, &sample.frame_id));
                    } else {
                        accel_frame = Some(sample.frame_id.clone());
                    }
                }
            }
            assert_eq!(received[0] + received[1], (seen + samples.len()) as u64);
            seen += samples.len();
            turns += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        COUNT_ALLOC.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "shared callback and reused drain must not allocate after initialization"
        );
        let shared_samples = seen;
        let shared_drains = turns;
        samples.clear();
        drop(imu.drain().unwrap()); // Warm the compatibility path's spare buffer too.
        let mut seen = 0;
        ALLOCATIONS.store(0, std::sync::atomic::Ordering::Relaxed);
        COUNT_ALLOC.store(true, std::sync::atomic::Ordering::Relaxed);
        while seen < 16 {
            assert!(Instant::now() < deadline);
            seen += imu.drain().unwrap().len();
            std::thread::sleep(Duration::from_millis(1));
        }
        COUNT_ALLOC.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed) >= seen);
        if let Some(path) = std::env::var_os("GDP_IMU_ALLOC_REPORT") {
            fs::write(path, serde_json::to_vec_pretty(&serde_json::json!({
                "scope":"isolated process, all Rust allocations after platform/SDK warmup; excludes C SDK malloc",
                "shared_samples":shared_samples,"shared_drains":shared_drains,"shared_allocations":0,
                "owned_samples":seen,"owned_allocations":ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed),
                "queue_capacity":128,"same_frame_storage":true,"two_reused_sdk_buffers":true
            })).unwrap()).unwrap();
        }
    } else if mode == "overflow" {
        let deadline = Instant::now() + Duration::from_secs(5);
        let path = std::env::var_os("GEMINIDEPTHPORT_MOCK_EVENTS").unwrap();
        while !fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("imu-burst-sent")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(imu.drain().unwrap_err().0, "IMU_QUEUE_OVERFLOW");
    } else if mode == "stale" || mode == "gyro-stale" {
        std::thread::sleep(Duration::from_millis(140));
        assert!(imu.drain().is_err());
    } else if mode != "normal" && mode != "slow" {
        let deadline = Instant::now() + Duration::from_secs(3);
        let error = loop {
            match imu.drain() {
                Err(e) => break e,
                Ok(_) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        };
        let expected = match mode.as_str() {
            "nan" | "regress" => "IMU_INVALID_OR_REGRESSING",
            "release" => "IMU_FRAME_RELEASE_FAILED",
            _ => "IMU_CALLBACK_FAILED",
        };
        assert_eq!(error.0, expected);
    } else {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut counts = [0; 2];
        while !counts.iter().all(|n| *n > 2) {
            assert!(Instant::now() < deadline);
            for sample in imu.drain().unwrap() {
                assert_eq!(sample.xyz, [1., 2., 9.800000190734863]);
                assert_eq!(
                    sample.device_timestamp_us,
                    sample.sequence * if mode == "slow" { 40000 } else { 5000 }
                );
                counts[sample.kind.slot()] += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(counts.iter().all(|n| *n > 2));
        assert!(imu.counters().1 <= 128);
    }
    imu.stop().unwrap();
    drop(imu);
    pipeline.stop().unwrap();
    drop(pipeline);
    drop(sdk);
}
#[test]
fn optional_imu_abi_covers_callbacks_boundedness_errors_and_cleanup() {
    let root = std::env::temp_dir().join(format!("gdp-imu-abi-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for (mode, env) in [
        ("normal", ""),
        ("reuse", ""),
        ("slow", ""),
        ("unsupported", "GDP_MOCK_IMU_UNSUPPORTED"),
        ("profile", "GDP_MOCK_IMU_PROFILE_ERROR"),
        ("start", "GDP_MOCK_IMU_START_ERROR"),
        ("overflow", "GDP_MOCK_IMU_BURST"),
        ("stale", "GDP_MOCK_IMU_STALL"),
        ("gyro-stale", "GDP_MOCK_IMU_GYRO_STALL"),
        ("nan", "GDP_MOCK_IMU_NAN"),
        ("null", "GDP_MOCK_IMU_NULL_FRAME"),
        ("type", "GDP_MOCK_IMU_BAD_TYPE"),
        ("read", "GDP_MOCK_IMU_READ_ERROR"),
        ("release", "GDP_MOCK_IMU_RELEASE_ERROR"),
        ("regress", "GDP_MOCK_IMU_REGRESS"),
        ("stop", "GDP_MOCK_IMU_STOP_ERROR"),
        ("partial-stop", "GDP_MOCK_IMU_GYRO_STOP_ERROR"),
        ("missing", ""),
    ] {
        let lib = root.join(format!("{mode}.so"));
        let mut cc = Command::new("cc");
        cc.args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        });
        cc.args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"]);
        if mode == "missing" {
            cc.arg("-DMOCK_NO_IMU");
        }
        let out = cc
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_sdk.c"))
            .arg("-o")
            .arg(&lib)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "imu_child", "--ignored", "--nocapture"])
            .env("GDP_IMU_LIBRARY", lib)
            .env("GDP_IMU_MODE", mode)
            .env(
                "GEMINIDEPTHPORT_MOCK_EVENTS",
                root.join(format!("{mode}.events")),
            );
        if !env.is_empty() {
            c.env(env, "1");
        }
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fs::remove_dir_all(root).unwrap();
}
