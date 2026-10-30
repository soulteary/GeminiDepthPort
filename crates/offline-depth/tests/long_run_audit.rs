#[allow(dead_code)]
#[path = "../examples/audit_imu_long_run.rs"]
mod auditor;
mod support;
use auditor::audit;
use offline_depth::{StorageEndian, recording::RawRecordingWriter};
use std::fs;
#[test]
fn audit_reads_complete_and_recovered_data_twice_and_rejects_corruption() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), true, 0.);
    let original: capture_contract::snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(source.path().join("snapshot.json")).unwrap()).unwrap();
    let mut pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(source.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(source.path().join("depth.raw16")).unwrap();
    let mut content = original.content;
    content.configuration["depth"] = serde_json::json!({"unit_policy":"normalize-mm"});
    content.binding.configuration_id = capture_contract::ConfigurationId::parse(
        capture_contract::content_digest(
            capture_contract::ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    for frame in [&mut pair.color, &mut pair.depth] {
        frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
        frame.context.snapshot_id = snapshot.snapshot_id.clone();
    }

    for global in [
        None,
        Some(1_700_000_000_000_000),
        Some(0),
        Some(u64::MAX),
        Some(2_147_483_648_000_000),
    ] {
        let valid = global == Some(1_700_000_000_000_000);
        let root = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot.clone(), 1_000_000)
                .unwrap();
        pair.color.context.global_timestamp_us = global;
        pair.depth.context.global_timestamp_us = pair.color.context.global_timestamp_us;
        writer
            .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
            .unwrap();
        writer.checkpoint().unwrap();
        let partial = root.path().join("capture.partial");
        let mut output = Vec::new();
        assert_eq!(audit(&partial, true, &mut output).is_ok(), valid);
        assert!(audit(&partial, false, &mut Vec::new()).is_err());
        let path = writer.finish().unwrap();
        if !valid {
            assert!(audit(&path, false, &mut Vec::new()).is_err());
            continue;
        }
        output.clear();
        audit(&path, false, &mut output).unwrap();
        let rows: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["color"]["stamp_ns"], 1_700_000_000_000_000_000u64);
        assert_eq!(rows[1]["independent_audit_passes"], 2);
        assert_eq!(rows[1]["recovered"], false);
        assert_eq!(rows[1]["audit_version"], 2);
        assert_eq!(rows[0]["depth"]["sha256"], offline_depth::sha256(&pixels));
        assert_eq!(
            rows[0]["depth"]["ros_sha256"],
            offline_depth::sha256(&[0xe8, 3, 0, 0])
        );
        assert_ne!(rows[0]["depth"]["sha256"], rows[0]["depth"]["ros_sha256"]);
        assert!(audit(&path, true, &mut Vec::new()).is_err());
        let payload = path.join("payload-00000000000000000001.bin");
        let mut bytes = fs::read(&payload).unwrap();
        bytes[0] ^= 1;
        fs::write(payload, bytes).unwrap();
        assert!(audit(&path, false, &mut Vec::new()).is_err());
    }
}

#[test]
fn independent_depth_reference_covers_scales_endian_padding_rounding_and_overflow() {
    use offline_depth::recording::{FileRef, StoredImage};
    let mut image = StoredImage {
        file: FileRef {
            path: "unused".into(),
            sha256: "0".repeat(64),
            range: None,
        },
        topic: None,
        record_timestamp_ns: None,
        header_timestamp_ns: None,
        frame_id: "depth".into(),
        width: 65536,
        height: 1,
        stride: 131072,
        encoding: "depth16".into(),
        endian: StorageEndian::Little,
        camera_info: None,
        metadata: None,
    };
    let bytes: Vec<_> = (0..=65535u16).flat_map(u16::to_le_bytes).collect();
    for scale in [0.1f32, 0.5, 1., 0.000001] {
        let expected = frame_core::normalize_depth_mm(
            &bytes,
            frame_core::FrameLayout {
                width: image.width,
                height: 1,
                stride: image.stride,
                format: frame_core::PixelFormat::Depth16,
            },
            false,
            scale,
        )
        .unwrap();
        assert_eq!(
            auditor::reference_depth_mm(&bytes, &image, f64::from(scale), true).unwrap(),
            expected.bytes
        );
    }
    assert_eq!(
        auditor::reference_depth_mm(&bytes, &image, 1., false).unwrap(),
        bytes
    );
    image.width = 4;
    image.stride = 10;
    image.endian = StorageEndian::Big;
    assert_eq!(
        auditor::reference_depth_mm(&[0, 0, 0, 1, 0, 5, 0xff, 0xff, 99, 99], &image, 0.1, true)
            .unwrap(),
        [0, 0, 1, 0, 1, 0, 0x9a, 0x19]
    );
    for (scale, normalize) in [
        (0., true),
        (f64::NAN, true),
        (f64::INFINITY, true),
        (-1., true),
        (0.1, false),
        (2., true),
    ] {
        assert!(
            auditor::reference_depth_mm(
                &[0, 0, 0, 1, 0, 5, 0xff, 0xff, 99, 99],
                &image,
                scale,
                normalize
            )
            .is_err()
        );
    }
    assert!(auditor::reference_depth_mm(&[], &image, 1., true).is_err());
    image.stride = 2;
    assert!(auditor::reference_depth_mm(&[0, 0], &image, 1., true).is_err());
}
