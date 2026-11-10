//! Synthetic management contracts. No SDK, USB or real private identities.
use geminidepthport::{capture_backend, config, control::Controller, generation, util::*};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Output},
};

fn policy() -> Value {
    json!({"depth_unit_policy":"normalize-mm","pairing":{"clock":"device","max_delta_us":20000},"poll_ms":100,"capacity":4,"startup_discovery_seconds":2,"service":{"idle_timeout_ms":2000,"startup_timeout_ms":15000,"heartbeat_ms":500,"heartbeat_timeout_ms":10000,"max_restarts":3,"restart_backoff_ms":1000,"shutdown_timeout_ms":5000,"ready_min_pairs":5}})
}
fn fixture() -> Value {
    let mut d: Value = serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
    d["capture_backend"] = "rust".into();
    d["rust_capture"] = policy();
    d["image"] = "local/gdp-rust:synthetic".into();
    let c = &mut d["cameras"][0];
    c["enabled"] = true.into();
    c["serial_number"] = "SYNTHETIC_CAPTURE_ONLY".into();
    c["timestamp_source"] = "host".into();
    c["health"] =
        json!({"discovery_seconds":1,"sample_seconds":2,"freshness_seconds":2,"min_fps_ratio":0.8});
    for stream in ["color", "depth"] {
        for (key, v) in [("width", 640), ("height", 480), ("fps", 30)] {
            c["parameters"][format!("{stream}_{key}")] = v.into();
        }
    }
    d
}
fn manifest() -> Value {
    json!({"version":1,"upstream_commit":capture_backend::SOURCE_COMMIT,"sdk_version":"2.10.6","communication_profile":config::COMMUNICATION,"sdk_sha256":"a".repeat(64),"bridge_sha256":"b".repeat(64),"service_sha256":"c".repeat(64),"files":{"/usr/local/bin/capture-service":"c".repeat(64),"/opt/orbbec-sdk/lib/libOrbbecSDK.so":"a".repeat(64),"/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so":"b".repeat(64)},"sdk_source":{"repository":"https://github.com/orbbec/OrbbecSDK_ROS2","commit":capture_backend::SOURCE_COMMIT,"license_files":{"/opt/third-party/orbbec/ROS2-LICENSE":"f".repeat(64)}}})
}
#[test]
fn rust_backend_is_explicit_and_rejects_unapplied_parameters() {
    let d = fixture();
    config::validate(&d, true).unwrap();
    let mut old = d.clone();
    old.as_object_mut().unwrap().remove("capture_backend");
    assert!(
        config::validate(&old, true)
            .unwrap_err()
            .to_string()
            .starts_with("CAPTURE_BACKEND")
    );
    for (key, value) in [
        ("color_brightness", json!(100)),
        ("color_contrast", json!(0)),
        ("sync_mode", json!("standalone")),
        ("enable_point_cloud", json!(false)),
    ] {
        let mut bad = d.clone();
        bad["cameras"][0]["parameters"][key] = value;
        assert!(
            config::validate(&bad, true)
                .unwrap_err()
                .to_string()
                .starts_with("RUST_PARAMETER_UNSUPPORTED")
        );
    }
    let mut bad = d.clone();
    bad["cameras"][0]["timestamp_source"] = "global".into();
    assert!(config::validate(&bad, true).is_err());
    let mut bad = d.clone();
    bad["cameras"][0]["parameters"]["enable_depth"] = false.into();
    assert!(config::validate(&bad, true).is_err());
    let mut bad = d.clone();
    bad["rust_capture"]["service"]["max_restarts"] = 21.into();
    assert!(config::validate(&bad, true).is_err());
    let mut bad = d;
    bad["rust_capture"]["service"]["heartbeat_timeout_ms"] = 500.into();
    assert!(config::validate(&bad, true).is_err());
}
#[test]
fn generated_service_binds_runtime_without_finite_probe_duration() {
    let d = fixture();
    let runtime = json!({"manifest":manifest()});
    let c = config::resolved(&d, &d["cameras"][0]);
    let value = capture_backend::service_config(&d, &c, &"d".repeat(32), &runtime);
    assert!(value.get("seconds").is_none());
    assert_eq!(value["service"]["status_path"], "/tmp/capture/status.json");
    assert_eq!(value["service"]["generation"], "d".repeat(32));
    assert_eq!(value["sdk_library"], capture_backend::SDK_PATH);
    assert_eq!(value["ros"]["bridge"], capture_backend::BRIDGE_PATH);
    assert_eq!(value["runtime_contract"]["sdk_sha256"], "a".repeat(64));
    assert_eq!(value["color"]["frame_id"], "camera_a_color_optical_frame");
    assert_eq!(value["depth"]["frame_id"], "camera_a_depth_optical_frame");
}
#[test]
fn preview_revision_binds_shared_policy_for_rust_and_official() {
    let original = fixture();
    let before = capture_backend::preview_revision(&original, &original["cameras"][0]);
    assert_eq!(
        before,
        config::camera_digest(&original, &original["cameras"][0])
    );
    for (key, value) in [
        ("depth_unit_policy", json!("require-mm")),
        ("pairing", json!({"clock":"system","max_delta_us":20000})),
        ("service", json!({"changed":true})),
    ] {
        let mut changed = original.clone();
        changed["rust_capture"][key] = value;
        assert_eq!(
            config::signature(&original["cameras"][0]),
            config::signature(&changed["cameras"][0])
        );
        assert_ne!(
            before,
            capture_backend::preview_revision(&changed, &changed["cameras"][0])
        );
    }
    let mut official = original;
    official.as_object_mut().unwrap().remove("rust_capture");
    official.as_object_mut().unwrap().remove("capture_backend");
    assert_eq!(
        capture_backend::preview_revision(&official, &official["cameras"][0]),
        config::camera_digest(&official, &official["cameras"][0])
    );
}
#[test]
fn gemini_305_is_rust_only_and_identification_is_exact() {
    let mut d = fixture();
    d["cameras"][0]["profile"] = "gemini_305".into();
    config::validate(&d, true).unwrap();
    assert_eq!(
        config::resolved(&d, &d["cameras"][0])["model"],
        "Gemini 305"
    );
    d.as_object_mut().unwrap().remove("rust_capture");
    d["capture_backend"] = "official".into();
    assert!(
        config::validate(&d, true)
            .unwrap_err()
            .to_string()
            .starts_with("CAPTURE_BACKEND_PROFILE")
    );
    assert_eq!(
        geminidepthport::control::identify("Orbbec Gemini 305"),
        Some("Gemini 305")
    );
    assert_eq!(geminidepthport::control::identify("Gemini 305g"), None);
    assert_eq!(geminidepthport::control::identify("Gemini 305 L"), None);
}
fn cli(root: &std::path::Path, args_: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
        .arg("--root")
        .arg(root)
        .args(args_)
        .env("PATH", path)
        .output()
        .unwrap()
}
#[test]
fn rust_generation_hashes_compose_and_runtime_are_bound() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("patches/public-orbbec")).unwrap();
    fs::write(
        root.join("patches/public-orbbec/manifest.json"),
        include_str!("../patches/public-orbbec/manifest.json"),
    )
    .unwrap();
    atomic_json(&root.join("inventory.json"), &fixture()).unwrap();
    atomic_json(&root.join("runtime-fixture.json"), &manifest()).unwrap();
    let docker = root.join("bin/docker");
    fs::write(&docker,r#"#!/usr/bin/env python3
import json,os,sys,subprocess
from pathlib import Path
root=Path(os.environ['ORBBEC_DEPLOY_DIR']);a=sys.argv[1:]
with (root/'docker-calls.jsonl').open('a') as f:f.write(json.dumps(a)+'\n')
if a[:2]==['image','inspect']:
 print(json.dumps([{'Id':'sha256:'+'d'*64,'Config':{'Labels':{'org.geminidepthport.capture-backend':'rust-v1','org.opencontainers.image.revision':'c153462518ad674650bafa4464fda72c27ab797a'}}}]))
elif a and a[0]=='run' and a[-1]=='/opt/gdp-capture/runtime.json':print((root/'runtime-fixture.json').read_text())
elif a and a[0]=='ps':pass
elif a and a[0]=='compose':
 p=Path(a[a.index('-f')+1]);json.loads(p.read_text())
 if os.environ.get('GDP_REAL_DOCKER'):
  result=subprocess.run([os.environ['GDP_REAL_DOCKER'],*a]);sys.exit(result.returncode)
else:sys.exit(2)
"#).unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let result = cli(root, &["render", "--draft"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let ctl = Controller::new(root).unwrap();
    let record = generation::load(&ctl).unwrap().unwrap();
    assert_eq!(record["capture_backend"], "rust");
    assert_eq!(
        record["capture_runtime"]["image_id"],
        format!("sha256:{}", "d".repeat(64))
    );
    let folder = root
        .join("state/generations")
        .join(text(&record["generation"]));
    let compose = read_json(&folder.join("compose.yaml")).unwrap();
    let service = &compose["services"]["camera_camera_a"];
    assert_eq!(service["build"]["dockerfile"], "capture.Dockerfile");
    assert_eq!(
        service["healthcheck"]["test"],
        json!(["CMD", "/app/entrypoint.sh", "python3", "/app/health.py"])
    );
    assert_eq!(service["environment"]["ROS_LOCALHOST_ONLY"], "0");
    assert_eq!(service["stop_signal"], "SIGINT");
    assert_eq!(service["read_only"], true);
    let config_path = folder.join("config/camera_a/capture.json");
    let config = read_json(&config_path).unwrap();
    assert_eq!(config["service"]["generation"], record["generation"]);
    assert_eq!(
        config["service"]["recording_control_path"],
        "/recording/control.json"
    );
    assert!(service["volumes"].as_array().unwrap().iter().any(|m| {
        m["target"] == "/recording"
            && m["read_only"] == false
            && m["source"].as_str()
                == Some(
                    root.canonicalize()
                        .unwrap()
                        .join("state/recording-gates/camera_a")
                        .to_str()
                        .unwrap(),
                )
    }));
    assert_eq!(
        fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A tag rebuilt with different SDK/bridge/service bytes invalidates prior
    // runtime-bound profile confirmation even if inventory settings are equal.
    let mut confirmed = fixture();
    let digest = config::camera_digest(&confirmed, &confirmed["cameras"][0]);
    confirmed["cameras"][0]["profiles_verified"] = true.into();
    confirmed["cameras"][0]["verification"] = json!({"method":"runtime-probe","configuration_digest":digest,"runtime_digest":capture_backend::runtime_digest(&record["capture_runtime"])});
    config::validate(&confirmed, false).unwrap();
    atomic_json(&root.join("inventory.json"), &confirmed).unwrap();
    let mut changed_runtime = manifest();
    changed_runtime["service_sha256"] = "e".repeat(64).into();
    changed_runtime["files"][capture_backend::SERVICE_PATH] = "e".repeat(64).into();
    atomic_json(&root.join("runtime-fixture.json"), &changed_runtime).unwrap();
    let refused = cli(root, &["render"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("RUST_CONFIRMATION_STALE"));
    fs::write(&config_path, "{}").unwrap();
    assert!(
        generation::load(&ctl)
            .unwrap_err()
            .to_string()
            .starts_with("GENERATION_DRIFT")
    );
    let calls = fs::read_to_string(root.join("docker-calls.jsonl")).unwrap();
    assert!(calls.contains("/opt/gdp-capture/runtime.json"));
    assert!(!calls.contains("/dev/bus/usb"));
}

#[test]
fn legacy_usb_descriptor_requires_verified_vendor_product_and_exact_name() {
    use geminidepthport::control::identify_usb;
    assert_eq!(
        identify_usb("2bc5", "0670", "Orbbec(R) Gemini(TM)"),
        Some("Gemini 2")
    );
    for (vid, pid, name) in [
        ("0000", "0670", "Orbbec(R) Gemini(TM)"),
        ("2bc5", "0804", "Orbbec(R) Gemini(TM)"),
        ("2bc5", "", "Orbbec(R) Gemini(TM)"),
        ("2bc5", "0670", "Orbbec(R) Gemini(TM) Pro"),
    ] {
        assert_eq!(identify_usb(vid, pid, name), None);
    }
}
