#![allow(dead_code)]
use capture_contract::{
    CalibrationId, CameraModel, ConfigurationId, DistortionModel, FrameContext, GenerationId,
    MonoTime, QualityAction, QualityPolicy, SessionId, SnapshotId,
};
use capture_observer::*;
use quality_core::*;
use std::collections::BTreeMap;
pub const BASE: u64 = 1_000_000_000;
pub fn session() -> SessionId {
    SessionId::parse("a".repeat(32)).unwrap()
}
pub fn at(offset_ns: u64) -> MonoTime {
    MonoTime {
        session_id: session(),
        offset_ns,
    }
}
pub fn stamp(value: u64) -> HeaderStamp {
    HeaderStamp {
        sec: (value / 1_000_000_000) as i64,
        nanosec: (value % 1_000_000_000) as u32,
    }
}
pub fn config() -> ObserverConfig {
    let quality = RuntimeQualityPolicy {
        version: 1,
        quality: QualityPolicy {
            version: 1,
            max_source_age_ns: 100_000_000,
            max_future_ns: 1000,
            max_pair_delta_us: 20,
            min_valid_fraction: 0.5,
            max_zero_fraction: 1.0,
            action: QualityAction::ReportOnly,
        },
        stale_after_ns: 100,
        event_capacity: 16,
        window_capacity: 8,
        window_horizon_ns: 1000,
        summary_capacity: 2,
        max_sessions: 4,
        rules: FrameViolation::ALL
            .into_iter()
            .map(|v| {
                (
                    v,
                    RulePolicy {
                        open_after: 1,
                        recover_after: 2,
                        severity: QualityState::Degraded,
                    },
                )
            })
            .collect(),
    };
    let streams = [StreamRole::Color, StreamRole::Depth]
        .into_iter()
        .map(|role| {
            let name = role.as_str();
            let camera = CameraModel {
                frame_id: format!("synthetic_{name}_optical"),
                width: 2,
                height: 2,
                k: [1., 0., 1., 0., 1., 1., 0., 0., 1.],
                distortion_model: DistortionModel::PlumbBob,
                distortion_coefficients: vec![0.; 5],
            };
            let context = FrameContext {
                version: 1,
                session_id: session(),
                generation_id: GenerationId::parse("a".repeat(32)).unwrap(),
                configuration_id: ConfigurationId::parse("a".repeat(64)).unwrap(),
                calibration_id: CalibrationId::parse("a".repeat(64)).unwrap(),
                snapshot_id: SnapshotId::parse("a".repeat(64)).unwrap(),
                stream_id: name.into(),
                sequence: None,
                device_timestamp_us: None,
                system_timestamp_us: None,
                global_timestamp_us: None,
                received_mono_offset_ns: 0,
            };
            (
                role,
                ExpectedStream {
                    context,
                    encoding: if role == StreamRole::Color {
                        ImageEncoding::Yuyv
                    } else {
                        ImageEncoding::Depth16
                    },
                    camera,
                    rectification: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                    projection: [1., 0., 1., 0., 0., 1., 1., 0., 0., 0., 1., 0.],
                    camera_info_tolerance: 1e-8,
                    mapper: ClockMapper::DeclaredObservedUnixHeader,
                    metadata_required: false,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    ObserverConfig {
        version: 1,
        streams,
        quality,
        association: AssociationPolicy {
            queue_capacity: 8,
            info_tolerance_ns: 0,
            metadata_tolerance_ns: 0,
            pair_tolerance_ns: 10,
            watermark_lateness_ns: 1000,
            max_payload_bytes: 1024,
        },
    }
}
pub fn image(role: StreamRole, time: u64) -> DecodedImage {
    DecodedImage {
        stamp: stamp(time),
        frame_id: format!("synthetic_{}_optical", role.as_str()),
        width: 2,
        height: 2,
        step: 4,
        encoding: if role == StreamRole::Color {
            ImageEncoding::Yuyv
        } else {
            ImageEncoding::Depth16
        },
        is_bigendian: 0,
        payload_len: 8,
    }
}
pub fn info(role: StreamRole, time: u64) -> DecodedCameraInfo {
    let expected = &config().streams[&role];
    DecodedCameraInfo {
        stamp: stamp(time),
        frame_id: expected.camera.frame_id.clone(),
        width: 2,
        height: 2,
        distortion_model: "plumb_bob".into(),
        k: expected.camera.k,
        d: vec![0.; 5],
        r: expected.rectification,
        p: expected.projection,
        binning: [0; 2],
        roi: [0; 4],
        roi_do_rectify: false,
    }
}
pub fn add(observer: &mut CaptureObserver, role: StreamRole, base: u64, time: u64) {
    observer
        .camera_info(role, at(base), info(role, time))
        .unwrap();
    observer
        .image(role, at(base + 1), Some(time + 10), image(role, time))
        .unwrap();
}
