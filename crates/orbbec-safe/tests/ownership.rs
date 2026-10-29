use orbbec_safe::{Format, Sdk, VideoProfile};
use std::{fs, path::PathBuf, process::Command};

#[test]
fn acquisition_does_not_require_optional_calibration_symbols() {
    let dir = std::env::temp_dir().join(format!("gdp-sdk-optional-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let library = dir.join("minimal-sdk.so");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_sdk.c");
    let output = Command::new("cc")
        .args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-DMOCK_NO_CALIBRATION",
        ])
        .arg(source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sdk = Sdk::load(&library).unwrap();
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    let mut pipeline = sdk.open("test-device", Some(p), Some(p)).unwrap();
    assert!(pipeline.calibration().is_err());
    let frames = pipeline.wait(10).unwrap().unwrap();
    assert!(frames.calibration().is_err());
    assert_eq!(frames.color().unwrap().unwrap().data().unwrap().len(), 8);
    drop(frames);
    pipeline.stop().unwrap();
    assert!(
        pipeline
            .calibration()
            .unwrap_err()
            .to_string()
            .contains("active pipeline")
    );
    drop(pipeline);
    drop(sdk);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dynamic_mock_checks_ownership_timeout_and_failure_cleanup() {
    let dir = std::env::temp_dir().join(format!("geminidepthport-sdk-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let library = dir.join(if cfg!(target_os = "macos") {
        "mock.dylib"
    } else {
        "mock.so"
    });
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_sdk.c");
    let output = Command::new("cc")
        .args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&library)
        .output()
        .expect("C compiler required for ABI fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Run the ownership scenario in a child, so mock environment and state are
    // isolated from Rust's parallel test runner. C fixture aborts on wrong order.
    let events = dir.join("events");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mock_child", "--ignored"])
        .env("GEMINIDEPTHPORT_MOCK_LIBRARY", &library)
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &events)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let log = fs::read_to_string(events).unwrap();
    assert!(log.contains("frame-add-ref\n"));
    assert_eq!(log.matches("start\n").count(), 1);
    assert_eq!(log.matches("stop\n").count(), 1);
    assert_eq!(log.matches("error-delete\n").count(), 2);
    assert_eq!(log.matches("context-delete\n").count(), 3);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "invoked in isolated child by dynamic_mock_checks_ownership_timeout_and_failure_cleanup"]
fn mock_child() {
    let sdk = Sdk::load(std::env::var_os("GEMINIDEPTHPORT_MOCK_LIBRARY").unwrap()).unwrap();
    assert_eq!(sdk.version(), 20903);
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    assert!(sdk.open("", Some(p), None).is_err());
    assert!(sdk.open("test-device", None, None).is_err());
    assert!(sdk.open("missing-device", Some(p), None).is_err());
    assert!(
        sdk.open("test-device", Some(VideoProfile { fps: 1, ..p }), None)
            .is_err()
    );
    let mut pipe = sdk.open("test-device", Some(p), Some(p)).unwrap();
    assert!(pipe.wait(0).is_err());
    assert!(pipe.wait(1).unwrap().is_none());
    let frames = pipe.wait(10).unwrap().unwrap();
    let calibration = frames.calibration().unwrap();
    let color = calibration.color.as_ref().unwrap();
    assert_eq!(color.profile, p);
    assert_eq!(color.fx, 1.);
    assert_eq!(color.fy, 2.);
    assert_eq!(color.distortion_model, "plumb_bob");
    assert_eq!(calibration.depth.as_ref().unwrap().format, Format::Depth16);
    assert_eq!(
        calibration.depth_to_color.as_ref().unwrap().translation_m,
        [0.1, 0.2, 0.3]
    );
    assert_eq!(
        calibration
            .depth_to_color
            .as_ref()
            .unwrap()
            .transform_depth_to_color([1., 2., 3.]),
        [-1.9, 1.2, 3.3]
    );
    assert!(pipe.calibration().unwrap().depth_to_color.is_some());
    let frame = frames.color().unwrap().unwrap();
    let cloned = frame.try_clone().unwrap();
    let metadata = frame.metadata().unwrap();
    assert_eq!(metadata.format, Format::Yuyv);
    assert_eq!(metadata.index, 42);
    assert_eq!(metadata.data_size, 8);
    let depth = frames.depth().unwrap().unwrap();
    assert_eq!(depth.metadata().unwrap().depth_scale_mm, Some(0.1));
    drop(depth);
    drop(frames);
    drop(frame);
    pipe.stop().unwrap();
    assert!(pipe.wait(10).is_err());
    drop(pipe);
    drop(sdk);
    assert_eq!(cloned.data().unwrap(), [16, 128, 32, 128, 64, 90, 80, 240]);
    drop(cloned); // last reference finally deletes pipeline and parent objects
}

#[test]
fn malformed_active_profile_abi_is_rejected_without_leaking_owned_handles() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-calibration-boundary-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let original = fs::read_to_string(root.join("tests/mock_sdk.c")).unwrap();
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    for (i, from, to, needle) in [
        (
            0,
            "return 20903;",
            "return 10000;",
            "unsupported SDK version",
        ),
        (
            1,
            "(void)p;(void)e;return 2;}\nprofile *ob_stream_profile_list_get_profile",
            "(void)p;(void)e;return 0;}\nprofile *ob_stream_profile_list_get_profile",
            "profile count",
        ),
        (2, "return p->stream;", "return 99;", "stream type"),
        (
            3,
            "return make_profile(index==0?2:3);",
            "return make_profile(2);",
            "duplicate color",
        ),
        (
            4,
            "return make_profile(index==0?2:3);",
            "return make_profile(3);",
            "duplicate depth",
        ),
        (
            5,
            "return(intrinsic){1,2,.5f,.5f,2,2};",
            "return(intrinsic){0,2,.5f,.5f,2,2};",
            "intrinsics",
        ),
        (6, "return make_profile(0);", "return NULL;", "null profile"),
        (
            7,
            "(void)p;(void)e;return 2;}\nprofile *ob_stream_profile_list_get_profile",
            "(void)p;(void)e;return 1;}\nprofile *ob_stream_profile_list_get_profile",
            "profile missing",
        ),
        (
            8,
            "return p->stream==2?0:8;",
            "return 17;",
            "profile differs",
        ),
    ] {
        assert!(original.contains(from), "{i}");
        let source = dir.join(format!("{i}.c"));
        fs::write(&source, original.replace(from, to)).unwrap();
        let lib = dir.join(format!("{i}.so"));
        let result = Command::new("cc")
            .args(if cfg!(target_os = "macos") {
                vec!["-dynamiclib"]
            } else {
                vec!["-shared", "-fPIC"]
            })
            .arg(&source)
            .arg("-o")
            .arg(&lib)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if i == 0 {
            assert!(Sdk::load(&lib).err().unwrap().to_string().contains(needle));
            continue;
        }
        let sdk = Sdk::load(&lib).unwrap();
        let mut pipeline = sdk.open("test-device", Some(p), Some(p)).unwrap();
        let error = pipeline.calibration().unwrap_err();
        assert!(error.to_string().contains(needle), "{i}: {error}");
        pipeline.stop().unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_null_payloads_missing_error_messages_and_single_stream_calibration_are_bounded() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-sdk-payload-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let original = fs::read_to_string(root.join("tests/mock_sdk.c")).unwrap();
    let p = VideoProfile {
        width: 2,
        height: 2,
        fps: 30,
    };
    for (i, from, to) in [
        (0, "return sizeof(bytes);", "return 0;"),
        (1, "return bytes;", "return NULL;"),
        (2, "return \"mock SDK rejection\";", "return NULL;"),
        (
            3,
            "(void)p;(void)e;return 2;}\nprofile *ob_stream_profile_list_get_profile",
            "(void)p;(void)e;return 1;}\nprofile *ob_stream_profile_list_get_profile",
        ),
    ] {
        assert!(original.contains(from));
        let source = dir.join(format!("{i}.c"));
        fs::write(&source, original.replace(from, to)).unwrap();
        let lib = dir.join(format!("{i}.so"));
        let output = Command::new("cc")
            .args(if cfg!(target_os = "macos") {
                vec!["-dynamiclib"]
            } else {
                vec!["-shared", "-fPIC"]
            })
            .arg(&source)
            .arg("-o")
            .arg(&lib)
            .output()
            .unwrap();
        assert!(output.status.success());
        let sdk = Sdk::load(&lib).unwrap();
        assert!(
            sdk.open("test-device", Some(VideoProfile { width: 0, ..p }), None)
                .is_err()
        );
        assert!(
            sdk.open("test-device", Some(VideoProfile { width: 3, ..p }), None)
                .is_err()
        );
        if i == 2 {
            let error = sdk
                .open("private-unknown", Some(p), None)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(error, "SDK error without message");
            continue;
        }
        let mut pipeline = sdk
            .open("test-device", Some(p), if i == 3 { None } else { Some(p) })
            .unwrap();
        if i == 3 {
            let calibration = pipeline.calibration().unwrap();
            assert!(calibration.color.is_some());
            assert!(calibration.depth_to_color.is_none());
        } else {
            let set = pipeline.wait(10).unwrap().unwrap();
            let frame = set.color().unwrap().unwrap();
            if i == 0 {
                assert!(frame.data().unwrap().is_empty());
            } else {
                assert!(
                    frame
                        .data()
                        .unwrap_err()
                        .to_string()
                        .contains("null frame data")
                );
            }
        }
        pipeline.stop().unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}
