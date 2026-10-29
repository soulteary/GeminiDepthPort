use capture_contract::{snapshot::*, *};
use frame_core::{BoundPairEvidence, FrameEvidence};
use offline_depth::{DirectoryManifest, PairInput, StorageEndian, sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};
fn fixture(root: &Path, counts: bool, tz: f64) -> DirectoryManifest {
    let session = SessionId::parse("a".repeat(32)).unwrap();
    let instance = InstanceId::parse("b".repeat(64)).unwrap();
    let model = |frame: &str| CameraModel {
        frame_id: frame.into(),
        width: 2,
        height: 1,
        k: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
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
        models: BTreeMap::from([
            ("color".into(), model("color")),
            ("depth".into(), model("depth")),
        ]),
        transforms: vec![RigidTransform {
            source_frame: "depth".into(),
            target_frame: "color".into(),
            rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            translation_m: [0., 0., tz],
        }],
        physical_accuracy_verified: false,
        legacy_source_sha256: None,
    };
    let configuration = serde_json::json!({"profile":"artificial"});
    let representation = if counts {
        DepthRepresentation::SdkCounts {
            scale_mm: f64::from(0.1f32),
        }
    } else {
        DepthRepresentation::Millimetres {
            quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
        }
    };
    let snapshot = CaptureSnapshot {
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
        depth: representation.clone(),
        quality_policy: QualityPolicy {
            version: 1,
            max_source_age_ns: 500_000_000,
            max_future_ns: 100_000_000,
            max_pair_delta_us: 20_000,
            min_valid_fraction: 0.,
            max_zero_fraction: 1.,
            action: QualityAction::FailClosed,
        },
    }
    .seal()
    .unwrap();
    fs::write(root.join("snapshot.json"), snapshot.to_bytes().unwrap()).unwrap();
    let context = |stream: &str| FrameContext {
        version: 1,
        session_id: snapshot.content.binding.session_id.clone(),
        generation_id: snapshot.content.binding.generation_id.clone(),
        configuration_id: snapshot.content.binding.configuration_id.clone(),
        calibration_id: snapshot.content.binding.calibration_id.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        stream_id: stream.into(),
        sequence: Some(1),
        device_timestamp_us: Some(1000),
        system_timestamp_us: Some(10000),
        received_mono_offset_ns: 100,
    };
    let frame = |stream: &str| FrameEvidence {
        context: context(stream),
        encoding: if stream == "color" { "yuyv" } else { "depth16" }.into(),
        width: 2,
        height: 1,
        stride: 4,
        source_depth_scale_mm: if stream == "depth" {
            Some(if counts { 0.1 } else { 1. })
        } else {
            None
        },
        output_depth_scale_mm: if stream == "depth" {
            Some(if counts { 0.1 } else { 1. })
        } else {
            None
        },
        normalized: false,
    };
    let evidence = BoundPairEvidence {
        version: 1,
        pair_sequence: 1,
        color: frame("color"),
        depth: frame("depth"),
        color_stamp_us: 1000,
        depth_stamp_us: 1000,
        clock_source: "device_us".into(),
        delta_us: 0,
        association: "sdk_frameset".into(),
        hardware_sync_verified: false,
    };
    let bytes = serde_json::to_vec(&evidence).unwrap();
    fs::write(root.join("pair.json"), &bytes).unwrap();
    let samples: Vec<u8> = if counts { [10000u16, 0] } else { [1000u16, 0] }
        .iter()
        .flat_map(|p| p.to_le_bytes())
        .collect();
    fs::write(root.join("depth.raw16"), &samples).unwrap();
    let manifest = DirectoryManifest {
        version: 1,
        source_kind: "bound-pair-directory-v1".into(),
        snapshot: "snapshot.json".into(),
        binding: snapshot.content.binding,
        pairs: vec![PairInput {
            pair_sequence: 1,
            evidence: "pair.json".into(),
            evidence_sha256: sha256(&bytes),
            depth: "depth.raw16".into(),
            depth_sha256: sha256(&samples),
            endian: StorageEndian::Little,
            storage_representation: representation,
        }],
    };
    write_manifest(root, &manifest);
    manifest
}
fn write_manifest(root: &Path, m: &DirectoryManifest) {
    fs::write(root.join("manifest.json"), serde_json::to_vec(m).unwrap()).unwrap()
}
fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .arg("--input")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
fn decoded(path: &Path) -> Vec<u16> {
    let mut r = png::Decoder::new(fs::File::open(path).unwrap())
        .read_info()
        .unwrap();
    let mut bytes = vec![0; r.output_buffer_size()];
    let i = r.next_frame(&mut bytes).unwrap();
    assert_eq!(i.bit_depth, png::BitDepth::Sixteen);
    bytes[..i.buffer_size()]
        .chunks_exact(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect()
}
#[test]
fn original_png_tar_and_validate_only_are_real_cli_paths() {
    let input = tempfile::tempdir().unwrap();
    fixture(input.path(), false, 0.);
    let result = cli(input.path(), &["--validate-only"]);
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(fs::read_dir(input.path()).unwrap().count(), 4);
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "original",
            "--mode",
            "both",
            "--capacity",
            "1",
        ],
    );
    assert!(result.status.success(), "{:?}", result);
    let job = output.path().join("original");
    assert_eq!(
        decoded(&job.join("frames/00000000000000000000.png")),
        [1000, 0]
    );
    let mut tar = tar::Archive::new(fs::File::open(job.join("depth.tar")).unwrap());
    assert_eq!(tar.entries().unwrap().count(), 1);
    assert!(job.join("COMPLETE").exists());
}
#[test]
fn nonidentity_d2c_uses_color_z_and_explicit_count_scale() {
    let input = tempfile::tempdir().unwrap();
    fixture(input.path(), true, 1.);
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "d2c",
            "--d2c",
        ],
    );
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(
        decoded(&output.path().join("d2c/frames/00000000000000000000.png")),
        [2000, 0]
    );
    let result = cli(input.path(), &["--validate-only"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("EXPLICIT_NORMALIZATION_REQUIRED"));
    assert!(
        cli(input.path(), &["--validate-only", "--normalize-mm"])
            .status
            .success()
    );
}
#[test]
fn bad_binding_and_hash_fail_nonzero_without_complete() {
    let input = tempfile::tempdir().unwrap();
    let m = fixture(input.path(), false, 0.);
    let mut evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
    evidence["depth"]["context"]["session_id"] = serde_json::json!("d".repeat(32));
    let b = serde_json::to_vec(&evidence).unwrap();
    fs::write(input.path().join("pair.json"), &b).unwrap();
    let mut changed = m.clone();
    changed.pairs[0].evidence_sha256 = sha256(&b);
    write_manifest(input.path(), &changed);
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &["--output", output.path().to_str().unwrap(), "--job", "bad"],
    );
    assert!(!result.status.success());
    assert!(output.path().join("bad.partial").exists());
    assert!(!output.path().join("bad").exists());
    assert!(!output.path().join("bad.partial/COMPLETE").exists());
    write_manifest(input.path(), &m);
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
}
#[test]
fn malformed_duplicate_paths_missing_selection_and_args_are_rejected() {
    let input = tempfile::tempdir().unwrap();
    let mut m = fixture(input.path(), false, 0.);
    for args in [
        vec!["--validate-only", "--capacity", "0"],
        vec!["--validate-only", "--pair", "2"],
        vec!["--validate-only", "--d2c", "--normalize-mm"],
        vec!["--validate-only", "--validate-only"],
    ] {
        assert!(!cli(input.path(), &args).status.success())
    }
    m.pairs[0].depth = "../escape.raw16".into();
    write_manifest(input.path(), &m);
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
    fs::write(
        input.path().join("manifest.json"),
        b"{\"version\":1,\"version\":1}",
    )
    .unwrap();
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
}
#[test]
fn independent_snapshot_unit_and_dimension_bindings_are_checked() {
    let input = tempfile::tempdir().unwrap();
    let original = fixture(input.path(), false, 0.);
    let mut m = original.clone();
    m.pairs[0].storage_representation = DepthRepresentation::SdkCounts {
        scale_mm: f64::from(0.1f32),
    };
    write_manifest(input.path(), &m);
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
    write_manifest(input.path(), &original);
    let evidence_path = input.path().join("pair.json");
    let initial = fs::read(&evidence_path).unwrap();
    for mutation in ["dimensions", "scale", "snapshot"] {
        let mut e: serde_json::Value = serde_json::from_slice(&initial).unwrap();
        match mutation {
            "dimensions" => e["depth"]["width"] = serde_json::json!(1),
            "scale" => {
                e["depth"]["source_depth_scale_mm"] = serde_json::json!(0.1);
                e["depth"]["output_depth_scale_mm"] = serde_json::json!(0.1);
            }
            _ => {
                e["color"]["context"]["snapshot_id"] = serde_json::json!("e".repeat(64));
                e["depth"]["context"]["snapshot_id"] = serde_json::json!("e".repeat(64));
            }
        }
        let bytes = serde_json::to_vec(&e).unwrap();
        fs::write(&evidence_path, &bytes).unwrap();
        let mut m = original.clone();
        m.pairs[0].evidence_sha256 = sha256(&bytes);
        write_manifest(input.path(), &m);
        assert!(
            !cli(input.path(), &["--validate-only"]).status.success(),
            "{mutation}"
        );
    }
}
#[test]
fn later_frame_corruption_leaves_partial_after_first_frame_and_selection_is_explicit() {
    let input = tempfile::tempdir().unwrap();
    let mut manifest = fixture(input.path(), false, 0.);
    let mut evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
    evidence["pair_sequence"] = serde_json::json!(2);
    for stream in ["color", "depth"] {
        evidence[stream]["context"]["sequence"] = serde_json::json!(2);
        evidence[stream]["context"]["device_timestamp_us"] = serde_json::json!(2000);
        evidence[stream]["context"]["received_mono_offset_ns"] = serde_json::json!(200);
    }
    evidence["color_stamp_us"] = serde_json::json!(2000);
    evidence["depth_stamp_us"] = serde_json::json!(2000);
    let bytes = serde_json::to_vec(&evidence).unwrap();
    fs::write(input.path().join("pair2.json"), &bytes).unwrap();
    let mut next = manifest.pairs[0].clone();
    next.pair_sequence = 2;
    next.evidence = "pair2.json".into();
    next.evidence_sha256 = sha256(&bytes);
    next.depth = "second.raw16".into();
    fs::write(input.path().join("second.raw16"), b"bad").unwrap();
    manifest.pairs.push(next);
    write_manifest(input.path(), &manifest);
    assert!(
        cli(input.path(), &["--validate-only", "--pair", "1"])
            .status
            .success()
    );
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "partial",
            "--capacity",
            "1",
        ],
    );
    assert!(!result.status.success());
    let stage = output.path().join("partial.partial");
    assert!(stage.join("frames/00000000000000000000.png").exists());
    assert!(!stage.join("COMPLETE").exists());
    assert!(stage.join("snapshot.json").exists());
    assert!(
        stage
            .join("pair-evidence-00000000000000000001.json")
            .exists()
    );
    assert!(!output.path().join("partial").exists());
}
#[test]
fn completed_export_keeps_resolvable_evidence_after_input_is_removed() {
    let input = tempfile::tempdir().unwrap();
    let manifest = fixture(input.path(), false, 0.);
    let evidence = fs::read(input.path().join("pair.json")).unwrap();
    let output = tempfile::tempdir().unwrap();
    assert!(
        cli(
            input.path(),
            &[
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "selfcontained"
            ]
        )
        .status
        .success()
    );
    drop(input);
    let job = output.path().join("selfcontained");
    let snapshot = SnapshotEnvelope::parse(
        &fs::read(job.join("snapshot.json")).unwrap(),
        &manifest.binding,
    )
    .unwrap();
    let stored = fs::read(job.join("pair-evidence-00000000000000000001.json")).unwrap();
    assert_eq!(stored, evidence);
    assert_eq!(sha256(&stored), manifest.pairs[0].evidence_sha256);
    let e: BoundPairEvidence = serde_json::from_slice(&stored).unwrap();
    assert_eq!(e.depth.context.snapshot_id, snapshot.snapshot_id);
}
#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected_but_in_tree_alias_preserves_explicit_hash() {
    use std::os::unix::fs::symlink;
    let input = tempfile::tempdir().unwrap();
    let original = fixture(input.path(), false, 0.);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.raw16"), [0xe8, 3, 0, 0]).unwrap();
    symlink(
        outside.path().join("secret.raw16"),
        input.path().join("outside.raw16"),
    )
    .unwrap();
    let mut m = original.clone();
    m.pairs[0].depth = "outside.raw16".into();
    write_manifest(input.path(), &m);
    let result = cli(input.path(), &["--validate-only"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("SOURCE_PATH_ESCAPE"));
    symlink(
        input.path().join("depth.raw16"),
        input.path().join("alias.raw16"),
    )
    .unwrap();
    m.pairs[0].depth = "alias.raw16".into();
    write_manifest(input.path(), &m);
    assert!(cli(input.path(), &["--validate-only"]).status.success());
}
#[test]
fn actual_cli_big_endian_padded_rows_match_numeric_png_samples() {
    let input = tempfile::tempdir().unwrap();
    let mut m = fixture(input.path(), false, 0.);
    let mut e: serde_json::Value =
        serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
    e["depth"]["stride"] = serde_json::json!(7);
    let eb = serde_json::to_vec(&e).unwrap();
    fs::write(input.path().join("pair.json"), &eb).unwrap();
    m.pairs[0].evidence_sha256 = sha256(&eb);
    m.pairs[0].endian = StorageEndian::Big;
    let raw = [3, 0xe8, 0, 0, 23, 24, 25];
    fs::write(input.path().join("depth.raw16"), raw).unwrap();
    m.pairs[0].depth_sha256 = sha256(&raw);
    write_manifest(input.path(), &m);
    let output = tempfile::tempdir().unwrap();
    assert!(
        cli(
            input.path(),
            &[
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "padded"
            ]
        )
        .status
        .success()
    );
    assert_eq!(
        decoded(&output.path().join("padded/frames/00000000000000000000.png")),
        [1000, 0]
    );
    fs::write(input.path().join("depth.raw16"), &raw[..6]).unwrap();
    m.pairs[0].depth_sha256 = sha256(&raw[..6]);
    write_manifest(input.path(), &m);
    let result = cli(input.path(), &["--validate-only"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("DEPTH_PAYLOAD_LENGTH"));
}
#[test]
fn each_stream_sequence_advances_independently_and_selected_clock_cannot_change() {
    let input = tempfile::tempdir().unwrap();
    let mut m = fixture(input.path(), false, 0.);
    let mut e: serde_json::Value =
        serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
    e["pair_sequence"] = serde_json::json!(2);
    for stream in ["color", "depth"] {
        e[stream]["context"]["device_timestamp_us"] = serde_json::json!(2000);
        e[stream]["context"]["system_timestamp_us"] = serde_json::json!(20000);
        e[stream]["context"]["received_mono_offset_ns"] = serde_json::json!(200);
    }
    e["color"]["context"]["sequence"] = serde_json::json!(8);
    e["depth"]["context"]["sequence"] = serde_json::json!(2);
    e["color_stamp_us"] = serde_json::json!(2000);
    e["depth_stamp_us"] = serde_json::json!(2000);
    let mut next = m.pairs[0].clone();
    next.pair_sequence = 2;
    next.evidence = "pair2.json".into();
    m.pairs.push(next);
    let update = |e: &serde_json::Value, m: &mut DirectoryManifest| {
        let bytes = serde_json::to_vec(e).unwrap();
        fs::write(input.path().join("pair2.json"), &bytes).unwrap();
        m.pairs[1].evidence_sha256 = sha256(&bytes);
        write_manifest(input.path(), m);
    };
    update(&e, &mut m);
    assert!(cli(input.path(), &["--validate-only"]).status.success());
    e["depth"]["context"]["sequence"] = serde_json::json!(1);
    update(&e, &mut m);
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
    e["depth"]["context"]["sequence"] = serde_json::json!(2);
    e["clock_source"] = serde_json::json!("sdk_system_us");
    e["color_stamp_us"] = serde_json::json!(20000);
    e["depth_stamp_us"] = serde_json::json!(20000);
    update(&e, &mut m);
    assert!(!cli(input.path(), &["--validate-only"]).status.success());
}

#[test]
fn d2c_overflow_is_explicit_and_never_clamped_or_completed() {
    let input = tempfile::tempdir().unwrap();
    let mut manifest = fixture(input.path(), false, 0.001);
    let bytes = [65535u16, 0]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    fs::write(input.path().join("depth.raw16"), &bytes).unwrap();
    manifest.pairs[0].depth_sha256 = sha256(&bytes);
    write_manifest(input.path(), &manifest);
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--d2c",
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "overflow",
        ],
    );
    assert!(!result.status.success());
    let error: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(error["code"], "WARP_FAILED");
    assert_eq!(error["detail_code"], "D2C_OUTPUT_OVERFLOW");
    assert_eq!(error["complete"], false);
    assert!(!output.path().join("overflow/COMPLETE").exists());
    assert!(!output.path().join("overflow.partial/COMPLETE").exists());
    assert_eq!(fs::read(input.path().join("depth.raw16")).unwrap(), bytes);
}

#[test]
fn d2c_distortion_models_and_big_endian_input_keep_numerical_depth() {
    for (name, count, accepted) in [
        ("plumb_bob", 5, true),
        ("rational_polynomial", 8, true),
        ("equidistant", 4, false),
    ] {
        let input = tempfile::tempdir().unwrap();
        let mut manifest = fixture(input.path(), false, 0.);
        let old: SnapshotEnvelope =
            serde_json::from_slice(&fs::read(input.path().join("snapshot.json")).unwrap()).unwrap();
        let mut snapshot = old.content;
        for model in snapshot.calibration.models.values_mut() {
            model.distortion_model = match name {
                "plumb_bob" => DistortionModel::PlumbBob,
                "rational_polynomial" => DistortionModel::RationalPolynomial,
                _ => DistortionModel::Equidistant,
            };
            model.distortion_coefficients = vec![0.; count];
        }
        snapshot.binding.calibration_id = snapshot.calibration.id().unwrap();
        let snapshot = snapshot.seal().unwrap();
        fs::write(
            input.path().join("snapshot.json"),
            snapshot.to_bytes().unwrap(),
        )
        .unwrap();
        manifest.binding = snapshot.content.binding.clone();
        let mut evidence: BoundPairEvidence =
            serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
        for frame in [&mut evidence.color, &mut evidence.depth] {
            frame.context.calibration_id = manifest.binding.calibration_id.clone();
            frame.context.snapshot_id = snapshot.snapshot_id.clone();
        }
        let bytes = serde_json::to_vec(&evidence).unwrap();
        fs::write(input.path().join("pair.json"), &bytes).unwrap();
        manifest.pairs[0].evidence_sha256 = sha256(&bytes);
        let pixels = [0x03, 0xe8, 0, 0];
        fs::write(input.path().join("depth.raw16"), pixels).unwrap();
        manifest.pairs[0].depth_sha256 = sha256(&pixels);
        manifest.pairs[0].endian = StorageEndian::Big;
        write_manifest(input.path(), &manifest);
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--d2c",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "model",
            ],
        );
        assert_eq!(result.status.success(), accepted, "{result:?}");
        if accepted {
            assert_eq!(
                decoded(&output.path().join("model/frames/00000000000000000000.png")),
                [1000, 0]
            );
        } else {
            assert!(String::from_utf8_lossy(&result.stdout).contains("D2C_MODEL_UNSUPPORTED"));
        }
    }
}

#[test]
fn directory_source_reports_manifest_limits_and_stable_schema_errors() {
    let input = tempfile::tempdir().unwrap();
    let original = fixture(input.path(), false, 0.);
    for mode in 0..5 {
        let mut m = original.clone();
        let expected = match mode {
            0 => {
                m.version = 2;
                "SOURCE_VERSION"
            }
            1 => {
                m.pairs.clear();
                "SOURCE_PAIR_LIMIT"
            }
            2 => {
                m.pairs[0].depth_sha256 = "X".repeat(64);
                "SOURCE_PAIR_MANIFEST"
            }
            3 => {
                m.snapshot = "missing.json".into();
                "INPUT_OUTPUT_IO"
            }
            _ => {
                m.pairs[0].pair_sequence = 2;
                "PAIR_SEQUENCE_BINDING"
            }
        };
        write_manifest(input.path(), &m);
        let result = cli(input.path(), &["--validate-only"]);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stdout).contains(expected),
            "{result:?}"
        );
    }
    fs::write(
        input.path().join("manifest.json"),
        b"{\"version\":\"invalid\"}",
    )
    .unwrap();
    let result = cli(input.path(), &["--validate-only"]);
    assert!(String::from_utf8_lossy(&result.stdout).contains("JSON_SCHEMA"));
}
