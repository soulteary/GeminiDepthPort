use frame_core::imu::{ImuConfig, ImuKind, ImuSample};
use ros_output::ImuPublisher;
use std::{fs, path::PathBuf, process::Command};
fn config() -> ImuConfig {
    serde_json::from_str(r#"{"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"}"#).unwrap()
}
#[test]
#[ignore = "isolated optional IMU bridge child"]
fn imu_bridge_child() {
    let lib = PathBuf::from(std::env::var_os("GDP_IMU_BRIDGE").unwrap());
    let mode = std::env::var("GDP_IMU_MODE").unwrap();
    let c = config();
    assert!(ImuPublisher::load(&lib, "relative", c.clone(), false).is_err());
    let loaded = ImuPublisher::load(&lib, "/camera", c.clone(), mode == "global");
    if ["missing", "create", "create-with-handle"].contains(&mode.as_str()) {
        assert!(loaded.is_err());
        return;
    }
    let mut publisher = loaded.unwrap();
    let mut sample = ImuSample {
        kind: ImuKind::Accel,
        sequence: 1,
        device_timestamp_us: 100,
        system_timestamp_us: 1_700_000_000_000_000,
        global_timestamp_us: None,
        xyz: [1., 2., 3.],
        frame_id: c.accel_frame_id,
    };
    if mode == "publish" || mode == "global" {
        assert!(publisher.publish(&sample).is_err());
        if mode == "publish" {
            return;
        }
        sample.global_timestamp_us = Some(sample.system_timestamp_us + 1);
    }
    let mut invalid = sample.clone();
    invalid.xyz[0] = f64::NAN;
    assert!(publisher.publish(&invalid).is_err());
    publisher.publish(&sample).unwrap();
    assert!(publisher.publish(&sample).is_err());
    sample.kind = ImuKind::Gyro;
    sample.frame_id = c.gyro_frame_id;
    publisher.publish(&sample).unwrap();
}
#[test]
fn optional_imu_bridge_preserves_old_abi_and_rejects_missing_extension_and_publish_errors() {
    let root = std::env::temp_dir().join(format!("gdp-imu-bridge-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (mode, flag) in [
        ("normal", ""),
        ("missing", ""),
        ("create", "GDP_ROS_IMU_CREATE_ERROR"),
        ("create-with-handle", "GDP_ROS_IMU_CREATE_WITH_HANDLE"),
        ("publish", "GDP_ROS_IMU_PUBLISH_ERROR"),
        ("global", ""),
    ] {
        let lib = root.join(format!("{mode}.so"));
        let mut cc = Command::new("c++");
        cc.args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(repo.join("ros/include"));
        if mode == "missing" {
            cc.arg("-DMOCK_NO_IMU");
        }
        let output = cc
            .arg(repo.join("crates/ros-output/tests/mock_bridge.cpp"))
            .arg("-o")
            .arg(&lib)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "imu_bridge_child", "--ignored", "--nocapture"])
            .env("GDP_IMU_BRIDGE", lib)
            .env("GDP_IMU_MODE", mode);
        if !flag.is_empty() {
            child.env(flag, "1");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::remove_dir_all(root).unwrap();
}
