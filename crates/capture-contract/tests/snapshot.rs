use capture_contract::{snapshot::*, store::SnapshotStore, *};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
fn live() -> CaptureSnapshot {
    let session = SessionId::parse("a".repeat(32)).unwrap();
    let instance = InstanceId::parse("b".repeat(64)).unwrap();
    let model = CameraModel {
        frame_id: "depth".into(),
        width: 640,
        height: 480,
        k: [500., 0., 320., 0., 500., 240., 0., 0., 1.],
        distortion_model: DistortionModel::None,
        distortion_coefficients: vec![],
    };
    let calibration = CalibrationRecord {
        version: 1,
        source: CalibrationSource::SdkActiveProfiles,
        actual_runtime_observed: true,
        source_session_id: Some(session.clone()),
        source_instance_id: Some(instance.clone()),
        observed_unix_ns: Some(1),
        models: BTreeMap::from([("depth".into(), model)]),
        transforms: vec![],
        physical_accuracy_verified: false,
        legacy_source_sha256: None,
    };
    let configuration = json!({"depth_width":640,"depth_height":480});
    CaptureSnapshot {
        version: 1,
        binding: SnapshotBinding {
            instance_id: instance,
            session_id: session,
            generation_id: GenerationId::parse("c".repeat(32)).unwrap(),
            configuration_id: ConfigurationId::parse(
                content_digest(
                    ContentKind::Configuration,
                    &serde_json::to_vec(&configuration).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
            calibration_id: calibration.id().unwrap(),
        },
        configuration,
        calibration,
        depth: DepthRepresentation::SdkCounts { scale_mm: 0.1 },
        quality_policy: QualityPolicy {
            version: 1,
            max_source_age_ns: 500_000_000,
            max_future_ns: 100_000_000,
            max_pair_delta_us: 20000,
            min_valid_fraction: 0.,
            max_zero_fraction: 1.,
            action: QualityAction::FailClosed,
        },
    }
}
#[test]
fn content_changes_cannot_reuse_ids_or_binding() {
    let sealed = live().seal().unwrap();
    sealed.require_runtime().unwrap();
    let bytes = sealed.to_bytes().unwrap();
    let mut expected = sealed.content.binding.clone();
    expected.instance_id = InstanceId::parse("d".repeat(64)).unwrap();
    assert!(SnapshotEnvelope::parse(&bytes, &expected).is_err());
    expected = sealed.content.binding.clone();
    expected.session_id = SessionId::parse("d".repeat(32)).unwrap();
    assert!(SnapshotEnvelope::parse(&bytes, &expected).is_err());
    let mut changed = sealed.clone();
    changed.content.configuration["depth_width"] = json!(320);
    assert!(changed.to_bytes().is_err());
    let mut changed = sealed.clone();
    changed
        .content
        .calibration
        .models
        .get_mut("depth")
        .unwrap()
        .k[0] = 501.;
    assert!(changed.to_bytes().is_err());
    let mut changed = sealed.clone();
    changed.snapshot_id = SnapshotId::parse("e".repeat(64)).unwrap();
    assert!(changed.to_bytes().is_err());
}
#[test]
fn store_publishes_private_immutable_content_and_refuses_tampering() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("store");
    let store = SnapshotStore::open(&directory).unwrap();
    let envelope = live().seal().unwrap();
    let id = store.publish(&envelope).unwrap();
    assert_eq!(id, store.publish(&envelope).unwrap());
    assert_eq!(
        store.read(&id, &envelope.content.binding).unwrap(),
        envelope
    );
    let file = directory.join(format!("{id}.json"));
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(&file, b"{}").unwrap();
    assert!(store.publish(&envelope).is_err());
    assert_eq!(fs::read(file).unwrap(), b"{}");
    assert!(fs::read_dir(directory).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pending")
    }));
}
#[test]
fn store_rejects_symlink_and_world_readable_input() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("store");
    let store = SnapshotStore::open(&directory).unwrap();
    let envelope = live().seal().unwrap();
    let id = store.publish(&envelope).unwrap();
    let file = directory.join(format!("{id}.json"));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.read(&id, &envelope.content.binding).is_err());
    fs::remove_file(&file).unwrap();
    symlink(temp.path().join("outside"), &file).unwrap();
    assert!(store.publish(&envelope).is_err());
    let link = temp.path().join("store-link");
    symlink(&directory, &link).unwrap();
    assert!(SnapshotStore::open(&link).is_err());
}
#[test]
fn legacy_import_verifies_old_digest_without_creating_runtime_claims() {
    let m = &live().calibration.models["depth"];
    let mut old = json!({"kind":"geminidepthport-calibration-v1","structural_validation":true,"physical_accuracy_verified":false,"cameras":[{"name":"synthetic","intrinsics":{"depth":{"frame_id":m.frame_id,"width":m.width,"height":m.height,"k":m.k,"distortion_model":"plumb_bob","d":[0,0,0,0,0]}}}]});
    let bytes = serde_json::to_vec(&old).unwrap();
    let hash = ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .flat_map(|b| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                HEX[(b >> 4) as usize] as char,
                HEX[(b & 15) as usize] as char,
            ]
        })
        .collect::<String>();
    old["snapshot_sha256"] = json!(hash);
    let original = serde_json::to_vec(&old).unwrap();
    let imported = import_legacy(&original, "synthetic").unwrap();
    assert_eq!(original, serde_json::to_vec(&old).unwrap());
    assert!(!imported.actual_runtime_observed);
    assert!(imported.source_session_id.is_none());
    let mut snapshot = live();
    snapshot.binding.calibration_id = imported.id().unwrap();
    snapshot.calibration = imported;
    let envelope = snapshot.seal().unwrap();
    assert!(envelope.require_runtime().is_err());
    let mut corrupt: Value = old;
    corrupt["cameras"][0]["name"] = json!("different");
    assert!(import_legacy(&serde_json::to_vec(&corrupt).unwrap(), "synthetic").is_err());
}

#[test]
fn calibration_rejects_uncertified_or_unbound_runtime_provenance() {
    for (mode, expected) in [
        (0, "CALIBRATION_VERSION"),
        (1, "PHYSICAL_VERIFICATION_NOT_CERTIFIED"),
        (2, "CALIBRATION_MODEL_COUNT"),
        (3, "MODEL_NAME_INVALID"),
        (4, "CALIBRATION_TRANSFORM_UNBOUND"),
        (5, "CALIBRATION_DUPLICATE_FRAME"),
        (6, "LEGACY_PROVENANCE_INVALID"),
        (7, "RUNTIME_CALIBRATION_PROVENANCE_REQUIRED"),
    ] {
        let mut c = live().calibration;
        match mode {
            0 => c.version = 2,
            1 => c.physical_accuracy_verified = true,
            2 => c.models.clear(),
            3 => {
                let model = c.models.remove("depth").unwrap();
                c.models.insert("bad name".into(), model);
            }
            4 => c.transforms.push(RigidTransform {
                source_frame: "depth".into(),
                target_frame: "missing".into(),
                rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                translation_m: [0.; 3],
            }),
            5 => {
                c.models.insert("other".into(), c.models["depth"].clone());
            }
            6 => c.source = CalibrationSource::LegacyImport,
            _ => c.observed_unix_ns = Some(0),
        }
        assert_eq!(c.validate().unwrap_err().code, expected);
    }
    let mut s = live();
    s.version = 2;
    assert_eq!(s.validate().unwrap_err().code, "SNAPSHOT_VERSION");
    let mut s = live();
    s.binding.session_id = SessionId::parse("f".repeat(32)).unwrap();
    assert_eq!(
        s.validate().unwrap_err().code,
        "CALIBRATION_SESSION_MISMATCH"
    );
}

#[test]
fn legacy_distortion_variants_and_ambiguous_camera_selection_keep_provenance_offline() {
    fn encode(mut value: Value) -> Vec<u8> {
        let digest =
            ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&value).unwrap());
        let mut hash = String::new();
        use std::fmt::Write;
        for b in digest.as_ref() {
            write!(&mut hash, "{b:02x}").unwrap();
        }
        value["snapshot_sha256"] = json!(hash);
        serde_json::to_vec(&value).unwrap()
    }
    for (name, count) in [
        ("rational_polynomial", 8),
        ("equidistant", 4),
        ("unsupported", 0),
    ] {
        let m = &live().calibration.models["depth"];
        let old = json!({"kind":"geminidepthport-calibration-v1","structural_validation":true,"physical_accuracy_verified":false,"cameras":[{"name":"synthetic","intrinsics":{"depth":{"frame_id":m.frame_id,"width":m.width,"height":m.height,"k":m.k,"distortion_model":name,"d":vec![0;count]}}}]});
        let result = import_legacy(&encode(old.clone()), "synthetic");
        if name == "unsupported" {
            assert_eq!(result.unwrap_err().code, "LEGACY_DISTORTION_UNSUPPORTED");
        } else {
            assert!(!result.unwrap().actual_runtime_observed);
        }
        assert_eq!(
            import_legacy(&encode(old.clone()), "missing")
                .unwrap_err()
                .code,
            "LEGACY_CAMERA_AMBIGUOUS_OR_MISSING"
        );
        let mut duplicate = old.clone();
        duplicate["cameras"]
            .as_array_mut()
            .unwrap()
            .push(old["cameras"][0].clone());
        assert_eq!(
            import_legacy(&encode(duplicate), "synthetic")
                .unwrap_err()
                .code,
            "LEGACY_CAMERA_AMBIGUOUS_OR_MISSING"
        );
    }
}

#[test]
fn legacy_digest_preserves_negative_fractional_and_scientific_parameters() {
    let mut old = json!({"kind":"geminidepthport-calibration-v1","structural_validation":true,"physical_accuracy_verified":false,"parameters":[-12.34,1.234e20,12.345],"cameras":[{"name":"synthetic","intrinsics":{"depth":{"frame_id":"depth","width":1,"height":1,"k":[1,0,0,0,1,0,0,0,1],"distortion_model":"plumb_bob","d":[0,0,0,0,0]}}}]});
    let digest = ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&old).unwrap());
    let mut hash = String::new();
    use std::fmt::Write;
    for b in digest.as_ref() {
        write!(&mut hash, "{b:02x}").unwrap();
    }
    old["snapshot_sha256"] = json!(hash);
    assert!(
        !import_legacy(&serde_json::to_vec(&old).unwrap(), "synthetic")
            .unwrap()
            .actual_runtime_observed
    );
}

#[test]
fn snapshot_envelope_version_and_geometry_overflow_remain_explicit_contract_failures() {
    let mut envelope = live().seal().unwrap();
    envelope.version = 2;
    assert!(envelope.verify(&envelope.content.binding).is_err());
    let transform = RigidTransform {
        source_frame: "depth".into(),
        target_frame: "color".into(),
        rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        translation_m: [f64::MAX, 0., 0.],
    };
    assert_eq!(
        transform
            .transform_point([f64::MAX, 0., 0.])
            .unwrap_err()
            .code,
        "POINT_OVERFLOW"
    );
    let parsed: SessionId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
    assert_eq!(parsed.to_string(), "a".repeat(32));
    for request in [
        json!([]),
        json!({}),
        json!({"protocols":{}}),
        json!({"protocols":{"snapshot":true}}),
        json!({"protocols":{"snapshot":1},"extra":true}),
    ] {
        assert_eq!(
            capabilities::negotiate("test", "1", &request)
                .unwrap_err()
                .code,
            "CAPABILITY_REQUEST_INVALID"
        );
    }
    assert!(content_digest(ContentKind::Snapshot, b"[]").is_err());
    assert_eq!(
        parse_strict(b"[-1,null,true,\"text\"]").unwrap(),
        json!([-1, null, true, "text"])
    );
}

#[test]
fn snapshot_store_rejects_non_private_roots_and_valid_content_under_the_wrong_filename() {
    assert!(SnapshotStore::open(std::path::Path::new("relative-store")).is_err());
    let temp = tempfile::tempdir().unwrap();
    assert!(SnapshotStore::open(&temp.path().join("missing/child")).is_err());
    let dir = temp.path().join("store");
    let store = SnapshotStore::open(&dir).unwrap();
    let first = live().seal().unwrap();
    let id = store.publish(&first).unwrap();
    let mut next = live();
    next.configuration["note"] = json!(true);
    next.binding.configuration_id = ConfigurationId::parse(
        content_digest(
            ContentKind::Configuration,
            &serde_json::to_vec(&next.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let next = next.seal().unwrap();
    fs::write(dir.join(format!("{id}.json")), next.to_bytes().unwrap()).unwrap();
    assert_eq!(
        store.read(&id, &next.content.binding).unwrap_err().code,
        "SNAPSHOT_FILENAME_MISMATCH"
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SnapshotStore::open(&dir).is_err());
}
#[test]
fn direct_rust_contract_inputs_reject_nonfinite_geometry_and_unsupported_versions() {
    let mut model = live().calibration.models["depth"].clone();
    model.k[0] = f64::NAN;
    assert_eq!(model.validate().unwrap_err().code, "CAMERA_NONFINITE");
    let mut transform = RigidTransform {
        source_frame: "same".into(),
        target_frame: "same".into(),
        rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        translation_m: [0.; 3],
    };
    assert_eq!(
        transform.validate().unwrap_err().code,
        "TRANSFORM_FRAMES_INVALID"
    );
    transform.target_frame = "other".into();
    assert_eq!(
        transform
            .transform_point([f64::NAN, 0., 0.])
            .unwrap_err()
            .code,
        "POINT_NONFINITE"
    );
    let mut policy = live().quality_policy;
    policy.version = 2;
    assert_eq!(
        policy.validate().unwrap_err().code,
        "QUALITY_POLICY_VERSION"
    );
}

#[test]
fn recording_gate_rejects_version_mismatch_in_both_transport_parse_and_digest() {
    use capture_contract::recording::*;
    let snapshot = live().seal().unwrap();
    let gate = GateRequest {
        version: 2,
        phase: GatePhase::Preparing,
        request_id: SessionId::parse("e".repeat(32)).unwrap(),
        binding: snapshot.content.binding,
        snapshot_id: snapshot.snapshot_id,
        runtime_config_sha256: ConfigurationId::parse("c".repeat(64)).unwrap(),
        token_sha256: ConfigurationId::parse("d".repeat(64)).unwrap(),
    };
    assert_eq!(gate.digest().unwrap_err().code, "RECORDING_GATE_VERSION");
    assert_eq!(
        GateRequest::parse(&serde_json::to_vec(&gate).unwrap())
            .unwrap_err()
            .code,
        "RECORDING_GATE_VERSION"
    );
}

#[test]
fn legacy_import_rejects_schema_ambiguous_units_and_nonunit_rotations() {
    let m = &live().calibration.models["depth"];
    let base = json!({"kind":"geminidepthport-calibration-v1","structural_validation":true,"physical_accuracy_verified":false,"cameras":[{"name":"synthetic","intrinsics":{"depth":{"frame_id":m.frame_id,"width":m.width,"height":m.height,"k":m.k,"distortion_model":"plumb_bob","d":[0.,0.,0.,0.,0.]}},"depth_to_color":{"source_frame":"depth","target_frame":"color","translation_unit":"m","rotation_representation":"quaternion_xyzw","translation":[0.,0.,0.],"rotation":[0.,0.,0.,1.]}}]});
    for (pointer, value, code) in [
        ("/kind", json!("unknown"), "LEGACY_CALIBRATION_SCHEMA"),
        (
            "/cameras/0/depth_to_color/translation_unit",
            json!("mm"),
            "LEGACY_TRANSFORM_UNIT",
        ),
        (
            "/cameras/0/depth_to_color/rotation",
            json!([0., 0., 0., 2.]),
            "LEGACY_ROTATION_INVALID",
        ),
    ] {
        let mut value_copy = base.clone();
        *value_copy.pointer_mut(pointer).unwrap() = value;
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            &serde_json::to_vec(&value_copy).unwrap(),
        );
        use std::fmt::Write;
        let mut hash = String::new();
        for b in digest.as_ref() {
            write!(&mut hash, "{b:02x}").unwrap();
        }
        value_copy["snapshot_sha256"] = json!(hash);
        assert_eq!(
            import_legacy(&serde_json::to_vec(&value_copy).unwrap(), "synthetic")
                .unwrap_err()
                .code,
            code
        );
    }
    let mut context: FrameContext = serde_json::from_value(json!({"version":2,"session_id":"a".repeat(32),"generation_id":"b".repeat(32),"configuration_id":"c".repeat(64),"calibration_id":"d".repeat(64),"snapshot_id":"e".repeat(64),"stream_id":"depth","sequence":null,"device_timestamp_us":null,"system_timestamp_us":null,"received_mono_offset_ns":0})).unwrap();
    assert_eq!(
        context.validate().unwrap_err().code,
        "FRAME_CONTEXT_VERSION"
    );
    context.version = 1;
    context.validate().unwrap();
}

#[test]
fn snapshot_configuration_is_bounded_before_it_can_be_hashed_or_sealed() {
    let mut snapshot = live();
    snapshot.configuration = json!({"oversized":"x".repeat(4*1024*1024)});
    assert_eq!(snapshot.validate().unwrap_err().code, "JSON_SIZE");
}
