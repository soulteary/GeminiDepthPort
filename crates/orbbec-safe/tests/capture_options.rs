use orbbec_safe::{CaptureOptions, FrameAggregateOutput, HostClock, Sdk, VideoProfile};
use std::{fs, path::PathBuf, process::Command};

#[test]
fn optional_capture_controls_apply_before_start_and_fail_closed() {
    let dir = std::env::temp_dir().join(format!("gdp-sdk-options-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let cases = [
        ("default", "MOCK_NO_CAPTURE_CONTROLS"),
        ("missing", "MOCK_NO_CAPTURE_CONTROLS"),
        ("partial-global", "MOCK_NO_GLOBAL_READ"),
        ("unsupported", "MOCK_GLOBAL_UNSUPPORTED"),
        ("zero", "MOCK_GLOBAL_ZERO"),
        ("all", "MOCK_CAPTURE_FAILURE=0"),
        ("any", "MOCK_CAPTURE_FAILURE=0"),
        ("disabled", "MOCK_CAPTURE_FAILURE=0"),
        ("global-off", "MOCK_CAPTURE_FAILURE=0"),
        ("support-error", "MOCK_CAPTURE_FAILURE=1"),
        ("enable-error", "MOCK_CAPTURE_FAILURE=2"),
        ("aggregate-error", "MOCK_CAPTURE_FAILURE=3"),
        ("sync-error", "MOCK_CAPTURE_FAILURE=4"),
        ("read-error", "MOCK_CAPTURE_FAILURE=5"),
        ("clock-set", "MOCK_INITIAL_CLOCK=1"),
        ("clock-monotonic", "MOCK_INITIAL_CLOCK=0"),
        ("clock-conflict", "MOCK_MULTIPLE_CONTEXTS"),
        ("clock-get-error", "MOCK_CAPTURE_FAILURE=6"),
        ("clock-set-error", "MOCK_CAPTURE_FAILURE=7"),
        ("clock-not-applied", "MOCK_CAPTURE_FAILURE=8"),
        ("clock-verify-error", "MOCK_CAPTURE_FAILURE=9"),
    ];
    for (case, define) in cases {
        let library = dir.join(format!("{case}.so"));
        let output = Command::new("cc")
            .args(if cfg!(target_os = "macos") {
                vec!["-dynamiclib"]
            } else {
                vec!["-shared", "-fPIC"]
            })
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-D{define}"))
            .args(
                if matches!(
                    case,
                    "clock-set-error" | "clock-not-applied" | "clock-verify-error"
                ) {
                    vec!["-DMOCK_INITIAL_CLOCK=1"]
                } else {
                    vec![]
                },
            )
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_sdk.c"))
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = dir.join(format!("{case}.events"));
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capture_options_child", "--ignored"])
            .env("GDP_CAPTURE_CASE", case)
            .env("GEMINIDEPTHPORT_MOCK_LIBRARY", &library)
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &events)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(events).unwrap();
        if matches!(
            case,
            "missing"
                | "partial-global"
                | "unsupported"
                | "support-error"
                | "enable-error"
                | "aggregate-error"
                | "sync-error"
                | "clock-get-error"
                | "clock-set-error"
                | "clock-not-applied"
                | "clock-verify-error"
        ) {
            assert!(!log.contains("start\n"), "{case}: {log}");
            assert_eq!(log.matches("context-delete\n").count(), 1, "{case}: {log}");
        } else {
            assert_eq!(log.matches("start\n").count(), 1, "{case}: {log}");
            assert_eq!(log.matches("stop\n").count(), 1, "{case}: {log}");
        }
        if case == "all" {
            assert!(
                log.starts_with(
                    "clock-get\nglobal-support\nglobal-on\naggregate-all\nsync-on\nstart\n"
                ),
                "{log}"
            );
        }
        if case == "any" {
            assert!(log.contains("aggregate-any\nsync-off\nstart\n"), "{log}");
        }
        if case == "disabled" {
            assert!(
                log.contains("aggregate-disabled\nsync-off\nstart\n"),
                "{log}"
            );
        }
        if case == "clock-set" {
            assert!(
                log.starts_with("clock-get\nclock-realtime\nclock-get\nglobal-support\n"),
                "{log}"
            );
        }
        if case == "clock-monotonic" {
            assert!(
                log.starts_with("clock-get\nclock-monotonic\nclock-get\nglobal-support\n"),
                "{log}"
            );
        }
        if case == "clock-conflict" {
            assert!(!log.contains("clock-monotonic\n"), "{log}");
        }
        if case == "partial-global" {
            assert!(!log.contains("global-on\n"));
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "isolated fixture subprocess"]
fn capture_options_child() {
    let case = std::env::var("GDP_CAPTURE_CASE").unwrap();
    let sdk = Sdk::load(std::env::var_os("GEMINIDEPTHPORT_MOCK_LIBRARY").unwrap()).unwrap();
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    let options = if case == "default" {
        CaptureOptions::default()
    } else {
        CaptureOptions {
            host_clock: Some(if case == "clock-monotonic" {
                HostClock::Monotonic
            } else {
                HostClock::Realtime
            }),
            software_sync: Some(case == "all"),
            aggregate: Some(match case.as_str() {
                "all" => FrameAggregateOutput::All,
                "disabled" => FrameAggregateOutput::Disabled,
                _ => FrameAggregateOutput::Any,
            }),
            global_timestamp: Some(case != "global-off"),
        }
    };
    let result = sdk.open_with_options("test-device", Some(p), Some(p), options);
    if matches!(
        case.as_str(),
        "missing"
            | "partial-global"
            | "unsupported"
            | "support-error"
            | "enable-error"
            | "aggregate-error"
            | "sync-error"
            | "clock-get-error"
            | "clock-set-error"
            | "clock-not-applied"
            | "clock-verify-error"
    ) {
        let message = result
            .err()
            .expect("explicit options must fail")
            .to_string();
        assert!(
            message.contains(if case == "clock-not-applied" {
                "not applied"
            } else if case == "unsupported" {
                "does not support"
            } else if case == "missing" || case == "partial-global" {
                "missing SDK symbol"
            } else {
                "mock SDK rejection"
            }),
            "{message}"
        );
        return;
    }
    let mut pipeline = result.unwrap();
    if case == "clock-conflict" {
        let error = sdk
            .open_with_options(
                "test-device",
                Some(p),
                Some(p),
                CaptureOptions {
                    host_clock: Some(HostClock::Monotonic),
                    ..CaptureOptions::default()
                },
            )
            .err()
            .expect("active runtime must not change clock");
        assert!(error.to_string().contains("pipeline is active"), "{error}");
    }
    let frames = pipeline.wait(10).unwrap().unwrap();
    let frame = frames.depth().unwrap().unwrap();
    match case.as_str() {
        "default" => assert!(
            frame
                .global_timestamp_us()
                .unwrap_err()
                .to_string()
                .contains("missing SDK symbol")
        ),
        "read-error" => assert!(
            frame
                .global_timestamp_us()
                .unwrap_err()
                .to_string()
                .contains("mock SDK rejection")
        ),
        "zero" | "global-off" => assert_eq!(frame.global_timestamp_us().unwrap(), None),
        _ => assert_eq!(frame.global_timestamp_us().unwrap(), Some(5000)),
    }
    drop(frame);
    drop(frames);
    pipeline.stop().unwrap();
}
