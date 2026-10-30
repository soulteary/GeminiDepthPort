mod support;
use capture_contract::*;
use capture_observer::*;
use offline_depth::{StorageEndian, recording::*, sha256};
use std::{fs, path::Path};
use support::*;
fn reference(root: &Path, name: &str, bytes: &[u8]) -> FileRef {
    fs::write(root.join(name), bytes).unwrap();
    FileRef {
        range: None,
        path: name.into(),
        sha256: sha256(bytes),
    }
}
fn index(root: &Path, record: &RecordingRecord) {
    let line = serde_json::to_vec(record).unwrap();
    let snapshot: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(root.join("snapshot.json")).unwrap()).unwrap();
    fs::write(root.join("records.jsonl"), &line).unwrap();
    let manifest = RecordingManifest {
        version: 2,
        source_kind: "recording-directory-v2".into(),
        snapshot: "snapshot.json".into(),
        binding: snapshot.content.binding,
        records: "records.jsonl".into(),
        records_sha256: sha256(&line),
        record_count: 1,
    };
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}
fn fixture_v2(root: &Path, observed: bool, counts: bool) -> RecordingRecord {
    let legacy = fixture(root, counts, 0.);
    let mut pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(root.join("pair.json")).unwrap()).unwrap();
    let make = |stream: &str| StoredImage {
        file: if stream == "depth" {
            FileRef {
                range: None,
                path: "depth.raw16".into(),
                sha256: legacy.pairs[0].depth_sha256.clone(),
            }
        } else {
            reference(root, "color.raw", &[1, 2, 3, 4])
        },
        topic: None,
        record_timestamp_ns: None,
        header_timestamp_ns: None,
        frame_id: stream.into(),
        width: 2,
        height: 1,
        stride: 4,
        encoding: if stream == "depth" { "depth16" } else { "yuyv" }.into(),
        endian: StorageEndian::Little,
        camera_info: None,
        metadata: None,
    };
    let mut color = make("color");
    let mut depth = make("depth");
    let evidence = if observed {
        let frame = |context: &mut FrameContext, id| {
            context.sequence = None;
            context.device_timestamp_us = None;
            context.system_timestamp_us = None;
            ObservedFrame {
                observation_sequence: id,
                context: context.clone(),
                observed_header_ns: 1_000_000_000,
                mapped_unix_ns: 1_000_000_000,
                metadata_provenance: MetadataProvenance::Unobserved,
                reported_source_session: None,
                camera_info_observation_sequence: None,
                metadata_observation_sequence: None,
            }
        };
        let c = frame(&mut pair.color.context, 1);
        let d = frame(&mut pair.depth.context, 2);
        for image in [&mut color, &mut depth] {
            image.topic = Some(format!("/synthetic/{}", image.frame_id));
            image.record_timestamp_ns = Some(1_000_000_001);
            image.header_timestamp_ns = Some(1_000_000_000);
            let info = DecodedCameraInfo {
                stamp: HeaderStamp { sec: 1, nanosec: 0 },
                frame_id: image.frame_id.clone(),
                width: 2,
                height: 1,
                distortion_model: "".into(),
                k: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                d: vec![],
                r: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                p: [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0.],
                binning: [0; 2],
                roi: [0; 4],
                roi_do_rectify: false,
            };
            image.camera_info = Some(reference(
                root,
                &format!("{}-info.json", image.frame_id),
                &serde_json::to_vec(&info).unwrap(),
            ));
        }
        depth.encoding = "16UC1".into();
        TaggedEvidence::ObservedAssociation {
            association: ObservedAssociation {
                version: 1,
                association_sequence: 1,
                provenance: PairingProvenance::ObservedTimestamp,
                color: c,
                depth: d,
                delta_ns: 0,
                same_sdk_frameset: None,
                hardware_sync_verified: false,
            },
            header_mapping: quality_core::ClockMapper::DeclaredObservedUnixHeader,
        }
    } else {
        TaggedEvidence::SdkBoundPair {
            pair,
            source_json: None,
        }
    };
    let e = reference(root, "tagged.json", &serde_json::to_vec(&evidence).unwrap());
    let record = RecordingRecord {
        imu: Vec::new(),
        sequence: 1,
        evidence: e,
        color,
        depth,
        storage_representation: legacy.pairs[0].storage_representation.clone(),
    };
    index(root, &record);
    record
}
#[test]
fn sdk_counts_original_cli_preserves_exact_integer_samples_and_scale() {
    let input = tempfile::tempdir().unwrap();
    fixture(input.path(), true, 0.);
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--preserve-counts",
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "counts",
        ],
    );
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        decoded(&output.path().join("counts/frames/00000000000000000000.png")),
        [10000, 0]
    );
    let record = fs::read_to_string(output.path().join("counts/manifest.jsonl")).unwrap();
    // Numeric samples and evidence remain exact; no ROS millimetre reverse conversion.
    assert!(record.contains("original_sdk_counts"));
    assert!(record.contains("sdk-counts"));
}
#[test]
fn both_v2_sources_use_actual_cli_without_fabricating_sdk_provenance() {
    for observed in [false, true] {
        let input = tempfile::tempdir().unwrap();
        fixture_v2(input.path(), observed, false);
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--recording-v2",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "export",
            ],
        );
        assert!(result.status.success(), "{result:?}");
        assert_eq!(
            decoded(&output.path().join("export/frames/00000000000000000000.png")),
            [1000, 0]
        );
        let evidence = fs::read_to_string(
            output
                .path()
                .join("export/pair-evidence-00000000000000000001.json"),
        )
        .unwrap();
        assert_eq!(evidence.contains("observed-association"), observed);
        assert_eq!(
            evidence.contains("\"association\":\"sdk_frameset\""),
            !observed
        );
        assert!(output.path().join("export/COMPLETE").is_file());
    }
}
#[test]
fn v2_counts_supported_but_ros_millimetres_cannot_recover_counts() {
    let input = tempfile::tempdir().unwrap();
    fixture_v2(input.path(), false, true);
    assert!(
        cli(
            input.path(),
            &["--recording-v2", "--validate-only", "--preserve-counts"]
        )
        .status
        .success()
    );
    let input = tempfile::tempdir().unwrap();
    fixture_v2(input.path(), true, true);
    let result = cli(
        input.path(),
        &["--recording-v2", "--validate-only", "--preserve-counts"],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("OBSERVED_COUNTS_UNRECOVERABLE"));
}
#[test]
fn independent_pixel_info_identity_and_header_bindings_are_enforced() {
    for mode in 0..5 {
        let input = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(input.path(), true, false);
        match mode {
            0 => {
                fs::write(input.path().join("color.raw"), [9, 9, 9, 9]).unwrap();
            }
            1 => {
                record.depth.header_timestamp_ns = Some(1_000_000_001);
            }
            2 => {
                record.depth.frame_id = "other".into();
            }
            3 => {
                let mut value: serde_json::Value = serde_json::from_slice(
                    &fs::read(input.path().join("depth-info.json")).unwrap(),
                )
                .unwrap();
                value["k"][0] = serde_json::json!(2.);
                record.depth.camera_info = Some(reference(
                    input.path(),
                    "wrong-info.json",
                    &serde_json::to_vec(&value).unwrap(),
                ));
            }
            _ => {
                let mut evidence: TaggedEvidence =
                    serde_json::from_slice(&fs::read(input.path().join("tagged.json")).unwrap())
                        .unwrap();
                if let TaggedEvidence::ObservedAssociation { association, .. } = &mut evidence {
                    association.depth.context.sequence = Some(4);
                }
                record.evidence = reference(
                    input.path(),
                    "wrong-tagged.json",
                    &serde_json::to_vec(&evidence).unwrap(),
                );
            }
        }
        index(input.path(), &record);
        assert!(
            !cli(input.path(), &["--recording-v2", "--validate-only"])
                .status
                .success(),
            "mode {mode}"
        );
    }
}
#[test]
fn truncated_or_digest_mismatched_index_never_marks_job_complete() {
    let input = tempfile::tempdir().unwrap();
    fixture_v2(input.path(), true, false);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(input.path().join("records.jsonl"))
        .unwrap();
    use std::io::Write;
    file.write_all(b"\n").unwrap();
    let output = tempfile::tempdir().unwrap();
    let result = cli(
        input.path(),
        &[
            "--recording-v2",
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "bad",
        ],
    );
    assert!(!result.status.success());
    assert!(!output.path().join("bad/COMPLETE").exists());
    assert!(output.path().join("bad.partial").exists());
}

#[test]
fn native_writer_commits_original_counts_and_drop_records_interruption() {
    let fixture_dir = tempfile::tempdir().unwrap();
    fixture(fixture_dir.path(), true, 0.);
    let snapshot: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("snapshot.json")).unwrap())
            .unwrap();
    let pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(fixture_dir.path().join("depth.raw16")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut writer = RawRecordingWriter::begin(output.path(), "native", snapshot.clone()).unwrap();
    writer
        .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    let path = writer.finish().unwrap();
    assert!(path.join("COMPLETE").exists());
    assert!(
        cli(
            &path,
            &["--recording-v2", "--validate-only", "--preserve-counts"]
        )
        .status
        .success()
    );
    let mut writer = RawRecordingWriter::begin(output.path(), "interrupted", snapshot).unwrap();
    assert!(
        writer
            .append(&pair, &[1], &pixels, StorageEndian::Little)
            .is_err()
    );
    drop(writer);
    assert!(!output.path().join("interrupted").exists());
    assert!(
        output
            .path()
            .join("interrupted.partial/INTERRUPTED.json")
            .exists()
    );
}

#[test]
fn native_writer_cannot_replace_racing_target_and_enforces_actual_byte_budget() {
    let fixture_dir = tempfile::tempdir().unwrap();
    fixture(fixture_dir.path(), true, 0.);
    let snapshot: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("snapshot.json")).unwrap())
            .unwrap();
    let pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(fixture_dir.path().join("depth.raw16")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut writer = RawRecordingWriter::begin(output.path(), "race", snapshot.clone()).unwrap();
    writer
        .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    fs::create_dir(output.path().join("race")).unwrap();
    assert!(writer.finish().is_err());
    assert!(output.path().join("race").is_dir());
    assert_eq!(fs::read_dir(output.path().join("race")).unwrap().count(), 0);
    assert!(!output.path().join("race.partial/COMPLETE").exists());
    let mut good = RawRecordingWriter::begin(output.path(), "good", snapshot.clone()).unwrap();
    good.append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    let path = good.finish().unwrap();
    let total: u64 = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    let mut limited =
        RawRecordingWriter::begin_with_limit(output.path(), "limit", snapshot, total - 1).unwrap();
    limited
        .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    assert!(limited.finish().is_err());
    assert!(!output.path().join("limit/COMPLETE").exists());
    let used: u64 = fs::read_dir(output.path().join("limit.partial"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(used < total);
}

#[test]
fn final_commit_gate_can_refuse_after_disk_validation_without_complete() {
    let fixture_dir = tempfile::tempdir().unwrap();
    fixture(fixture_dir.path(), true, 0.);
    let snapshot: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("snapshot.json")).unwrap())
            .unwrap();
    let pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(fixture_dir.path().join("depth.raw16")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut writer = RawRecordingWriter::begin(output.path(), "aborted", snapshot).unwrap();
    writer
        .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    let called = std::cell::Cell::new(false);
    let result = writer.finish_guarded(|| -> offline_depth::Result<()> {
        called.set(true);
        Err(offline_depth::OfflineError::Invalid(
            "APPLICATION_RECORDING_ABORTED",
        ))
    });
    assert!(called.get());
    assert!(result.is_err());
    assert!(!output.path().join("aborted").exists());
    assert!(!output.path().join("aborted.partial/COMPLETE").exists());
    assert!(
        output
            .path()
            .join("aborted.partial/INTERRUPTED.json")
            .exists()
    );
}

#[test]
fn native_recording_persists_and_exports_independently_verified_ros_snapshot_link() {
    let fixture_dir = tempfile::tempdir().unwrap();
    fixture(fixture_dir.path(), true, 0.);
    let native: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("snapshot.json")).unwrap())
            .unwrap();
    let mut ros_content = native.content.clone();
    ros_content.depth = DepthRepresentation::Millimetres {
        quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
    };
    let ros = ros_content.seal().unwrap();
    let pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(fixture_dir.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(fixture_dir.path().join("depth.raw16")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut writer =
        RawRecordingWriter::begin_with_limit(output.path(), "linked", native.clone(), 100000)
            .unwrap();
    writer.attach_ros_snapshot(&ros).unwrap();
    writer
        .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    let path = writer.finish().unwrap();
    let link: SnapshotAssociation =
        serde_json::from_slice(&fs::read(path.join("snapshot-association.json")).unwrap()).unwrap();
    assert_eq!(link.native_snapshot_id, native.snapshot_id);
    assert_eq!(link.ros_snapshot_id, ros.snapshot_id);
    let export = tempfile::tempdir().unwrap();
    assert!(
        cli(
            &path,
            &[
                "--recording-v2",
                "--preserve-counts",
                "--output",
                export.path().to_str().unwrap(),
                "--job",
                "export"
            ]
        )
        .status
        .success()
    );
    let copy = export
        .path()
        .join("export/source-blobs")
        .join(sha256(&ros.to_bytes().unwrap()));
    assert_eq!(fs::read(copy).unwrap(), ros.to_bytes().unwrap());
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("snapshot-association.json")).unwrap()).unwrap();
    value["ros_snapshot_id"] = serde_json::json!("f".repeat(64));
    fs::write(
        path.join("snapshot-association.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(RecordingDirectorySource::open(&path).is_err());
    let mut limited = RawRecordingWriter::begin_with_limit(
        output.path(),
        "link-budget",
        native.clone(),
        native.to_bytes().unwrap().len() as u64 + 10,
    )
    .unwrap();
    assert!(limited.attach_ros_snapshot(&ros).is_err());
    assert!(
        limited
            .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
            .is_err()
    );
    drop(limited);
    let mut late = RawRecordingWriter::begin(output.path(), "late-link", native).unwrap();
    late.append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
        .unwrap();
    assert!(late.attach_ros_snapshot(&ros).is_err());
    assert!(late.finish().is_err());
}

#[test]
fn both_v2_provenances_export_d2c_diagnostics_without_upgrading_identity() {
    for observed in [false, true] {
        let input = tempfile::tempdir().unwrap();
        fixture_v2(input.path(), observed, false);
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--recording-v2",
                "--d2c",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "d2c",
            ],
        );
        assert!(result.status.success(), "{result:?}");
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["version"], 2);
        assert_eq!(report["d2c_diagnostics"]["valid_output"], 1);
        let stats: serde_json::Value = serde_json::from_slice(
            &fs::read(
                output
                    .path()
                    .join("d2c/d2c-diagnostics-00000000000000000000.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(stats["source_context"]["sequence"].is_null(), observed);
        assert_eq!(stats["counters"]["source_pixels"], 2);
    }
}

fn tagged(root: &Path, record: &RecordingRecord) -> TaggedEvidence {
    serde_json::from_slice(&fs::read(root.join(&record.evidence.path)).unwrap()).unwrap()
}
fn store_tagged(root: &Path, record: &mut RecordingRecord, evidence: &TaggedEvidence) {
    record.evidence = reference(
        root,
        "changed-tagged.json",
        &serde_json::to_vec(evidence).unwrap(),
    );
    index(root, record);
}
#[test]
fn observed_publisher_metadata_is_independently_bound_and_never_upgraded_to_sdk() {
    for invalid in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(dir.path(), true, false);
        let mut evidence = tagged(dir.path(), &record);
        if let TaggedEvidence::ObservedAssociation { association, .. } = &mut evidence {
            for (stored, frame) in [
                (&mut record.color, &mut association.color),
                (&mut record.depth, &mut association.depth),
            ] {
                frame.metadata_provenance = MetadataProvenance::PublisherReported;
                frame.context.sequence = Some(17);
                frame.context.device_timestamp_us = Some(0);
                frame.context.system_timestamp_us = Some(1_000_000);
                frame.reported_source_session = Some(SessionId::parse("e".repeat(32)).unwrap());
                let value = serde_json::json!({"version":1,"frame_id":stored.frame_id,"stamp_sec":1,"stamp_nanosec":0,"sequence":if invalid{18}else{17},"device_timestamp_us":0,"system_timestamp_us":1_000_000,"session_id":"e".repeat(32)});
                stored.metadata = Some(reference(
                    dir.path(),
                    &format!("{}-metadata.json", stored.frame_id),
                    &serde_json::to_vec(&value).unwrap(),
                ));
            }
        }
        store_tagged(dir.path(), &mut record, &evidence);
        let mut source = RecordingDirectorySource::open(dir.path()).unwrap();
        if invalid {
            assert_eq!(
                source
                    .next_frame()
                    .err()
                    .expect("must reject invalid recording")
                    .to_string(),
                "OBSERVED_METADATA_BINDING"
            );
        } else {
            let frame = source.next_frame().unwrap().unwrap();
            assert!(matches!(
                frame.evidence,
                TaggedEvidence::ObservedAssociation { .. }
            ));
            assert!(source.next_frame().unwrap().is_none());
            assert!(source.next_frame().unwrap().is_none());
            let output = tempfile::tempdir().unwrap();
            let mut fresh = RecordingDirectorySource::open(dir.path()).unwrap();
            process(
                &mut fresh,
                Some((output.path(), "metadata", depth_output::Mode::Directory)),
                offline_depth::Operation::Original {
                    normalize_mm: false,
                },
            )
            .unwrap();
            let file = record.color.metadata.as_ref().unwrap();
            assert_eq!(
                fs::read(
                    output
                        .path()
                        .join("metadata/source-blobs")
                        .join(&file.sha256)
                )
                .unwrap(),
                fs::read(dir.path().join(&file.path)).unwrap()
            );
        }
    }
}
#[test]
fn sdk_original_json_and_ros_wire_headers_must_match_the_recorded_pair_exactly() {
    for invalid in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(dir.path(), false, false);
        let mut evidence = tagged(dir.path(), &record);
        if let TaggedEvidence::SdkBoundPair { pair, source_json } = &mut evidence {
            let mut original = pair.clone();
            if invalid {
                original.pair_sequence += 1;
            }
            *source_json = Some(reference(
                dir.path(),
                "original.json",
                &serde_json::to_vec(&original).unwrap(),
            ));
            for (stored, frame) in [
                (&mut record.color, &pair.color),
                (&mut record.depth, &pair.depth),
            ] {
                stored.topic = Some(format!("/synthetic/{}", stored.frame_id));
                stored.record_timestamp_ns = Some(1);
                stored.header_timestamp_ns = frame.context.system_timestamp_us.map(|n| n * 1000);
            }
        }
        store_tagged(dir.path(), &mut record, &evidence);
        let mut source = RecordingDirectorySource::open(dir.path()).unwrap();
        if invalid {
            assert_eq!(
                source
                    .next_frame()
                    .err()
                    .expect("must reject invalid recording")
                    .to_string(),
                "SDK_ORIGINAL_JSON_BINDING"
            );
        } else {
            assert!(source.next_frame().unwrap().is_some());
            assert!(source.next_frame().unwrap().is_none());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut record = fixture_v2(dir.path(), false, false);
    record.color.topic = Some("/color".into());
    record.color.record_timestamp_ns = Some(1);
    record.color.header_timestamp_ns = Some(1);
    index(dir.path(), &record);
    assert_eq!(
        RecordingDirectorySource::open(dir.path())
            .unwrap()
            .next_frame()
            .err()
            .expect("must reject invalid recording")
            .to_string(),
        "SDK_WIRE_HEADER_BINDING"
    );
}
#[test]
fn recording_manifest_payload_and_encoding_limits_are_not_silent_truncation() {
    for mode in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(dir.path(), true, false);
        match mode {
            0 => record.color.encoding = "unsupported".into(),
            1 => record.color.stride = 1,
            2 => record.color.file.sha256 = "G".repeat(64),
            3 => record.color.record_timestamp_ns = None,
            4 => record.color.camera_info = None,
            5 => record.color.stride = usize::MAX,
            6 => record.color.file = reference(dir.path(), "short.raw", &[0]),
            _ => record.color.topic = Some("relative".into()),
        }
        index(dir.path(), &record);
        assert!(
            RecordingDirectorySource::open(dir.path())
                .unwrap()
                .next_frame()
                .is_err(),
            "{mode}"
        );
    }
    for field in ["version", "record_count", "records_sha256"] {
        let dir = tempfile::tempdir().unwrap();
        fixture_v2(dir.path(), false, false);
        let path = dir.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest[field] = if field == "records_sha256" {
            serde_json::json!("bad")
        } else {
            serde_json::json!(0)
        };
        fs::write(path, manifest.to_string()).unwrap();
        assert!(RecordingDirectorySource::open(dir.path()).is_err());
    }
}
#[test]
fn observed_rgb_and_bgr_pixels_preserve_native_depth_samples() {
    for encoding in ["rgb8", "bgr8"] {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(dir.path(), true, false);
        record.color.encoding = encoding.into();
        record.color.stride = 6;
        record.color.file = reference(dir.path(), "color-rgb.raw", &[1, 2, 3, 4, 5, 6]);
        index(dir.path(), &record);
        assert!(
            RecordingDirectorySource::open(dir.path())
                .unwrap()
                .next_frame()
                .is_ok()
        );
    }
}
#[test]
fn a_second_record_must_advance_source_sequence_and_both_observed_clocks() {
    let dir = tempfile::tempdir().unwrap();
    let record = fixture_v2(dir.path(), true, false);
    let line = serde_json::to_string(&record).unwrap();
    let bytes = format!("{line}\n{line}\n");
    fs::write(dir.path().join("records.jsonl"), &bytes).unwrap();
    let path = dir.path().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["record_count"] = serde_json::json!(2);
    manifest["records_sha256"] = serde_json::json!(sha256(bytes.as_bytes()));
    fs::write(path, manifest.to_string()).unwrap();
    let mut source = RecordingDirectorySource::open(dir.path()).unwrap();
    assert!(source.next_frame().unwrap().is_some());
    assert_eq!(
        source
            .next_frame()
            .err()
            .expect("must reject invalid recording")
            .to_string(),
        "SOURCE_PROGRESSION"
    );
}

#[test]
fn sdk_raw_counts_require_the_snapshot_source_scale_and_export_original_evidence() {
    for valid in [true, false] {
        let input = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(input.path(), false, true);
        let mut evidence = tagged(input.path(), &record);
        let mut snapshot: snapshot::SnapshotEnvelope =
            serde_json::from_slice(&fs::read(input.path().join("snapshot.json")).unwrap()).unwrap();
        snapshot.content.depth = DepthRepresentation::Millimetres {
            quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
        };
        snapshot.content.configuration["source_depth_scale_mm"] =
            serde_json::json!(if valid { f64::from(0.1f32) } else { 1. });
        snapshot.content.binding.configuration_id = ConfigurationId::parse(
            content_digest(
                ContentKind::Configuration,
                &serde_json::to_vec(&snapshot.content.configuration).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let snapshot = snapshot.content.seal().unwrap();
        fs::write(
            input.path().join("snapshot.json"),
            snapshot.to_bytes().unwrap(),
        )
        .unwrap();
        if let TaggedEvidence::SdkBoundPair { pair, source_json } = &mut evidence {
            for frame in [&mut pair.color, &mut pair.depth] {
                frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
                frame.context.snapshot_id = snapshot.snapshot_id.clone();
            }
            *source_json = Some(reference(
                input.path(),
                "original.json",
                &serde_json::to_vec(&pair).unwrap(),
            ));
        }
        store_tagged(input.path(), &mut record, &evidence);
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--recording-v2",
                "--preserve-counts",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "raw",
            ],
        );
        assert_eq!(result.status.success(), valid, "{result:?}");
        if valid {
            assert_eq!(
                decoded(&output.path().join("raw/frames/00000000000000000000.png")),
                [10000, 0]
            );
            let blobs = output.path().join("raw/source-blobs");
            // The exact source JSON is retained alongside the tagged evidence.
            assert!(output.path().join("raw/COMPLETE").is_file());
            assert!(fs::read_dir(blobs).unwrap().count() >= 3);
        } else {
            assert!(String::from_utf8_lossy(&result.stdout).contains("SDK_RAW_SOURCE_BINDING"));
        }
    }
}

#[test]
fn incomplete_recordings_can_be_validated_but_never_exported_as_completed_jobs() {
    for marker in ["INTERRUPTED.json", "FAILED.json"] {
        let input = tempfile::tempdir().unwrap();
        fixture_v2(input.path(), false, false);
        fs::write(input.path().join(marker), b"{}").unwrap();
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--recording-v2",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "invalid",
            ],
        );
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("INCOMPLETE_RECORDING_INPUT"));
        assert!(!output.path().join("invalid").exists());
        assert!(
            cli(input.path(), &["--recording-v2", "--validate-only"])
                .status
                .success()
        );
    }
}

#[test]
fn observed_distortion_models_and_none_alias_keep_exact_camera_info_in_exports() {
    for (kind, name, count) in [
        (DistortionModel::None, "none", 0),
        (DistortionModel::PlumbBob, "plumb_bob", 5),
        (
            DistortionModel::RationalPolynomial,
            "rational_polynomial",
            8,
        ),
        (DistortionModel::Equidistant, "equidistant", 4),
    ] {
        let input = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(input.path(), true, false);
        let mut evidence = tagged(input.path(), &record);
        let mut snapshot: snapshot::SnapshotEnvelope =
            serde_json::from_slice(&fs::read(input.path().join("snapshot.json")).unwrap()).unwrap();
        for m in snapshot.content.calibration.models.values_mut() {
            m.distortion_model = kind;
            m.distortion_coefficients = vec![0.; count];
        }
        snapshot.content.binding.calibration_id = snapshot.content.calibration.id().unwrap();
        let snapshot = snapshot.content.seal().unwrap();
        fs::write(
            input.path().join("snapshot.json"),
            snapshot.to_bytes().unwrap(),
        )
        .unwrap();
        if let TaggedEvidence::ObservedAssociation { association, .. } = &mut evidence {
            for frame in [&mut association.color, &mut association.depth] {
                frame.context.calibration_id = snapshot.content.binding.calibration_id.clone();
                frame.context.snapshot_id = snapshot.snapshot_id.clone();
            }
        }
        for image in [&mut record.color, &mut record.depth] {
            let mut info: DecodedCameraInfo = serde_json::from_slice(
                &fs::read(input.path().join(&image.camera_info.as_ref().unwrap().path)).unwrap(),
            )
            .unwrap();
            info.distortion_model = name.into();
            info.d = vec![0.; count];
            image.camera_info = Some(reference(
                input.path(),
                &format!("{}-model.json", image.frame_id),
                &serde_json::to_vec(&info).unwrap(),
            ));
        }
        store_tagged(input.path(), &mut record, &evidence);
        let output = tempfile::tempdir().unwrap();
        let result = cli(
            input.path(),
            &[
                "--recording-v2",
                "--output",
                output.path().to_str().unwrap(),
                "--job",
                "models",
            ],
        );
        assert!(result.status.success(), "{result:?}");
        assert_eq!(
            decoded(&output.path().join("models/frames/00000000000000000000.png")),
            [1000, 0]
        );
    }
}
#[test]
fn sdk_and_observed_records_reject_independent_sequence_layout_scale_and_provenance_changes() {
    for mode in 0..5 {
        let input = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(input.path(), mode == 4, false);
        let mut evidence = tagged(input.path(), &record);
        if let TaggedEvidence::SdkBoundPair { pair, .. } = &mut evidence {
            match mode {
                0 => record.sequence = 2,
                1 => pair.color.width = 1,
                2 => {
                    record.storage_representation = DepthRepresentation::SdkCounts { scale_mm: 2. }
                }
                _ => {
                    pair.color.context.snapshot_id = SnapshotId::parse("f".repeat(64)).unwrap();
                    pair.depth.context.snapshot_id = pair.color.context.snapshot_id.clone();
                }
            }
        }
        if let TaggedEvidence::ObservedAssociation { association, .. } = &mut evidence {
            association.version = 2;
        }
        store_tagged(input.path(), &mut record, &evidence);
        let mut source = RecordingDirectorySource::open(input.path()).unwrap();
        assert!(source.next_frame().is_err(), "{mode}");
    }
}
#[test]
fn native_writer_rejects_unsafe_names_normalized_pixels_and_unbound_pair_contexts() {
    let input = tempfile::tempdir().unwrap();
    fixture_v2(input.path(), false, true);
    let snapshot: snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(input.path().join("snapshot.json")).unwrap()).unwrap();
    assert!(RawRecordingWriter::begin(input.path(), "../escape", snapshot.clone()).is_err());
    let record = fixture_v2(input.path(), false, true);
    let original = tagged(input.path(), &record);
    let TaggedEvidence::SdkBoundPair { pair, .. } = original else {
        panic!("SDK fixture")
    };
    for mode in 0..2 {
        let mut pair = pair.clone();
        if mode == 0 {
            pair.depth.normalized = true;
            pair.depth.output_depth_scale_mm = Some(1.);
        } else {
            pair.color.context.snapshot_id = SnapshotId::parse("f".repeat(64)).unwrap();
            pair.depth.context.snapshot_id = pair.color.context.snapshot_id.clone();
        }
        let output = tempfile::tempdir().unwrap();
        let mut writer = RawRecordingWriter::begin(output.path(), "bad", snapshot.clone()).unwrap();
        assert!(
            writer
                .append(&pair, &[0; 4], &[0; 4], StorageEndian::Little)
                .is_err()
        );
        assert!(writer.finish().is_err());
        assert!(!output.path().join("bad/COMPLETE").exists());
    }
}

#[test]
fn malformed_snapshot_links_camera_info_and_extra_index_records_are_rejected() {
    for mode in 0..4 {
        let input = tempfile::tempdir().unwrap();
        let mut record = fixture_v2(input.path(), mode == 2, false);
        if mode == 0 {
            fs::write(input.path().join("snapshot.json"), b"{}").unwrap();
            assert!(RecordingDirectorySource::open(input.path()).is_err());
            continue;
        }
        if mode == 1 {
            fs::write(input.path().join("snapshot-association.json"), b"{}").unwrap();
            assert!(RecordingDirectorySource::open(input.path()).is_err());
            continue;
        }
        if mode == 2 {
            record.color.camera_info = Some(reference(input.path(), "invalid-info.json", b"{}"));
            index(input.path(), &record);
            let mut source = RecordingDirectorySource::open(input.path()).unwrap();
            assert!(source.next_frame().is_err());
            continue;
        }
        let bytes = serde_json::to_vec(&record).unwrap();
        let mut twice = bytes.clone();
        twice.push(b'\n');
        twice.extend_from_slice(&bytes);
        fs::write(input.path().join("records.jsonl"), &twice).unwrap();
        let mut manifest: RecordingManifest =
            serde_json::from_slice(&fs::read(input.path().join("manifest.json")).unwrap()).unwrap();
        manifest.records_sha256 = sha256(&twice);
        fs::write(
            input.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let mut source = RecordingDirectorySource::open(input.path()).unwrap();
        source.next_frame().unwrap();
        assert!(source.next_frame().is_err());
    }
}

#[test]
fn recording_cli_selection_output_conflicts_and_relative_output_are_rejected() {
    let input = tempfile::tempdir().unwrap();
    fixture_v2(input.path(), false, false);
    for (args, code) in [
        (
            vec!["--recording-v2", "--validate-only", "--pair", "1"],
            "V2_PAIR_SELECTION_UNSUPPORTED",
        ),
        (
            vec!["--recording-v2", "--validate-only", "--job", "unexpected"],
            "VALIDATE_OUTPUT_ARGUMENT",
        ),
        (
            vec!["--recording-v2", "--output", "relative", "--job", "export"],
            "ABSOLUTE_OUTPUT_REQUIRED",
        ),
    ] {
        let result = cli(input.path(), &args);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stdout).contains(code),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}

#[cfg(target_os = "linux")]
#[path = "../../../tests/support/io_faults.rs"]
mod io_faults;
#[cfg(target_os = "linux")]
#[test]
fn recording_reads_and_export_sidecar_failures_preserve_incomplete_output() {
    let temp = tempfile::tempdir().unwrap();
    let library = io_faults::compile(temp.path());
    for (i, (op, suffix, observed)) in [
        ("read", "/manifest.json", false),
        ("mkdir", "/source-blobs", false),
        ("read", "/color-info.json", true),
        (
            "write",
            "/recording-record-00000000000000000001.json",
            false,
        ),
        ("write", "/pair-evidence-00000000000000000001.json", false),
    ]
    .into_iter()
    .enumerate()
    {
        let input = tempfile::tempdir().unwrap();
        let record = fixture_v2(input.path(), observed, false);
        let output = temp.path().join(format!("export-{i}"));
        fs::create_dir(&output).unwrap();
        let marker = temp.path().join(format!("injected-{i}"));
        let suffix = if observed {
            record.color.camera_info.as_ref().unwrap().path.as_str()
        } else {
            suffix
        };
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_offline-depth"));
        command
            .arg("--input")
            .arg(input.path())
            .args(["--recording-v2", "--output"])
            .arg(&output)
            .args(["--job", "failed", "--mode", "png"]);
        io_faults::inject(&mut command, &library, op, suffix, &marker);
        let result = command.output().unwrap();
        assert!(
            !result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        io_faults::assert_injected(&marker, op);
        assert!(!output.join("failed/COMPLETE").exists());
    }
}

#[test]
fn valid_evidence_with_excessive_skew_or_unsupported_storage_mapping_is_refused() {
    for mode in 0..4 {
        let input = tempfile::tempdir().unwrap();
        let observed = mode >= 2;
        let mut record = fixture_v2(input.path(), observed, mode == 1);
        let mut evidence = tagged(input.path(), &record);
        match &mut evidence {
            TaggedEvidence::SdkBoundPair { pair, .. } if mode == 0 => {
                pair.depth_stamp_us += 30_000;
                pair.depth.context.device_timestamp_us = Some(pair.depth_stamp_us);
                pair.delta_us = pair.color_stamp_us.abs_diff(pair.depth_stamp_us);
            }
            TaggedEvidence::SdkBoundPair { pair, .. } => {
                pair.depth.output_depth_scale_mm = Some(1.);
                pair.depth.source_depth_scale_mm = Some(1.);
                pair.depth.normalized = false;
                record.storage_representation = DepthRepresentation::Millimetres {
                    quantization:
                        QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
                };
            }
            TaggedEvidence::ObservedAssociation { header_mapping, .. } => {
                *header_mapping = if mode == 2 {
                    quality_core::ClockMapper::Unknown
                } else {
                    quality_core::ClockMapper::MeasuredObservedHeaderOffset { offset_ns: 100 }
                };
            }
        }
        store_tagged(input.path(), &mut record, &evidence);
        let mut source = RecordingDirectorySource::open(input.path()).unwrap();
        assert!(source.next_frame().is_err(), "mode {mode}");
    }
}

#[test]
fn streaming_and_materialized_v2_validation_agree_for_sdk_and_observed_inputs() {
    use std::io::Write;
    for observed in [false, true] {
        for fault in [
            "none",
            "color-digest",
            "depth-digest",
            "depth-short",
            "layout",
            "sequence",
            "index",
            "info",
        ] {
            let input = tempfile::tempdir().unwrap();
            let mut record = fixture_v2(input.path(), observed, false);
            match fault {
                "color-digest" => {
                    fs::write(input.path().join(&record.color.file.path), b"bad!").unwrap();
                }
                "depth-digest" => {
                    fs::write(input.path().join(&record.depth.file.path), b"bad!").unwrap();
                }
                "depth-short" => {
                    fs::write(input.path().join(&record.depth.file.path), [0u8]).unwrap();
                }
                "layout" => {
                    record.depth.stride += 2;
                    index(input.path(), &record);
                }
                "sequence" => {
                    record.sequence += 1;
                    index(input.path(), &record);
                }
                "index" => {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(input.path().join("records.jsonl"))
                        .unwrap()
                        .write_all(b" ")
                        .unwrap();
                }
                "info" if observed => {
                    fs::write(
                        input
                            .path()
                            .join(&record.depth.camera_info.as_ref().unwrap().path),
                        b"{}",
                    )
                    .unwrap();
                }
                _ => {}
            }
            let validate = |materialize: bool| -> offline_depth::Result<usize> {
                let mut source = RecordingDirectorySource::open(input.path())?;
                let mut count = 0;
                if materialize {
                    while source.next_frame()?.is_some() {
                        count += 1;
                    }
                } else {
                    while source.validate_next()? {
                        count += 1;
                    }
                    assert!(!source.validate_next()?);
                }
                Ok(count)
            };
            let expected = validate(true).map_err(|e| e.code());
            let actual = validate(false).map_err(|e| e.code());
            assert_eq!(actual, expected, "observed={observed}, fault={fault}");
        }
    }
}
