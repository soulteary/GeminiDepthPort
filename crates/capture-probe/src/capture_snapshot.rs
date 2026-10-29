//! Convert actual enabled SDK profile calibration; no model defaults or fake precision proof.
use crate::rgbd::Config;
use anyhow::{Context, Result, ensure};
use capture_contract::{snapshot::*, *};
use orbbec_safe::{SessionCalibration, StreamCalibration};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};
fn model(source: &StreamCalibration, frame: &str) -> Result<CameraModel> {
    let distortion_model = match source.distortion_model.as_str() {
        "none" => DistortionModel::None,
        "plumb_bob" => DistortionModel::PlumbBob,
        "rational_polynomial" => DistortionModel::RationalPolynomial,
        "equidistant" => DistortionModel::Equidistant,
        _ => anyhow::bail!("unsupported SDK distortion model"),
    };
    let m = CameraModel {
        frame_id: frame.into(),
        width: source.profile.width,
        height: source.profile.height,
        k: [
            source.fx, 0., source.cx, 0., source.fy, source.cy, 0., 0., 1.,
        ],
        distortion_model,
        distortion_coefficients: source.distortion.clone(),
    };
    m.validate()?;
    Ok(m)
}
pub(crate) struct RuntimeBinding<'a> {
    pub generation: &'a str,
    pub session: &'a str,
    pub instance: &'a str,
    pub runtime_sha: &'a str,
    pub runtime_contract: Option<&'a Value>,
}
pub(crate) fn build(
    config: &Config,
    calibration: &SessionCalibration,
    binding: RuntimeBinding<'_>,
    source_depth_scale_mm: f32,
) -> Result<SnapshotEnvelope> {
    let RuntimeBinding {
        generation,
        session,
        instance,
        runtime_sha,
        runtime_contract,
    } = binding;
    let session = SessionId::parse(session)?;
    let instance = InstanceId::parse(instance)?;
    ensure!(
        source_depth_scale_mm.is_finite() && source_depth_scale_mm > 0.,
        "invalid source depth scale"
    );
    let c = calibration
        .color
        .as_ref()
        .context("actual color calibration required")?;
    let d = calibration
        .depth
        .as_ref()
        .context("actual depth calibration required")?;
    ensure!(
        c.profile == config.color.profile && d.profile == config.depth.profile,
        "active SDK profiles differ from configuration"
    );
    let transform = calibration
        .depth_to_color
        .as_ref()
        .context("actual SDK extrinsics required")?;
    transform.validate()?;
    let record = CalibrationRecord {
        version: 1,
        source: CalibrationSource::SdkActiveProfiles,
        actual_runtime_observed: true,
        source_session_id: Some(session.clone()),
        source_instance_id: Some(instance.clone()),
        observed_unix_ns: Some(u64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        )?),
        models: BTreeMap::from([
            ("color".into(), model(c, &config.color.frame_id)?),
            ("depth".into(), model(d, &config.depth.frame_id)?),
        ]),
        transforms: vec![RigidTransform {
            source_frame: config.depth.frame_id.clone(),
            target_frame: config.color.frame_id.clone(),
            rotation_row_major: transform.rotation_row_major,
            translation_m: transform.translation_m,
        }],
        physical_accuracy_verified: false,
        legacy_source_sha256: None,
    };
    let configuration = json!({"capture":config.configuration,"runtime_config_sha256":runtime_sha,"runtime_artifacts":runtime_contract,"actual_color_profile":{"width":c.profile.width,"height":c.profile.height,"fps":c.profile.fps},"actual_depth_profile":{"width":d.profile.width,"height":d.profile.height,"fps":d.profile.fps},"source_depth_scale_mm":source_depth_scale_mm,"timestamp_mapping":config.message_timestamp().ros_stamp_source(),"acquisition_effective":config.acquisition,"sync_mode":"sdk_frameset_association_only"});
    let configuration_id = ConfigurationId::parse(content_digest(
        ContentKind::Configuration,
        &serde_json::to_vec(&configuration)?,
    )?)?;
    CaptureSnapshot {
        version: 1,
        binding: SnapshotBinding {
            instance_id: instance,
            session_id: session,
            generation_id: GenerationId::parse(generation)?,
            configuration_id,
            calibration_id: record.id()?,
        },
        configuration,
        calibration: record,
        depth: DepthRepresentation::Millimetres {
            quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
        },
        quality_policy: config
            .quality_policy
            .clone()
            .context("explicit quality policy required")?,
    }
    .seal()
    .map_err(Into::into)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::rgbd::StreamConfig;
    use frame_core::{PairClock, PairPolicy};
    use orbbec_safe::{Extrinsics, Format, StreamCalibration, VideoProfile};
    fn config() -> Config {
        let profile = VideoProfile {
            width: 2,
            height: 1,
            fps: 30,
        };
        Config {
            sdk_library: "/capture/lib/libOrbbecSDK.so".into(),
            serial: "observed_device".into(),
            color: StreamConfig {
                profile,
                frame_id: "color_optical".into(),
            },
            depth: StreamConfig {
                profile,
                frame_id: "depth_optical".into(),
            },
            normalize: true,
            acquisition: None,
            quality_policy: Some(QualityPolicy {
                version: 1,
                max_source_age_ns: 1_000_000_000,
                max_future_ns: 100_000_000,
                max_pair_delta_us: 20_000,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: QualityAction::ReportOnly,
            }),
            configuration: json!({"role":"front"}),
            policy: PairPolicy {
                clock: PairClock::Device,
                max_delta_us: 20_000,
            },
            seconds: 1.,
            poll_ms: 10,
            capacity: 2,
            startup_discovery_seconds: 0.,
            ros: None,
        }
    }
    fn calibration() -> SessionCalibration {
        let stream = |format| StreamCalibration {
            profile: VideoProfile {
                width: 2,
                height: 1,
                fps: 30,
            },
            format,
            fx: 1.,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            distortion_model: "none".into(),
            distortion: vec![],
        };
        SessionCalibration {
            color: Some(stream(Format::Yuyv)),
            depth: Some(stream(Format::Depth16)),
            depth_to_color: Some(Extrinsics {
                rotation_row_major: [0., -1., 0., 1., 0., 0., 0., 0., 1.],
                translation_m: [0.025, 0., 0.],
            }),
        }
    }
    #[test]
    fn actual_sdk_calibration_snapshot_keeps_units_source_and_false_precision_claim() {
        let cfg = config();
        let generation = "a".repeat(32);
        let session = "b".repeat(32);
        let instance = "c".repeat(64);
        let binding = || RuntimeBinding {
            generation: &generation,
            session: &session,
            instance: &instance,
            runtime_sha: "verified_runtime_hash",
            runtime_contract: None,
        };
        for scale in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(
                build(&cfg, &calibration(), binding(), scale)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid source depth scale")
            );
        }
        let s = build(&cfg, &calibration(), binding(), 0.1).unwrap();
        s.require_runtime().unwrap();
        assert_eq!(
            s.content.calibration.transforms[0].translation_m,
            [0.025, 0., 0.]
        );
        assert!(!s.content.calibration.physical_accuracy_verified);
        assert_eq!(
            s.content.calibration.source,
            CalibrationSource::SdkActiveProfiles
        );
        assert!(matches!(
            s.content.depth,
            DepthRepresentation::Millimetres { .. }
        ));
        let mut invalid = calibration();
        invalid.depth.as_mut().unwrap().profile.width = 4;
        assert!(build(&cfg, &invalid, binding(), 0.1).is_err());
        let mut invalid = calibration();
        invalid.depth.as_mut().unwrap().distortion_model = "unverified_approximation".into();
        assert!(build(&cfg, &invalid, binding(), 0.1).is_err());
    }
}
