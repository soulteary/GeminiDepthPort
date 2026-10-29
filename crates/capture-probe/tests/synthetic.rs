use std::process::Command;
fn probe(extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_capture-probe"))
        .args([
            "--synthetic",
            "--stream",
            "color",
            "--width",
            "640",
            "--height",
            "480",
            "--fps",
            "30",
        ])
        .args(extra)
        .output()
        .unwrap()
}
#[test]
fn overflow_is_bounded_and_accounted() {
    let out = probe(&["--frames", "10", "--capacity", "2", "--drain-every", "5"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["captured"], 10);
    assert_eq!(v["validated"], 10);
    assert_eq!(v["processed"], 4);
    assert_eq!(v["queue"]["dropped_oldest"], 6);
    assert_eq!(v["queue"]["high_water"], 2);
    assert_eq!(v["queue"]["remaining"], 0);
    assert_eq!(v["mode"], "synthetic");
    assert_eq!(v["hardware_verified"], false);
}
#[test]
fn invalid_limits_fail_without_exposing_arguments() {
    let out = probe(&["--capacity", "0", "--serial", "private-device-123"]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private-device-123"));
}
#[test]
fn no_overflow_when_draining_each_frame() {
    let out = probe(&["--frames", "9", "--capacity", "1"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["processed"], 9);
    assert_eq!(v["queue"]["dropped_oldest"], 0);
}
#[test]
fn exact_profile_is_required() {
    let out = Command::new(env!("CARGO_BIN_EXE_capture-probe"))
        .arg("--synthetic")
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn token_synthetic_cannot_claim_pixel_normalization() {
    let out = probe(&["--depth-unit-policy", "normalize-mm"]);
    assert!(!out.status.success());
}
#[test]
fn invalid_depth_policy_fails() {
    let out = probe(&["--depth-unit-policy", "guess"]);
    assert!(!out.status.success());
}
#[test]
fn color_normalization_policy_fails_before_sdk_access() {
    let out = Command::new(env!("CARGO_BIN_EXE_capture-probe"))
        .args([
            "--stream",
            "color",
            "--width",
            "640",
            "--height",
            "480",
            "--fps",
            "30",
            "--sdk-library",
            "missing-library",
            "--serial",
            "private-device",
            "--depth-unit-policy",
            "normalize-mm",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private-device"));
}

#[test]
fn partial_ros_arguments_are_rejected() {
    let out = probe(&["--ros-bridge", "/private/bridge.so"]);
    assert!(!out.status.success());
}
#[test]
fn synthetic_tokens_cannot_be_published_as_images() {
    let out = probe(&[
        "--ros-bridge",
        "/private/bridge.so",
        "--ros-namespace",
        "/test_camera",
        "--ros-frame-id",
        "test_color_optical_frame",
        "--ros-calibration",
        "/private/calibration.json",
        "--ros-stamp-source",
        "sdk-system-unix",
    ]);
    assert!(!out.status.success());
}

#[test]
fn cli_rejects_unbounded_experiments_and_incomplete_live_bindings_without_sdk_access() {
    for extra in [
        vec!["--unknown", "1"],
        vec!["--seconds", "301"],
        vec!["--width", "0"],
        vec!["--serial", "private-device"],
    ] {
        let out = probe(&extra);
        assert!(!out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-device"));
    }
    let help = Command::new(env!("CARGO_BIN_EXE_capture-probe"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Synthetic mode exercises software only")
    );
    for extra in [
        vec![],
        vec![
            "--sdk-library",
            "/missing.so",
            "--serial",
            "private-device",
            "--frames",
            "1",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_capture-probe"))
            .args([
                "--stream", "color", "--width", "640", "--height", "480", "--fps", "30",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-device"));
    }
}

#[test]
fn ros_bindings_reject_relative_paths_and_invalid_names_before_loading_libraries() {
    let base = vec![
        "--stream",
        "color",
        "--width",
        "640",
        "--height",
        "480",
        "--fps",
        "30",
        "--sdk-library",
        "/missing.so",
        "--serial",
        "private-device",
        "--ros-bridge",
        "/missing-bridge.so",
        "--ros-namespace",
        "/test",
        "--ros-frame-id",
        "test_optical",
        "--ros-calibration",
        "/missing.json",
        "--ros-stamp-source",
        "sdk-system-unix",
    ];
    for (key, replacement) in [
        ("--ros-bridge", "relative.so"),
        ("--ros-calibration", "relative.json"),
        ("--ros-namespace", "/bad//name"),
        ("--ros-frame-id", "/bad"),
    ] {
        let mut args = base.clone();
        let index = args.iter().position(|v| *v == key).unwrap();
        args[index + 1] = replacement;
        let out = Command::new(env!("CARGO_BIN_EXE_capture-probe"))
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-device"));
    }
}
