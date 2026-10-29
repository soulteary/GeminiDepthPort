#![allow(dead_code)]
use capture_contract::{snapshot::*, *};
use frame_core::{BoundPairEvidence, FrameEvidence};
use offline_depth::{DirectoryManifest, PairInput, StorageEndian, sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};
pub fn fixture(root: &Path, counts: bool, tz: f64) -> DirectoryManifest {
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
        global_timestamp_us: None,
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
pub fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .arg("--input")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
pub fn decoded(path: &Path) -> Vec<u16> {
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
