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
        "probe-unsupported",
        "probe-error",
        "outside",
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
        "auto-grid",
        "grid-probe",
        "grid-step",
        "grid-outside",
        "default-grid",
        "negative-step",
        "default-outside",
        "range-current-outside",
        "get-current-outside",
        "write-support-error",
        "auto-grid-rollback",
        "long-sdk-error",
        "uvc-30",
        "uvc-31",
        "uvc-32",
        "uvc-33",
        "uvc-34",
        "uvc-35",
        "uvc-36",
        "uvc-37",
        "uvc-38",
        "uvc-39",
    ] {
        let failure = if let Some(code) = case.strip_prefix("uvc-") {
            code.parse::<u32>().unwrap()
        } else {
            match case {
                "unsupported" | "probe-unsupported" => 3,
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
                "get-error" | "probe-error" => 16,
                "interference" => 17,
                "auto-grid" | "grid-probe" | "grid-step" | "grid-outside" => 18,
                "default-grid" => 19,
                "negative-step" => 20,
                "default-outside" => 21,
                "range-current-outside" => 22,
                "get-current-outside" => 23,
                "write-support-error" => 24,
                "auto-grid-rollback" => 26,
                "long-sdk-error" => 25,
                _ => 0,
            }
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
            "ok" | "uvc-36"
                | "auto-grid"
                | "default-grid"
                | "auto-grid-rollback"
                | "write-error"
                | "readback"
                | "rollback-error"
                | "interference"
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
    if matches!(
        case.as_str(),
        "probe" | "grid-probe" | "probe-unsupported" | "probe-error" | "missing"
    ) {
        controls = Default::default();
    }
    if case.starts_with("uvc-") {
        controls.color_gain = Some(103);
    }
    if case == "step" {
        controls.color_exposure = Some(103);
    }
    if case == "outside" {
        controls.ir_gain = Some(i32::MAX);
    }
    if [
        "auto-grid",
        "grid-step",
        "grid-outside",
        "default-grid",
        "negative-step",
        "default-outside",
        "range-current-outside",
        "get-current-outside",
        "auto-grid-rollback",
    ]
    .contains(&case.as_str())
    {
        controls.color_gain = Some(match case.as_str() {
            "grid-step" => 104,
            "grid-outside" => 257,
            _ => 103,
        });
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
    if matches!(
        case.as_str(),
        "ok" | "uvc-36"
            | "auto-grid"
            | "default-grid"
            | "probe"
            | "grid-probe"
            | "probe-unsupported"
            | "probe-error"
            | "missing"
    ) {
        let mut pipeline = result.unwrap();
        let report = pipeline.device_report().unwrap();
        if case == "missing" {
            assert!(!report.available);
            assert!(report.info.is_none());
        } else {
            assert!(report.available);
            assert_eq!(report.info.as_ref().unwrap().firmware, "1.4.98");
            assert_eq!(report.properties.len(), 6);
            if case == "probe-unsupported" {
                assert!(
                    report
                        .properties
                        .iter()
                        .all(|p| !p.readable && !p.writable && p.range.is_none())
                );
            } else {
                assert!(report.properties.iter().all(|p| p.readable && p.writable));
            }
            if case == "probe-error" {
                assert!(
                    report
                        .properties
                        .iter()
                        .filter(|p| p.kind == "int")
                        .all(|p| p.error.is_some() && p.observed.is_none())
                );
            }
            assert!(report.properties.iter().all(|p| p.readback_verified
                == ["ok", "uvc-36", "auto-grid", "default-grid"].contains(&case.as_str())));
            if case == "uvc-36" {
                assert_eq!(report.properties[3].getter_uvc.backend, Some("libuvc"));
                assert_eq!(report.properties[3].getter_returned, Some(101));
                assert_eq!(report.properties[3].observed, Some(103));
            }
            if case == "ok" {
                assert_eq!(report.properties[3].observed, Some(24));
            }
            if ["auto-grid", "default-grid", "grid-probe"].contains(&case.as_str()) {
                let gain = &report.properties[3];
                assert_eq!(gain.range_current, Some(100));
                assert_eq!(
                    gain.observed_before,
                    Some(if case == "default-grid" { 101 } else { 102 })
                );
                assert_eq!(
                    gain.observed,
                    Some(if case == "grid-probe" { 102 } else { 103 })
                );
                assert_eq!(report.properties[0].observed_before, Some(1));
                assert_eq!(report.properties[1].observed_before, Some(1));
                assert!(gain.finished_monotonic_ns.unwrap() >= gain.started_monotonic_ns.unwrap());
            }
        }
        assert!(pipeline.wait(10).unwrap().is_some());
        pipeline.stop().unwrap();
    } else {
        let failure = result.err().unwrap();
        let diagnostic = failure.device_property_failure();
        let error = failure.to_string();
        let code = match case.as_str() {
            "uvc-33" | "uvc-39" => "RANGE_QUERY",
            "uvc-35" => "UVC_READ_FAILED",
            "uvc-37" | "uvc-38" => "UVC_TRACE_INVALID",
            "uvc-30" | "uvc-31" | "uvc-32" | "uvc-34" => "VALUE_RANGE",
            "missing-required" => "API_UNAVAILABLE",
            "unsupported" | "readonly" => "UNSUPPORTED",
            "range" | "negative-step" | "default-outside" => "RANGE_SCHEMA",
            "support-error" | "write-support-error" => "SUPPORT_QUERY",
            "range-error" | "long-sdk-error" => "RANGE_QUERY",
            "get-error" => "VALUE_READ",
            "range-current-outside" | "get-current-outside" => "VALUE_RANGE",
            "step" | "outside" | "grid-step" | "grid-outside" => "OUT_OF_RANGE",
            "readback" | "interference" => "READBACK_MISMATCH",
            "null" => "INFO_NULL",
            "utf8" => "INFO_UTF8",
            "long" => "TEXT_LIMIT",
            "text" => "INFO_TEXT",
            "pid" => "USB_ID",
            _ => "mock SDK rejection",
        };
        assert!(error.contains(code), "{case}: {error}");
        if let Some(ref diagnostic) = diagnostic {
            assert!(error.len() < 16384, "bounded private diagnostic: {case}");
            assert_eq!(diagnostic["profiles"]["color"]["width"], 2);
            assert_eq!(diagnostic["profiles"]["depth"]["fps"], 30);
            assert!(diagnostic["property"]["started_monotonic_ns"].is_u64());
            assert!(diagnostic["property"]["finished_monotonic_ns"].is_u64());
            assert!(diagnostic["property"]["failure_monotonic_ns"].is_u64());
            assert!(
                diagnostic["property"]["failure_monotonic_ns"]
                    .as_u64()
                    .unwrap()
                    <= diagnostic["property"]["finished_monotonic_ns"]
                        .as_u64()
                        .unwrap()
            );
            if case.starts_with("grid-") {
                assert_eq!(diagnostic["property"]["id"], 2002);
                assert_eq!(diagnostic["property"]["range_current"], 100);
                assert_eq!(diagnostic["property"]["observed_before"], 102);
                assert_eq!(diagnostic["auto_exposure_before"]["color"], 1);
                assert_eq!(diagnostic["property"]["stage"], "requested_range");
            }
            if case == "write-support-error" {
                assert_eq!(diagnostic["property"]["readable_queried"], true);
                assert_eq!(diagnostic["property"]["writable_queried"], false);
                assert_eq!(diagnostic["property"]["stage"], "write_support");
                assert_eq!(diagnostic["property"]["getter_attempted"], true);
            }
            if case == "long-sdk-error" {
                assert_eq!(diagnostic["property"]["error_truncated"], true);
                assert_eq!(diagnostic["property"]["stage"], "range_query");
            }
            if case.starts_with("uvc-")
                || [
                    "range",
                    "range-error",
                    "negative-step",
                    "default-outside",
                    "range-current-outside",
                    "long-sdk-error",
                ]
                .contains(&case.as_str())
            {
                assert_eq!(diagnostic["property"]["getter_attempted"], true);
                assert!(
                    diagnostic["property"]["getter_finished_monotonic_ns"]
                        .as_u64()
                        .unwrap()
                        >= diagnostic["property"]["getter_started_monotonic_ns"]
                            .as_u64()
                            .unwrap()
                );
                assert!(diagnostic["rollback_verified"].is_null());
            }
            if case.starts_with("uvc-") {
                let p = &diagnostic["property"];
                assert_eq!(p["range_uvc"]["available"], true);
                assert!(!p["getter_uvc"]["events"].as_array().unwrap().is_empty());
                if ["uvc-32", "uvc-34", "uvc-37", "uvc-38", "uvc-39"].contains(&case.as_str()) {
                    assert!(p["getter_error"].is_string());
                    assert!(p["observed_before"].is_null());
                }
                if case == "uvc-30" {
                    assert_eq!(p["range_current"], 256);
                    assert_eq!(p["getter_returned"], 101);
                    assert_eq!(p["observed_before"], 101);
                    assert_eq!(p["getter_uvc"]["backend"], "v4l2");
                }
            }
            if case == "range" {
                assert!(diagnostic["property"]["range"].is_object());
            }
        }
        if [
            "support-error",
            "write-support-error",
            "range-error",
            "get-error",
            "range",
            "negative-step",
            "default-outside",
            "range-current-outside",
            "get-current-outside",
        ]
        .contains(&case.as_str())
        {
            assert!(
                !error.contains("UNSUPPORTED"),
                "specific error must survive: {error}"
            );
            assert!(diagnostic.is_some());
        }
        if matches!(
            case.as_str(),
            "write-error" | "readback" | "interference" | "auto-grid-rollback"
        ) {
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
        if matches!(
            case.as_str(),
            "write-error" | "readback" | "interference" | "auto-grid-rollback"
        ) {
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
            assert_eq!(
                values,
                vec![
                    1,
                    1,
                    100,
                    if case == "auto-grid-rollback" {
                        102
                    } else {
                        10
                    },
                    200,
                    20
                ]
            );
            restored.stop().unwrap();
        }
    }
}

#[test]
fn invalid_or_unrelated_errors_do_not_claim_device_diagnostics() {
    for message in [
        "unrelated",
        "DEVICE_PROPERTY_TEST",
        "DEVICE_PROPERTY_TEST: invalid",
        "DEVICE_PROPERTY_TEST: {}",
        "DEVICE_PROPERTY_TEST: {\"kind\":\"device-property-failure\",\"version\":2}",
    ] {
        assert!(
            orbbec_safe::Error(message.into())
                .device_property_failure()
                .is_none()
        );
    }
}
