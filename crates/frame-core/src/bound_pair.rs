//! Versioned evidence keeps source clocks and layouts alongside owned pixels.
//! Observed ROS timestamp associations are deliberately a different contract.
use crate::{DepthUnitPolicy, PairAcceptance};
use crate::{
    FrameEncoding, FramePacket, PacketMetadata, PairClock, PairEvidence, PairGuard, PairPolicy,
    PairProvenance, RgbdPair,
};
use capture_contract::{FrameContext, snapshot::SnapshotEnvelope};
use serde::{Deserialize, Serialize};
use std::time::Instant;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameEvidence {
    pub context: FrameContext,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub source_depth_scale_mm: Option<f32>,
    pub output_depth_scale_mm: Option<f32>,
    pub normalized: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundPairEvidence {
    pub version: u32,
    pub pair_sequence: u64,
    pub color: FrameEvidence,
    pub depth: FrameEvidence,
    pub color_stamp_us: u64,
    pub depth_stamp_us: u64,
    pub clock_source: String,
    pub delta_us: u64,
    pub association: String,
    pub hardware_sync_verified: bool,
}
impl BoundPairEvidence {
    pub fn validate(&self) -> Result<(), BoundPairError> {
        let c = &self.color.context;
        let d = &self.depth.context;
        c.validate().map_err(|_| BoundPairError::Context)?;
        d.validate().map_err(|_| BoundPairError::Context)?;
        if self.version != 1
            || self.pair_sequence == 0
            || self.association != "sdk_frameset"
            || self.hardware_sync_verified
            || !matches!(self.clock_source.as_str(), "device_us" | "sdk_system_us")
        {
            return Err(BoundPairError::Context);
        }
        if c.session_id != d.session_id
            || c.generation_id != d.generation_id
            || c.configuration_id != d.configuration_id
            || c.calibration_id != d.calibration_id
            || c.snapshot_id != d.snapshot_id
            || c.stream_id == d.stream_id
        {
            return Err(BoundPairError::Binding);
        }
        let stamps = if self.clock_source == "device_us" {
            (c.device_timestamp_us, d.device_timestamp_us)
        } else {
            (c.system_timestamp_us, d.system_timestamp_us)
        };
        if stamps != (Some(self.color_stamp_us), Some(self.depth_stamp_us))
            || self.color_stamp_us == 0
            || self.depth_stamp_us == 0
            || self.delta_us != self.color_stamp_us.abs_diff(self.depth_stamp_us)
            || c.sequence.is_none()
            || d.sequence.is_none()
        {
            return Err(BoundPairError::Context);
        }
        for (frame, encoding) in [(&self.color, "yuyv"), (&self.depth, "depth16")] {
            if frame.encoding != encoding
                || frame.width == 0
                || frame.height == 0
                || frame.stride
                    < (frame.width as usize)
                        .checked_mul(2)
                        .ok_or(BoundPairError::Context)?
                || frame.stride.checked_mul(frame.height as usize).is_none()
            {
                return Err(BoundPairError::Context);
            }
        }
        if self.color.source_depth_scale_mm.is_some()
            || self.color.output_depth_scale_mm.is_some()
            || self.color.normalized
            || !self
                .depth
                .source_depth_scale_mm
                .is_some_and(|s| s.is_finite() && s > 0.)
            || !self
                .depth
                .output_depth_scale_mm
                .is_some_and(|s| s.is_finite() && s > 0.)
            || if self.depth.normalized {
                self.depth.output_depth_scale_mm != Some(1.)
                    || self.depth.stride != self.depth.width as usize * 2
            } else {
                self.depth.source_depth_scale_mm != self.depth.output_depth_scale_mm
            }
        {
            return Err(BoundPairError::Context);
        }
        Ok(())
    }
}
#[derive(Debug)]
pub enum BoundPairError {
    Snapshot,
    Binding,
    Context,
    ReceiveClock,
    Pair(crate::PairError),
}
impl std::fmt::Display for BoundPairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BoundPairError {}
pub struct BoundPair<T, U> {
    pub pair: RgbdPair<T, U>,
    pub evidence: BoundPairEvidence,
}
/// One immutable snapshot and one receive-clock origin per acquisition session.
pub struct BoundPairGuard {
    snapshot: SnapshotEnvelope,
    origin: Instant,
    guard: PairGuard,
}
impl BoundPairGuard {
    pub fn new(snapshot: SnapshotEnvelope, origin: Instant) -> Result<Self, BoundPairError> {
        snapshot
            .require_runtime()
            .map_err(|_| BoundPairError::Snapshot)?;
        Ok(Self {
            snapshot,
            origin,
            guard: PairGuard::default(),
        })
    }
    pub fn snapshot_id(&self) -> &capture_contract::SnapshotId {
        &self.snapshot.snapshot_id
    }
    fn frame(&self, m: &PacketMetadata, stream: &str) -> Result<FrameEvidence, BoundPairError> {
        if stream == "depth" {
            if let Some(expected) = self
                .snapshot
                .content
                .configuration
                .get("source_depth_scale_mm")
            {
                if m.source_depth_scale_mm
                    .and_then(|v| serde_json::to_value(v).ok())
                    .as_ref()
                    != Some(expected)
                {
                    return Err(BoundPairError::Binding);
                }
            }
        }
        let model = self
            .snapshot
            .content
            .calibration
            .models
            .get(stream)
            .ok_or(BoundPairError::Binding)?;
        if model.width != m.layout.width || model.height != m.layout.height {
            return Err(BoundPairError::Binding);
        }
        let ns = m
            .received_at
            .checked_duration_since(self.origin)
            .ok_or(BoundPairError::ReceiveClock)?
            .as_nanos();
        let b = &self.snapshot.content.binding;
        Ok(FrameEvidence {
            context: FrameContext {
                version: 1,
                session_id: b.session_id.clone(),
                generation_id: b.generation_id.clone(),
                configuration_id: b.configuration_id.clone(),
                calibration_id: b.calibration_id.clone(),
                snapshot_id: self.snapshot.snapshot_id.clone(),
                stream_id: stream.into(),
                sequence: Some(m.sequence),
                device_timestamp_us: m.device_timestamp_us,
                system_timestamp_us: m.system_timestamp_us,
                global_timestamp_us: m.global_timestamp_us,
                received_mono_offset_ns: u64::try_from(ns)
                    .map_err(|_| BoundPairError::ReceiveClock)?,
            },
            encoding: match m.encoding {
                FrameEncoding::Yuyv => "yuyv",
                FrameEncoding::Depth16 => "depth16",
            }
            .into(),
            width: m.layout.width,
            height: m.layout.height,
            stride: m.layout.stride,
            source_depth_scale_mm: m.source_depth_scale_mm,
            output_depth_scale_mm: m.output_depth_scale_mm,
            normalized: m.normalized,
        })
    }
    /// Evidence errors are checked before the stateful guard commits either stream.
    pub fn accept<T, U>(
        &mut self,
        sequence: u64,
        color: Option<FramePacket<T>>,
        depth: Option<FramePacket<U>>,
        lengths: (usize, usize),
        policy: PairPolicy,
        provenance: PairProvenance,
    ) -> Result<BoundPair<T, U>, BoundPairError> {
        if sequence == 0 {
            return Err(BoundPairError::Context);
        }
        let c = color
            .as_ref()
            .map(|p| self.frame(&p.metadata, "color"))
            .transpose()?;
        let d = depth
            .as_ref()
            .map(|p| self.frame(&p.metadata, "depth"))
            .transpose()?;
        if let Some(frame) = &d {
            if let capture_contract::DepthRepresentation::SdkCounts { scale_mm } =
                self.snapshot.content.depth
            {
                if frame.output_depth_scale_mm.map(f64::from) != Some(scale_mm) {
                    return Err(BoundPairError::Binding);
                }
            }
        }
        let pair = self
            .guard
            .accept_with_policy(
                sequence,
                color,
                depth,
                lengths,
                PairAcceptance {
                    pairing: policy,
                    provenance,
                    depth_unit: match self.snapshot.content.depth {
                        capture_contract::DepthRepresentation::SdkCounts { .. } => {
                            DepthUnitPolicy::PreserveSdkCounts
                        }
                        _ => DepthUnitPolicy::RequireMillimetres,
                    },
                },
            )
            .map_err(BoundPairError::Pair)?;
        let e: PairEvidence = pair.evidence;
        // frame()/PairGuard jointly establish all validate() invariants; no new fallible operation after commit.
        let evidence = BoundPairEvidence {
            version: 1,
            pair_sequence: e.pair_sequence,
            color: c.expect("accepted color"),
            depth: d.expect("accepted depth"),
            color_stamp_us: e.color_timestamp_us,
            depth_stamp_us: e.depth_timestamp_us,
            clock_source: match e.clock {
                PairClock::Device => "device_us",
                PairClock::System => "sdk_system_us",
            }
            .into(),
            delta_us: e.delta_us,
            association: "sdk_frameset".into(),
            hardware_sync_verified: false,
        };
        debug_assert!(evidence.validate().is_ok());
        Ok(BoundPair { pair, evidence })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameLayout, PixelFormat};
    use capture_contract::{snapshot::*, *};
    fn snapshot(native: bool) -> SnapshotEnvelope {
        let session = SessionId::parse("a".repeat(32)).unwrap();
        let instance = InstanceId::parse("b".repeat(64)).unwrap();
        let model = |id: &str| CameraModel {
            frame_id: id.into(),
            width: 2,
            height: 1,
            k: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            distortion_model: DistortionModel::None,
            distortion_coefficients: vec![],
        };
        let calibration = CalibrationRecord {
            version: 1,
            source: CalibrationSource::SdkActiveProfiles,
            actual_runtime_observed: true,
            source_session_id: Some(session.clone()),
            source_instance_id: Some(instance.clone()),
            observed_unix_ns: Some(1),
            models: std::collections::BTreeMap::from([
                ("color".into(), model("color_optical")),
                ("depth".into(), model("depth_optical")),
            ]),
            transforms: vec![],
            physical_accuracy_verified: false,
            legacy_source_sha256: None,
        };
        let config = serde_json::json!({"test":1});
        let binding = SnapshotBinding {
            instance_id: instance,
            session_id: session,
            generation_id: GenerationId::parse("c".repeat(32)).unwrap(),
            configuration_id: ConfigurationId::parse(
                content_digest(
                    ContentKind::Configuration,
                    &serde_json::to_vec(&config).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
            calibration_id: calibration.id().unwrap(),
        };
        CaptureSnapshot {
            version: 1,
            binding,
            configuration: config,
            calibration,
            depth: if native {
                DepthRepresentation::SdkCounts {
                    scale_mm: f64::from(0.1_f32),
                }
            } else {
                DepthRepresentation::Millimetres {
                    quantization:
                        QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
                }
            },
            quality_policy: QualityPolicy {
                version: 1,
                max_source_age_ns: 1,
                max_future_ns: 0,
                max_pair_delta_us: 5,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: QualityAction::ReportOnly,
            },
        }
        .seal()
        .unwrap()
    }
    fn packet(
        depth: bool,
        seq: u64,
        stamp: u64,
        received: Instant,
        native: bool,
    ) -> FramePacket<Vec<u8>> {
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
                sequence: seq,
                device_timestamp_us: Some(stamp),
                system_timestamp_us: Some(stamp + 1000),
                global_timestamp_us: Some(stamp + 900),
                sdk_return_unix_ns: None,
                sdk_system_clock_verified: false,

                source_depth_scale_mm: depth.then_some(if native { 0.1 } else { 1. }),
                output_depth_scale_mm: depth.then_some(if native { 0.1 } else { 1. }),
                normalized: false,
                received_at: received,
            },
            payload: vec![0, 0, 1, 0],
        }
    }
    #[test]
    fn native_and_millimetres_keep_original_clocks_and_whole_queue_identity() {
        for native in [false, true] {
            let origin = Instant::now();
            let mut g = BoundPairGuard::new(snapshot(native), origin).unwrap();
            let bound = g
                .accept(
                    1,
                    Some(packet(false, 20, 100, origin, native)),
                    Some(packet(true, 30, 101, origin, native)),
                    (4, 4),
                    PairPolicy {
                        clock: PairClock::Device,
                        max_delta_us: 5,
                    },
                    PairProvenance::CallerObservedSdkFrameset,
                )
                .unwrap();
            bound.evidence.validate().unwrap();
            assert_eq!(bound.evidence.depth.context.system_timestamp_us, Some(1101));
            assert_eq!(bound.evidence.depth.context.global_timestamp_us, Some(1001));
            assert_eq!(bound.evidence.depth_stamp_us, 101);
            assert_eq!(bound.evidence.color.context.sequence, Some(20));
            let id = bound.evidence.depth.context.snapshot_id.clone();
            let mut q = crate::LatestQueue::new(1).unwrap();
            q.push(bound);
            assert_eq!(q.pop().unwrap().evidence.depth.context.snapshot_id, id);
        }
    }
    #[test]
    fn identity_receive_clock_and_pair_failure_do_not_advance() {
        let origin = Instant::now();
        let mut g = BoundPairGuard::new(snapshot(false), origin).unwrap();
        let policy = PairPolicy {
            clock: PairClock::Device,
            max_delta_us: 5,
        };
        assert!(
            g.accept(
                1,
                Some(packet(
                    false,
                    20,
                    100,
                    origin - std::time::Duration::from_nanos(1),
                    false
                )),
                Some(packet(true, 30, 101, origin, false)),
                (4, 4),
                policy,
                PairProvenance::CallerObservedSdkFrameset
            )
            .is_err()
        );
        let mut bound = g
            .accept(
                1,
                Some(packet(false, 20, 100, origin, false)),
                Some(packet(true, 30, 101, origin, false)),
                (4, 4),
                policy,
                PairProvenance::CallerObservedSdkFrameset,
            )
            .unwrap();
        bound.evidence.depth.context.session_id = SessionId::parse("b".repeat(32)).unwrap();
        assert!(bound.evidence.validate().is_err());
        bound.evidence.depth.context = bound.evidence.color.context.clone();
        assert!(bound.evidence.validate().is_err());
    }
}
