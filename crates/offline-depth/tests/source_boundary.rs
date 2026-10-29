mod support;
use capture_contract::{DepthRepresentation, snapshot::SnapshotEnvelope};
use offline_depth::*;
struct Once {
    snapshot: SnapshotEnvelope,
    frame: Option<SourceFrame>,
}
impl FrameSource for Once {
    fn snapshot(&self) -> &SnapshotEnvelope {
        &self.snapshot
    }
    fn next_frame(&mut self) -> Result<Option<SourceFrame>> {
        Ok(self.frame.take())
    }
}
fn source(counts: bool) -> Once {
    let dir = tempfile::tempdir().unwrap();
    support::fixture(dir.path(), counts, 0.);
    let mut source = DirectoryFrameSource::open(dir.path(), None).unwrap();
    Once {
        snapshot: source.snapshot().clone(),
        frame: source.next_frame().unwrap(),
    }
}
fn rewrite(s: &mut Once) {
    let frame = s.frame.as_mut().unwrap();
    frame.evidence_json = serde_json::to_vec(&frame.evidence).unwrap();
    frame.evidence_sha256 = sha256(&frame.evidence_json);
}
#[test]
fn consumers_recheck_custom_provider_evidence_even_without_a_directory_adapter() {
    for mode in 0..6 {
        let mut s = source(false);
        let frame = s.frame.as_mut().unwrap();
        let expected = match mode {
            0 => {
                frame.evidence_sha256 = "a".repeat(64);
                "EVIDENCE_DIGEST"
            }
            1 => {
                frame.evidence.pair_sequence += 1;
                "EVIDENCE_CONTENT_BINDING"
            }
            2 => {
                frame.evidence.color.context.stream_id = "other".into();
                "PAIR_STREAM_BINDING"
            }
            3 => {
                frame.evidence.color_stamp_us = 31000;
                frame.evidence.color.context.device_timestamp_us = Some(31000);
                frame.evidence.delta_us = 30000;
                "PAIR_SKEW"
            }
            4 => {
                frame.evidence.depth.source_depth_scale_mm = Some(0.1);
                "PAIR_EVIDENCE_INVALID"
            }
            _ => {
                s.frame = None;
                "SOURCE_EMPTY"
            }
        };
        if (2..5).contains(&mode) {
            rewrite(&mut s);
        }
        let error = validate(
            &mut s,
            Operation::Original {
                normalize_mm: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), expected);
    }
    let mut s = source(false);
    assert_eq!(
        validate(&mut s, Operation::PreserveCounts)
            .unwrap_err()
            .code(),
        "COUNTS_SOURCE_REQUIRED"
    );
}
#[test]
fn normalization_rejects_overflow_and_clamps_nonzero_submillimetre_counts() {
    for (scale, pixels, expected) in [(10.0f32, [65535u16, 0], None), (0.01, [1, 0], Some([1, 0]))]
    {
        let mut s = source(true);
        let mut content = s.snapshot.content;
        content.depth = DepthRepresentation::SdkCounts {
            scale_mm: f64::from(scale),
        };
        s.snapshot = content.seal().unwrap();
        let f = s.frame.as_mut().unwrap();
        f.evidence.color.context.snapshot_id = s.snapshot.snapshot_id.clone();
        f.evidence.depth.context.snapshot_id = s.snapshot.snapshot_id.clone();
        f.evidence.depth.source_depth_scale_mm = Some(scale);
        f.evidence.depth.output_depth_scale_mm = Some(scale);
        f.depth = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        rewrite(&mut s);
        let output = tempfile::tempdir().unwrap();
        let result = run(
            &mut s,
            output.path(),
            "converted",
            RunOptions {
                operation: Operation::Original { normalize_mm: true },
                mode: depth_output::Mode::Directory,
                queue_capacity: 1,
            },
        );
        if let Some(pixels) = expected {
            assert!(result.unwrap().complete);
            assert_eq!(
                support::decoded(
                    &output
                        .path()
                        .join("converted/frames/00000000000000000000.png")
                ),
                pixels
            );
        } else {
            assert_eq!(result.unwrap_err().code(), "NORMALIZATION_OVERFLOW");
            assert!(!output.path().join("converted/COMPLETE").exists());
        }
    }
}

#[test]
fn bound_pair_builder_rejects_receive_clock_geometry_and_unit_rebinding_before_commit() {
    use frame_core::*;
    use std::time::{Duration, Instant};
    fn packet(depth: bool, received: Instant) -> FramePacket<Vec<u8>> {
        FramePacket {
            metadata: PacketMetadata {
                layout: FrameLayout {
                    width: 2,
                    height: 1,
                    stride: 4,
                    format: if depth {
                        PixelFormat::Depth16
                    } else {
                        PixelFormat::Yuyv
                    },
                },
                encoding: if depth {
                    FrameEncoding::Depth16
                } else {
                    FrameEncoding::Yuyv
                },
                sequence: 1,
                device_timestamp_us: Some(100),
                system_timestamp_us: Some(200),
                global_timestamp_us: None,
                sdk_return_unix_ns: None,
                sdk_system_clock_verified: false,

                source_depth_scale_mm: depth.then_some(1.),
                output_depth_scale_mm: depth.then_some(1.),
                normalized: false,
                received_at: received,
            },
            payload: vec![0; 4],
        }
    }
    let base = source(false);
    let origin = Instant::now();
    let mut invalid = base.snapshot.clone();
    invalid.version = 2;
    assert!(
        BoundPairGuard::new(invalid, origin)
            .err()
            .unwrap()
            .to_string()
            .contains("Snapshot")
    );
    let mut guard = BoundPairGuard::new(base.snapshot.clone(), origin).unwrap();
    assert_eq!(guard.snapshot_id(), &base.snapshot.snapshot_id);
    for mode in 0..3 {
        let received = if mode == 0 {
            origin - Duration::from_nanos(1)
        } else {
            origin
        };
        let mut c = packet(false, received);
        let d = packet(true, received);
        if mode == 1 {
            c.metadata.layout.width = 1;
        }
        let error = guard
            .accept(
                if mode == 2 { 0 } else { 1 },
                Some(c),
                Some(d),
                (4, 4),
                PairPolicy {
                    clock: PairClock::System,
                    max_delta_us: 10,
                },
                PairProvenance::CallerObservedSdkFrameset,
            )
            .err()
            .unwrap();
        assert!(!error.to_string().is_empty());
    }
    let mut pair = guard
        .accept(
            1,
            Some(packet(false, origin)),
            Some(packet(true, origin)),
            (4, 4),
            PairPolicy {
                clock: PairClock::System,
                max_delta_us: 10,
            },
            PairProvenance::CallerObservedSdkFrameset,
        )
        .unwrap()
        .evidence;
    assert_eq!(pair.clock_source, "sdk_system_us");
    for mode in 0..4 {
        let mut changed = pair.clone();
        match mode {
            0 => changed.hardware_sync_verified = true,
            1 => changed.color.context.sequence = None,
            2 => changed.color.encoding = "rgb8".into(),
            _ => changed.depth.normalized = true,
        }
        if mode == 3 {
            changed.depth.output_depth_scale_mm = Some(0.1);
        }
        assert!(changed.validate().is_err());
    }
    pair.color.context.stream_id = "same".into();
    pair.depth.context.stream_id = "same".into();
    assert!(pair.validate().is_err());
    let mut changed = base.snapshot.content.clone();
    changed.configuration["source_depth_scale_mm"] = serde_json::json!(2.);
    changed.binding.configuration_id = capture_contract::ConfigurationId::parse(
        capture_contract::content_digest(
            capture_contract::ContentKind::Configuration,
            &serde_json::to_vec(&changed.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut changed_guard = BoundPairGuard::new(changed.seal().unwrap(), origin).unwrap();
    assert!(
        changed_guard
            .accept(
                1,
                Some(packet(false, origin)),
                Some(packet(true, origin)),
                (4, 4),
                PairPolicy {
                    clock: PairClock::Device,
                    max_delta_us: 10
                },
                PairProvenance::CallerObservedSdkFrameset
            )
            .is_err()
    );
    let counts = source(true);
    let mut guard = BoundPairGuard::new(counts.snapshot, origin).unwrap();
    let mut raw_depth = packet(true, origin);
    raw_depth.metadata.source_depth_scale_mm = Some(0.1);
    raw_depth.metadata.output_depth_scale_mm = Some(0.1);
    raw_depth.metadata.normalized = false;
    assert!(
        guard
            .accept(
                1,
                Some(packet(false, origin)),
                Some(raw_depth),
                (4, 4),
                PairPolicy {
                    clock: PairClock::Device,
                    max_delta_us: 10
                },
                PairProvenance::CallerObservedSdkFrameset
            )
            .is_ok()
    );

    assert!(
        guard
            .accept(
                1,
                Some(packet(false, origin)),
                Some(packet(true, origin)),
                (4, 4),
                PairPolicy {
                    clock: PairClock::Device,
                    max_delta_us: 10
                },
                PairProvenance::CallerObservedSdkFrameset
            )
            .is_err()
    );
}

#[test]
fn geometry_limits_reject_valid_contracts_before_allocating_large_ray_tables_or_output() {
    for dimension in [4000, 5000] {
        let mut s = source(false);
        let mut content = s.snapshot.content.clone();
        for model in content.calibration.models.values_mut() {
            model.width = dimension;
            model.height = dimension;
        }
        content.binding.calibration_id = content.calibration.id().unwrap();
        s.snapshot = content.seal().unwrap();
        s.frame = None;
        let output = tempfile::tempdir().unwrap();
        assert!(validate(&mut s, Operation::DepthToColor).is_err());
        assert!(
            run(
                &mut s,
                output.path(),
                "oversized",
                RunOptions {
                    operation: Operation::DepthToColor,
                    mode: depth_output::Mode::Directory,
                    queue_capacity: 1
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
    }
}

#[test]
fn long_tar_only_exports_advance_progress_and_preserve_each_independent_source_receipt() {
    struct Repeated {
        snapshot: SnapshotEnvelope,
        frame: SourceFrame,
        next: u64,
    }
    impl FrameSource for Repeated {
        fn snapshot(&self) -> &SnapshotEnvelope {
            &self.snapshot
        }
        fn next_frame(&mut self) -> Result<Option<SourceFrame>> {
            if self.next == 101 {
                return Ok(None);
            }
            let n = self.next;
            self.next += 1;
            let mut evidence = self.frame.evidence.clone();
            evidence.pair_sequence += n;
            evidence.color_stamp_us += n * 1000;
            evidence.depth_stamp_us += n * 1000;
            for frame in [&mut evidence.color, &mut evidence.depth] {
                frame.context.sequence = frame.context.sequence.map(|v| v + n);
                frame.context.device_timestamp_us =
                    frame.context.device_timestamp_us.map(|v| v + n * 1000);
                frame.context.system_timestamp_us =
                    frame.context.system_timestamp_us.map(|v| v + n * 1000);
                frame.context.received_mono_offset_ns += n * 1_000_000;
            }
            let bytes = serde_json::to_vec(&evidence).unwrap();
            Ok(Some(SourceFrame {
                evidence,
                evidence_sha256: sha256(&bytes),
                evidence_json: bytes,
                depth: self.frame.depth.clone(),
                endian: self.frame.endian,
            }))
        }
    }
    let once = source(false);
    let mut input = Repeated {
        snapshot: once.snapshot,
        frame: once.frame.unwrap(),
        next: 0,
    };
    let output = tempfile::tempdir().unwrap();
    let path = output.path().join("progress.jsonl");
    let control = job_control::JobControl::new(Some(&path)).unwrap();
    let report = control
        .execute(|| {
            run(
                &mut input,
                output.path(),
                "tar-only",
                RunOptions {
                    operation: Operation::Original {
                        normalize_mm: false,
                    },
                    mode: depth_output::Mode::Tar,
                    queue_capacity: 2,
                },
            )
        })
        .unwrap();
    assert_eq!(report.validated_pairs, 101);
    assert_eq!(report.written_frames, 101);
    assert!(output.path().join("tar-only/COMPLETE").is_file());
    assert!(!output.path().join("tar-only/frames").exists());
    let events = std::fs::read_to_string(path).unwrap();
    assert!(
        events
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .any(|v| v["state"] == "running" && v["processed_pairs"] == 100)
    );
}

#[test]
fn large_but_structurally_valid_depth_inputs_are_rejected_at_processing_resource_bounds() {
    let mut s = source(false);
    let mut content = s.snapshot.content.clone();
    let model = content.calibration.models.get_mut("depth").unwrap();
    model.width = 6000;
    model.height = 4000;
    content.binding.calibration_id = content.calibration.id().unwrap();
    s.snapshot = content.seal().unwrap();
    let frame = s.frame.as_mut().unwrap();
    frame.depth = vec![0; 6000 * 4000 * 2];
    frame.evidence.depth.width = 6000;
    frame.evidence.depth.height = 4000;
    frame.evidence.depth.stride = 12000;
    for frame in [&mut frame.evidence.color, &mut frame.evidence.depth] {
        frame.context.calibration_id = s.snapshot.content.binding.calibration_id.clone();
        frame.context.snapshot_id = s.snapshot.snapshot_id.clone();
    }
    rewrite(&mut s);
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = support::fixture(dir.path(), false, 0.);
    let frame = s.frame.as_ref().unwrap();
    manifest.binding = s.snapshot.content.binding.clone();
    manifest.pairs[0].evidence_sha256 = frame.evidence_sha256.clone();
    manifest.pairs[0].depth_sha256 = sha256(&frame.depth);
    std::fs::write(
        dir.path().join("snapshot.json"),
        s.snapshot.to_bytes().unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("pair.json"), &frame.evidence_json).unwrap();
    std::fs::write(dir.path().join("depth.raw16"), &frame.depth).unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut disk = DirectoryFrameSource::open(dir.path(), None).unwrap();
    assert!(disk.next_frame().is_err());
    let out = tempfile::tempdir().unwrap();
    assert!(
        run(
            &mut s,
            out.path(),
            "large",
            RunOptions {
                operation: Operation::Original {
                    normalize_mm: false
                },
                mode: depth_output::Mode::Directory,
                queue_capacity: 1
            }
        )
        .is_err()
    );
    assert!(!out.path().join("large/COMPLETE").exists());
}
