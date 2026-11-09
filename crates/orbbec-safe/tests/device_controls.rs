use capture_contract::device::DeviceControls;
use orbbec_safe::{CaptureOptions, Sdk, VideoProfile};
use std::{fs, path::PathBuf, process::Command};

#[test]
fn device_startup_controls_probe_preflight_readback_and_restore_real_abi() {
    let root = std::env::temp_dir().join(format!("gdp-properties-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for case in [
        "ok",
        "probe",
        "missing",
        "missing-required",
        "unsupported",
        "readonly",
        "range",
        "step",
        "write-error",
        "readback",
        "rollback-error",
        "info-error",
        "null",
        "utf8",
        "long",
        "text",
        "pid",
        "support-error",
        "range-error",
        "get-error",
        "interference",
    ] {
        let failure = match case {
            "unsupported" => 3,
            "readonly" => 4,
            "range" => 5,
            "step" => 6,
            "write-error" => 7,
            "readback" => 8,
            "rollback-error" => 9,
            "info-error" => 1,
            "null" => 2,
            "utf8" => 10,
            "long" => 11,
            "text" => 12,
            "pid" => 13,
            "support-error" => 14,
            "range-error" => 15,
            "get-error" => 16,
            "interference" => 17,
            _ => 0,
        };
        let library = root.join(format!("{case}.so"));
        let mut command = Command::new("cc");
        command.args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        });
        command
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-DMOCK_DEVICE_FAILURE={failure}"));
        if case.starts_with("missing") {
            command.arg("-DMOCK_NO_DEVICE_PROPERTIES");
        }
        let out = command
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_sdk.c"))
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let events = root.join(format!("{case}.events"));
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "device_controls_child", "--ignored"])
            .env("GDP_PROPERTY_CASE", case)
            .env("GEMINIDEPTHPORT_MOCK_LIBRARY", &library)
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &events)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{case}: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let log = fs::read_to_string(events).unwrap();
        if !matches!(
            case,
            "ok" | "write-error" | "readback" | "rollback-error" | "interference"
        ) {
            assert!(
                !log.contains("property-write"),
                "preflight must not mutate: {case}"
            );
        }
        if !case.starts_with("missing") {
            assert_eq!(
                log.matches("device-info\n").count(),
                log.matches("device-info-delete\n").count(),
                "info ownership: {case}"
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "isolated dynamic ABI property fixture"]
fn device_controls_child() {
    let case = std::env::var("GDP_PROPERTY_CASE").unwrap();
    let sdk = Sdk::load(std::env::var("GEMINIDEPTHPORT_MOCK_LIBRARY").unwrap()).unwrap();
    let mut controls = DeviceControls {
        enable_color_auto_exposure: Some(false),
        color_exposure: Some(104),
        color_gain: Some(24),
        enable_ir_auto_exposure: Some(false),
        ir_exposure: Some(204),
        ir_gain: Some(28),
    };
    if matches!(case.as_str(), "probe" | "missing") {
        controls = Default::default();
    }
    if case == "step" {
        controls.color_exposure = Some(103);
    }
    let profile = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    let result = sdk.open_with_controls(
        "test-device",
        Some(profile),
        Some(profile),
        CaptureOptions::default(),
        &controls,
    );
    if matches!(case.as_str(), "ok" | "probe" | "missing") {
        let mut pipeline = result.unwrap();
        let report = pipeline.device_report().unwrap();
        if case == "missing" {
            assert!(!report.available);
            assert!(report.info.is_none());
        } else {
            assert!(report.available);
            assert_eq!(report.info.as_ref().unwrap().firmware, "1.4.98");
            assert_eq!(report.properties.len(), 6);
            assert!(report.properties.iter().all(|p| p.readable && p.writable));
            assert!(
                report
                    .properties
                    .iter()
                    .all(|p| p.readback_verified == (case == "ok"))
            );
            if case == "ok" {
                assert_eq!(report.properties[3].observed, Some(24));
            }
        }
        assert!(pipeline.wait(10).unwrap().is_some());
        pipeline.stop().unwrap();
    } else {
        let error = result.err().unwrap().to_string();
        let code = match case.as_str() {
            "missing-required" => "API_UNAVAILABLE",
            "unsupported" | "readonly" | "range" | "support-error" | "range-error"
            | "get-error" => "UNSUPPORTED",
            "step" => "OUT_OF_RANGE",
            "readback" | "interference" => "READBACK_MISMATCH",
            "null" => "INFO_NULL",
            "utf8" => "INFO_UTF8",
            "long" => "TEXT_LIMIT",
            "text" => "INFO_TEXT",
            "pid" => "USB_ID",
            _ => "mock SDK rejection",
        };
        assert!(error.contains(code), "{case}: {error}");
        if matches!(case.as_str(), "write-error" | "readback" | "interference") {
            assert!(error.contains("rollback_verified=true"), "{error}");
        }
        if case == "rollback-error" {
            assert!(error.contains("rollback_verified=false"));
        }
        let log =
            fs::read_to_string(std::env::var("GEMINIDEPTHPORT_MOCK_EVENTS").unwrap()).unwrap();
        assert!(
            !log.contains("wait\n"),
            "failed settings cannot deliver frames"
        );
        if matches!(case.as_str(), "write-error" | "readback" | "interference") {
            let mut restored = sdk
                .open_with_controls(
                    "test-device",
                    Some(profile),
                    Some(profile),
                    CaptureOptions::default(),
                    &DeviceControls::default(),
                )
                .unwrap();
            let values: Vec<_> = restored
                .device_report()
                .unwrap()
                .properties
                .iter()
                .map(|p| p.observed.unwrap())
                .collect();
            assert_eq!(values, vec![1, 1, 100, 10, 200, 20]);
            restored.stop().unwrap();
        }
    }
}
