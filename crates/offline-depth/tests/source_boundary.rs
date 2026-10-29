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
    let counts = source(true);
    let mut guard = BoundPairGuard::new(counts.snapshot, origin).unwrap();
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
