mod common;
use capture_observer::*;
use common::*;
use quality_core::*;

#[test]
fn observed_pair_is_not_an_sdk_frameset_and_unknown_sdk_fields_stay_none() {
    let mut o = CaptureObserver::new(config()).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    add(&mut o, StreamRole::Depth, 2, BASE + 105);
    let update = o.advance_watermark(at(4), BASE + 200).unwrap();
    assert_eq!(update.pairs.len(), 1);
    assert!(update.rejections.is_empty());
    let pair = &update.pairs[0];
    assert_eq!(pair.provenance, PairingProvenance::ObservedTimestamp);
    assert_eq!(pair.delta_ns, 5);
    assert_eq!(pair.color.camera_info_observation_sequence, Some(1));
    assert_eq!(pair.depth.camera_info_observation_sequence, Some(3));
    assert_eq!(pair.color.metadata_observation_sequence, None);
    assert_eq!(pair.same_sdk_frameset, None);
    assert!(!pair.hardware_sync_verified);
    assert_eq!(pair.color.context.sequence, None);
    assert_eq!(pair.color.context.device_timestamp_us, None);
    assert_eq!(pair.color.context.system_timestamp_us, None);
    assert_eq!(
        pair.color.metadata_provenance,
        MetadataProvenance::Unobserved
    );
    assert_eq!(pair.color.context.received_mono_offset_ns, 1);
    let text = serde_json::to_string(pair).unwrap();
    assert!(!text.contains("synthetic_"));
    assert!(text.contains("observed_timestamp"));
}

#[test]
fn multiple_info_candidates_and_shared_info_are_rejected_as_whole_components() {
    for shared in [false, true] {
        let mut c = config();
        c.association.info_tolerance_ns = 2;
        let mut o = CaptureObserver::new(c).unwrap();
        o.camera_info(
            StreamRole::Color,
            at(0),
            info(StreamRole::Color, BASE + 100),
        )
        .unwrap();
        if shared {
            o.image(
                StreamRole::Color,
                at(1),
                Some(BASE + 110),
                image(StreamRole::Color, BASE + 101),
            )
            .unwrap();
        } else {
            o.camera_info(
                StreamRole::Color,
                at(1),
                info(StreamRole::Color, BASE + 101),
            )
            .unwrap();
        }
        o.image(
            StreamRole::Color,
            at(2),
            Some(BASE + 110),
            image(StreamRole::Color, BASE + 100),
        )
        .unwrap();
        let update = o.advance_watermark(at(3), BASE + 200).unwrap();
        assert!(update.pairs.is_empty());
        let expected = if shared {
            RejectionReason::ImageCandidatesAmbiguous
        } else {
            RejectionReason::CameraInfoAmbiguous
        };
        assert_eq!(update.rejections.len(), 3);
        assert!(update.rejections.iter().all(|r| r.reason == expected));
    }
}

#[test]
fn pair_candidate_ambiguity_never_selects_a_first_or_nearest_candidate() {
    let mut c = config();
    c.association.pair_tolerance_ns = 5;
    let mut o = CaptureObserver::new(c).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    add(&mut o, StreamRole::Color, 2, BASE + 106);
    add(&mut o, StreamRole::Depth, 4, BASE + 103);
    assert!(
        o.advance_watermark(at(6), BASE + 108)
            .unwrap()
            .pairs
            .is_empty()
    );
    let update = o.advance_watermark(at(7), BASE + 200).unwrap();
    assert!(update.pairs.is_empty());
    assert_eq!(update.rejections.len(), 3);
    assert!(
        update
            .rejections
            .iter()
            .all(|r| r.reason == RejectionReason::PairAmbiguous)
    );
}

#[test]
fn delayed_camera_info_closing_cannot_prematurely_report_pair_missing() {
    let mut c = config();
    c.association.info_tolerance_ns = 100;
    let mut o = CaptureObserver::new(c).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    add(&mut o, StreamRole::Depth, 2, BASE + 105);
    let early = o.advance_watermark(at(4), BASE + 202).unwrap();
    assert!(early.pairs.is_empty());
    assert!(early.rejections.is_empty());
    let final_update = o.advance_watermark(at(5), BASE + 500).unwrap();
    assert_eq!(final_update.pairs.len(), 1);
    assert!(final_update.rejections.is_empty());
}

#[test]
fn exact_image_info_matching_accepts_all_arrival_orders_without_greedy_pairing() {
    for reverse_roles in [false, true] {
        for images_first in [false, true] {
            let mut o = CaptureObserver::new(config()).unwrap();
            let mut next = 0;
            let roles = if reverse_roles {
                [StreamRole::Depth, StreamRole::Color]
            } else {
                [StreamRole::Color, StreamRole::Depth]
            };
            for role in roles {
                let time = BASE + 100 + if role == StreamRole::Depth { 5 } else { 0 };
                if images_first {
                    o.image(role, at(next), Some(time + 10), image(role, time))
                        .unwrap();
                    o.camera_info(role, at(next + 1), info(role, time)).unwrap();
                } else {
                    add(&mut o, role, next, time);
                }
                next += 2;
            }
            let update = o.advance_watermark(at(next), BASE + 200).unwrap();
            assert_eq!(update.pairs.len(), 1);
            assert_eq!(update.pairs[0].color.observed_header_ns, BASE + 100);
        }
    }
}

#[test]
fn source_offset_is_declared_and_does_not_populate_unknown_sdk_system_time() {
    let mut c = config();
    c.streams.get_mut(&StreamRole::Depth).unwrap().mapper =
        ClockMapper::MeasuredObservedHeaderOffset { offset_ns: 100 };
    let mut o = CaptureObserver::new(c).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    o.camera_info(StreamRole::Depth, at(2), info(StreamRole::Depth, BASE + 5))
        .unwrap();
    o.image(
        StreamRole::Depth,
        at(3),
        Some(BASE + 115),
        image(StreamRole::Depth, BASE + 5),
    )
    .unwrap();
    let update = o.advance_watermark(at(4), BASE + 300).unwrap();
    assert_eq!(update.pairs.len(), 1);
    assert_eq!(update.pairs[0].depth.observed_header_ns, BASE + 5);
    assert_eq!(update.pairs[0].depth.mapped_unix_ns, BASE + 105);
    assert_eq!(update.pairs[0].depth.context.system_timestamp_us, None);
}

#[test]
fn known_metadata_is_only_publisher_reported_and_zero_device_origin_is_preserved() {
    let mut c = config();
    c.streams
        .get_mut(&StreamRole::Color)
        .unwrap()
        .metadata_required = true;
    let mut o = CaptureObserver::new(c).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    add(&mut o, StreamRole::Depth, 2, BASE + 105);
    o.metadata(StreamRole::Color,at(4),DecodedMetadata{stamp:stamp(BASE+100),data:serde_json::json!({"version":1,"frame_id":"synthetic_color_optical","stamp_sec":1,"stamp_nanosec":100,
        "sequence":17,"device_timestamp_us":0,"system_timestamp_us":null}).to_string()}).unwrap();
    let update = o.advance_watermark(at(5), BASE + 200).unwrap();
    let pair = &update.pairs[0];
    assert_eq!(pair.color.context.sequence, Some(17));
    assert_eq!(pair.color.context.device_timestamp_us, Some(0));
    assert_eq!(pair.depth.context.sequence, None);
    assert_eq!(
        pair.color.metadata_provenance,
        MetadataProvenance::PublisherReported
    );
    assert_eq!(pair.same_sdk_frameset, None);
}

#[test]
fn global_headers_preserve_distinct_reported_clocks_without_claiming_hardware_sync() {
    let mut o = CaptureObserver::new(config()).unwrap();
    let time = BASE + 1000;
    add(&mut o, StreamRole::Color, 0, time);
    add(&mut o, StreamRole::Depth, 2, time);
    for (role, offset) in [(StreamRole::Color, 4), (StreamRole::Depth, 5)] {
        let data = serde_json::json!({"version":1,"frame_id":format!("synthetic_{}_optical",role.as_str()),"stamp_sec":1,"stamp_nanosec":1000,"sequence":17,"device_timestamp_us":123,"system_timestamp_us":1_000_100,"global_timestamp_us":1_000_001,"stamp_mapping":"sdk-global-unix"});
        assert!(
            o.metadata(
                role,
                at(offset),
                DecodedMetadata {
                    stamp: stamp(time),
                    data: data.to_string()
                }
            )
            .unwrap()
            .rejections
            .is_empty()
        );
    }
    let update = o.advance_watermark(at(6), time + 100).unwrap();
    let pair = &update.pairs[0];
    assert_eq!(pair.color.context.global_timestamp_us, Some(1_000_001));
    assert_eq!(pair.color.context.system_timestamp_us, Some(1_000_100));
    assert_eq!(pair.color.context.device_timestamp_us, Some(123));
    assert_eq!(pair.color.observed_header_ns, time);
    assert_eq!(
        pair.color.metadata_provenance,
        MetadataProvenance::PublisherReported
    );
    assert_eq!(pair.same_sdk_frameset, None);
    assert!(!pair.hardware_sync_verified);
    // Older serialized contexts round-trip without adding an absent field and
    // changing canonical identities of existing sealed records.
    let mut old = serde_json::to_value(&pair.color.context).unwrap();
    old.as_object_mut().unwrap().remove("global_timestamp_us");
    let context: capture_contract::FrameContext = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(context.global_timestamp_us, None);
    assert_eq!(serde_json::to_value(context).unwrap(), old);
}

#[test]
fn explicit_sdk_metadata_mappings_require_valid_matching_clock_values() {
    for (mapping, global, system, valid) in [
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::json!(1_000_001),
            serde_json::json!(1_000_100),
            true,
        ),
        (
            serde_json::json!("sdk-system-unix"),
            serde_json::Value::Null,
            serde_json::json!(1_000_001),
            true,
        ),
        (
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            true,
        ),
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::Value::Null,
            serde_json::json!(1_000_001),
            false,
        ),
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::json!(0),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::json!(1_000_002),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::json!(u64::MAX),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::json!("sdk-global-unix"),
            serde_json::json!("bad"),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::json!("sdk-system-unix"),
            serde_json::json!(1_000_001),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::json!("device"),
            serde_json::json!(1_000_001),
            serde_json::Value::Null,
            false,
        ),
        (
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::Value::Null,
            false,
        ),
    ] {
        let mut o = CaptureObserver::new(config()).unwrap();
        let data = serde_json::json!({"version":1,"frame_id":"synthetic_color_optical","stamp_sec":1,"stamp_nanosec":1000,"stamp_mapping":mapping,"global_timestamp_us":global,"system_timestamp_us":system});
        let update = o
            .metadata(
                StreamRole::Color,
                at(0),
                DecodedMetadata {
                    stamp: stamp(BASE + 1000),
                    data: data.to_string(),
                },
            )
            .unwrap();
        assert_eq!(update.rejections.is_empty(), valid, "{data}");
    }
    let mut c = config();
    c.streams
        .get_mut(&StreamRole::Color)
        .unwrap()
        .context
        .global_timestamp_us = Some(1);
    assert_eq!(c.validate().unwrap_err(), "OBSERVER_BINDING_INVALID");
}

#[test]
fn missing_required_metadata_and_malformed_layout_are_explicit_rejections() {
    let mut c = config();
    c.streams
        .get_mut(&StreamRole::Color)
        .unwrap()
        .metadata_required = true;
    let mut o = CaptureObserver::new(c).unwrap();
    add(&mut o, StreamRole::Color, 0, BASE + 100);
    let update = o.advance_watermark(at(2), BASE + 200).unwrap();
    assert!(
        update
            .rejections
            .iter()
            .any(|r| r.reason == RejectionReason::MetadataMissing)
    );
    let mut bad = image(StreamRole::Depth, BASE + 300);
    bad.payload_len -= 1;
    let update = o
        .image(StreamRole::Depth, at(3), Some(BASE + 310), bad)
        .unwrap();
    assert_eq!(
        update.rejections[0].reason,
        RejectionReason::FrameContractInvalid
    );
}

#[test]
fn bounded_queue_overflow_late_data_and_clock_regression_are_not_silently_reset() {
    let mut c = config();
    c.association.queue_capacity = 2;
    let mut o = CaptureObserver::new(c).unwrap();
    for n in 0..3 {
        let update = o
            .image(
                StreamRole::Color,
                at(n),
                Some(BASE + 110 + n),
                image(StreamRole::Color, BASE + 100 + n),
            )
            .unwrap();
        if n == 2 {
            assert_eq!(update.rejections[0].reason, RejectionReason::QueueOverflow);
        }
    }
    o.advance_watermark(at(3), BASE + 200).unwrap();
    let late = o
        .image(
            StreamRole::Color,
            at(4),
            Some(BASE + 210),
            image(StreamRole::Color, BASE + 150),
        )
        .unwrap();
    assert_eq!(late.rejections[0].reason, RejectionReason::LateObservation);
    let regressed = o.advance_watermark(at(5), BASE + 100).unwrap();
    assert_eq!(
        regressed.rejections[0].reason,
        RejectionReason::WatermarkRegression
    );
    let query = o
        .query(QualityQueryRequest {
            version: 1,
            query: QualityQuery::Snapshot,
        })
        .unwrap();
    assert_eq!(query.watermark_unix_ns, Some(BASE + 200));
    let QualityQueryResult::Snapshot { snapshot } = query.quality.result else {
        panic!()
    };
    assert!(
        snapshot
            .active_episodes
            .iter()
            .any(|v| v.violation == FrameViolation::QueueOverflow)
    );
}

#[test]
fn absent_stream_tick_and_finalization_have_queryable_quality_not_sdk_disconnect_claims() {
    let mut o = CaptureObserver::new(config()).unwrap();
    o.tick(at(101), Some(BASE + 1000)).unwrap();
    let query = o
        .query(QualityQueryRequest {
            version: 1,
            query: QualityQuery::Snapshot,
        })
        .unwrap();
    let QualityQueryResult::Snapshot { snapshot } = query.quality.result else {
        panic!()
    };
    assert_eq!(snapshot.active_episodes.len(), 2);
    assert!(
        snapshot
            .active_episodes
            .iter()
            .all(|e| e.violation == FrameViolation::StreamStale)
    );
    o.finalize().unwrap();
    assert!(o.tick(at(102), Some(BASE + 1001)).is_err());
}

#[test]
fn pending_image_never_borrows_previous_camera_info_as_complete_contract_evidence() {
    let mut cfg = config();
    cfg.quality.quality.action = capture_contract::QualityAction::FailClosed;
    let mut o = CaptureObserver::new(cfg).unwrap();
    for (index, role) in [StreamRole::Color, StreamRole::Depth]
        .into_iter()
        .enumerate()
    {
        o.camera_info(role, at(index as u64 * 2 + 1), info(role, BASE))
            .unwrap();
        o.image(
            role,
            at(index as u64 * 2 + 2),
            Some(BASE + 1),
            image(role, BASE + 1),
        )
        .unwrap();
    }
    let q = o
        .query(QualityQueryRequest {
            version: 1,
            query: QualityQuery::Snapshot,
        })
        .unwrap();
    let QualityQueryResult::Snapshot { snapshot } = q.quality.result else {
        panic!()
    };
    assert!(!snapshot.continue_allowed);
}
#[test]
fn explicit_lifecycle_interruption_is_bounded_terminal_and_does_not_fake_frame_or_device_facts() {
    for reason in [
        InterruptionReason::DriverExited,
        InterruptionReason::ObserverInterrupted,
        InterruptionReason::CoreUnavailable,
    ] {
        let mut observer = CaptureObserver::new(config()).unwrap();
        observer.interrupt(at(1), reason).unwrap();
        observer.interrupt(at(2), reason).unwrap();
        observer.finalize().unwrap();
        let request = QualityQueryRequest {
            version: 1,
            query: QualityQuery::Snapshot,
        };
        let response = observer.query(request).unwrap();
        let QualityQueryResult::Snapshot { snapshot } = response.quality.result else {
            panic!("snapshot required")
        };
        assert_eq!(snapshot.state, QualityState::Failed);
        assert_eq!(snapshot.interruption_reason, Some(reason));
        assert!(
            snapshot
                .streams
                .values()
                .all(|stream| stream.frame_count == 0)
        );
        assert_eq!(
            snapshot
                .finalized_sessions
                .last()
                .unwrap()
                .interruption_reason,
            Some(reason)
        );
        assert!(
            !snapshot
                .active_episodes
                .iter()
                .any(|episode| episode.violation == FrameViolation::DeviceDisconnected)
        );
        let response = observer
            .query(QualityQueryRequest {
                version: 1,
                query: QualityQuery::Events {
                    cursor: None,
                    limit: config().quality.event_capacity,
                },
            })
            .unwrap();
        let QualityQueryResult::Events { page } = response.quality.result else {
            panic!("events required")
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0].violation,
            FrameViolation::ObserverInterrupted
        );
        assert_eq!(page.events[0].interruption_reason, Some(reason));
        assert!(observer.interrupt(at(3), reason).is_err());
    }
    let mut normal = CaptureObserver::new(config()).unwrap();
    normal.finalize().unwrap();
    let response = normal
        .query(QualityQueryRequest {
            version: 1,
            query: QualityQuery::Snapshot,
        })
        .unwrap();
    let QualityQueryResult::Snapshot { snapshot } = response.quality.result else {
        panic!("snapshot required")
    };
    assert_eq!(snapshot.state, QualityState::Normal);
    assert_eq!(snapshot.interruption_reason, None);
    assert_eq!(
        snapshot
            .finalized_sessions
            .last()
            .unwrap()
            .interruption_reason,
        None
    );
}
