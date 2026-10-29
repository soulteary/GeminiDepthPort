mod common;
use capture_observer::*;
use common::*;
use serde_json::{Value, json};
#[test]
fn observer_configuration_rejects_unbounded_policies_and_invented_source_bindings() {
    for (pointer, value) in [
        ("/version", json!(2)),
        ("/association/queue_capacity", json!(0)),
        (
            "/association/metadata_tolerance_ns",
            json!(1_000_000_000_001u64),
        ),
        ("/association/pair_tolerance_ns", json!(100_000)),
        ("/streams/color/context/sequence", json!(1)),
        ("/streams/color/camera/width", json!(0)),
        ("/streams/color/camera_info_tolerance", json!(0)),
        ("/streams/depth/context/session_id", json!("b".repeat(32))),
    ] {
        let mut value_config = serde_json::to_value(config()).unwrap();
        *value_config.pointer_mut(pointer).unwrap() = value;
        let cfg: ObserverConfig = serde_json::from_value(value_config).unwrap();
        assert!(CaptureObserver::new(cfg).is_err(), "{pointer}");
    }
    for stamp in [
        HeaderStamp {
            sec: -1,
            nanosec: 0,
        },
        HeaderStamp { sec: 0, nanosec: 0 },
        HeaderStamp {
            sec: 1,
            nanosec: 1_000_000_000,
        },
        HeaderStamp {
            sec: i64::from(i32::MAX) + 1,
            nanosec: 0,
        },
    ] {
        assert!(stamp.to_ns().is_err());
    }
}
fn metadata(time: u64) -> DecodedMetadata {
    DecodedMetadata{stamp:stamp(time),data:json!({"version":1,"frame_id":"synthetic_color_optical","stamp_sec":time/BASE,"stamp_nanosec":time%BASE,"sequence":7,"device_timestamp_us":0,"system_timestamp_us":null,"session_id":"b".repeat(32)}).to_string()}
}
#[test]
fn malformed_unsupported_and_mismatched_metadata_never_become_frame_provenance() {
    for (pointer, replacement) in [
        ("/version", json!(2)),
        ("/frame_id", json!("other")),
        ("/stamp_sec", Value::Null),
        ("/stamp_nanosec", json!(u64::MAX)),
        ("/stamp_nanosec", json!(101)),
        ("/sequence", json!(-1)),
        ("/session_id", json!(false)),
        ("/session_id", json!("bad")),
    ] {
        let mut cfg = config();
        cfg.streams
            .get_mut(&StreamRole::Color)
            .unwrap()
            .metadata_required = true;
        let mut observer = CaptureObserver::new(cfg).unwrap();
        let mut input = metadata(BASE + 100);
        let mut value: Value = serde_json::from_str(&input.data).unwrap();
        *value.pointer_mut(pointer).unwrap() = replacement;
        input.data = value.to_string();
        let update = observer.metadata(StreamRole::Color, at(0), input).unwrap();
        assert_eq!(update.rejections.len(), 1);
        assert!(
            matches!(
                update.rejections[0].reason,
                RejectionReason::MetadataInvalid | RejectionReason::MetadataUnsupported
            ),
            "{pointer}"
        );
    }
    for data in ["invalid".to_string(), "x".repeat(4097)] {
        let mut observer = CaptureObserver::new(config()).unwrap();
        let update = observer
            .metadata(
                StreamRole::Color,
                at(0),
                DecodedMetadata {
                    stamp: stamp(BASE + 100),
                    data,
                },
            )
            .unwrap();
        assert_eq!(
            update.rejections[0].reason,
            RejectionReason::MetadataInvalid
        );
    }
}
#[test]
fn metadata_ambiguity_and_info_metadata_queue_overflow_are_explicit() {
    let mut observer = CaptureObserver::new(config()).unwrap();
    add(&mut observer, StreamRole::Color, 0, BASE + 100);
    for offset in [2, 3] {
        observer
            .metadata(StreamRole::Color, at(offset), metadata(BASE + 100))
            .unwrap();
    }
    let update = observer.advance_watermark(at(4), BASE + 200).unwrap();
    assert!(update.pairs.is_empty());
    assert_eq!(update.rejections.len(), 4);
    assert!(
        update
            .rejections
            .iter()
            .all(|r| r.reason == RejectionReason::MetadataAmbiguous)
    );
    for info_queue in [false, true] {
        let mut cfg = config();
        cfg.association.queue_capacity = 1;
        let mut observer = CaptureObserver::new(cfg).unwrap();
        for n in 0..2 {
            let update = if info_queue {
                observer
                    .camera_info(
                        StreamRole::Color,
                        at(n),
                        info(StreamRole::Color, BASE + 100 + n),
                    )
                    .unwrap()
            } else {
                observer
                    .metadata(StreamRole::Color, at(n), metadata(BASE + 100 + n))
                    .unwrap()
            };
            if n == 1 {
                assert_eq!(update.rejections[0].reason, RejectionReason::QueueOverflow);
            }
        }
        let closed = observer.finalize().unwrap();
        assert_eq!(closed.rejections.len(), 1);
        assert_eq!(
            closed.rejections[0].reason,
            RejectionReason::ObservationWindowClosed
        );
        assert!(observer.finalize().is_err());
    }
}
#[test]
fn invalid_and_late_camera_info_is_rejected_without_silent_reuse() {
    for kind in 0..6 {
        let mut observer = CaptureObserver::new(config()).unwrap();
        let mut input = info(StreamRole::Color, BASE + 100);
        match kind {
            0 => input.frame_id = "wrong".into(),
            1 => input.k[0] = 0.,
            2 => input.roi = [1, 0, 0, 0],
            3 => input.distortion_model = "unsupported".into(),
            4 => input.binning = [2, 0],
            _ => input.r[0] = f64::NAN,
        }
        let update = observer
            .camera_info(StreamRole::Color, at(0), input)
            .unwrap();
        assert_eq!(
            update.rejections[0].reason,
            RejectionReason::CameraInfoInvalid
        );
    }
    let mut observer = CaptureObserver::new(config()).unwrap();
    observer.advance_watermark(at(0), BASE + 200).unwrap();
    assert_eq!(
        observer
            .camera_info(
                StreamRole::Color,
                at(1),
                info(StreamRole::Color, BASE + 100)
            )
            .unwrap()
            .rejections[0]
            .reason,
        RejectionReason::LateObservation
    );
    assert_eq!(
        observer
            .metadata(StreamRole::Color, at(2), metadata(BASE + 100))
            .unwrap()
            .rejections[0]
            .reason,
        RejectionReason::LateObservation
    );
    let mut input = info(StreamRole::Color, BASE);
    input.stamp = HeaderStamp {
        sec: -1,
        nanosec: 0,
    };
    assert_eq!(
        observer
            .camera_info(StreamRole::Color, at(3), input)
            .unwrap()
            .rejections[0]
            .reason,
        RejectionReason::HeaderTimestampInvalid
    );
}
#[test]
fn unknown_or_out_of_range_time_mapping_never_produces_an_observed_pair() {
    for mapper in [
        quality_core::ClockMapper::Unknown,
        quality_core::ClockMapper::MeasuredObservedHeaderOffset {
            offset_ns: i64::MIN,
        },
    ] {
        let mut cfg = config();
        cfg.streams.get_mut(&StreamRole::Color).unwrap().mapper = mapper;
        let mut observer = CaptureObserver::new(cfg).unwrap();
        let update = observer
            .image(
                StreamRole::Color,
                at(0),
                Some(BASE + 110),
                image(StreamRole::Color, BASE + 100),
            )
            .unwrap();
        assert_eq!(
            update.rejections[0].reason,
            RejectionReason::TimeMappingInvalid
        );
        assert_eq!(
            observer
                .camera_info(
                    StreamRole::Color,
                    at(1),
                    info(StreamRole::Color, BASE + 100)
                )
                .unwrap()
                .rejections[0]
                .reason,
            RejectionReason::HeaderTimestampInvalid
        );
        assert_eq!(
            observer
                .metadata(StreamRole::Color, at(2), metadata(BASE + 100))
                .unwrap()
                .rejections[0]
                .reason,
            RejectionReason::HeaderTimestampInvalid
        );
    }
    for received in [BASE - 10000, BASE + 200_000_000] {
        let mut observer = CaptureObserver::new(config()).unwrap();
        let update = observer
            .image(
                StreamRole::Color,
                at(0),
                Some(received),
                image(StreamRole::Color, BASE + 100),
            )
            .unwrap();
        assert_eq!(
            update.rejections[0].reason,
            RejectionReason::TimeMappingInvalid
        );
    }
}
#[test]
fn rgb_and_bgr_payload_contracts_and_reported_session_remain_distinct_from_sdk() {
    for encoding in [ImageEncoding::Rgb8, ImageEncoding::Bgr8] {
        let mut cfg = config();
        cfg.streams.get_mut(&StreamRole::Color).unwrap().encoding = encoding;
        let mut observer = CaptureObserver::new(cfg).unwrap();
        observer
            .camera_info(
                StreamRole::Color,
                at(0),
                info(StreamRole::Color, BASE + 100),
            )
            .unwrap();
        let mut input = image(StreamRole::Color, BASE + 100);
        input.encoding = encoding;
        input.step = 6;
        input.payload_len = 12;
        observer
            .image(StreamRole::Color, at(1), Some(BASE + 110), input)
            .unwrap();
        observer
            .metadata(StreamRole::Color, at(2), metadata(BASE + 100))
            .unwrap();
        add(&mut observer, StreamRole::Depth, 3, BASE + 105);
        let update = observer.advance_watermark(at(5), BASE + 200).unwrap();
        assert_eq!(update.pairs.len(), 1);
        let color = &update.pairs[0].color;
        assert_eq!(
            color.reported_source_session.as_ref().unwrap().as_str(),
            "b".repeat(32)
        );
        assert_eq!(color.context.session_id, session());
        assert_eq!(update.pairs[0].same_sdk_frameset, None);
    }
}
#[test]
fn completed_stream_queue_cannot_grow_without_a_matching_stream() {
    let mut cfg = config();
    cfg.association.queue_capacity = 1;
    let mut observer = CaptureObserver::new(cfg).unwrap();
    add(&mut observer, StreamRole::Color, 0, BASE + 100);
    observer.advance_watermark(at(2), BASE + 105).unwrap();
    add(&mut observer, StreamRole::Color, 3, BASE + 120);
    let update = observer.advance_watermark(at(5), BASE + 125).unwrap();
    assert!(update.pairs.is_empty());
    assert!(
        update
            .rejections
            .iter()
            .any(|r| r.reason == RejectionReason::QueueOverflow)
    );
}
