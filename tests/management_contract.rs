use geminidepthport::{config, control::Controller, generation, util::*};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::symlink, process::Command, time::Instant};
fn fixture() -> Value {
    let mut d: Value = serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
    for (i, c) in d["cameras"].as_array_mut().unwrap().iter_mut().enumerate() {
        c["enabled"] = true.into();
        c["serial_number"] = format!("SYNTHETIC_{i}").into();
        c["timestamp_source"] = "global".into();
        c["health"] = json!({"discovery_seconds":1,"sample_seconds":2,"freshness_seconds":2,"min_fps_ratio":0.8});
        for stream in ["color", "depth"] {
            for (key, v) in [("width", 640), ("height", 480), ("fps", 30)] {
                c["parameters"][format!("{stream}_{key}")] = v.into();
            }
        }
    }
    d
}
#[test]
fn confirmations_are_bound_to_shared_and_per_camera_configuration() {
    let mut d = fixture();
    assert!(config::validate(&d, true).is_ok());
    assert!(config::validate(&d, false).is_err());
    let snapshot = d.clone();
    for c in d["cameras"].as_array_mut().unwrap() {
        c["profiles_verified"] = true.into();
        c["verification"] = json!({"method":"runtime-probe","configuration_digest":config::camera_digest(&snapshot,c)});
    }
    config::validate(&d, false).unwrap();
    let mut changed = d.clone();
    changed["cameras"][1]["parameters"]["color_fps"] = 15.into();
    let result = config::invalidate(&d, &changed);
    assert_eq!(result["cameras"][0]["profiles_verified"], true);
    assert_eq!(result["cameras"][1]["profiles_verified"], false);
    assert!(result["cameras"][1]["verification"].is_null());
    changed["ros_domain_id"] = 43.into();
    assert!(
        config::invalidate(&d, &changed)["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["profiles_verified"] == false)
    );
}
#[test]
fn rejected_configurations_cannot_be_deployed() {
    for (key, value, code) in [
        ("color_width", json!(641), "COLOR_WIDTH"),
        ("color_qos", json!("SENSOR_DATA"), "QOS_CONTRACT"),
        ("depth_registration", json!(true), "DEPTH_FORMAT"),
        ("serial_number", json!("SECRET / BAD"), "CAMERA_BINDING"),
    ] {
        let mut d = fixture();
        if key == "serial_number" {
            d["cameras"][0][key] = value;
        } else {
            d["cameras"][0]["parameters"][key] = value;
        }
        let e = config::validate(&d, true).unwrap_err().to_string();
        assert!(e.starts_with(code), "{e}");
        assert!(!e.contains("SECRET"));
    }
    let mut d = fixture();
    d["cameras"][1]["namespace"] = "/camera_a/child".into();
    assert!(
        config::validate(&d, true)
            .unwrap_err()
            .to_string()
            .starts_with("CAMERA_NAMESPACE")
    );
}
#[test]
fn python_canonical_json_keeps_unicode_and_float_semantics() {
    let v: Value =
        serde_json::from_str(r#"{"z":[1.0,0.000001,-0.000001,0.0001,1e16,1e15,-0.0],"中文":"😀"}"#)
            .unwrap();
    assert_eq!(
        canonical(&v),
        r#"{"z":[1.0,1e-06,-1e-06,0.0001,1e+16,1000000000000000.0,-0.0],"\u4e2d\u6587":"\ud83d\ude00"}"#
    );
}
#[test]
fn cross_process_lock_and_corrupt_recording_guard_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let ctl = Controller::new(tmp.path()).unwrap();
    atomic_json(&tmp.path().join("inventory.json"), &fixture()).unwrap();
    let lock = ctl.lock(true).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
        .args(["--root", tmp.path().to_str().unwrap(), "down"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("CONTROL_BUSY"));
    drop(lock);
    fs::write(tmp.path().join("state/recording-guard.json"), "broken").unwrap();
    assert!(
        ctl.lock(true)
            .unwrap_err_string()
            .contains("RECORDING_PROTECTED")
    );
    assert!(ctl.lock(false).is_ok());
    assert!(ctl.release("wrong").is_err());
    fs::remove_file(tmp.path().join("state/recording-guard.json")).unwrap();
    symlink("missing", tmp.path().join("state/recording-guard.json")).unwrap();
    assert!(ctl.unprotected().is_err());
}
trait ErrorString {
    fn unwrap_err_string(self) -> String;
}
impl<T> ErrorString for anyhow::Result<T> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected failure"),
            Err(e) => e.to_string(),
        }
    }
}
#[test]
fn generation_tampering_and_path_escape_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctl = Controller::new(tmp.path()).unwrap();
    let id = "a".repeat(32);
    let folder = tmp.path().join("state/generations").join(&id);
    private_dir(&folder).unwrap();
    let manifest = json!({"kind":generation::KIND,"generation":id,"cameras":[],"files":{"../../../outside":"x"}});
    atomic_json(&folder.join("manifest.json"), &manifest).unwrap();
    atomic_json(&tmp.path().join("state/current-generation.json"),&json!({"generation":id,"manifest_sha256":file_hash(&folder.join("manifest.json")).unwrap()})).unwrap();
    assert!(
        generation::load(&ctl)
            .unwrap_err()
            .to_string()
            .contains("GENERATION_INVALID")
    );
    fs::write(folder.join("manifest.json"), "{}").unwrap();
    assert!(
        generation::load(&ctl)
            .unwrap_err()
            .to_string()
            .contains("GENERATION_DRIFT")
    );
}
#[test]
fn command_deadline_terminates_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let e = command(
        tmp.path(),
        &args(&["/bin/sh", "-c", "sleep 10 & wait"]),
        1,
        None,
    )
    .unwrap_err();
    assert!(e.to_string().contains("COMMAND_TIMEOUT"));
    assert!(start.elapsed().as_secs() < 4);
}
#[test]
fn web_compose_uses_only_rust_management_entrypoints() {
    let s = generation::web_service();
    assert_eq!(s["command"][0], "geminidepthport");
    assert_eq!(s["healthcheck"]["test"][1], "geminidepthport");
    assert_eq!(s["volumes"][0]["source"], s["volumes"][0]["target"]);
    assert_eq!(s["ports"][0], "127.0.0.1:${WEB_PORT:-8765}:8765");
}

#[test]
fn python_oracle_configuration_migration_and_calibration_contracts() {
    let oracle: Value = serde_json::from_str(include_str!("rust/contracts.json")).unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        let result = config::validate(&case["data"], case["draft"] == true);
        let code = result
            .err()
            .map(|e| e.to_string().split(':').next().unwrap().to_string());
        assert_eq!(
            code.as_deref(),
            case["error"].as_str(),
            "case {}",
            case["name"]
        );
    }
    assert_eq!(
        config::migrate(&oracle["legacy"], &Value::Null).unwrap(),
        oracle["migrated"]
    );
    for case in oracle["requests"].as_array().unwrap() {
        assert_eq!(
            geminidepthport::calibration::request(&case["data"], text(&case["generation"]))
                .unwrap(),
            case["request"]
        );
        assert_eq!(
            geminidepthport::calibration::snapshot(
                &case["data"],
                text(&case["generation"]),
                &case["observations"]
            )
            .unwrap(),
            case["snapshot"]
        );
    }
}

#[test]
fn save_rejects_malformed_inventory_without_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let ctl = Controller::new(tmp.path()).unwrap();
    atomic_json(&tmp.path().join("inventory.json"), &fixture()).unwrap();
    for malformed in [
        json!(null),
        json!({"cameras":null}),
        json!({"cameras":[null]}),
    ] {
        let result = std::panic::catch_unwind(|| ctl.save(&malformed, false));
        assert!(result.is_ok(), "malformed input panicked");
        assert!(result.unwrap().is_err());
    }
}

#[test]
fn generation_requires_complete_file_manifest_and_inventory_camera_set() {
    let oracle: Value = serde_json::from_str(include_str!("rust/contracts.json")).unwrap();
    let d = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "three confirmed")
        .unwrap()["data"]
        .clone();
    let tmp = tempfile::tempdir().unwrap();
    let ctl = Controller::new(tmp.path()).unwrap();
    atomic_json(&tmp.path().join("inventory.json"), &d).unwrap();
    let id = "b".repeat(32);
    let folder = tmp.path().join("state/generations").join(&id);
    private_dir(&folder).unwrap();
    let manifest = json!({"kind":generation::KIND,"generation":id,"draft":false,"schema_version":2,"inventory_sha256":ctl.revision().unwrap(),"communication_profile":config::COMMUNICATION,"cameras":[{"name":"camera_a","service":"camera_camera_a"}],"files":{}});
    atomic_json(&folder.join("manifest.json"), &manifest).unwrap();
    atomic_json(&tmp.path().join("state/current-generation.json"),&json!({"generation":id,"manifest_sha256":file_hash(&folder.join("manifest.json")).unwrap()})).unwrap();
    assert!(
        generation::load(&ctl).is_err(),
        "incomplete file manifest was accepted"
    );
    assert!(
        ctl.check_render(&d).is_err(),
        "partial camera set was accepted"
    );
}

#[test]
fn worker_subcommand_deadline_kills_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    if std::env::var_os("GDP_TEST_WORKER").is_some() {
        install_worker_signals();
        let start = Instant::now();
        let error = command(
            tmp.path(),
            &args(&["/bin/sh", "-c", "sleep 10 & wait"]),
            1,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("COMMAND_TIMEOUT"));
        assert!(start.elapsed().as_secs() < 3);
    } else {
        let exe = std::env::current_exe().unwrap();
        command(
            tmp.path(),
            &args(&[
                "/usr/bin/env",
                "GDP_JOB_WORKER=1",
                "GDP_TEST_WORKER=1",
                exe.to_str().unwrap(),
                "--exact",
                "worker_subcommand_deadline_kills_descendants",
            ]),
            4,
            None,
        )
        .unwrap();
    }
}

#[test]
fn calibration_rejects_malformed_or_nonzero_roi_evidence() {
    let oracle: Value = serde_json::from_str(include_str!("rust/contracts.json")).unwrap();
    let case = &oracle["requests"][0];
    for roi in [
        Value::Null,
        json!(false),
        json!([]),
        json!({"width":true}),
        json!({"width":1}),
    ] {
        let mut observations = case["observations"].clone();
        observations["cameras"]["camera_a"]["color"]["roi"] = roi;
        assert!(
            geminidepthport::calibration::snapshot(
                &case["data"],
                text(&case["generation"]),
                &observations
            )
            .is_err()
        );
    }
}

#[test]
fn guarded_atomic_write_keeps_external_edits_and_cleans_temporary_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("inventory.json");
    std::fs::write(&path, b"external edit").unwrap();
    let result = atomic_checked(&path, b"old snapshot", || {
        require(false, "INVENTORY_DRIFT", "Inventory changed")
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"external edit");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn save_at_revision_rejects_an_outdated_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let ctl = Controller::new(tmp.path()).unwrap();
    let original = fixture();
    atomic_json(&tmp.path().join("inventory.json"), &original).unwrap();
    let (_, revision) = ctl.load_revision().unwrap();
    let mut changed = original.clone();
    changed["cameras"][0]["calibration_revision"] = "external".into();
    atomic_json(&tmp.path().join("inventory.json"), &changed).unwrap();
    assert!(
        ctl.save_at_revision(&original, false, &revision)
            .unwrap_err()
            .to_string()
            .starts_with("INVENTORY_DRIFT")
    );
    assert_eq!(ctl.load().unwrap(), changed);
}

#[test]
fn validation_rejects_incomplete_identity_profiles_and_legacy_migration_bindings() {
    let original: Value =
        serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
    for (path, value, code) in [
        ("/usbfs_minimum_mb", json!(0), "USBFS_POLICY"),
        ("/upstream_commit", json!("wrong"), "PROFILE_BASELINE"),
        ("/cameras/0/name", json!("bad name"), "CAMERA_ID"),
        ("/cameras/0/timestamp_source", json!(3), "CAMERA_METADATA"),
        ("/cameras/0/parameters", json!([]), "PARAMETER_FIELDS"),
    ] {
        let mut d = original.clone();
        if path == "/usbfs_minimum_mb" {
            d["usbfs_minimum_mb"] = value;
        } else {
            *d.pointer_mut(path).unwrap() = value;
        }
        assert!(
            config::validate(&d, true)
                .unwrap_err()
                .to_string()
                .starts_with(code),
            "{path}"
        );
    }
    let legacy: Value = serde_json::from_str(include_str!("fixtures/inventory.v1.json")).unwrap();
    for bindings in [
        json!([]),
        json!({"unknown":{"name":"test","namespace":"/test","frame_prefix":"test"}}),
        json!({"head":{"name":"test"}}),
    ] {
        assert!(config::migrate(&legacy, &bindings).is_err());
    }
    assert!(config::validate(&json!([]), true).is_err());
}

#[test]
fn fractional_negative_and_scientific_values_match_python_configuration_digests() {
    let value: Value = serde_json::from_str("[-12.34,1.234e20,1.234e-8,12.345,0.125]").unwrap();
    assert_eq!(
        canonical(&value),
        "[-12.34,1.234e+20,1.234e-08,12.345,0.125]"
    );
}

#[test]
fn legacy_inventory_cannot_enable_unbound_streams_or_unsupported_sources() {
    let original: Value = serde_json::from_str(include_str!("fixtures/inventory.v1.json")).unwrap();
    config::validate(&original, true).unwrap();
    for (pointer, value, reason) in [
        (
            "/startup_timeout_seconds",
            json!(0),
            "Invalid legacy settings",
        ),
        (
            "/cameras/0/enabled",
            json!(false),
            "Both head and wrist required",
        ),
        ("/cameras/0/name", json!("other"), "Maintain head and wrist"),
        (
            "/cameras/0/model",
            json!("other"),
            "Invalid legacy camera role",
        ),
        (
            "/cameras/0/parameters/uvc_backend",
            json!({}),
            "Invalid parameter",
        ),
        (
            "/cameras/0/parameters/uvc_backend",
            json!("v4l2"),
            "Unsupported legacy source",
        ),
        (
            "/cameras/0/parameters/enumerate_net_device",
            json!(true),
            "Unsupported stream/source",
        ),
        (
            "/cameras/0/parameters/enable_accel",
            json!(true),
            "Unsupported extra streams",
        ),
        (
            "/cameras/0/parameters/enable_depth",
            json!("true"),
            "Boolean streams required",
        ),
        (
            "/cameras/0/parameters/depth_width",
            json!(0),
            "Positive profile required",
        ),
        (
            "/cameras/0/serial_number",
            json!(""),
            "Empty or duplicate binding",
        ),
    ] {
        let mut d = original.clone();
        *d.pointer_mut(pointer).unwrap() = value;
        let error = config::validate(&d, true).unwrap_err().to_string();
        assert!(error.contains(reason), "{pointer}: {error}");
    }
    let mut d = original;
    d["cameras"][0]["parameters"]["enable_color"] = json!(false);
    d["cameras"][0]["parameters"]["enable_depth"] = json!(false);
    assert!(
        config::validate(&d, true)
            .unwrap_err()
            .to_string()
            .contains("Enable image stream")
    );
}
