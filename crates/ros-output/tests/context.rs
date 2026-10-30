use frame_core::imu::{ImuConfig, ImuKind, ImuSample};
use ros_output::{Binding, Calibration, ImuPublisher, Publisher, SharedRosContext};
use std::{fs, path::PathBuf, process::Command};
fn imu_config() -> ImuConfig {
    serde_json::from_str(r#"{"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"}"#).unwrap()
}
#[test]
#[ignore = "isolated explicit ROS context ABI child"]
fn context_child() {
    let path = PathBuf::from(std::env::var_os("GDP_CONTEXT_BRIDGE").unwrap());
    let mode = std::env::var("GDP_CONTEXT_MODE").unwrap();
    let opened = SharedRosContext::try_load(&path);
    if mode == "legacy" {
        assert!(opened.unwrap().is_none());
        return;
    }
    if mode != "normal" && mode != "global" {
        assert!(opened.is_err());
        return;
    }
    let context = opened.unwrap().unwrap();
    let calibration=Calibration::from_json(r#"{"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]}"#).unwrap();
    let image = Publisher::load_shared(
        &context,
        Binding {
            node_name: "image".into(),
            namespace: "/camera".into(),
            image_topic: "/camera/image".into(),
            camera_info_topic: "/camera/info".into(),
            metadata_topic: "/camera/meta".into(),
            frame_id: "test_optical".into(),
        },
        calibration,
    )
    .unwrap();
    let mut imu =
        ImuPublisher::load_shared(&context, "/camera", imu_config(), mode == "global").unwrap();
    drop(context);
    let events = PathBuf::from(std::env::var_os("GDP_ROS_MOCK_EVENTS").unwrap());
    assert!(
        !fs::read_to_string(&events)
            .unwrap()
            .contains("context-destroy")
    );
    drop(image);
    let mut sample = ImuSample {
        kind: ImuKind::Accel,
        sequence: 1,
        device_timestamp_us: 100,
        system_timestamp_us: 1_700_000_000_000_000,
        global_timestamp_us: (mode == "global").then_some(1_700_000_000_001_000),
        xyz: [1., 2., 3.],
        frame_id: "accel_frame".into(),
    };
    imu.publish(&sample).unwrap();
    if mode == "global" {
        sample.sequence += 1;
        sample.device_timestamp_us += 5000;
        sample.global_timestamp_us = Some(sample.global_timestamp_us.unwrap() + 5000);
        imu.publish(&sample).unwrap();
        let mut valid = sample.clone();
        valid.sequence += 1;
        valid.device_timestamp_us += 5000;
        valid.global_timestamp_us = Some(valid.global_timestamp_us.unwrap() + 5000);
        for field in 0..3 {
            let mut bad = valid.clone();
            match field {
                0 => bad.device_timestamp_us = sample.device_timestamp_us,
                1 => bad.global_timestamp_us = sample.global_timestamp_us,
                _ => bad.system_timestamp_us -= 1,
            }
            assert!(imu.publish(&bad).is_err());
        }
        imu.publish(&valid).unwrap();
    }
    assert!(
        !fs::read_to_string(&events)
            .unwrap()
            .contains("context-destroy")
    );
    drop(imu);
    let log = fs::read_to_string(events).unwrap();
    assert!(log.contains("image-shared-context") && log.contains("imu-shared-context"));
    assert_eq!(log.lines().filter(|l| *l == "context-destroy").count(), 1);
    assert!(log.rfind("context-destroy").unwrap() > log.rfind("imu-destroy").unwrap());
}
#[test]
fn explicit_context_extension_preserves_legacy_and_cleans_failed_creations() {
    let root = std::env::temp_dir().join(format!("gdp-context-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (mode, flag, env) in [
        ("normal", "", ""),
        ("global", "", ""),
        ("legacy", "MOCK_NO_CONTEXT", ""),
        ("partial", "MOCK_PARTIAL_CONTEXT", ""),
        ("bad-abi", "", "GDP_ROS_CONTEXT_BAD_ABI"),
        ("create", "", "GDP_ROS_CONTEXT_CREATE_ERROR"),
        ("with-handle", "", "GDP_ROS_CONTEXT_WITH_HANDLE"),
        ("null", "", "GDP_ROS_CONTEXT_NULL"),
    ] {
        let lib = root.join(format!("{mode}.so"));
        let events = root.join(format!("{mode}.events"));
        let mut cc = Command::new("c++");
        cc.args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(repo.join("ros/include"));
        if !flag.is_empty() {
            cc.arg(format!("-D{flag}"));
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
            .args(["--exact", "context_child", "--ignored", "--nocapture"])
            .env("GDP_CONTEXT_BRIDGE", lib)
            .env("GDP_CONTEXT_MODE", mode)
            .env("GDP_ROS_MOCK_EVENTS", &events);
        if !env.is_empty() {
            child.env(env, "1");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "with-handle" {
            assert_eq!(
                fs::read_to_string(events)
                    .unwrap()
                    .lines()
                    .filter(|l| *l == "context-destroy")
                    .count(),
                1
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}
