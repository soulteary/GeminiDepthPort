//! Association of observed header timestamps is not SDK frameset provenance.
//! This crate receives decoded message facts, never opens a device or ROS node.
use capture_contract::{CameraModel, DistortionModel, FrameContext, MonoTime, SessionId};
use quality_core::{
    ClockMapper, Condition, FrameObservation, FrameViolation, QualityEngine, QualityQueryRequest,
    QualityQueryResponse, QualityResult, RuntimeQualityPolicy, Scope, SourceClock, SourceStamp,
    map_source_time, validate_source_age,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const OBSERVER_VERSION: u32 = 1;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamRole {
    Color,
    Depth,
}
impl StreamRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Color => "color",
            Self::Depth => "depth",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageEncoding {
    #[serde(rename = "yuyv")]
    Yuyv,
    #[serde(rename = "rgb8")]
    Rgb8,
    #[serde(rename = "bgr8")]
    Bgr8,
    #[serde(rename = "16UC1")]
    Depth16,
}
impl ImageEncoding {
    fn bytes_per_pixel(self) -> u64 {
        match self {
            Self::Rgb8 | Self::Bgr8 => 3,
            _ => 2,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedStream {
    pub context: FrameContext,
    pub encoding: ImageEncoding,
    pub camera: CameraModel,
    pub rectification: [f64; 9],
    pub projection: [f64; 12],
    pub camera_info_tolerance: f64,
    pub mapper: ClockMapper,
    pub metadata_required: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssociationPolicy {
    pub queue_capacity: usize,
    pub info_tolerance_ns: u64,
    pub metadata_tolerance_ns: u64,
    pub pair_tolerance_ns: u64,
    pub watermark_lateness_ns: u64,
    pub max_payload_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverConfig {
    pub version: u32,
    pub streams: BTreeMap<StreamRole, ExpectedStream>,
    pub association: AssociationPolicy,
    pub quality: RuntimeQualityPolicy,
}
impl ObserverConfig {
    pub fn validate(&self) -> QualityResult<()> {
        self.quality.validate()?;
        let p = &self.association;
        if self.version != 1
            || self.streams.len() != 2
            || !(1..=4096).contains(&p.queue_capacity)
            || p.pair_tolerance_ns == 0
            || p.max_payload_bytes == 0
            || p.max_payload_bytes > 512 * 1024 * 1024
            || [
                p.info_tolerance_ns,
                p.metadata_tolerance_ns,
                p.pair_tolerance_ns,
                p.watermark_lateness_ns,
            ]
            .iter()
            .any(|v| *v > 1_000_000_000_000)
            || self
                .quality
                .quality
                .max_pair_delta_us
                .checked_mul(1000)
                .is_none_or(|v| p.pair_tolerance_ns > v)
        {
            return Err("OBSERVER_POLICY_INVALID");
        }
        let mut session = None;
        for role in [StreamRole::Color, StreamRole::Depth] {
            let stream = self.streams.get(&role).ok_or("OBSERVER_STREAMS_REQUIRED")?;
            stream
                .context
                .validate()
                .map_err(|_| "OBSERVER_CONTEXT_INVALID")?;
            stream
                .camera
                .validate()
                .map_err(|_| "OBSERVER_CAMERA_INVALID")?;
            if stream.context.stream_id != role.as_str()
                || stream.context.sequence.is_some()
                || stream.context.device_timestamp_us.is_some()
                || stream.context.system_timestamp_us.is_some()
                || stream.context.global_timestamp_us.is_some()
                || stream.context.received_mono_offset_ns != 0
                || !stream.camera_info_tolerance.is_finite()
                || stream.camera_info_tolerance <= 0.0
                || stream
                    .rectification
                    .iter()
                    .chain(stream.projection.iter())
                    .any(|v| !v.is_finite())
                || !matches!(
                    stream.mapper,
                    ClockMapper::Unknown
                        | ClockMapper::DeclaredObservedUnixHeader
                        | ClockMapper::MeasuredObservedHeaderOffset { .. }
                )
            {
                return Err("OBSERVER_BINDING_INVALID");
            }
            if session
                .as_ref()
                .is_some_and(|old| old != &stream.context.session_id)
            {
                return Err("OBSERVER_SESSION_MISMATCH");
            }
            session = Some(stream.context.session_id.clone());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderStamp {
    pub sec: i64,
    pub nanosec: u32,
}
impl HeaderStamp {
    pub fn to_ns(self) -> QualityResult<u64> {
        if self.sec < 0 || self.sec > i64::from(i32::MAX) || self.nanosec >= 1_000_000_000 {
            return Err("HEADER_TIMESTAMP_INVALID");
        }
        let ns = self.sec as u64 * 1_000_000_000 + u64::from(self.nanosec);
        if ns == 0 {
            return Err("HEADER_TIMESTAMP_INVALID");
        }
        Ok(ns)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedImage {
    pub stamp: HeaderStamp,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub step: u32,
    pub encoding: ImageEncoding,
    pub is_bigendian: u8,
    pub payload_len: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedCameraInfo {
    pub stamp: HeaderStamp,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub distortion_model: String,
    pub k: [f64; 9],
    pub d: Vec<f64>,
    pub r: [f64; 9],
    pub p: [f64; 12],
    pub binning: [u32; 2],
    pub roi: [u32; 4],
    pub roi_do_rectify: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedMetadata {
    pub stamp: HeaderStamp,
    pub data: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectionReason {
    HeaderTimestampInvalid,
    TimeMappingInvalid,
    FrameContractInvalid,
    CameraInfoInvalid,
    MetadataUnsupported,
    MetadataInvalid,
    LateObservation,
    WatermarkRegression,
    QueueOverflow,
    CameraInfoMissing,
    CameraInfoAmbiguous,
    ImageCandidatesAmbiguous,
    MetadataMissing,
    MetadataAmbiguous,
    UnmatchedComponent,
    PairMissing,
    PairAmbiguous,
    ObservationWindowClosed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRejection {
    pub reason: RejectionReason,
    pub stream: Option<StreamRole>,
    pub observation_sequence: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataProvenance {
    Unobserved,
    PublisherReported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedFrame {
    pub observation_sequence: u64,
    pub context: FrameContext,
    pub observed_header_ns: u64,
    pub mapped_unix_ns: u64,
    pub metadata_provenance: MetadataProvenance,
    pub reported_source_session: Option<SessionId>,
    /// Exact retained input components selected by the Rust matcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_info_observation_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_observation_sequence: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingProvenance {
    ObservedTimestamp,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedAssociation {
    pub version: u32,
    pub association_sequence: u64,
    pub provenance: PairingProvenance,
    pub color: ObservedFrame,
    pub depth: ObservedFrame,
    pub delta_ns: u64,
    pub same_sdk_frameset: Option<bool>,
    pub hardware_sync_verified: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AssociationStats {
    pub images: u64,
    pub camera_infos: u64,
    pub metadata: u64,
    pub completed_frames: u64,
    pub pairs: u64,
    pub rejections: BTreeMap<RejectionReason, u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssociationUpdate {
    /// Input receipt token, including rejected inputs; never an SDK sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_observation_sequence: Option<u64>,
    pub version: u32,
    pub watermark_unix_ns: Option<u64>,
    pub pairs: Vec<ObservedAssociation>,
    pub rejections: Vec<ObservationRejection>,
}
impl AssociationUpdate {
    fn new(watermark: Option<u64>) -> Self {
        Self {
            version: 1,
            watermark_unix_ns: watermark,
            input_observation_sequence: None,
            pairs: vec![],
            rejections: vec![],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverQueryResponse {
    pub version: u32,
    pub quality: QualityQueryResponse,
    pub association: AssociationStats,
    pub watermark_unix_ns: Option<u64>,
}

#[derive(Clone)]
struct Item<T> {
    id: u64,
    stamp: u64,
    mapped: u64,
    at: MonoTime,
    value: T,
}
#[derive(Clone)]
struct ReportedMetadata {
    sequence: Option<u64>,
    device: Option<u64>,
    system: Option<u64>,
    global: Option<u64>,
    session: Option<SessionId>,
}
#[derive(Default)]
struct Queues {
    images: VecDeque<Item<DecodedImage>>,
    infos: VecDeque<Item<DecodedCameraInfo>>,
    metadata: VecDeque<Item<ReportedMetadata>>,
    complete: VecDeque<ObservedFrame>,
}
pub struct CaptureObserver {
    config: ObserverConfig,
    quality: QualityEngine,
    queues: BTreeMap<StreamRole, Queues>,
    stats: AssociationStats,
    sequence: u64,
    pair_sequence: u64,
    watermark: Option<u64>,
    now: MonoTime,
    finalized: bool,
}
impl CaptureObserver {
    pub fn new(config: ObserverConfig) -> QualityResult<Self> {
        config.validate()?;
        let session = config.streams[&StreamRole::Color]
            .context
            .session_id
            .clone();
        let quality = QualityEngine::new(
            config.quality.clone(),
            session.clone(),
            vec!["color".into(), "depth".into()],
        )?;
        Ok(Self {
            config,
            quality,
            queues: BTreeMap::from([
                (StreamRole::Color, Queues::default()),
                (StreamRole::Depth, Queues::default()),
            ]),
            stats: AssociationStats::default(),
            sequence: 0,
            pair_sequence: 0,
            watermark: None,
            now: MonoTime {
                session_id: session,
                offset_ns: 0,
            },
            finalized: false,
        })
    }
    fn time(&mut self, at: &MonoTime) -> QualityResult<()> {
        if self.finalized {
            return Err("OBSERVER_FINALIZED");
        }
        at.elapsed_since(&self.now)
            .map_err(|_| "OBSERVER_MONOTONIC_INVALID")?;
        self.now = at.clone();
        Ok(())
    }
    fn next_id(&mut self) -> QualityResult<u64> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("OBSERVATION_SEQUENCE_EXHAUSTED")?;
        Ok(self.sequence)
    }
    fn reject(
        &mut self,
        update: &mut AssociationUpdate,
        reason: RejectionReason,
        stream: Option<StreamRole>,
        id: Option<u64>,
    ) {
        *self.stats.rejections.entry(reason).or_default() += 1;
        update.rejections.push(ObservationRejection {
            reason,
            stream,
            observation_sequence: id,
        });
    }
    fn mapping(&self, role: StreamRole, stamp: HeaderStamp) -> QualityResult<(u64, u64)> {
        let raw = stamp.to_ns()?;
        let mapped = map_source_time(
            SourceStamp {
                clock: SourceClock::ObservedHeaderNanoseconds,
                value: raw,
            },
            self.config.streams[&role].mapper,
        )
        .map_err(|_| "TIME_MAPPING_INVALID")?;
        Ok((raw, mapped))
    }
    fn violation(
        &mut self,
        role: Option<StreamRole>,
        code: FrameViolation,
        condition: Condition,
    ) -> QualityResult<()> {
        self.quality.evaluate(
            self.now.clone(),
            role.map_or(Scope::Group, |role| Scope::Stream(role.as_str().into())),
            code,
            condition,
        )
    }
    fn late(&self, mapped: u64) -> bool {
        self.watermark.is_some_and(|watermark| mapped <= watermark)
    }
    fn image_valid(&self, role: StreamRole, image: &DecodedImage) -> bool {
        let expected = &self.config.streams[&role];
        let row = u64::from(image.width) * image.encoding.bytes_per_pixel();
        image.frame_id == expected.camera.frame_id
            && image.width == expected.camera.width
            && image.height == expected.camera.height
            && image.encoding == expected.encoding
            && image.is_bigendian <= 1
            && image.payload_len <= self.config.association.max_payload_bytes
            && u64::from(image.step) >= row
            && u64::from(image.step) * u64::from(image.height) == image.payload_len
            && (image.encoding != ImageEncoding::Yuyv
                || image.width % 2 == 0 && u64::from(image.step) == row)
            && (image.encoding != ImageEncoding::Depth16 || image.step % 2 == 0)
    }
    fn info_valid(&self, role: StreamRole, info: &DecodedCameraInfo) -> bool {
        let expected = &self.config.streams[&role];
        let tolerance = expected.camera_info_tolerance;
        let model = match info.distortion_model.as_str() {
            "plumb_bob" => DistortionModel::PlumbBob,
            "rational_polynomial" => DistortionModel::RationalPolynomial,
            "equidistant" => DistortionModel::Equidistant,
            "none" | "" => DistortionModel::None,
            _ => return false,
        };
        let camera = CameraModel {
            frame_id: info.frame_id.clone(),
            width: info.width,
            height: info.height,
            k: info.k,
            distortion_model: model,
            distortion_coefficients: info.d.clone(),
        };
        camera.validate().is_ok()
            && camera.frame_id == expected.camera.frame_id
            && camera.width == expected.camera.width
            && camera.height == expected.camera.height
            && camera.distortion_model == expected.camera.distortion_model
            && camera.distortion_coefficients.len() == expected.camera.distortion_coefficients.len()
            && info.binning.iter().all(|v| *v <= 1)
            && info.roi == [0; 4]
            && !info.roi_do_rectify
            && info
                .k
                .iter()
                .zip(expected.camera.k.iter())
                .chain(
                    info.d
                        .iter()
                        .zip(expected.camera.distortion_coefficients.iter()),
                )
                .chain(info.r.iter().zip(expected.rectification.iter()))
                .chain(info.p.iter().zip(expected.projection.iter()))
                .all(|(a, b)| a.is_finite() && (*a - *b).abs() <= tolerance)
    }
    pub fn image(
        &mut self,
        role: StreamRole,
        at: MonoTime,
        received_unix_ns: Option<u64>,
        image: DecodedImage,
    ) -> QualityResult<AssociationUpdate> {
        self.time(&at)?;
        let id = self.next_id()?;
        self.stats.images += 1;
        let mut update = AssociationUpdate::new(self.watermark);
        update.input_observation_sequence = Some(id);
        let valid = self.image_valid(role, &image);
        let mapped = self.mapping(role, image.stamp);
        let mut context = self.config.streams[&role].context.clone();
        context.received_mono_offset_ns = at.offset_ns;
        let raw = image.stamp.to_ns().ok();
        self.quality.record_frame_with_evidence(
            FrameObservation {
                context,
                contract_valid: valid,
                valid_fraction: None,
                zero_fraction: None,
                source: raw.map(|value| SourceStamp {
                    clock: SourceClock::ObservedHeaderNanoseconds,
                    value,
                }),
                mapper: self.config.streams[&role].mapper,
                received_unix_ns,
            },
            Condition::Unknown,
        )?;
        if !valid {
            self.reject(
                &mut update,
                RejectionReason::FrameContractInvalid,
                Some(role),
                Some(id),
            );
            return Ok(update);
        }
        let (stamp, mapped) = match mapped {
            Ok(v) => v,
            Err(code) => {
                self.reject(
                    &mut update,
                    if code == "HEADER_TIMESTAMP_INVALID" {
                        RejectionReason::HeaderTimestampInvalid
                    } else {
                        RejectionReason::TimeMappingInvalid
                    },
                    Some(role),
                    Some(id),
                );
                return Ok(update);
            }
        };
        if validate_source_age(
            SourceStamp {
                clock: SourceClock::ObservedHeaderNanoseconds,
                value: stamp,
            },
            self.config.streams[&role].mapper,
            received_unix_ns,
            &self.config.quality.quality,
        )
        .is_err()
        {
            self.reject(
                &mut update,
                RejectionReason::TimeMappingInvalid,
                Some(role),
                Some(id),
            );
            return Ok(update);
        }
        if self.late(mapped) {
            self.reject(
                &mut update,
                RejectionReason::LateObservation,
                Some(role),
                Some(id),
            );
            self.violation(
                Some(role),
                FrameViolation::TimeMappingInvalid,
                Condition::Violated,
            )?;
            return Ok(update);
        }
        let capacity = self.config.association.queue_capacity;
        let evicted = if self.queues[&role].images.len() == capacity {
            self.queues
                .get_mut(&role)
                .unwrap()
                .images
                .pop_front()
                .map(|v| v.id)
        } else {
            None
        };
        if let Some(old) = evicted {
            self.reject(
                &mut update,
                RejectionReason::QueueOverflow,
                Some(role),
                Some(old),
            );
            self.violation(None, FrameViolation::QueueOverflow, Condition::Violated)?;
        }
        self.queues.get_mut(&role).unwrap().images.push_back(Item {
            id,
            stamp,
            mapped,
            at,
            value: image,
        });
        Ok(update)
    }
    pub fn camera_info(
        &mut self,
        role: StreamRole,
        at: MonoTime,
        info: DecodedCameraInfo,
    ) -> QualityResult<AssociationUpdate> {
        self.time(&at)?;
        let id = self.next_id()?;
        self.stats.camera_infos += 1;
        let mut update = AssociationUpdate::new(self.watermark);
        update.input_observation_sequence = Some(id);
        let valid = self.info_valid(role, &info);
        if !valid {
            self.reject(
                &mut update,
                RejectionReason::CameraInfoInvalid,
                Some(role),
                Some(id),
            );
            self.violation(
                Some(role),
                FrameViolation::FrameContractInvalid,
                Condition::Violated,
            )?;
            return Ok(update);
        }
        let (stamp, mapped) = match self.mapping(role, info.stamp) {
            Ok(v) => v,
            Err(_) => {
                self.reject(
                    &mut update,
                    RejectionReason::HeaderTimestampInvalid,
                    Some(role),
                    Some(id),
                );
                return Ok(update);
            }
        };
        if self.late(mapped) {
            self.reject(
                &mut update,
                RejectionReason::LateObservation,
                Some(role),
                Some(id),
            );
            return Ok(update);
        }
        if self.queues[&role].infos.len() == self.config.association.queue_capacity {
            let old = self
                .queues
                .get_mut(&role)
                .unwrap()
                .infos
                .pop_front()
                .unwrap()
                .id;
            self.reject(
                &mut update,
                RejectionReason::QueueOverflow,
                Some(role),
                Some(old),
            );
            self.violation(None, FrameViolation::QueueOverflow, Condition::Violated)?;
        }
        self.queues.get_mut(&role).unwrap().infos.push_back(Item {
            id,
            stamp,
            mapped,
            at,
            value: info,
        });
        Ok(update)
    }
    pub fn metadata(
        &mut self,
        role: StreamRole,
        at: MonoTime,
        input: DecodedMetadata,
    ) -> QualityResult<AssociationUpdate> {
        self.time(&at)?;
        let id = self.next_id()?;
        self.stats.metadata += 1;
        let mut update = AssociationUpdate::new(self.watermark);
        update.input_observation_sequence = Some(id);
        let parsed = (|| -> QualityResult<ReportedMetadata> {
            if input.data.len() > 4096 {
                return Err("METADATA_INVALID");
            }
            let value = capture_contract::parse_strict(input.data.as_bytes())
                .map_err(|_| "METADATA_INVALID")?;
            if value.get("version").and_then(|v| v.as_u64()) != Some(1) {
                return Err("METADATA_UNSUPPORTED");
            }
            if value.get("frame_id").and_then(|v| v.as_str())
                != Some(self.config.streams[&role].camera.frame_id.as_str())
            {
                return Err("METADATA_INVALID");
            }
            let stamp = HeaderStamp {
                sec: value
                    .get("stamp_sec")
                    .and_then(|v| v.as_i64())
                    .ok_or("METADATA_UNSUPPORTED")?,
                nanosec: u32::try_from(
                    value
                        .get("stamp_nanosec")
                        .and_then(|v| v.as_u64())
                        .ok_or("METADATA_UNSUPPORTED")?,
                )
                .map_err(|_| "METADATA_INVALID")?,
            };
            if stamp != input.stamp {
                return Err("METADATA_INVALID");
            }
            let optional = |key: &str| -> QualityResult<Option<u64>> {
                match value.get(key) {
                    None | Some(serde_json::Value::Null) => Ok(None),
                    Some(v) => v.as_u64().map(Some).ok_or("METADATA_INVALID"),
                }
            };
            let session = match value.get("session_id") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(
                    SessionId::parse(v.as_str().ok_or("METADATA_INVALID")?)
                        .map_err(|_| "METADATA_INVALID")?,
                ),
            };
            let reported = ReportedMetadata {
                sequence: optional("sequence")?,
                device: optional("device_timestamp_us")?,
                system: optional("system_timestamp_us")?,
                global: optional("global_timestamp_us")?,
                session,
            };
            // An explicit SDK mapping must describe this header exactly. Keep
            // capture and host-receipt times separate; neither is sync proof.
            let mapped = match value.get("stamp_mapping") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) if v == "sdk-system-unix" => Some(reported.system),
                Some(v) if v == "sdk-global-unix" => Some(reported.global),
                Some(_) => return Err("METADATA_UNSUPPORTED"),
            };
            if let Some(mapped) = mapped {
                let source = mapped.filter(|v| *v > 0).ok_or("METADATA_INVALID")?;
                if source.checked_mul(1000) != Some(stamp.to_ns()?) {
                    return Err("METADATA_INVALID");
                }
            }
            if reported.global == Some(0) {
                return Err("METADATA_INVALID");
            }
            Ok(reported)
        })();
        let metadata = match parsed {
            Ok(v) => v,
            Err(code) => {
                self.reject(
                    &mut update,
                    if code == "METADATA_UNSUPPORTED" {
                        RejectionReason::MetadataUnsupported
                    } else {
                        RejectionReason::MetadataInvalid
                    },
                    Some(role),
                    Some(id),
                );
                if self.config.streams[&role].metadata_required {
                    self.violation(
                        Some(role),
                        FrameViolation::FrameContractInvalid,
                        Condition::Violated,
                    )?;
                }
                return Ok(update);
            }
        };
        let (stamp, mapped) = match self.mapping(role, input.stamp) {
            Ok(v) => v,
            Err(_) => {
                self.reject(
                    &mut update,
                    RejectionReason::HeaderTimestampInvalid,
                    Some(role),
                    Some(id),
                );
                return Ok(update);
            }
        };
        if self.late(mapped) {
            self.reject(
                &mut update,
                RejectionReason::LateObservation,
                Some(role),
                Some(id),
            );
            return Ok(update);
        }
        if self.queues[&role].metadata.len() == self.config.association.queue_capacity {
            let old = self
                .queues
                .get_mut(&role)
                .unwrap()
                .metadata
                .pop_front()
                .unwrap()
                .id;
            self.reject(
                &mut update,
                RejectionReason::QueueOverflow,
                Some(role),
                Some(old),
            );
            self.violation(None, FrameViolation::QueueOverflow, Condition::Violated)?;
        }
        self.queues
            .get_mut(&role)
            .unwrap()
            .metadata
            .push_back(Item {
                id,
                stamp,
                mapped,
                at,
                value: metadata,
            });
        Ok(update)
    }
    pub fn tick(
        &mut self,
        at: MonoTime,
        received_unix_ns: Option<u64>,
    ) -> QualityResult<AssociationUpdate> {
        self.time(&at)?;
        self.quality.tick(at.clone())?;
        match received_unix_ns {
            Some(now) => self.advance_watermark(
                at,
                now.saturating_sub(self.config.association.watermark_lateness_ns),
            ),
            None => {
                self.violation(
                    None,
                    FrameViolation::TimeMappingInvalid,
                    Condition::Violated,
                )?;
                Ok(AssociationUpdate::new(self.watermark))
            }
        }
    }
    pub fn advance_watermark(
        &mut self,
        at: MonoTime,
        unix_ns: u64,
    ) -> QualityResult<AssociationUpdate> {
        self.time(&at)?;
        let mut update = AssociationUpdate::new(self.watermark);
        if self.watermark.is_some_and(|old| unix_ns < old) {
            self.reject(
                &mut update,
                RejectionReason::WatermarkRegression,
                None,
                None,
            );
            self.violation(
                None,
                FrameViolation::TimeMappingInvalid,
                Condition::Violated,
            )?;
            return Ok(update);
        }
        self.watermark = Some(unix_ns);
        update.watermark_unix_ns = self.watermark;
        for role in [StreamRole::Color, StreamRole::Depth] {
            self.close_frames(role, &mut update)?;
        }
        self.close_pairs(&mut update)?;
        Ok(update)
    }
    fn close_frames(
        &mut self,
        role: StreamRole,
        update: &mut AssociationUpdate,
    ) -> QualityResult<()> {
        let wm = self.watermark.unwrap();
        let policy = &self.config.association;
        let tolerance = policy.info_tolerance_ns.max(policy.metadata_tolerance_ns);
        let queues = &self.queues[&role];
        let nodes = queues
            .images
            .iter()
            .map(|v| Node {
                kind: 0,
                id: v.id,
                time: v.mapped,
            })
            .chain(queues.infos.iter().map(|v| Node {
                kind: 1,
                id: v.id,
                time: v.mapped,
            }))
            .chain(queues.metadata.iter().map(|v| Node {
                kind: 2,
                id: v.id,
                time: v.mapped,
            }))
            .collect::<Vec<_>>();
        let components = components(&nodes, |a, b| {
            let (image, other) = if a.kind == 0 { (a, b) } else { (b, a) };
            image.kind == 0
                && other.kind != 0
                && image.time.abs_diff(other.time)
                    <= if other.kind == 1 {
                        policy.info_tolerance_ns
                    } else {
                        policy.metadata_tolerance_ns
                    }
        });
        let mut plans = vec![];
        for component in components {
            if component
                .iter()
                .map(|i| nodes[*i].time)
                .max()
                .unwrap()
                .checked_add(tolerance)
                .is_none_or(|end| end > wm)
            {
                continue;
            }
            let ids = component.iter().map(|i| nodes[*i]).collect::<Vec<_>>();
            let images = ids.iter().filter(|v| v.kind == 0).count();
            let infos = ids.iter().filter(|v| v.kind == 1).count();
            let metadata = ids.iter().filter(|v| v.kind == 2).count();
            let reason = if images == 0 {
                Some(RejectionReason::UnmatchedComponent)
            } else if images > 1 {
                Some(RejectionReason::ImageCandidatesAmbiguous)
            } else if infos == 0 {
                Some(RejectionReason::CameraInfoMissing)
            } else if infos > 1 {
                Some(RejectionReason::CameraInfoAmbiguous)
            } else if metadata > 1 {
                Some(RejectionReason::MetadataAmbiguous)
            } else if metadata == 0 && self.config.streams[&role].metadata_required {
                Some(RejectionReason::MetadataMissing)
            } else {
                None
            };
            let info_id = ids.iter().find(|v| v.kind == 1).map(|v| v.id);
            let image = ids
                .iter()
                .find(|v| v.kind == 0)
                .and_then(|id| queues.images.iter().find(|item| item.id == id.id))
                .cloned();
            let meta = ids
                .iter()
                .find(|v| v.kind == 2)
                .and_then(|id| queues.metadata.iter().find(|item| item.id == id.id))
                .cloned();
            plans.push((ids, image, meta, info_id, reason));
        }
        for (ids, image, metadata, info_id, reason) in plans {
            let set = ids.iter().map(|v| v.id).collect::<BTreeSet<_>>();
            let queues = self.queues.get_mut(&role).unwrap();
            queues.images.retain(|v| !set.contains(&v.id));
            queues.infos.retain(|v| !set.contains(&v.id));
            queues.metadata.retain(|v| !set.contains(&v.id));
            if let Some(reason) = reason {
                for item in ids {
                    self.reject(update, reason, Some(role), Some(item.id));
                }
                self.violation(
                    Some(role),
                    FrameViolation::FrameContractInvalid,
                    Condition::Violated,
                )?;
                continue;
            }
            self.violation(
                Some(role),
                FrameViolation::FrameContractInvalid,
                Condition::Healthy,
            )?;
            let image = image.expect("a valid component has one image");
            let mut context = self.config.streams[&role].context.clone();
            context.received_mono_offset_ns = image.at.offset_ns;
            if let Some(meta) = &metadata {
                context.sequence = meta.value.sequence;
                context.device_timestamp_us = meta.value.device;
                context.system_timestamp_us = meta.value.system;
                context.global_timestamp_us = meta.value.global;
            }
            let frame = ObservedFrame {
                observation_sequence: image.id,
                camera_info_observation_sequence: info_id,
                metadata_observation_sequence: metadata.as_ref().map(|meta| meta.id),
                context,
                observed_header_ns: image.stamp,
                mapped_unix_ns: image.mapped,
                metadata_provenance: if metadata.is_some() {
                    MetadataProvenance::PublisherReported
                } else {
                    MetadataProvenance::Unobserved
                },
                reported_source_session: metadata.and_then(|meta| meta.value.session),
            };
            if self.queues[&role].complete.len() == self.config.association.queue_capacity {
                let old = self
                    .queues
                    .get_mut(&role)
                    .unwrap()
                    .complete
                    .pop_front()
                    .unwrap()
                    .observation_sequence;
                self.reject(
                    update,
                    RejectionReason::QueueOverflow,
                    Some(role),
                    Some(old),
                );
                self.violation(None, FrameViolation::QueueOverflow, Condition::Violated)?;
            }
            self.queues
                .get_mut(&role)
                .unwrap()
                .complete
                .push_back(frame);
            self.stats.completed_frames += 1;
        }
        Ok(())
    }
    fn close_pairs(&mut self, update: &mut AssociationUpdate) -> QualityResult<()> {
        let wm = self.watermark.unwrap();
        let policy = &self.config.association;
        let tolerance = policy.pair_tolerance_ns;
        let closing = tolerance + policy.info_tolerance_ns.max(policy.metadata_tolerance_ns) * 2;
        let frames = self.queues[&StreamRole::Color]
            .complete
            .iter()
            .map(|v| (StreamRole::Color, v.clone()))
            .chain(
                self.queues[&StreamRole::Depth]
                    .complete
                    .iter()
                    .map(|v| (StreamRole::Depth, v.clone())),
            )
            .collect::<Vec<_>>();
        let nodes = frames
            .iter()
            .map(|(role, v)| Node {
                kind: if *role == StreamRole::Color { 0 } else { 1 },
                id: v.observation_sequence,
                time: v.mapped_unix_ns,
            })
            .collect::<Vec<_>>();
        let groups = components(&nodes, |a, b| {
            a.kind != b.kind && a.time.abs_diff(b.time) <= tolerance
        });
        for group in groups {
            if group
                .iter()
                .map(|i| nodes[*i].time)
                .max()
                .unwrap()
                .checked_add(closing)
                .is_none_or(|end| end > wm)
            {
                continue;
            }
            let selected = group
                .iter()
                .map(|index| frames[*index].clone())
                .collect::<Vec<_>>();
            let ids = selected
                .iter()
                .map(|(_, frame)| frame.observation_sequence)
                .collect::<BTreeSet<_>>();
            for queue in self.queues.values_mut() {
                queue
                    .complete
                    .retain(|frame| !ids.contains(&frame.observation_sequence));
            }
            let colors = selected
                .iter()
                .filter(|(role, _)| *role == StreamRole::Color)
                .count();
            let depths = selected.len() - colors;
            if colors != 1 || depths != 1 {
                let reason = if colors == 0 || depths == 0 {
                    RejectionReason::PairMissing
                } else {
                    RejectionReason::PairAmbiguous
                };
                for (role, frame) in selected {
                    self.reject(update, reason, Some(role), Some(frame.observation_sequence));
                }
                self.violation(
                    None,
                    FrameViolation::FrameContractInvalid,
                    Condition::Violated,
                )?;
                continue;
            }
            let color = selected
                .iter()
                .find(|(role, _)| *role == StreamRole::Color)
                .unwrap()
                .1
                .clone();
            let depth = selected
                .into_iter()
                .find(|(role, _)| *role == StreamRole::Depth)
                .unwrap()
                .1;
            self.pair_sequence = self
                .pair_sequence
                .checked_add(1)
                .ok_or("ASSOCIATION_SEQUENCE_EXHAUSTED")?;
            let delta_ns = color.mapped_unix_ns.abs_diff(depth.mapped_unix_ns);
            self.quality
                .observe_pair_delta(self.now.clone(), delta_ns.div_ceil(1000))?;
            self.violation(
                None,
                FrameViolation::FrameContractInvalid,
                Condition::Healthy,
            )?;
            update.pairs.push(ObservedAssociation {
                version: 1,
                association_sequence: self.pair_sequence,
                provenance: PairingProvenance::ObservedTimestamp,
                color,
                depth,
                delta_ns,
                same_sdk_frameset: None,
                hardware_sync_verified: false,
            });
            self.stats.pairs += 1;
        }
        Ok(())
    }
    pub fn query(&self, request: QualityQueryRequest) -> QualityResult<ObserverQueryResponse> {
        Ok(ObserverQueryResponse {
            version: 1,
            quality: self.quality.query(request)?,
            association: self.stats.clone(),
            watermark_unix_ns: self.watermark,
        })
    }
    pub fn interrupt(
        &mut self,
        at: MonoTime,
        reason: quality_core::InterruptionReason,
    ) -> QualityResult<()> {
        self.time(&at)?;
        self.quality.interrupt(at, reason)
    }
    pub fn finalize(&mut self) -> QualityResult<AssociationUpdate> {
        if self.finalized {
            return Err("OBSERVER_FINALIZED");
        }
        let mut update = AssociationUpdate::new(self.watermark);
        for role in [StreamRole::Color, StreamRole::Depth] {
            let queues = self.queues.get_mut(&role).unwrap();
            let ids = queues
                .images
                .iter()
                .map(|v| v.id)
                .chain(queues.infos.iter().map(|v| v.id))
                .chain(queues.metadata.iter().map(|v| v.id))
                .chain(queues.complete.iter().map(|v| v.observation_sequence))
                .collect::<Vec<_>>();
            queues.images.clear();
            queues.infos.clear();
            queues.metadata.clear();
            queues.complete.clear();
            for id in ids {
                self.reject(
                    &mut update,
                    RejectionReason::ObservationWindowClosed,
                    Some(role),
                    Some(id),
                );
            }
        }
        self.quality.finalize_session();
        self.finalized = true;
        Ok(update)
    }
}

#[derive(Clone, Copy)]
struct Node {
    kind: u8,
    id: u64,
    time: u64,
}
fn components(nodes: &[Node], edge: impl Fn(&Node, &Node) -> bool) -> Vec<Vec<usize>> {
    let mut visited = vec![false; nodes.len()];
    let mut result = vec![];
    let mut order = (0..nodes.len()).collect::<Vec<_>>();
    order.sort_by_key(|i| (nodes[*i].time, nodes[*i].kind, nodes[*i].id));
    for first in order {
        if visited[first] {
            continue;
        }
        visited[first] = true;
        let mut group = vec![first];
        let mut cursor = 0;
        while cursor < group.len() {
            let current = group[cursor];
            for next in 0..nodes.len() {
                if !visited[next] && edge(&nodes[current], &nodes[next]) {
                    visited[next] = true;
                    group.push(next);
                }
            }
            cursor += 1;
        }
        result.push(group);
    }
    result
}
