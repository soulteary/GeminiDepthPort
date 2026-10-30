use capture_contract::imu::ImuConfig;
use orbbec_safe::{CaptureOptions, Sdk, VideoProfile};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
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
    if mode == "overflow" {
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
    } else if mode != "normal" {
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
