//! Public rejection diagnostics retain the failed boundary and never repair input.
use frame_core::*;
use std::{error::Error, time::Instant};
fn packet(depth: bool, sequence: u64, timestamp: u64) -> FramePacket<Vec<u8>> {
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
            sequence,
            device_timestamp_us: Some(timestamp),
            system_timestamp_us: Some(timestamp),
            source_depth_scale_mm: depth.then_some(1.),
            output_depth_scale_mm: depth.then_some(1.),
            normalized: false,
            received_at: Instant::now(),
        },
        payload: vec![0; 4],
    }
}
#[test]
fn malformed_packet_and_normalization_diagnostics_identify_the_invalid_input() {
    for (mode, needle) in [
        (0, "dimensions"),
        (1, "even"),
        (2, "stride"),
        (3, "payload"),
        (4, "addressable"),
    ] {
        let mut layout = packet(true, 1, 1).metadata.layout;
        let length = match mode {
            0 => {
                layout.width = 0;
                4
            }
            1 => {
                layout.format = PixelFormat::Yuyv;
                layout.width = 1;
                4
            }
            2 => {
                layout.stride = 3;
                4
            }
            3 => 3,
            _ => {
                layout.stride = usize::MAX - 1;
                layout.height = 2;
                4
            }
        };
        let err = layout.validate(length).unwrap_err();
        assert!(err.to_string().contains(needle));
        let err = normalize_depth_mm(&[0; 4][..length.min(4)], layout, false, 1.).unwrap_err();
        assert!(!err.to_string().is_empty());
        if let Some(source) = err.source() {
            assert!(source.to_string().contains(needle));
        }
    }
    for (mode, needle) in [(0, "encoding"), (1, "scales"), (2, "normalization")] {
        let mut m = packet(true, 1, 1).metadata;
        match mode {
            0 => m.encoding = FrameEncoding::Yuyv,
            1 => m.output_depth_scale_mm = None,
            _ => m.source_depth_scale_mm = Some(0.1),
        }
        assert!(m.validate(4).unwrap_err().to_string().contains(needle));
    }
    for scale in [0., f32::NAN, 2.] {
        let err = normalize_depth_mm(&[255; 4], packet(true, 1, 1).metadata.layout, false, scale)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(if scale == 2. { "range" } else { "scale" })
        );
        assert!(err.source().is_none());
    }
    assert!(
        LatestQueue::<u8>::new(0)
            .unwrap_err()
            .to_string()
            .contains("capacity")
    );
}
#[test]
fn invalid_clock_diagnostics_do_not_advance_the_publisher() {
    for (value, needle) in [
        (None, "missing"),
        (Some(0), "zero"),
        (Some(u64::MAX), "i32"),
    ] {
        assert!(
            sdk_system_unix_stamp(value)
                .unwrap_err()
                .to_string()
                .contains(needle)
        );
    }
    assert!(
        UnixStamp {
            sec: 1,
            nanosec: 1_000_000_000
        }
        .validate()
        .unwrap_err()
        .to_string()
        .contains("nanoseconds")
    );
    let mut guard = StampGuard::default();
    let initial = UnixStamp { sec: 2, nanosec: 0 };
    guard.accept(initial).unwrap();
    assert!(
        guard
            .check(initial)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
    assert!(
        guard
            .check(UnixStamp { sec: 1, nanosec: 0 })
            .unwrap_err()
            .to_string()
            .contains("regressed")
    );
    assert_eq!(guard.last(), Some(initial));
}
#[test]
fn pair_diagnostics_preserve_stream_identity_and_failed_candidates_do_not_advance_state() {
    for mode in 0..17 {
        let mut guard = PairGuard::default();
        let mut c = Some(packet(false, 2, 200));
        let mut d = Some(packet(true, 2, 200));
        let mut sequence = 2;
        let mut policy = PairPolicy {
            clock: PairClock::Device,
            max_delta_us: 10,
        };
        let mut provenance = PairProvenance::CallerObservedSdkFrameset;
        if mode >= 10 {
            guard
                .accept(
                    1,
                    Some(packet(false, 1, 100)),
                    Some(packet(true, 1, 100)),
                    (4, 4),
                    policy,
                    provenance,
                )
                .unwrap();
        }
        let needle = match mode {
            0 => {
                policy.max_delta_us = 0;
                "budget"
            }
            1 => {
                c = None;
                "missing color"
            }
            2 => {
                d = None;
                "missing depth"
            }
            3 => {
                provenance = PairProvenance::Unverified;
                "provenance"
            }
            4 => {
                c.as_mut().unwrap().metadata.layout.width = 0;
                "color metadata"
            }
            5 => {
                d.as_mut().unwrap().metadata.output_depth_scale_mm = None;
                "depth metadata"
            }
            6 => {
                c = Some(packet(true, 2, 200));
                "YUYV color"
            }
            7 => {
                let m = &mut d.as_mut().unwrap().metadata;
                m.source_depth_scale_mm = Some(0.1);
                m.output_depth_scale_mm = Some(0.1);
                "millimetres"
            }
            8 => {
                c.as_mut().unwrap().metadata.device_timestamp_us = None;
                "missing"
            }
            9 => {
                c.as_mut().unwrap().metadata.device_timestamp_us = Some(0);
                "zero"
            }
            10 => {
                sequence = 1;
                "pair sequence"
            }
            11 => {
                c.as_mut().unwrap().metadata.sequence = 1;
                "color sequence"
            }
            12 => {
                d.as_mut().unwrap().metadata.sequence = 1;
                "depth sequence"
            }
            13 => {
                c.as_mut().unwrap().metadata.device_timestamp_us = Some(100);
                d.as_mut().unwrap().metadata.device_timestamp_us = Some(100);
                "color timestamp"
            }
            14 => {
                c.as_mut().unwrap().metadata.device_timestamp_us = Some(101);
                d.as_mut().unwrap().metadata.device_timestamp_us = Some(100);
                "depth timestamp"
            }
            15 => {
                policy.clock = PairClock::System;
                "budget"
            }
            _ => {
                d.as_mut().unwrap().metadata.device_timestamp_us = Some(1000);
                "difference"
            }
        };
        let before = guard.last();
        let error = guard
            .accept(sequence, c, d, (4, 4), policy, provenance)
            .unwrap_err();
        assert!(error.to_string().contains(needle), "{mode}: {error}");
        assert_eq!(guard.last(), before);
    }
}

#[test]
fn frame_span_and_depth_unit_policy_changes_are_checked_before_pair_state_advances() {
    let layout = FrameLayout {
        width: 1,
        height: 1,
        stride: isize::MAX as usize + 1,
        format: PixelFormat::Depth16,
    };
    assert_eq!(
        layout.validate(isize::MAX as usize + 1),
        Err(LayoutError::ArithmeticOverflow)
    );
    assert!(measure_depth_samples(&[0; 4], packet(false, 1, 100).metadata.layout, false).is_err());
    let mut guard = PairGuard::default();
    let policy = PairPolicy {
        clock: PairClock::Device,
        max_delta_us: 10,
    };
    guard
        .accept_with_policy(
            1,
            Some(packet(false, 1, 100)),
            Some(packet(true, 1, 100)),
            (4, 4),
            PairAcceptance {
                pairing: policy,
                provenance: PairProvenance::CallerObservedSdkFrameset,
                depth_unit: DepthUnitPolicy::PreserveSdkCounts,
            },
        )
        .unwrap();
    let before = guard.last();
    assert!(matches!(
        guard.accept(
            2,
            Some(packet(false, 2, 200)),
            Some(packet(true, 2, 200)),
            (4, 4),
            policy,
            PairProvenance::CallerObservedSdkFrameset
        ),
        Err(PairError::InvalidPolicy)
    ));
    assert_eq!(guard.last(), before);
}
