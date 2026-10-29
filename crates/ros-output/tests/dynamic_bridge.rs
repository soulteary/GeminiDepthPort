use frame_core::{
    FrameEncoding, FrameLayout, PacketMetadata, PairClock, PairEvidence, PixelFormat,
};
use ros_output::{AuxBinding, Binding, Calibration, Publisher, RosAuxPublisher, StaticTransform};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

fn compile(source: &Path, output: &Path, include: &Path, bad_abi: bool) {
    let result = Command::new("c++")
        .args(if cfg!(target_os = "macos") {
            vec!["-dynamiclib"]
        } else {
            vec!["-shared", "-fPIC"]
        })
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror"])
        .arg(if bad_abi {
            "-DMOCK_ABI_VERSION=2"
        } else {
            "-DMOCK_ABI_VERSION=1"
        })
        .arg("-I")
        .arg(include)
        .arg(source)
        .arg("-o")
        .arg(output)
        .output()
        .expect("C++ compiler required for native ABI fixture");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn binding() -> Binding {
    Binding {
        node_name: "test_probe".into(),
        namespace: "/test".into(),
        image_topic: "/test/image_raw".into(),
        camera_info_topic: "/test/camera_info".into(),
        metadata_topic: "/test/frame_metadata".into(),
        frame_id: "test_optical".into(),
    }
}
fn calibration() -> Calibration {
    Calibration::from_json(r#"{"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]}"#).unwrap()
}
fn packet() -> PacketMetadata {
    PacketMetadata {
        layout: FrameLayout {
            width: 2,
            height: 2,
            stride: 4,
            format: PixelFormat::Yuyv,
        },
        encoding: FrameEncoding::Yuyv,
        sequence: 1,
        device_timestamp_us: Some(10),
        system_timestamp_us: Some(1_700_000_000_000_001),
        source_depth_scale_mm: None,
        output_depth_scale_mm: None,
        normalized: false,
        received_at: Instant::now(),
    }
}

#[test]
fn dynamic_abi_copy_guards_and_cleanup() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-ros-bridge-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = root.join("tests/mock_bridge.cpp");
    let include = root.join("../../ros/include");
    let library = dir.join("mock.so");
    let bad_abi = dir.join("bad-abi.so");
    compile(&source, &library, &include, false);
    compile(&source, &bad_abi, &include, true);
    assert!(Publisher::load("relative.so", binding(), calibration()).is_err());
    assert!(Publisher::load(&bad_abi, binding(), calibration()).is_err());
    let events = dir.join("events");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mock_child", "--ignored"])
        .env("GDP_ROS_MOCK_LIBRARY", &library)
        .env("GDP_ROS_MOCK_EVENTS", &events)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let log = fs::read_to_string(events).unwrap();
    assert_eq!(log.lines().filter(|l| *l == "create").count(), 2);
    assert_eq!(log.lines().filter(|l| *l == "destroy").count(), 2); // includes error+non-null handle
    assert_eq!(log.matches("publish-copied\n").count(), 3);
    assert_eq!(log.lines().filter(|l| *l == "publish-failed").count(), 1);
    assert_eq!(log.lines().filter(|l| *l == "aux-create").count(), 2);
    assert_eq!(log.lines().filter(|l| *l == "aux-destroy").count(), 2);
    assert_eq!(log.lines().filter(|l| *l == "aux-publish").count(), 2);
    assert_eq!(log.lines().filter(|l| *l == "evidence-copied").count(), 1);
    assert_eq!(log.lines().filter(|l| *l == "evidence-failed").count(), 1);
    assert_eq!(
        log.lines().filter(|l| *l == "aux-publish-failed").count(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "executed in isolated subprocess by dynamic_abi_copy_guards_and_cleanup"]
fn mock_child() {
    let library = PathBuf::from(std::env::var_os("GDP_ROS_MOCK_LIBRARY").unwrap());
    let mut failing = binding();
    failing.node_name = "mock_fail_create".into();
    assert!(Publisher::load(&library, failing, calibration()).is_err());
    let mut publisher = Publisher::load(&library, binding(), calibration()).unwrap();
    for invalid in [
        "",
        "short",
        "0123456789abcdef0123456789abcdeG",
        "0123456789abcdef0123456789abcde\"",
    ] {
        assert!(publisher.set_session_id(invalid).is_err());
    }
    publisher
        .set_session_id("0123456789abcdef0123456789abcdef")
        .unwrap();
    assert!(
        publisher
            .set_session_id("fedcba9876543210fedcba9876543210")
            .is_err()
    );
    let mut metadata = packet();
    let stamp = metadata.system_timestamp_us.unwrap() * 1000;
    let mut bytes = vec![16, 128, 32, 128, 64, 90, 80, 240];
    assert!(publisher.publish(&metadata, &bytes[..7], stamp).is_err());
    assert!(publisher.publish(&metadata, &bytes, stamp + 1).is_err());
    // A device clock can start at zero; the independently mapped Unix/system
    // timestamp remains positive. Preserve this zero, do not replace with null.
    metadata.device_timestamp_us = Some(0);
    publisher.publish(&metadata, &bytes, stamp).unwrap();
    bytes.fill(255);
    drop(bytes); // native side must own a copy after call
    assert!(
        publisher
            .publish(&metadata, &[16, 128, 32, 128, 64, 90, 80, 240], stamp)
            .is_err()
    );
    metadata.sequence = 2;
    metadata.system_timestamp_us = Some(stamp / 1000 + 1);
    let bytes = [16, 128, 32, 128, 64, 90, 80, 240];
    publisher.publish(&metadata, &bytes, stamp + 1000).unwrap();
    metadata.sequence = 999;
    metadata.system_timestamp_us = Some(stamp / 1000 + 2);
    assert!(publisher.publish(&metadata, &bytes, stamp + 2000).is_err());
    // Failed C publication cannot advance Rust-side timestamp/sequence guards.
    metadata.sequence = 3;
    publisher.publish(&metadata, &bytes, stamp + 2000).unwrap();
    metadata.sequence = 4;
    metadata.system_timestamp_us = Some(stamp / 1000 + 3);
    metadata.layout.format = PixelFormat::Depth16;
    metadata.encoding = FrameEncoding::Depth16;
    metadata.source_depth_scale_mm = Some(0.1);
    metadata.output_depth_scale_mm = Some(0.1);
    assert!(publisher.publish(&metadata, &bytes, stamp + 3000).is_err());
    drop(publisher);
    let extrinsics = orbbec_safe::Extrinsics::from_sdk(
        [0., -1., 0., 1., 0., 0., 0., 0., 1.],
        [100., 200., 300.],
    )
    .unwrap();
    let transform =
        StaticTransform::from_depth_to_color(&extrinsics, "color_optical", "depth_optical")
            .unwrap();
    let binding = AuxBinding {
        node_name: "mock_fail_aux_create".into(),
        namespace: "/test".into(),
        pair_metadata_topic: "/test/rgbd/pair_metadata".into(),
    };
    assert!(RosAuxPublisher::load(&library, binding, transform.clone(), stamp).is_err());
    let binding = AuxBinding {
        node_name: "aux_probe".into(),
        namespace: "/test".into(),
        pair_metadata_topic: "/test/rgbd/pair_metadata".into(),
    };
    let mut auxiliary = RosAuxPublisher::load(&library, binding, transform, stamp).unwrap();
    let mut evidence = PairEvidence {
        pair_sequence: 1,
        color_sequence: 10,
        depth_sequence: 20,
        color_timestamp_us: 1,
        depth_timestamp_us: 11,
        delta_us: 10,
        clock: PairClock::Device,
        same_frameset: true,
        hardware_sync_verified: false,
    };
    auxiliary
        .set_session_id("0123456789abcdef0123456789abcdef")
        .unwrap();
    assert!(
        auxiliary
            .set_session_id("fedcba9876543210fedcba9876543210")
            .is_err()
    );
    let zero_evidence = PairEvidence {
        color_timestamp_us: 0,
        depth_timestamp_us: 10,
        ..evidence
    };
    assert!(
        auxiliary
            .publish_pair(&zero_evidence, stamp, stamp + 1000)
            .is_err()
    );
    auxiliary
        .publish_pair(&evidence, stamp, stamp + 1000)
        .unwrap();
    assert!(
        auxiliary
            .publish_pair(&evidence, stamp, stamp + 1000)
            .is_err()
    );
    let frame = |depth: bool| {
        frame_core::FrameEvidence {context:serde_json::from_value(serde_json::json!({"version":1,"session_id":"0123456789abcdef0123456789abcdef","generation_id":"a".repeat(32),"configuration_id":"b".repeat(64),"calibration_id":"c".repeat(64),"snapshot_id":"d".repeat(64),"stream_id":if depth {"depth"}else{"color"},"sequence":if depth{20}else{10},"device_timestamp_us":if depth{11}else{1},"system_timestamp_us":(stamp+if depth{1000}else{0})/1000,"received_mono_offset_ns":5})).unwrap(),encoding:if depth{"depth16"}else{"yuyv"}.into(),width:2,height:2,stride:4,source_depth_scale_mm:depth.then_some(1.),output_depth_scale_mm:depth.then_some(1.),normalized:false}
    };
    let mut bound = frame_core::BoundPairEvidence {
        version: 1,
        pair_sequence: 1,
        color: frame(false),
        depth: frame(true),
        color_stamp_us: 1,
        depth_stamp_us: 11,
        clock_source: "device_us".into(),
        delta_us: 10,
        association: "sdk_frameset".into(),
        hardware_sync_verified: false,
    };
    auxiliary
        .publish_bound_evidence(&bound, stamp, stamp + 1000)
        .unwrap();
    bound.color_stamp_us = 2;
    bound.color.context.device_timestamp_us = Some(2);
    bound.delta_us = 9;
    assert!(
        auxiliary
            .publish_bound_evidence(&bound, stamp, stamp + 1000)
            .is_err()
    );
    bound.color_stamp_us = 1;
    bound.color.context.device_timestamp_us = Some(1);
    bound.delta_us = 10;
    bound.color.context.session_id =
        serde_json::from_value(serde_json::json!("e".repeat(32))).unwrap();
    assert!(
        auxiliary
            .publish_bound_evidence(&bound, stamp, stamp + 1000)
            .is_err()
    );
    evidence.pair_sequence = 999;
    evidence.color_sequence += 1;
    evidence.depth_sequence += 1;
    evidence.color_timestamp_us += 100;
    evidence.depth_timestamp_us += 100;
    assert!(
        auxiliary
            .publish_pair(&evidence, stamp + 2000, stamp + 3000)
            .is_err()
    );
    evidence.pair_sequence = 2;
    auxiliary
        .publish_pair(&evidence, stamp + 2000, stamp + 3000)
        .unwrap();
    bound.pair_sequence = 2;
    bound.color = frame(false);
    bound.depth = frame(true);
    bound.color.context.sequence = Some(11);
    bound.depth.context.sequence = Some(21);
    bound.color_stamp_us = 101;
    bound.depth_stamp_us = 111;
    bound.color.context.device_timestamp_us = Some(101);
    bound.depth.context.device_timestamp_us = Some(111);
    bound.color.context.system_timestamp_us = Some(stamp / 1000 + 2);
    bound.depth.context.system_timestamp_us = Some(stamp / 1000 + 3);
    assert!(
        auxiliary
            .publish_bound_evidence(&bound, stamp + 2000, stamp + 3000)
            .is_err()
    );
    evidence.pair_sequence = 3;
    evidence.color_sequence += 1;
    evidence.depth_sequence += 1;
    // Header stamps advance but selected pairing clock timestamps do not.
    assert!(
        auxiliary
            .publish_pair(&evidence, stamp + 4000, stamp + 5000)
            .is_err()
    );
    let regressed = PairEvidence {
        color_timestamp_us: 100,
        depth_timestamp_us: 110,
        ..evidence
    };
    assert!(
        auxiliary
            .publish_pair(&regressed, stamp + 4000, stamp + 5000)
            .is_err()
    );
    let changed_clock = PairEvidence {
        clock: PairClock::System,
        color_timestamp_us: stamp / 1000 + 4,
        depth_timestamp_us: stamp / 1000 + 5,
        delta_us: 1,
        ..evidence
    };
    assert!(
        auxiliary
            .publish_pair(&changed_clock, stamp + 4000, stamp + 5000)
            .is_err()
    );
    evidence.color_timestamp_us += 100;
    evidence.depth_timestamp_us += 100;
    evidence.hardware_sync_verified = true;
    assert!(
        auxiliary
            .publish_pair(&evidence, stamp + 4000, stamp + 5000)
            .is_err()
    );
    drop(auxiliary);
}

#[test]
fn invalid_bindings_and_calibration_fail_before_any_native_library_is_opened() {
    for mode in 0..4 {
        let mut b = binding();
        match mode {
            0 => b.node_name = "bad name".into(),
            1 => b.namespace = "relative".into(),
            2 => b.image_topic = "/foreign/image".into(),
            _ => b.metadata_topic = b.image_topic.clone(),
        }
        assert!(b.validate().is_err());
    }
    let base=serde_json::to_value(serde_json::json!({"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]})).unwrap();
    for (pointer, value) in [
        ("/width", serde_json::json!(-1)),
        ("/frame_id", serde_json::json!(3)),
        ("/k", serde_json::json!([1])),
        ("/d", serde_json::json!(true)),
        ("/r", serde_json::json!(vec![0; 9])),
        ("/p", serde_json::json!([1])),
        ("/distortion_model", serde_json::json!("unknown")),
    ] {
        let mut v = base.clone();
        *v.pointer_mut(pointer).unwrap() = value;
        assert!(Calibration::from_json(&v.to_string()).is_err(), "{pointer}");
    }
    let mut c = calibration();
    c.frame_id = "other".into();
    assert!(
        Publisher::load("/missing.so", binding(), c)
            .err()
            .unwrap()
            .to_string()
            .contains("optical frame")
    );
    let mut c = calibration();
    c.k[0] = f64::NAN;
    assert!(c.validate().is_err());
}
#[test]
fn optional_native_session_extensions_are_checked_and_missing_symbols_close_the_library() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-bridge-boundary-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let original = fs::read_to_string(root.join("tests/mock_bridge.cpp")).unwrap();
    for (i, from, to) in [
        (0, "gdp_ros_create(", "hidden_create("),
        (1, "gdp_ros_set_session_id(", "hidden_set_session_id("),
        (
            2,
            "p.session_id=id;event(\"session-bound\");return 0;",
            "return 2;",
        ),
    ] {
        assert!(original.contains(from));
        let source = dir.join(format!("{i}.cpp"));
        fs::write(&source, original.replace(from, to)).unwrap();
        let lib = dir.join(format!("{i}.so"));
        compile(&source, &lib, &root.join("../../ros/include"), false);
        if i == 0 {
            assert!(
                Publisher::load(&lib, binding(), calibration())
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("symbol")
            );
        } else {
            let mut p = Publisher::load(&lib, binding(), calibration()).unwrap();
            assert!(p.set_session_id(&"a".repeat(32)).is_err());
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn absent_or_non_library_files_return_loader_errors_after_validating_complete_bindings() {
    for path in ["/nonexistent-gdp-bridge.so", env!("CARGO_MANIFEST_DIR")] {
        let error = Publisher::load(path, binding(), calibration())
            .err()
            .unwrap()
            .to_string();
        assert!(!error.is_empty());
        assert!(!error.contains("calibration"));
    }
    let extrinsics = orbbec_safe::Extrinsics {
        rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        translation_m: [0.; 3],
    };
    let transform =
        StaticTransform::from_depth_to_color(&extrinsics, "color_optical", "depth_optical")
            .unwrap();
    for (namespace, topic) in [
        ("/", "/pair"),
        ("relative", "/pair"),
        ("/test", "/foreign/pair"),
    ] {
        let binding = AuxBinding {
            node_name: "test".into(),
            namespace: namespace.into(),
            pair_metadata_topic: topic.into(),
        };
        assert!(RosAuxPublisher::load("/missing-gdp.so", binding, transform.clone(), 1).is_err());
    }
}

#[test]
fn system_clock_pair_evidence_is_bound_to_images_session_and_clock_before_native_submission() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-aux-system-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let library = dir.join("bridge.so");
    compile(
        &root.join("tests/mock_bridge.cpp"),
        &library,
        &root.join("../../ros/include"),
        false,
    );
    let transform = StaticTransform::from_depth_to_color(
        &orbbec_safe::Extrinsics {
            rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            translation_m: [0.1, 0.2, 0.3],
        },
        "color_optical",
        "depth_optical",
    )
    .unwrap();
    let mut aux = RosAuxPublisher::load(
        &library,
        AuxBinding {
            node_name: "aux_system".into(),
            namespace: "/test".into(),
            pair_metadata_topic: "/test/pair".into(),
        },
        transform,
        1_000_000_000,
    )
    .unwrap();
    aux.set_session_id(&"a".repeat(32)).unwrap();
    let pair = PairEvidence {
        pair_sequence: 1,
        color_sequence: 10,
        depth_sequence: 20,
        color_timestamp_us: 1_000_000,
        depth_timestamp_us: 1_000_010,
        delta_us: 10,
        clock: PairClock::System,
        same_frameset: true,
        hardware_sync_verified: false,
    };
    let mut bad_delta = pair;
    bad_delta.delta_us = 11;
    assert!(
        aux.publish_pair(&bad_delta, 1_000_000_000, 1_000_010_000)
            .unwrap_err()
            .to_string()
            .contains("pair delta mismatch")
    );
    let context = |stream: &str, sequence: u64, time: u64| serde_json::json!({"version":1,"session_id":"a".repeat(32),"generation_id":"b".repeat(32),"configuration_id":"c".repeat(64),"calibration_id":"d".repeat(64),"snapshot_id":"e".repeat(64),"stream_id":stream,"sequence":sequence,"device_timestamp_us":time,"system_timestamp_us":time,"received_mono_offset_ns":1});
    let evidence:frame_core::BoundPairEvidence=serde_json::from_value(serde_json::json!({"version":1,"pair_sequence":1,"color":{"context":context("color",10,1_000_000),"encoding":"yuyv","width":2,"height":2,"stride":4,"source_depth_scale_mm":null,"output_depth_scale_mm":null,"normalized":false},"depth":{"context":context("depth",20,1_000_010),"encoding":"depth16","width":2,"height":2,"stride":4,"source_depth_scale_mm":1.0,"output_depth_scale_mm":1.0,"normalized":false},"color_stamp_us":1_000_000,"depth_stamp_us":1_000_010,"clock_source":"sdk_system_us","delta_us":10,"association":"sdk_frameset","hardware_sync_verified":false})).unwrap();
    aux.publish_pair(&pair, 1_000_000_000, 1_000_010_000)
        .unwrap();
    for mode in 0..4 {
        let mut e = evidence.clone();
        match mode {
            0 => e.pair_sequence = 2,
            1 => {
                e.color.context.system_timestamp_us = Some(1_000_001);
                e.color_stamp_us = 1_000_001;
                e.delta_us = 9;
            }
            2 => {
                e.color.context.session_id = "f".repeat(32).parse().unwrap();
                e.depth.context.session_id = e.color.context.session_id.clone();
            }
            _ => e.clock_source = "device_us".into(),
        }
        assert!(
            aux.publish_bound_evidence(&e, 1_000_000_000, 1_000_010_000)
                .is_err(),
            "{mode}"
        );
    }
    assert!(
        aux.publish_pair(&pair, 1_000_000_001, 1_000_010_000)
            .is_err()
    );
    aux.publish_bound_evidence(&evidence, 1_000_000_000, 1_000_010_000)
        .unwrap();
    drop(aux);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn publisher_rejections_do_not_commit_sequence_or_timestamp_and_calibration_is_bounded() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-publisher-boundaries-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let library = dir.join("bridge.so");
    compile(
        &root.join("tests/mock_bridge.cpp"),
        &library,
        &root.join("../../ros/include"),
        false,
    );
    let mut root_binding = binding();
    root_binding.namespace = "/".into();
    root_binding.validate().unwrap();
    let mut p = Publisher::load(&library, root_binding, calibration()).unwrap();
    let bytes = vec![0; 512 * 1024 * 1024 + 2];
    let mut oversized = packet();
    oversized.layout = FrameLayout {
        width: 1,
        height: 1,
        stride: bytes.len(),
        format: PixelFormat::Depth16,
    };
    oversized.encoding = FrameEncoding::Depth16;
    oversized.source_depth_scale_mm = Some(1.);
    oversized.output_depth_scale_mm = Some(1.);
    assert!(
        p.publish(&oversized, &bytes, 1)
            .unwrap_err()
            .to_string()
            .contains("ROS image exceeds payload limit")
    );
    drop(bytes);
    let mut m = packet();
    m.layout.height = 1;
    assert!(
        p.publish(&m, &[0; 4], m.system_timestamp_us.unwrap() * 1000)
            .is_err()
    );
    let mut m = packet();
    m.encoding = FrameEncoding::Depth16;
    m.layout.format = PixelFormat::Depth16;
    m.source_depth_scale_mm = Some(0.1);
    m.output_depth_scale_mm = Some(0.1);
    assert!(
        p.publish(&m, &[0; 8], m.system_timestamp_us.unwrap() * 1000)
            .is_err()
    );
    let bytes = [16, 128, 32, 128, 64, 90, 80, 240];
    let mut m = packet();
    m.device_timestamp_us = Some(0);
    let stamp = m.system_timestamp_us.unwrap() * 1000;
    p.publish(&m, &bytes, stamp).unwrap();
    m.system_timestamp_us = Some(m.system_timestamp_us.unwrap() + 1);
    assert!(p.publish(&m, &bytes, stamp + 1000).is_err());
    m.sequence += 1;
    p.publish(&m, &bytes, stamp + 1000).unwrap();
    let raw = serde_json::json!({"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]});
    for (path, value) in [
        ("/width", serde_json::json!(u64::MAX)),
        ("/width", serde_json::json!(0)),
        ("/p", serde_json::json!(vec![0; 12])),
    ] {
        let mut v = raw.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(Calibration::from_json(&v.to_string()).is_err());
    }
    drop(p);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_creation_without_a_handle_and_auxiliary_session_extension_failure_are_safe() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("gdp-null-creation-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("bridge.cpp");
    let library = dir.join("bridge.so");
    let text=fs::read_to_string(root.join("tests/mock_bridge.cpp")).unwrap().replace("if (!gdp_ros::config_valid(c)) return 1;","if (!gdp_ros::config_valid(c)) return 1;\n  if(std::string(c->node_name)==\"mock_fail_null\")return 2;").replace("if(!gdp_ros::aux_config_valid(c))return 1;","if(!gdp_ros::aux_config_valid(c))return 1;\n  if(std::string(c->node_name)==\"mock_fail_null\")return 2;").replace("p.session_id=id;event(\"aux-session-bound\");return 0;","return 2;");
    fs::write(&source, text).unwrap();
    compile(&source, &library, &root.join("../../ros/include"), false);
    let mut b = binding();
    b.node_name = "mock_fail_null".into();
    assert!(Publisher::load(&library, b, calibration()).is_err());
    let transform = StaticTransform::from_depth_to_color(
        &orbbec_safe::Extrinsics {
            rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            translation_m: [0.1, 0.2, 0.3],
        },
        "color_optical",
        "depth_optical",
    )
    .unwrap();
    let b = AuxBinding {
        node_name: "mock_fail_null".into(),
        namespace: "/test".into(),
        pair_metadata_topic: "/test/pair".into(),
    };
    assert!(RosAuxPublisher::load(&library, b, transform.clone(), 1).is_err());
    let b = AuxBinding {
        node_name: "aux_normal".into(),
        namespace: "/test".into(),
        pair_metadata_topic: "/test/pair".into(),
    };
    let mut invalid = transform.clone();
    invalid.rotation_row_major = [1.; 9];
    assert!(RosAuxPublisher::load(&library, b.clone(), invalid, 1).is_err());
    let mut aux = RosAuxPublisher::load(&library, b, transform, 1).unwrap();
    assert!(aux.set_session_id(&"a".repeat(32)).is_err());
    drop(aux);
    fs::remove_dir_all(dir).unwrap();
}
