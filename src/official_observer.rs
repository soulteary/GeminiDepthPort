//! Official-driver observed evidence: observer sessions never claim SDK session
//! identity or frameset pairing. Bootstrap uses actual collected CameraInfo/TF.
use crate::{calibration, config, util::*};
use anyhow::Result;
use capture_contract::{snapshot::*, *};
use capture_observer::{
    AssociationPolicy, ExpectedStream, ImageEncoding, ObserverConfig, StreamRole,
};
use quality_core::{ClockMapper, FrameViolation, QualityState, RulePolicy, RuntimeQualityPolicy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub fn policy(d: &Value, camera: &Value, generation: &str) -> Result<Value> {
    let raw = d["cameras"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == camera["name"]))
        .ok_or_else(|| anyhow::anyhow!("OBSERVER_CAMERA_BINDING"))?;
    let mut inventory = d.clone();
    inventory["cameras"] = json!([raw]);
    require(
        raw["parameters"]["enable_color"] == true && raw["parameters"]["enable_depth"] == true,
        "OFFICIAL_OBSERVER_RGBD_REQUIRED",
        "Observed association requires both declared RGB-D streams",
    )?;
    let request = calibration::request(&inventory, generation)?;
    Ok(
        json!({"version":1,"kind":"official-observer-policy","generation_id":generation,"inventory":inventory,"request":request,"camera_name":camera["name"],"namespace":camera["namespace"],"timestamp_mapping":if camera["timestamp_source"]=="host"{"declared-observed-unix-header"}else{"unknown"}}),
    )
}
pub fn bootstrap(
    policy: &Value,
    observations: &Value,
    instance: &str,
    session: &str,
    observed_unix_ns: u64,
) -> Result<Value> {
    require(
        policy["version"] == 1
            && policy["kind"] == "official-observer-policy"
            && observations["source"] == "ros-camera-info-tf",
        "OBSERVER_BOOTSTRAP_SCHEMA",
        "Actual ROS observations required",
    )?;
    let instance = InstanceId::parse(instance)?;
    let session = SessionId::parse(session)?;
    let inventory = &policy["inventory"];
    config::validate(inventory, true)?;
    let generation = GenerationId::parse(text(&policy["generation_id"]))?;
    let old = calibration::snapshot(inventory, generation.as_str(), observations)?;
    let name = text(&policy["camera_name"]);
    let mut record = import_legacy(&serde_json::to_vec(&old)?, name)?;
    record.source = CalibrationSource::RosCameraInfoTf;
    record.actual_runtime_observed = true;
    record.source_instance_id = Some(instance.clone());
    record.source_session_id = Some(session.clone());
    record.observed_unix_ns = Some(observed_unix_ns);
    record.legacy_source_sha256 = None;
    record.validate()?;
    let camera = &inventory["cameras"][0];
    let quality = QualityPolicy {
        version: 1,
        max_source_age_ns: (camera["health"]["freshness_seconds"].as_f64().unwrap() * 1e9) as u64,
        max_future_ns: 100_000_000,
        max_pair_delta_us: 20000,
        min_valid_fraction: 0.,
        max_zero_fraction: 1.,
        action: QualityAction::ReportOnly,
    };
    let configuration = json!({"runtime_config_sha256":hash(serde_json::to_vec_pretty(policy)?),"observer_policy":policy});
    let binding = SnapshotBinding {
        instance_id: instance,
        session_id: session.clone(),
        generation_id: generation,
        configuration_id: ConfigurationId::parse(content_digest(
            ContentKind::Configuration,
            &serde_json::to_vec(&configuration)?,
        )?)?,
        calibration_id: record.id()?,
    };
    let snapshot = CaptureSnapshot {
        version: 1,
        binding: binding.clone(),
        configuration,
        calibration: record.clone(),
        depth: DepthRepresentation::Millimetres {
            quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
        },
        quality_policy: quality.clone(),
    }
    .seal()?;
    let runtime_quality = RuntimeQualityPolicy {
        version: 1,
        quality: quality.clone(),
        stale_after_ns: quality.max_source_age_ns,
        event_capacity: 1024,
        window_capacity: 256,
        window_horizon_ns: 10_000_000_000,
        summary_capacity: 8,
        max_sessions: 64,
        rules: FrameViolation::ALL
            .into_iter()
            .map(|v| {
                (
                    v,
                    RulePolicy {
                        open_after: 1,
                        recover_after: 3,
                        severity: QualityState::Degraded,
                    },
                )
            })
            .collect(),
    };
    let mut streams = BTreeMap::new();
    for (role, key) in [(StreamRole::Color, "color"), (StreamRole::Depth, "depth")] {
        let info = &observations["cameras"][name][key];
        let encoding = if key == "depth" {
            ImageEncoding::Depth16
        } else {
            match text(&camera["parameters"]["color_format"]) {
                "YUYV" => ImageEncoding::Yuyv,
                "RGB" | "MJPG" => ImageEncoding::Rgb8,
                _ => anyhow::bail!("OBSERVER_ENCODING_UNSUPPORTED"),
            }
        };
        let context = FrameContext {
            version: 1,
            session_id: session.clone(),
            generation_id: binding.generation_id.clone(),
            configuration_id: binding.configuration_id.clone(),
            calibration_id: binding.calibration_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            stream_id: key.into(),
            sequence: None,
            device_timestamp_us: None,
            system_timestamp_us: None,
            received_mono_offset_ns: 0,
        };
        streams.insert(
            role,
            ExpectedStream {
                context,
                encoding,
                camera: record.models[key].clone(),
                rectification: serde_json::from_value(info["r"].clone())?,
                projection: serde_json::from_value(info["p"].clone())?,
                camera_info_tolerance: 1e-6,
                mapper: if policy["timestamp_mapping"] == "declared-observed-unix-header" {
                    ClockMapper::DeclaredObservedUnixHeader
                } else {
                    ClockMapper::Unknown
                },
                metadata_required: false,
            },
        );
    }
    let observer_config = ObserverConfig {
        version: 1,
        streams,
        association: AssociationPolicy {
            queue_capacity: 128,
            info_tolerance_ns: 0,
            metadata_tolerance_ns: 0,
            pair_tolerance_ns: 20_000_000,
            watermark_lateness_ns: 100_000_000,
            max_payload_bytes: 128 * 1024 * 1024,
        },
        quality: runtime_quality,
    };
    observer_config.validate().map_err(anyhow::Error::msg)?;
    Ok(json!({"version":1,"snapshot":snapshot,"observer_config":observer_config}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Value, Value) {
        let contracts: Value =
            serde_json::from_str(include_str!("../tests/rust/contracts.json")).unwrap();
        let d = contracts["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "three draft")
            .unwrap()["data"]
            .clone();
        let mut d = d;
        d["cameras"][0]["timestamp_source"] = "host".into();
        let p = policy(&d, &d["cameras"][0], &"d".repeat(32)).unwrap();
        let camera = &p["request"]["cameras"][0];
        let mut streams = json!({});
        for (role, s) in camera["streams"].as_object().unwrap() {
            streams[role] = json!({"frame_id":s["frame_id"],"width":s["width"],"height":s["height"],"distortion_model":"plumb_bob","k":[500.0,0.0,320.0,0.0,500.0,240.0,0.0,0.0,1.0],"d":[0.0,0.0,0.0,0.0,0.0],"r":[1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0],"p":[500.0,0.0,320.0,0.0,0.0,500.0,240.0,0.0,0.0,0.0,1.0,0.0]});
        }
        streams["depth_to_color"] = json!({"source_frame":camera["streams"]["depth"]["frame_id"],"target_frame":camera["streams"]["color"]["frame_id"],"translation_unit":"m","rotation_representation":"quaternion_xyzw","translation":[0.025,0.0,0.0],"rotation":[0.0,0.0,0.0,1.0]});
        let observations = json!({"source":"ros-camera-info-tf","request_sha256":calibration::digest(&p["request"]),"cameras":{camera["name"].as_str().unwrap():streams}});
        (p, observations)
    }
    #[test]
    fn actual_observation_bootstrap_keeps_sdk_unknown_and_observer_identity() {
        let (p, o) = fixture();
        let result = bootstrap(&p, &o, &"a".repeat(64), &"b".repeat(32), 1).unwrap();
        let snapshot: SnapshotEnvelope =
            serde_json::from_value(result["snapshot"].clone()).unwrap();
        snapshot.require_runtime().unwrap();
        assert_eq!(
            snapshot.content.calibration.source,
            CalibrationSource::RosCameraInfoTf
        );
        assert!(!snapshot.content.calibration.physical_accuracy_verified);
        let config: ObserverConfig =
            serde_json::from_value(result["observer_config"].clone()).unwrap();
        for stream in config.streams.values() {
            assert!(stream.context.sequence.is_none());
            assert!(stream.context.device_timestamp_us.is_none());
            assert!(stream.context.system_timestamp_us.is_none());
            assert_eq!(stream.context.snapshot_id, snapshot.snapshot_id);
            assert_eq!(stream.mapper, ClockMapper::DeclaredObservedUnixHeader);
            assert!(!stream.metadata_required);
        }
    }
    #[test]
    fn configured_rust_observer_associates_actual_decoded_inputs_without_sdk_claims() {
        use capture_observer::{
            CaptureObserver, DecodedCameraInfo, DecodedImage, HeaderStamp, PairingProvenance,
        };
        let (p, o) = fixture();
        let result = bootstrap(&p, &o, &"a".repeat(64), &"b".repeat(32), 1).unwrap();
        let config: ObserverConfig =
            serde_json::from_value(result["observer_config"].clone()).unwrap();
        let session = SessionId::parse("b".repeat(32)).unwrap();
        let mut observer = CaptureObserver::new(config.clone()).unwrap();
        let stamp = HeaderStamp {
            sec: 1_700_000_000,
            nanosec: 0,
        };
        let mut pairs = vec![];
        let mut tick = 0;
        for role in [StreamRole::Color, StreamRole::Depth] {
            let expected = &config.streams[&role];
            let c = &expected.camera;
            let info = DecodedCameraInfo {
                stamp,
                frame_id: c.frame_id.clone(),
                width: c.width,
                height: c.height,
                distortion_model: "plumb_bob".into(),
                k: c.k,
                d: c.distortion_coefficients.clone(),
                r: expected.rectification,
                p: expected.projection,
                binning: [0; 2],
                roi: [0; 4],
                roi_do_rectify: false,
            };
            tick += 1;
            observer
                .camera_info(
                    role,
                    MonoTime {
                        session_id: session.clone(),
                        offset_ns: tick,
                    },
                    info,
                )
                .unwrap();
            let step = c.width
                * match expected.encoding {
                    ImageEncoding::Rgb8 | ImageEncoding::Bgr8 => 3,
                    _ => 2,
                };
            let image = DecodedImage {
                stamp,
                frame_id: c.frame_id.clone(),
                width: c.width,
                height: c.height,
                step,
                encoding: expected.encoding,
                is_bigendian: 0,
                payload_len: u64::from(step) * u64::from(c.height),
            };
            tick += 1;
            pairs.extend(
                observer
                    .image(
                        role,
                        MonoTime {
                            session_id: session.clone(),
                            offset_ns: tick,
                        },
                        Some(1_700_000_000_001_000_000),
                        image,
                    )
                    .unwrap()
                    .pairs,
            );
        }
        pairs.extend(
            observer
                .advance_watermark(
                    MonoTime {
                        session_id: session,
                        offset_ns: 100_000_000,
                    },
                    1_700_000_000_100_000_000,
                )
                .unwrap()
                .pairs,
        );
        assert_eq!(pairs.len(), 1);
        let pair = &pairs[0];
        assert_eq!(pair.provenance, PairingProvenance::ObservedTimestamp);
        assert_eq!(pair.same_sdk_frameset, None);
        assert!(!pair.hardware_sync_verified);
        assert!(pair.color.context.sequence.is_none());
        assert!(pair.depth.context.device_timestamp_us.is_none());
        assert!(pair.color.context.system_timestamp_us.is_none());
    }
    #[test]
    fn unknown_clock_is_not_assumed_unix_and_session_changes_snapshot() {
        let (mut p, o) = fixture();
        p["timestamp_mapping"] = "unknown".into();
        let a = bootstrap(&p, &o, &"a".repeat(64), &"b".repeat(32), 1).unwrap();
        let b = bootstrap(&p, &o, &"a".repeat(64), &"c".repeat(32), 2).unwrap();
        assert_ne!(a["snapshot"]["snapshot_id"], b["snapshot"]["snapshot_id"]);
        assert_eq!(
            a["observer_config"]["streams"]["color"]["mapper"]["kind"],
            "unknown"
        );
    }
    #[test]
    fn mismatched_actual_geometry_and_tf_fail_before_context_exists() {
        let (p, mut o) = fixture();
        let name = p["camera_name"].as_str().unwrap();
        o["cameras"][name]["color"]["width"] = json!(1);
        assert!(bootstrap(&p, &o, &"a".repeat(64), &"b".repeat(32), 1).is_err());
        let (p, mut o) = fixture();
        o["cameras"][name]["depth_to_color"]["rotation"] = json!([1, 0, 0, 1]);
        assert!(bootstrap(&p, &o, &"a".repeat(64), &"b".repeat(32), 1).is_err());
    }
    #[test]
    fn formal_configuration_still_rejects_four_cameras() {
        let contracts: Value =
            serde_json::from_str(include_str!("../tests/rust/contracts.json")).unwrap();
        let mut d = contracts["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "three draft")
            .unwrap()["data"]
            .clone();
        let camera = d["cameras"][0].clone();
        d["cameras"].as_array_mut().unwrap().push(camera);
        assert!(
            config::validate(&d, true)
                .unwrap_err()
                .to_string()
                .starts_with("CAMERA_COUNT")
        );
    }
    #[test]
    fn bootstrap_requires_observed_schema_and_native_color_format() {
        let (p, o) = fixture();
        let mut invalid = p.clone();
        invalid["version"] = json!(2);
        assert!(
            bootstrap(&invalid, &o, &"a".repeat(64), &"b".repeat(32), 1)
                .unwrap_err()
                .to_string()
                .starts_with("OBSERVER_BOOTSTRAP_SCHEMA")
        );
        let mut inventory = p["inventory"].clone();
        inventory["cameras"][0]["parameters"]["enable_depth"] = json!(false);
        assert!(
            policy(&inventory, &inventory["cameras"][0], &"d".repeat(32))
                .unwrap_err()
                .to_string()
                .starts_with("OFFICIAL_OBSERVER_RGBD_REQUIRED")
        );
        for format in ["RGB", "MJPG"] {
            let mut candidate = p.clone();
            candidate["inventory"]["cameras"][0]["parameters"]["color_format"] = json!(format);
            assert!(
                bootstrap(&candidate, &o, &"a".repeat(64), &"b".repeat(32), 1)
                    .unwrap_err()
                    .to_string()
                    .starts_with("COLOR_FORMAT")
            );
        }
    }
}
