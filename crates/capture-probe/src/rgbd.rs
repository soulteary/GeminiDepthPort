//! Single-Pipeline RGB-D experiment. Frameset association is not exposure sync.
use anyhow::{Context, Result, ensure};
use capture_contract::{
    AcquisitionPolicy, FrameAggregate, MessageTimestamp, ResolvedAcquisition, UnpairedPolicy,
};
use frame_core::{
    FrameEncoding, FrameLayout, FramePacket, LatestQueue, PacketMetadata, PairClock, PairGuard,
    PairPolicy, PairProvenance, PixelFormat, RgbdPair, StampGuard, sdk_system_unix_stamp,
};
use orbbec_safe::{Format, Frame, FrameMetadata, VideoProfile};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) struct StreamConfig {
    pub(crate) profile: VideoProfile,
    pub(crate) frame_id: String,
}
pub(crate) struct RosConfig {
    pub(crate) bridge: String,
    pub(crate) namespace: String,
}
pub(crate) struct Config {
    pub(crate) imu: Option<capture_contract::imu::ImuConfig>,
    pub(crate) sdk_library: String,
    pub(crate) serial: String,
    pub(crate) quality_policy: Option<capture_contract::QualityPolicy>,
    pub(crate) acquisition: Option<ResolvedAcquisition>,
    pub(crate) configuration: Value,
    pub(crate) color: StreamConfig,
    pub(crate) depth: StreamConfig,
    pub(crate) normalize: bool,
    pub(crate) policy: PairPolicy,
    pub(crate) seconds: f64,
    pub(crate) poll_ms: u32,
    pub(crate) capacity: usize,
    pub(crate) startup_discovery_seconds: f64,
    pub(crate) ros: Option<RosConfig>,
}
fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let object = value.as_object().context("object required")?;
    ensure!(
        required.iter().all(|k| object.contains_key(*k))
            && object
                .keys()
                .all(|k| required.contains(&k.as_str()) || optional.contains(&k.as_str())),
        "unexpected/missing config field"
    );
    Ok(())
}
fn text(value: &Value, key: &str) -> Result<String> {
    Ok(value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("nonempty string required")?
        .into())
}
fn integer(value: &Value, key: &str) -> Result<u32> {
    Ok(u32::try_from(
        value[key].as_u64().context("unsigned integer required")?,
    )?)
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || i > 0 && c.is_ascii_digit())
}
fn stream(value: &Value, depth: bool) -> Result<StreamConfig> {
    fields(
        value,
        &["width", "height", "fps", "frame_id"],
        if depth { &["unit_policy"] } else { &[] },
    )?;
    let profile = VideoProfile {
        width: integer(value, "width")?,
        height: integer(value, "height")?,
        fps: integer(value, "fps")?,
    };
    ensure!(
        (1..=16384).contains(&profile.width)
            && (1..=16384).contains(&profile.height)
            && (1..=1000).contains(&profile.fps),
        "profile limit"
    );
    if !depth {
        ensure!(profile.width % 2 == 0, "YUYV even width required");
    }
    let frame_id = text(value, "frame_id")?;
    ensure!(token(&frame_id), "invalid optical frame");
    Ok(StreamConfig { profile, frame_id })
}
impl Config {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        ensure!(raw.len() <= 64 * 1024, "config size limit");
        let value: Value = capture_contract::parse_strict(raw.as_bytes())?;
        fields(
            &value,
            &[
                "version",
                "sdk_library",
                "serial",
                "color",
                "depth",
                "pairing",
                "seconds",
                "poll_ms",
                "capacity",
            ],
            &[
                "ros",
                "startup_discovery_seconds",
                "quality_policy",
                "acquisition",
                "imu",
            ],
        )?;
        ensure!(value["version"].as_u64() == Some(1), "config version");
        let sdk_library = text(&value, "sdk_library")?;
        ensure!(
            Path::new(&sdk_library).is_absolute(),
            "absolute SDK library required"
        );
        let serial = text(&value, "serial")?;
        let color = stream(&value["color"], false)?;
        let depth = stream(&value["depth"], true)?;
        ensure!(
            color.frame_id != depth.frame_id,
            "distinct optical frames required"
        );
        let normalize = match text(&value["depth"], "unit_policy")?.as_str() {
            "require-mm" => false,
            "normalize-mm" => true,
            _ => anyhow::bail!("explicit depth unit policy required"),
        };
        fields(&value["pairing"], &["clock", "max_delta_us"], &[])?;
        let clock = match text(&value["pairing"], "clock")?.as_str() {
            "device" => PairClock::Device,
            "system" => PairClock::System,
            _ => anyhow::bail!("pair clock required"),
        };
        let max_delta_us = value["pairing"]["max_delta_us"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= 1_000_000)
            .context("explicit bounded positive pair tolerance required")?;
        let seconds = value["seconds"]
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 300.0)
            .context("duration limit")?;
        let poll_ms = integer(&value, "poll_ms")?;
        ensure!((1..=1000).contains(&poll_ms), "poll limit");
        let capacity = usize::try_from(value["capacity"].as_u64().context("capacity required")?)?;
        ensure!((1..=1024).contains(&capacity), "capacity limit");
        let startup_discovery_seconds = value
            .get("startup_discovery_seconds")
            .map_or(Some(0.0), Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 30.0)
            .context("startup discovery limit")?;
        let acquisition = value
            .get("acquisition")
            .map(|v| serde_json::from_value::<AcquisitionPolicy>(v.clone()))
            .transpose()?
            .map(|p| p.resolve())
            .transpose()?;
        let message_timestamp =
            acquisition.map_or(MessageTimestamp::System, |p| p.message_timestamp);
        let ros = if let Some(ros) = value.get("ros") {
            fields(ros, &["bridge", "namespace", "stamp_source"], &[])?;
            let bridge = text(ros, "bridge")?;
            ensure!(Path::new(&bridge).is_absolute(), "absolute bridge required");
            let namespace = text(ros, "namespace")?;
            ensure!(
                namespace.starts_with('/')
                    && namespace.len() > 1
                    && namespace[1..].split('/').all(token),
                "explicit namespace required"
            );
            ensure!(
                text(ros, "stamp_source")? == message_timestamp.ros_stamp_source(),
                "explicit matching Unix timestamp mapping required"
            );
            Some(RosConfig { bridge, namespace })
        } else {
            None
        };
        let quality_policy = value
            .get("quality_policy")
            .map(|v| serde_json::from_value::<capture_contract::QualityPolicy>(v.clone()))
            .transpose()?;
        if let Some(policy) = &quality_policy {
            policy.validate()?;
            ensure!(
                policy.max_pair_delta_us == max_delta_us,
                "quality and pairing skew policies differ"
            );
        }
        let imu = value
            .get("imu")
            .map(|v| serde_json::from_value::<capture_contract::imu::ImuConfig>(v.clone()))
            .transpose()?;
        if let Some(imu) = &imu {
            imu.validate()?;
            ensure!(
                !imu.enabled || value.get("acquisition").is_some(),
                "IMU requires explicit acquisition clock policy"
            );
            ensure!(
                imu.accel_frame_id != color.frame_id
                    && imu.accel_frame_id != depth.frame_id
                    && imu.gyro_frame_id != color.frame_id
                    && imu.gyro_frame_id != depth.frame_id,
                "IMU frames must differ from image frames"
            );
            ensure!(
                !imu.enabled
                    || (imu.accel_rate_hz + imu.gyro_rate_hz) as u64 * poll_ms as u64 * 2
                        <= imu.queue_capacity as u64 * 1000,
                "IMU queue requires two polling periods of headroom"
            );
        }
        Ok(Self {
            imu,
            acquisition,
            quality_policy,
            configuration: value.clone(),
            sdk_library,
            serial,
            color,
            depth,
            normalize,
            policy: PairPolicy {
                clock,
                max_delta_us,
            },
            seconds,
            poll_ms,
            capacity,
            startup_discovery_seconds,
            ros,
        })
    }
}
impl Config {
    pub(crate) fn message_timestamp(&self) -> MessageTimestamp {
        self.acquisition
            .map_or(MessageTimestamp::System, |p| p.message_timestamp)
    }
    pub(crate) fn preserve_images(&self) -> bool {
        self.acquisition
            .is_some_and(|p| p.unpaired == UnpairedPolicy::Preserve)
    }
    pub(crate) fn publish_partial(&self) -> bool {
        self.acquisition
            .is_none_or(|p| p.unpaired == UnpairedPolicy::Preserve)
    }
    pub(crate) fn sdk_options(&self) -> orbbec_safe::CaptureOptions {
        self.acquisition
            .map_or_else(orbbec_safe::CaptureOptions::default, |p| {
                orbbec_safe::CaptureOptions {
                    host_clock: Some(orbbec_safe::HostClock::Realtime),
                    software_sync: Some(p.sdk_frame_sync),
                    aggregate: Some(match p.aggregate {
                        FrameAggregate::Any => orbbec_safe::FrameAggregateOutput::Any,
                        FrameAggregate::All => orbbec_safe::FrameAggregateOutput::All,
                        FrameAggregate::Disabled => orbbec_safe::FrameAggregateOutput::Disabled,
                    }),
                    global_timestamp: (p.message_timestamp == MessageTimestamp::Global)
                        .then_some(true),
                }
            })
    }
}

pub(crate) enum Storage {
    Sdk(Frame),
    Owned(Vec<u8>),
}
impl Storage {
    pub(crate) fn data(&self) -> Result<&[u8]> {
        match self {
            Self::Sdk(frame) => Ok(frame.data()?),
            Self::Owned(bytes) => Ok(bytes),
        }
    }
}
pub(crate) struct QueuedPair {
    pub(crate) pair: RgbdPair<Storage, Storage>,
    pub(crate) enqueued_at: Instant,
    pub(crate) images_published: bool,
    pub(crate) bound_evidence: Option<frame_core::BoundPairEvidence>,
}
#[derive(Default)]
pub(crate) struct Metrics {
    pub(crate) imu: Option<Box<crate::imu::ImuMetrics>>,
    pub(crate) framesets: u64,
    pub(crate) accepted: u64,
    pub(crate) rejected: u64,
    pub(crate) missing: u64,
    pub(crate) missing_color: u64,
    pub(crate) missing_depth: u64,
    pub(crate) skew: u64,
    pub(crate) monotonic: u64,
    pub(crate) processed: u64,
    pub(crate) published_pairs: u64,
    pub(crate) publish_calls: u64,
    pub(crate) unpaired_images_published: u64,
    pub(crate) publish_failures: u64,
    pub(crate) stamp_failures: u64,
    pub(crate) timeouts: u64,
    pub(crate) complete_observed: u64,
    pub(crate) delta_min_us: Option<u64>,
    pub(crate) delta_max_us: Option<u64>,
    pub(crate) delta_sum_us: u128,
    pub(crate) delta_over_budget: u64,
    pub(crate) normalization_ns: u128,
    pub(crate) normalization_frames: u64,
    pub(crate) normalization_pixels: u64,
    pub(crate) zero_samples: u64,
    pub(crate) clamped_samples: u64,
    pub(crate) publish_ns: u128,
    pub(crate) sdk_wait: capture_contract::timing::StageStats,
    pub(crate) quality_tick: capture_contract::timing::StageStats,
    pub(crate) sdk_wait_timeouts: u64,
    pub(crate) sdk_wait_errors: u64,
    pub(crate) sdk_return_interval: capture_contract::timing::StageStats,
    pub(crate) between_sdk_polls: capture_contract::timing::StageStats,
    pub(crate) queue_wait: capture_contract::timing::StageStats,
    pub(crate) sdk_age: [capture_contract::timing::StageStats; 2],
    pub(crate) processing: [capture_contract::timing::StageStats; 2],
    pub(crate) image_publish: [capture_contract::timing::StageStats; 2],
    pub(crate) sdk_age_clock_regressions: [u64; 2],
    pub(crate) sdk_age_unavailable: [u64; 2],
    pub(crate) sdk_clock_verified: bool,
    pub(crate) last_sdk_return_at: Option<Instant>,
    pub(crate) last_sdk_call_return_at: Option<Instant>,
    pub(crate) last_sdk_return_unix_ns: Option<u64>,
    pub(crate) queue_capacity: usize,
    pub(crate) queue_current: usize,
    pub(crate) queue_high_water: usize,
    pub(crate) queue_dropped: u64,
    pub(crate) reasons: BTreeMap<String, u64>,
}
pub(crate) fn unix_ns() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_nanos()).ok())
}
impl Metrics {
    pub(crate) fn before_sdk_wait(&mut self, at: Instant) {
        if let Some(previous) = self.last_sdk_call_return_at {
            self.between_sdk_polls
                .observe(at.saturating_duration_since(previous));
        }
    }
    pub(crate) fn sdk_wait_finished(&mut self, started: Instant, returned: Instant) {
        self.sdk_wait
            .observe(returned.saturating_duration_since(started));
        self.last_sdk_call_return_at = Some(returned);
    }
    pub(crate) fn sdk_return(&mut self, at: Instant, wall: Option<u64>) {
        if let Some(previous) = self.last_sdk_return_at {
            self.sdk_return_interval
                .observe(at.saturating_duration_since(previous));
        }
        self.last_sdk_return_at = Some(at);
        self.last_sdk_call_return_at = Some(at);
        self.last_sdk_return_unix_ns = wall;
    }
    pub(crate) fn sdk_frame(&mut self, frame: &Frame, depth: bool) {
        let system = frame.metadata().ok().map(|v| v.system_timestamp_us);
        self.sdk_system_return(depth, system);
    }
    fn sdk_system_return(&mut self, depth: bool, system_us: Option<u64>) {
        let i = usize::from(depth);
        let system = system_us.and_then(|v| v.checked_mul(1000));
        if !self.sdk_clock_verified {
            self.sdk_age_unavailable[i] = self.sdk_age_unavailable[i].saturating_add(1);
        } else if let Some((returned, system)) = self.last_sdk_return_unix_ns.zip(system) {
            if let Some(age) = returned.checked_sub(system).filter(|_| system > 0) {
                self.sdk_age[i].observe_ns(age);
            } else {
                self.sdk_age_clock_regressions[i] =
                    self.sdk_age_clock_regressions[i].saturating_add(1);
            }
        } else {
            self.sdk_age_unavailable[i] = self.sdk_age_unavailable[i].saturating_add(1);
        }
    }
    pub(crate) fn queue(&mut self, stats: frame_core::QueueStats) {
        self.queue_capacity = stats.capacity;
        self.queue_current = stats.len;
        self.queue_high_water = stats.high_water;
        self.queue_dropped = stats.dropped;
    }
    pub(crate) fn timing_report(&self) -> Value {
        json!({"version":1,"local_duration_clock":"monotonic","unix_clock_scope":"SDK realtime verified for explicit acquisition policies; legacy clock unverified",
          "sdk_wait_call":self.sdk_wait.report(),"sdk_wait_scope":"pipeline.wait only, including timeout and error; excludes IMU drain/publish and recorder buffering","sdk_wait_timeouts":self.sdk_wait_timeouts,"sdk_wait_errors":self.sdk_wait_errors,"quality_tick_wall":self.quality_tick.report(),"sdk_success_return_interval":self.sdk_return_interval.report(),"between_sdk_polls":self.between_sdk_polls.report(),
          "streams": (["color","depth"].into_iter().enumerate().map(|(i,name)|(name,json!({"sdk_system_to_return":self.sdk_age[i].report(),"sdk_system_to_return_invalid_clock_samples":self.sdk_age_clock_regressions[i],"sdk_system_to_return_unavailable_samples":self.sdk_age_unavailable[i],"sdk_return_to_publish_start":self.processing[i].report(),"ros_image_publish_call":self.image_publish[i].report()}))).collect::<BTreeMap<_,_>>()),
          "derived_pair_queue":{"capacity":self.queue_capacity,"current":self.queue_current,"high_water":self.queue_high_water,"dropped_oldest":self.queue_dropped,"wait":self.queue_wait.report()},
          "sdk_internal_queue":{"current":null,"high_water":null,"drops":null,"scope":"not observable through the current SDK API; source sequence gaps are separate evidence"},
          "dds_queue":{"current":null,"scope":"publish call duration includes local bridge work and possible middleware backpressure, not subscriber delivery"}})
    }
    pub(crate) fn reject(&mut self, reason: impl Into<String>) {
        self.rejected += 1;
        *self.reasons.entry(reason.into()).or_default() += 1;
    }
}
pub(crate) struct MeasuredPacket {
    pub packet: FramePacket<Storage>,
    pub source_pixels: Option<frame_core::DepthSampleStats>,
    pub output_pixels: Option<frame_core::DepthSampleStats>,
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn packet_measured_with_timestamp(
    frame: Frame,
    config: &StreamConfig,
    depth: bool,
    normalize: bool,
    received_at: Instant,
    metrics: &mut Metrics,
    measure: bool,
    timestamp: MessageTimestamp,
) -> Result<MeasuredPacket> {
    let global = if timestamp == MessageTimestamp::Global {
        Some(
            frame
                .global_timestamp_us()?
                .context("global timestamp unavailable")?,
        )
    } else {
        None
    };
    let source = frame.metadata()?;
    let expected = if depth { Format::Depth16 } else { Format::Yuyv };
    ensure!(
        source.format == expected
            && source.width == config.profile.width
            && source.height == config.profile.height,
        "frame profile mismatch"
    );
    let layout = FrameLayout {
        width: source.width,
        height: source.height,
        stride: source.width as usize * 2,
        format: if depth {
            PixelFormat::Depth16
        } else {
            PixelFormat::Yuyv
        },
    };
    layout.validate(source.data_size)?;
    ensure!(
        frame.data()?.len() == source.data_size,
        "SDK payload size mismatch"
    );
    let source_scale = if depth { source.depth_scale_mm } else { None };
    let mut source_pixels = None;
    let mut output_pixels = None;
    let storage = if depth && normalize {
        let start = Instant::now();
        let converted = frame_core::normalize_depth_mm(
            frame.data()?,
            layout,
            false,
            source_scale.unwrap_or(f32::NAN),
        );
        metrics.normalization_ns += start.elapsed().as_nanos();
        let converted = converted?;
        layout.validate(converted.bytes.len())?;
        if measure {
            source_pixels = Some(converted.stats.source_samples());
            output_pixels = Some(converted.stats.output_samples());
        }
        metrics.normalization_frames += 1;
        metrics.normalization_pixels +=
            converted.stats.zero_samples + converted.stats.nonzero_samples;
        metrics.zero_samples += converted.stats.zero_samples;
        metrics.clamped_samples += converted.stats.clamped_samples;
        Storage::Owned(converted.bytes)
    } else {
        ensure!(
            !depth || source_scale == Some(1.0),
            "raw depth is not millimeter"
        );
        if depth && measure {
            let stats = frame_core::measure_depth_samples(frame.data()?, layout, false)?;
            source_pixels = Some(stats);
            output_pixels = Some(stats);
        }
        Storage::Sdk(frame)
    };
    let mut metadata = make_metadata(source, layout, depth, normalize, received_at);
    metadata.global_timestamp_us = global;
    metadata.sdk_return_unix_ns = metrics.last_sdk_return_unix_ns;
    metadata.sdk_system_clock_verified = metrics.sdk_clock_verified;
    metadata.validate(storage.data()?.len())?;
    Ok(MeasuredPacket {
        packet: FramePacket {
            metadata,
            payload: storage,
        },
        source_pixels,
        output_pixels,
    })
}
fn make_metadata(
    source: FrameMetadata,
    layout: FrameLayout,
    depth: bool,
    normalize: bool,
    received_at: Instant,
) -> PacketMetadata {
    PacketMetadata {
        layout,
        encoding: if depth {
            FrameEncoding::Depth16
        } else {
            FrameEncoding::Yuyv
        },
        sequence: source.index,
        device_timestamp_us: Some(source.timestamp_us),
        system_timestamp_us: Some(source.system_timestamp_us),
        global_timestamp_us: None,
        sdk_return_unix_ns: None,
        sdk_system_clock_verified: false,
        source_depth_scale_mm: if depth { source.depth_scale_mm } else { None },
        output_depth_scale_mm: if depth { Some(1.0) } else { None },
        normalized: depth && normalize,
        received_at,
    }
}

pub(crate) struct Ros {
    pub(crate) color: ros_output::Publisher,
    pub(crate) depth: ros_output::Publisher,
    pub(crate) aux: ros_output::RosAuxPublisher,
    pub(crate) color_guard: StampGuard,
    pub(crate) depth_guard: StampGuard,
    pub(crate) timestamp: MessageTimestamp,
}
pub(crate) fn setup_ros(
    config: &Config,
    calibration: &orbbec_safe::SessionCalibration,
) -> Result<Option<Ros>> {
    let Some(ros) = &config.ros else {
        return Ok(None);
    };
    let build = |name: &str,
                 stream: &StreamConfig,
                 sdk: &orbbec_safe::StreamCalibration|
     -> Result<ros_output::Publisher> {
        ensure!(
            sdk.profile == stream.profile,
            "SDK calibration profile mismatch"
        );
        let calibration = ros_output::Calibration::from_sdk(sdk, &stream.frame_id)?;
        let prefix = format!("{}/{name}", ros.namespace);
        let binding = ros_output::Binding {
            node_name: format!("geminidepthport_rgbd_{name}"),
            namespace: ros.namespace.clone(),
            image_topic: format!("{prefix}/image_raw"),
            camera_info_topic: format!("{prefix}/camera_info"),
            metadata_topic: format!("{prefix}/frame_metadata"),
            frame_id: stream.frame_id.clone(),
        };
        ros_output::Publisher::load(&ros.bridge, binding, calibration)
    };
    let color = build(
        "color",
        &config.color,
        calibration
            .color
            .as_ref()
            .context("SDK color calibration unavailable")?,
    )?;
    let depth = build(
        "depth",
        &config.depth,
        calibration
            .depth
            .as_ref()
            .context("SDK depth calibration unavailable")?,
    )?;
    let transform = ros_output::StaticTransform::from_depth_to_color(
        calibration
            .depth_to_color
            .as_ref()
            .context("SDK depth-to-color unavailable")?,
        &config.color.frame_id,
        &config.depth.frame_id,
    )?;
    let stamp = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
    let aux = ros_output::RosAuxPublisher::load(
        &ros.bridge,
        ros_output::AuxBinding {
            node_name: "geminidepthport_rgbd_aux".into(),
            namespace: ros.namespace.clone(),
            pair_metadata_topic: format!("{}/rgbd/pair_metadata", ros.namespace),
        },
        transform,
        stamp,
    )?;
    Ok(Some(Ros {
        color,
        depth,
        aux,
        color_guard: StampGuard::default(),
        depth_guard: StampGuard::default(),
        timestamp: config.message_timestamp(),
    }))
}
fn stamp_ns_with_source(
    metadata: &PacketMetadata,
    guard: &StampGuard,
    source: MessageTimestamp,
) -> Result<(frame_core::UnixStamp, u64)> {
    let timestamp = match source {
        MessageTimestamp::System => metadata.system_timestamp_us,
        MessageTimestamp::Global => metadata.global_timestamp_us,
    };
    let stamp = sdk_system_unix_stamp(timestamp)?;
    guard.check(stamp)?;
    let ns = u64::try_from(stamp.sec)?
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(u64::from(stamp.nanosec)))
        .context("stamp overflow")?;
    Ok((stamp, ns))
}
/// Publish an available stream without manufacturing RGB-D pair evidence.
/// The caller must drain older pairs first to preserve per-stream stamp order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_unpaired(
    frame: Frame,
    config: &StreamConfig,
    depth: bool,
    normalize: bool,
    received: Instant,
    ros: &mut Option<Ros>,
    metrics: &mut Metrics,
    timestamp: MessageTimestamp,
) -> Result<()> {
    let packet = packet_measured_with_timestamp(
        frame, config, depth, normalize, received, metrics, false, timestamp,
    )?
    .packet;
    publish_packet(&packet, depth, ros, metrics, true)
}
pub(crate) fn publish_packet(
    packet: &FramePacket<Storage>,
    depth: bool,
    ros: &mut Option<Ros>,
    metrics: &mut Metrics,
    unpaired: bool,
) -> Result<()> {
    let bytes = packet.payload.data()?;
    packet.metadata.validate(bytes.len())?;
    if let Some(ros) = ros {
        let (publisher, guard) = if depth {
            (&mut ros.depth, &mut ros.depth_guard)
        } else {
            (&mut ros.color, &mut ros.color_guard)
        };
        let (stamp, ns) = stamp_ns_with_source(&packet.metadata, guard, ros.timestamp)
            .inspect_err(|_| {
                metrics.stamp_failures += 1;
            })?;
        let started = Instant::now();
        metrics.processing[usize::from(depth)]
            .observe(started.saturating_duration_since(packet.metadata.received_at));
        let result = publisher.publish_with_timestamp_source(
            &packet.metadata,
            bytes,
            ns,
            match ros.timestamp {
                MessageTimestamp::System => ros_output::TimestampSource::System,
                MessageTimestamp::Global => ros_output::TimestampSource::Global,
            },
        );
        let elapsed = started.elapsed();
        metrics.image_publish[usize::from(depth)].observe(elapsed);
        metrics.publish_ns = metrics.publish_ns.saturating_add(elapsed.as_nanos());
        result.inspect_err(|_| {
            metrics.publish_failures += 1;
        })?;
        guard.accept(stamp)?;
        metrics.publish_calls += 1;
        metrics.unpaired_images_published += u64::from(unpaired);
    }
    Ok(())
}
pub(crate) fn drain(
    queue: &mut LatestQueue<QueuedPair>,
    ros: &mut Option<Ros>,
    metrics: &mut Metrics,
) -> Result<()> {
    while let Some(item) = queue.pop() {
        let pair = item.pair;
        let color = pair.color.payload.data()?;
        let depth = pair.depth.payload.data()?;
        pair.color.metadata.validate(color.len())?;
        pair.depth.metadata.validate(depth.len())?;
        metrics.queue_wait.observe(item.enqueued_at.elapsed());
        metrics.queue(queue.stats());
        if let Some(ros) = ros {
            let stamp_guard = StampGuard::default();
            let stamps = (|| -> Result<_> {
                Ok((
                    stamp_ns_with_source(
                        &pair.color.metadata,
                        if item.images_published {
                            &stamp_guard
                        } else {
                            &ros.color_guard
                        },
                        ros.timestamp,
                    )?,
                    stamp_ns_with_source(
                        &pair.depth.metadata,
                        if item.images_published {
                            &stamp_guard
                        } else {
                            &ros.depth_guard
                        },
                        ros.timestamp,
                    )?,
                ))
            })();
            let ((color_stamp, color_ns), (depth_stamp, depth_ns)) = match stamps {
                Ok(value) => value,
                Err(_) => {
                    metrics.stamp_failures += 1;
                    anyhow::bail!("pair ROS stamp invalid")
                }
            };
            let start = Instant::now();
            let result = (|| -> Result<()> {
                if !item.images_published {
                    let image_started = Instant::now();
                    metrics.processing[0].observe(
                        image_started.saturating_duration_since(pair.color.metadata.received_at),
                    );
                    let image_result = ros.color.publish_with_timestamp_source(
                        &pair.color.metadata,
                        color,
                        color_ns,
                        match ros.timestamp {
                            MessageTimestamp::System => ros_output::TimestampSource::System,
                            MessageTimestamp::Global => ros_output::TimestampSource::Global,
                        },
                    );
                    metrics.image_publish[0].observe(image_started.elapsed());
                    image_result?;
                    metrics.publish_calls += 1;
                    let image_started = Instant::now();
                    metrics.processing[1].observe(
                        image_started.saturating_duration_since(pair.depth.metadata.received_at),
                    );
                    let image_result = ros.depth.publish_with_timestamp_source(
                        &pair.depth.metadata,
                        depth,
                        depth_ns,
                        match ros.timestamp {
                            MessageTimestamp::System => ros_output::TimestampSource::System,
                            MessageTimestamp::Global => ros_output::TimestampSource::Global,
                        },
                    );
                    metrics.image_publish[1].observe(image_started.elapsed());
                    image_result?;
                    metrics.publish_calls += 1;
                }
                ros.aux.publish_pair(&pair.evidence, color_ns, depth_ns)?;
                metrics.publish_calls += 1;
                if let Some(evidence) = &item.bound_evidence {
                    ros.aux
                        .publish_bound_evidence(evidence, color_ns, depth_ns)?;
                    metrics.publish_calls += 1;
                }
                Ok(())
            })();
            metrics.publish_ns += start.elapsed().as_nanos();
            if result.is_err() {
                metrics.publish_failures += 1;
                anyhow::bail!("pair ROS publication failed")
            }
            if !item.images_published {
                ros.color_guard.accept(color_stamp)?;
                ros.depth_guard.accept(depth_stamp)?;
            }
            metrics.published_pairs += 1;
        }
        metrics.processed += 1;
    }
    Ok(())
}
fn run(config: Config) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux") && cfg!(target_endian = "little"),
        "live RGBD requires little-endian Linux"
    );
    let sdk = orbbec_safe::Sdk::load(&config.sdk_library)?;
    // Both exact profiles are enabled in one Pipeline; no independent device opens.
    let mut pipeline = sdk.open_with_options(
        &config.serial,
        Some(config.color.profile),
        Some(config.depth.profile),
        config.sdk_options(),
    )?;
    let mut imu = crate::imu::RuntimeImu::open(&config, &pipeline)?;
    let mut ros = None;
    let mut queue = LatestQueue::new(config.capacity)?;
    let mut guard = PairGuard::default();
    let mut metrics = Metrics {
        sdk_clock_verified: config.acquisition.is_some(),
        ..Metrics::default()
    };
    let mut startup_metrics = Metrics {
        sdk_clock_verified: config.acquisition.is_some(),
        ..Metrics::default()
    };
    let mut frameset_sequence = 0_u64;
    metrics.queue(queue.stats());
    let mut global_seen = [false; 2];
    let started = Instant::now();
    let warmup = Duration::from_secs_f64(config.startup_discovery_seconds);
    let duration = warmup + Duration::from_secs_f64(config.seconds);
    let outcome = (|| -> Result<()> {
        while started.elapsed() < duration {
            let poll = config.poll_ms.min(
                duration
                    .saturating_sub(started.elapsed())
                    .as_millis()
                    .max(1)
                    .min(u32::MAX as u128) as u32,
            );
            if let Some(imu) = &mut imu {
                imu.tick(&mut metrics, started.elapsed() >= warmup)?;
            }
            let waiting_in_startup = started.elapsed() < warmup;
            let wait_started = Instant::now();
            let wait_result = pipeline.wait(poll);
            let returned_at = Instant::now();
            let returned_unix = unix_ns();
            if waiting_in_startup {
                startup_metrics.before_sdk_wait(wait_started);
                startup_metrics.sdk_wait_finished(wait_started, returned_at);
            } else {
                metrics.before_sdk_wait(wait_started);
                metrics.sdk_wait_finished(wait_started, returned_at);
            }
            if let Some(imu) = &mut imu {
                imu.tick(&mut metrics, started.elapsed() >= warmup)?;
            }
            let wait_metrics = if waiting_in_startup {
                &mut startup_metrics
            } else {
                &mut metrics
            };
            match &wait_result {
                Ok(None) => wait_metrics.sdk_wait_timeouts += 1,
                Err(_) => wait_metrics.sdk_wait_errors += 1,
                _ => {}
            }
            let Some(set) = wait_result? else {
                if started.elapsed() < warmup {
                    startup_metrics.timeouts += 1;
                } else {
                    metrics.timeouts += 1;
                }
                continue;
            };
            let received_at = returned_at;
            let startup = started.elapsed() < warmup;
            let metrics = if startup {
                &mut startup_metrics
            } else {
                &mut metrics
            };
            metrics.sdk_return(returned_at, returned_unix);
            frameset_sequence += 1;
            metrics.framesets += 1;
            let mut color = set.color()?;
            let mut depth = set.depth()?;
            if let Some(frame) = color.as_ref() {
                metrics.sdk_frame(frame, false);
            }
            if let Some(frame) = depth.as_ref() {
                metrics.sdk_frame(frame, true);
            }
            if config.message_timestamp() == MessageTimestamp::Global {
                let mut unavailable = false;
                let was_ready = global_seen.iter().all(|v| *v);
                for (index, frame) in [&mut color, &mut depth].into_iter().enumerate() {
                    if let Some(available) = frame.as_ref() {
                        if available.global_timestamp_us()?.is_some() {
                            global_seen[index] = true;
                        } else {
                            unavailable = true;
                            if config.preserve_images() && !was_ready {
                                *frame = None;
                            }
                        }
                    }
                }
                if unavailable {
                    metrics.reject("GLOBAL_TIMESTAMP_UNAVAILABLE");
                    ensure!(
                        !was_ready,
                        "global timestamp became unavailable after publication"
                    );
                    if !config.preserve_images() {
                        continue;
                    }
                }
            }
            if color.is_none() || depth.is_none() {
                let missing_color = color.is_none();
                let missing_depth = depth.is_none();
                if !startup && config.publish_partial() {
                    if config.ros.is_some() && ros.is_none() {
                        ros = setup_ros(&config, &pipeline.calibration()?)?;
                    }
                    drain(&mut queue, &mut ros, metrics)?;
                    for (frame, stream, is_depth) in
                        [(color, &config.color, false), (depth, &config.depth, true)]
                    {
                        if let Some(frame) = frame {
                            publish_unpaired(
                                frame,
                                stream,
                                is_depth,
                                is_depth && config.normalize,
                                received_at,
                                &mut ros,
                                metrics,
                                config.message_timestamp(),
                            )?;
                        }
                    }
                }
                metrics.missing += 1;
                if missing_color {
                    metrics.missing_color += 1;
                }
                if missing_depth {
                    metrics.missing_depth += 1;
                }
                metrics.reject(match (missing_color, missing_depth) {
                    (true, true) => "MISSING_BOTH",
                    (true, false) => "MISSING_COLOR",
                    _ => "MISSING_DEPTH",
                });
                continue;
            }
            if config.ros.is_some() && ros.is_none() {
                ros = setup_ros(&config, &set.calibration()?)?;
            }
            let color_meta = color.as_ref().unwrap().metadata()?;
            let depth_meta = depth.as_ref().unwrap().metadata()?;
            let (color_time, depth_time) = match config.policy.clock {
                PairClock::Device => (color_meta.timestamp_us, depth_meta.timestamp_us),
                PairClock::System => (
                    color_meta.system_timestamp_us,
                    depth_meta.system_timestamp_us,
                ),
            };
            let delta = color_time.abs_diff(depth_time);
            metrics.complete_observed += 1;
            metrics.delta_min_us = Some(metrics.delta_min_us.map_or(delta, |v| v.min(delta)));
            metrics.delta_max_us = Some(metrics.delta_max_us.map_or(delta, |v| v.max(delta)));
            metrics.delta_sum_us += u128::from(delta);
            if delta > config.policy.max_delta_us {
                metrics.delta_over_budget += 1;
            }
            let color_packet = packet_measured_with_timestamp(
                color.unwrap(),
                &config.color,
                false,
                false,
                received_at,
                metrics,
                false,
                config.message_timestamp(),
            )
            .map(|p| p.packet);
            let depth_packet = packet_measured_with_timestamp(
                depth.unwrap(),
                &config.depth,
                true,
                config.normalize,
                received_at,
                metrics,
                false,
                config.message_timestamp(),
            )
            .map(|p| p.packet);
            if !startup && config.preserve_images() {
                drain(&mut queue, &mut ros, metrics)?;
                if let Ok(color) = &color_packet {
                    publish_packet(color, false, &mut ros, metrics, false)?;
                }
                if let Ok(depth) = &depth_packet {
                    publish_packet(depth, true, &mut ros, metrics, false)?;
                }
            }
            let packets = (|| -> Result<_> { Ok((color_packet?, depth_packet?)) })();
            let (color, depth) = match packets {
                Ok(value) => value,
                Err(_) => {
                    metrics.reject("FRAME_CONTRACT_REJECTED");
                    continue;
                }
            };
            let lengths = (color.payload.data()?.len(), depth.payload.data()?.len());
            let pair = match guard.accept(
                frameset_sequence,
                Some(color),
                Some(depth),
                lengths,
                config.policy,
                PairProvenance::CallerObservedSdkFrameset,
            ) {
                Ok(pair) => pair,
                Err(error) => {
                    let reason = format!("{error:?}");
                    if matches!(error, frame_core::PairError::SkewExceeded) {
                        metrics.skew += 1;
                    }
                    if matches!(
                        error,
                        frame_core::PairError::PairSequenceNotAdvancing
                            | frame_core::PairError::ColorSequenceNotAdvancing
                            | frame_core::PairError::DepthSequenceNotAdvancing
                            | frame_core::PairError::ColorTimestampNotAdvancing
                            | frame_core::PairError::DepthTimestampNotAdvancing
                    ) {
                        metrics.monotonic += 1;
                    }
                    metrics.reject(reason);
                    continue;
                }
            };
            metrics.accepted += 1;
            if startup {
                continue;
            }
            queue.push(QueuedPair {
                bound_evidence: None,
                images_published: config.preserve_images(),
                pair,
                enqueued_at: Instant::now(),
            });
            metrics.queue(queue.stats());
            drain(&mut queue, &mut ros, metrics)?;
        }
        drain(&mut queue, &mut ros, &mut metrics)?;
        Ok(())
    })();
    while queue.pop().is_some() {}
    metrics.queue(queue.stats());
    let imu_stopped = imu
        .as_mut()
        .map(|i| i.stop(&mut metrics, started.elapsed() >= warmup))
        .transpose();
    let stopped = pipeline
        .stop()
        .map_err(anyhow::Error::from)
        .and(imu_stopped.map(|_| ()));
    let p99 = metrics
        .queue_wait
        .quantile_bounds(99)
        .map(|(_, upper)| upper);
    let ok = outcome.is_ok() && stopped.is_ok() && metrics.rejected == 0 && metrics.accepted > 0;
    let error = if ok {
        None
    } else if metrics.stamp_failures > 0 {
        Some("ROS_STAMP_INVALID")
    } else if metrics.publish_failures > 0 {
        Some("ROS_PUBLISH_FAILED")
    } else if outcome.is_err() || stopped.is_err() {
        Some("SDK_OR_OUTPUT_FAILED")
    } else if metrics.accepted == 0 {
        Some("NO_ACCEPTED_PAIRS")
    } else {
        Some("PAIR_CONTRACT_REJECTED")
    };
    Ok(
        json!({"version":1,"kind":"rgbd-probe","ok":ok,"error":error,"sdk_version":sdk.version(),"framesets_observed":metrics.framesets,"pairs_accepted":metrics.accepted,"pairs_rejected":metrics.rejected,"pairs_missing_frames":metrics.missing,"framesets_missing_color":metrics.missing_color,"framesets_missing_depth":metrics.missing_depth,"pairs_skew_rejected":metrics.skew,"pairs_monotonic_rejected":metrics.monotonic,"pairs_processed":metrics.processed,
        "observed_pair_timing":{"complete_framesets":metrics.complete_observed,"delta_min_us":metrics.delta_min_us,"delta_max_us":metrics.delta_max_us,"delta_mean_us":if metrics.complete_observed>0 {Some(metrics.delta_sum_us as f64/metrics.complete_observed as f64)} else {None},"over_budget":metrics.delta_over_budget,"scope":"all-complete-observed-framesets-including-contract-rejections"},
        "startup_discarded":{"budget_seconds":config.startup_discovery_seconds,"framesets":startup_metrics.framesets,"missing":startup_metrics.missing,"missing_color":startup_metrics.missing_color,"missing_depth":startup_metrics.missing_depth,"skew_rejected":startup_metrics.skew,"accepted_not_published":startup_metrics.accepted,"rejected":startup_metrics.rejected,"rejection_reasons":startup_metrics.reasons,"poll_timeouts":startup_metrics.timeouts,"complete_framesets":startup_metrics.complete_observed,"delta_min_us":startup_metrics.delta_min_us,"delta_max_us":startup_metrics.delta_max_us,"over_budget":startup_metrics.delta_over_budget},"rejection_reasons":metrics.reasons,"poll_timeouts":metrics.timeouts,
        "imu":metrics.imu.as_ref().map(|i|i.report()),"pipeline_timing":metrics.timing_report(),"acquisition":config.acquisition,"message_timestamp":config.message_timestamp(),
        "pairing":{"clock":match config.policy.clock {PairClock::Device=>"device",PairClock::System=>"system"},"max_delta_us":config.policy.max_delta_us,"source":"single-sdk-pipeline-single-frameset","hardware_sync_verified":false,"independent_stream_indices_allowed":true},
        "queue":{"capacity":queue.stats().capacity,"high_water":queue.stats().high_water,"pairs_dropped_oldest":queue.stats().dropped,"policy":"whole-pair-drop-oldest","wait_p99_ns":p99,"wait_percentile_scope":"fixed-histogram-bucket-upper-bound"},
        "normalization":{"enabled":config.normalize,"frames":metrics.normalization_frames,"pixels":metrics.normalization_pixels,"invalid_zero_samples":metrics.zero_samples,"nonzero_clamped_to_one_mm":metrics.clamped_samples,"conversion_wall_ns":metrics.normalization_ns,"owned_copy_including_unit_scale":config.normalize},
        "ros":{"enabled":config.ros.is_some(),"pairs_publish_calls_succeeded":metrics.published_pairs,"individual_publish_calls_succeeded":metrics.publish_calls,"unpaired_images_published":metrics.unpaired_images_published,"publish_failures":metrics.publish_failures,"stamp_failures":metrics.stamp_failures,"publish_wall_ns":metrics.publish_ns,"delivery_verified":false,"cross_topic_atomicity":false,"calibration_source":if config.ros.is_some() {Some("active-sdk-stream-profiles")} else {None},"tf_direction":if config.ros.is_some() {Some("p_color=R_depth_to_color*p_depth+t_m;parent_color_child_depth")} else {None}},
        "elapsed_seconds":started.elapsed().as_secs_f64(),"startup_discovery_seconds":config.startup_discovery_seconds,"physical_calibration_accuracy_verified":false,"rgbd_exposure_sync_verified":false}),
    )
}
pub fn main_entry() {
    let result = (|| -> Result<Value> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        ensure!(
            args.len() == 2 && args[0] == "--config" && Path::new(&args[1]).is_absolute(),
            "usage: rgbd-probe --config ABS_PRIVATE_JSON"
        );
        run(Config::parse(&std::fs::read_to_string(&args[1])?)?)
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            if value["ok"] != true {
                std::process::exit(1)
            }
        }
        Err(_) => {
            eprintln!(
                "{}",
                json!({"version":1,"kind":"rgbd-probe","ok":false,"error":"RGBD_PROBE_FAILED","scope":"private-config-or-SDK-or-calibration-setup-failure-no-identifiers-emitted"})
            );
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Value {
        json!({"version":1,"sdk_library":"/private/sdk.so","serial":"private-device","color":{"width":640,"height":480,"fps":30,"frame_id":"color_optical"},"depth":{"width":640,"height":480,"fps":30,"frame_id":"depth_optical","unit_policy":"normalize-mm"},"pairing":{"clock":"device","max_delta_us":20000},"seconds":1,"poll_ms":100,"capacity":4})
    }
    fn owned_packet(depth: bool, sequence: u64, time: u64) -> FramePacket<Storage> {
        let source = FrameMetadata {
            format: if depth { Format::Depth16 } else { Format::Yuyv },
            width: 2,
            height: 1,
            index: sequence,
            timestamp_us: time,
            system_timestamp_us: 1_700_000_000_000_000 + time,
            data_size: 4,
            depth_scale_mm: if depth { Some(0.1) } else { None },
        };
        let layout = FrameLayout {
            width: 2,
            height: 1,
            stride: 4,
            format: if depth {
                PixelFormat::Depth16
            } else {
                PixelFormat::Yuyv
            },
        };
        FramePacket {
            metadata: make_metadata(source, layout, depth, depth, Instant::now()),
            payload: Storage::Owned(if depth {
                vec![0, 0, 10, 0]
            } else {
                vec![16, 128, 32, 128]
            }),
        }
    }
    #[test]
    fn stage_timings_separate_wait_processing_clocks_and_unavailable_queues() {
        let mut metrics = Metrics::default();
        let now = Instant::now();
        let wall = 1_700_000_000_000_000_000_u64;
        metrics.sdk_return(now - Duration::from_millis(355), Some(wall));
        metrics.before_sdk_wait(now - Duration::from_millis(10));
        metrics.sdk_return(now, Some(wall + 355_000_000));
        metrics.sdk_system_return(false, Some(wall / 1000));
        assert_eq!(metrics.sdk_age_unavailable[0], 1);
        metrics.sdk_clock_verified = true;
        metrics.sdk_system_return(false, Some((wall + 350_000_000) / 1000));
        metrics.sdk_system_return(true, Some((wall + 356_000_000) / 1000));
        metrics.sdk_system_return(true, Some(0));
        metrics.sdk_system_return(true, Some(u64::MAX));
        metrics.sdk_system_return(true, None);
        let report = metrics.timing_report();
        assert_eq!(
            report["sdk_success_return_interval"]["maximum_ns"],
            355_000_000
        );
        assert_eq!(report["between_sdk_polls"]["maximum_ns"], 345_000_000);
        assert_eq!(
            report["streams"]["color"]["sdk_system_to_return"]["maximum_ns"],
            5_000_000
        );
        assert_eq!(
            report["streams"]["depth"]["sdk_system_to_return_invalid_clock_samples"],
            2
        );
        assert_eq!(
            report["streams"]["depth"]["sdk_system_to_return_unavailable_samples"],
            2
        );
        assert!(report["sdk_internal_queue"]["current"].is_null());
        assert!(report["dds_queue"]["current"].is_null());
        assert!(std::mem::size_of_val(&metrics) < 100_000);
        metrics.last_sdk_return_unix_ns = None;
        metrics.sdk_system_return(false, Some(wall / 1000));
        assert_eq!(metrics.sdk_age_unavailable[0], 2);
    }
    #[test]
    fn bounded_pair_queue_wait_keeps_tail_without_growing_a_sample_vector() {
        let mut guard = PairGuard::default();
        let mut queue = LatestQueue::new(1).unwrap();
        let pair = guard
            .accept(
                1,
                Some(owned_packet(false, 1, 10)),
                Some(owned_packet(true, 1, 11)),
                (4, 4),
                PairPolicy {
                    clock: PairClock::Device,
                    max_delta_us: 20,
                },
                PairProvenance::CallerObservedSdkFrameset,
            )
            .unwrap();
        queue.push(QueuedPair {
            pair,
            enqueued_at: Instant::now() - Duration::from_millis(355),
            images_published: false,
            bound_evidence: None,
        });
        let mut metrics = Metrics::default();
        metrics.queue(queue.stats());
        drain(&mut queue, &mut None, &mut metrics).unwrap();
        assert_eq!(metrics.queue_current, 0);
        assert_eq!(metrics.queue_high_water, 1);
        assert_eq!(metrics.queue_capacity, 1);
        assert!(metrics.queue_wait.report()["maximum_ns"].as_u64().unwrap() >= 355_000_000);
        assert_eq!(metrics.queue_wait.samples(), 1);
    }
    #[test]
    fn whole_pair_eviction_preserves_independent_stream_identity() {
        let mut guard = PairGuard::default();
        let mut queue = LatestQueue::new(1).unwrap();
        let policy = PairPolicy {
            clock: PairClock::Device,
            max_delta_us: 20_000,
        };
        for pair_sequence in [1, 2] {
            let pair = guard
                .accept(
                    pair_sequence,
                    Some(owned_packet(
                        false,
                        100 + pair_sequence,
                        pair_sequence * 33333,
                    )),
                    Some(owned_packet(
                        true,
                        200 + pair_sequence,
                        pair_sequence * 33333 + 1000,
                    )),
                    (4, 4),
                    policy,
                    PairProvenance::CallerObservedSdkFrameset,
                )
                .unwrap();
            let evicted = queue.push(QueuedPair {
                bound_evidence: None,
                images_published: false,
                pair,
                enqueued_at: Instant::now(),
            });
            if pair_sequence == 2 {
                let old = evicted.unwrap();
                assert_eq!(old.pair.color.metadata.sequence, 101);
                assert_eq!(old.pair.depth.metadata.sequence, 201);
            }
        }
        let item = queue.pop().unwrap();
        assert_eq!(item.pair.evidence.pair_sequence, 2);
        assert_eq!(item.pair.color.metadata.sequence, 102);
        assert_eq!(item.pair.depth.metadata.sequence, 202);
        assert_eq!(item.pair.depth.metadata.source_depth_scale_mm, Some(0.1));
        assert_eq!(item.pair.depth.metadata.output_depth_scale_mm, Some(1.0));
        assert_eq!(item.pair.depth.payload.data().unwrap(), &[0, 0, 10, 0]);
        assert_eq!(queue.stats().dropped, 1);
    }
    #[test]
    fn skewed_pair_does_not_reach_processing_queue() {
        let mut guard = PairGuard::default();
        let result = guard.accept(
            1,
            Some(owned_packet(false, 1, 100)),
            Some(owned_packet(true, 9, 30101)),
            (4, 4),
            PairPolicy {
                clock: PairClock::Device,
                max_delta_us: 20_000,
            },
            PairProvenance::CallerObservedSdkFrameset,
        );
        assert!(matches!(result, Err(frame_core::PairError::SkewExceeded)));
        assert!(guard.last().is_none());
    }
    #[test]
    fn explicit_valid_configuration() {
        let c = Config::parse(&config().to_string()).unwrap();
        assert!(c.normalize);
        assert_eq!(c.policy.max_delta_us, 20000);
        assert_eq!(c.startup_discovery_seconds, 0.0);
    }
    #[test]
    fn no_implicit_profile_or_pair_tolerance() {
        let mut v = config();
        v["pairing"].as_object_mut().unwrap().remove("max_delta_us");
        assert!(Config::parse(&v.to_string()).is_err());
        let mut v = config();
        v["depth"].as_object_mut().unwrap().remove("unit_policy");
        assert!(Config::parse(&v.to_string()).is_err());
    }
    #[test]
    fn malformed_or_unknown_configuration_fails() {
        for key in ["typo", "calibration"] {
            let mut v = config();
            v[key] = json!({});
            assert!(Config::parse(&v.to_string()).is_err());
        }
        let mut v = config();
        v["color"]["width"] = json!(641);
        assert!(Config::parse(&v.to_string()).is_err());
    }
    #[test]
    fn ros_requires_explicit_system_timestamp_mapping() {
        let mut v = config();
        v["ros"] =
            json!({"bridge":"/private/bridge.so","namespace":"/rgbd","stamp_source":"device"});
        assert!(Config::parse(&v.to_string()).is_err());
        v["ros"]["stamp_source"] = json!("sdk-system-unix");
        assert!(Config::parse(&v.to_string()).is_ok());
    }
    #[test]
    fn invalid_binding_profiles_and_clock_policies_fail_before_sdk_loading() {
        for (pointer, value, reason) in [
            (
                "/sdk_library",
                json!("relative.so"),
                "absolute SDK library required",
            ),
            (
                "/depth/frame_id",
                json!("color"),
                "distinct optical frames required",
            ),
            (
                "/depth/unit_policy",
                json!("guess"),
                "explicit depth unit policy required",
            ),
            ("/pairing/clock", json!("guess"), "pair clock required"),
            ("/color/width", json!(20000), "profile limit"),
        ] {
            let mut v = config();
            if pointer == "/depth/frame_id" {
                v["color"]["frame_id"] = json!("color");
            }
            *v.pointer_mut(pointer).unwrap() = value;
            let error = Config::parse(&v.to_string())
                .err()
                .expect("invalid input must fail");
            assert!(error.to_string().contains(reason), "{error}");
        }
        let mut v = config();
        v["ros"] = json!({"bridge":"/private/bridge.so","namespace":"relative","stamp_source":"sdk-system-unix"});
        assert!(
            Config::parse(&v.to_string())
                .err()
                .unwrap()
                .to_string()
                .contains("explicit namespace required")
        );
        let mut v = config();
        v["quality_policy"] = json!({"version":1,"max_source_age_ns":500000000,"max_future_ns":100000000,"max_pair_delta_us":1,"min_valid_fraction":0.0,"max_zero_fraction":1.0,"action":"fail-closed"});
        assert!(
            Config::parse(&v.to_string())
                .err()
                .unwrap()
                .to_string()
                .contains("quality and pairing skew policies differ")
        );
    }
    #[test]
    fn integer_overflow_and_unrecognized_stream_fields_are_rejected_before_sdk_access() {
        for mode in 0..2 {
            let mut value = config();
            if mode == 0 {
                value["color"]["width"] = json!(u64::MAX);
            } else {
                value["depth"]["extra"] = json!(true);
            }
            assert!(Config::parse(&value.to_string()).is_err());
        }
    }
    #[test]
    fn ros_setup_rejects_profile_calibration_transform_and_auxiliary_failures() {
        use std::{fs, path::PathBuf, process::Command};
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dir = std::env::temp_dir().join(format!(
            "gdp-setup-ros-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let normal = fs::read_to_string(root.join("../ros-output/tests/mock_bridge.cpp")).unwrap();
        for missing_aux in [false, true] {
            let lib = dir.join(if missing_aux {
                "missing.so"
            } else {
                "normal.so"
            });
            let source = dir.join("mock.cpp");
            fs::write(
                &source,
                if missing_aux {
                    normal.replace("gdp_ros_aux_create(", "removed_aux_create(")
                } else {
                    normal.clone()
                },
            )
            .unwrap();
            let output = Command::new("c++")
                .args(if cfg!(target_os = "macos") {
                    vec!["-dynamiclib"]
                } else {
                    vec!["-shared", "-fPIC"]
                })
                .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-I"])
                .arg(root.join("../../ros/include"))
                .arg(&source)
                .arg("-o")
                .arg(&lib)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            for mode in 0..6 {
                let mut c = Config::parse(&config().to_string()).unwrap();
                let make = |format| orbbec_safe::StreamCalibration {
                    profile: c.color.profile,
                    format,
                    fx: 1.,
                    fy: 1.,
                    cx: 0.,
                    cy: 0.,
                    distortion_model: "plumb_bob".into(),
                    distortion: vec![0.; 5],
                };
                let mut calibration = orbbec_safe::SessionCalibration {
                    color: Some(make(Format::Yuyv)),
                    depth: Some(make(Format::Depth16)),
                    depth_to_color: Some(orbbec_safe::Extrinsics {
                        rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                        translation_m: [0.1, 0.2, 0.3],
                    }),
                };
                assert!(setup_ros(&c, &calibration).unwrap().is_none());
                c.ros = Some(RosConfig {
                    bridge: lib.to_string_lossy().into(),
                    namespace: "/test".into(),
                });
                match mode {
                    0 => {
                        calibration.color.as_mut().unwrap().profile.width = 1;
                    }
                    1 => {
                        calibration.depth.as_mut().unwrap().profile.height = 1;
                    }
                    2 => {
                        calibration.color.as_mut().unwrap().fx = 0.;
                    }
                    3 => {
                        calibration.depth.as_mut().unwrap().fx = 0.;
                    }
                    4 => {
                        calibration.depth_to_color.as_mut().unwrap().translation_m[0] = f64::NAN;
                    }
                    _ => {}
                }
                let result = setup_ros(&c, &calibration);
                assert_eq!(
                    result.is_ok(),
                    mode == 5 && !missing_aux,
                    "mode {mode}, auxiliary {missing_aux}"
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn acquisition_presets_bind_sdk_clock_and_ros_mapping_without_changing_legacy() {
        let legacy = Config::parse(&config().to_string()).unwrap();
        assert!(legacy.acquisition.is_none());
        assert!(legacy.publish_partial());
        assert!(!legacy.preserve_images());
        assert_eq!(legacy.message_timestamp(), MessageTimestamp::System);
        let mut value = config();
        value["acquisition"] = json!({"preset":"complete"});
        value["ros"] = json!({"bridge":"/private/bridge.so","namespace":"/capture","stamp_source":"sdk-global-unix"});
        let complete = Config::parse(&value.to_string()).unwrap();
        assert!(complete.preserve_images());
        assert!(complete.publish_partial());
        assert_eq!(complete.sdk_options().software_sync, Some(false));
        assert_eq!(complete.sdk_options().global_timestamp, Some(true));
        assert_eq!(
            complete.sdk_options().host_clock,
            Some(orbbec_safe::HostClock::Realtime)
        );
        value["ros"]["stamp_source"] = json!("sdk-system-unix");
        assert!(Config::parse(&value.to_string()).is_err());
        value["acquisition"] = json!({"preset":"strict-rgbd","advanced":{"message_timestamp":"system","aggregate":"all"}});
        let strict = Config::parse(&value.to_string()).unwrap();
        assert!(!strict.preserve_images());
        assert!(!strict.publish_partial());
        assert_eq!(strict.sdk_options().software_sync, Some(true));
        assert_eq!(
            strict.sdk_options().aggregate,
            Some(orbbec_safe::FrameAggregateOutput::All)
        );
        assert_eq!(strict.sdk_options().global_timestamp, None);
        value["acquisition"] = json!({"preset":"complete","advanced":{"aggregate":"disabled"}});
        value["ros"]["stamp_source"] = json!("sdk-global-unix");
        assert_eq!(
            Config::parse(&value.to_string())
                .unwrap()
                .sdk_options()
                .aggregate,
            Some(orbbec_safe::FrameAggregateOutput::Disabled)
        );
        for policy in [
            json!({"preset":"hardware-sync"}),
            json!({"preset":"unknown"}),
            json!({"preset":"complete","extra":true}),
            json!({"preset":"complete","advanced":{"aggregate":"all"}}),
        ] {
            value["acquisition"] = policy;
            assert!(Config::parse(&value.to_string()).is_err());
        }
    }
    #[test]
    fn global_message_stamps_preserve_receipt_time_and_never_fallback() {
        let mut metadata = owned_packet(false, 1, 1000).metadata;
        let original_system = metadata.system_timestamp_us;
        let global = 1_699_999_999_999_000;
        metadata.global_timestamp_us = Some(global);
        let guard = StampGuard::default();
        let (_, stamp) = stamp_ns_with_source(&metadata, &guard, MessageTimestamp::Global).unwrap();
        assert_eq!(stamp, global * 1000);
        assert_eq!(metadata.system_timestamp_us, original_system);
        assert!(stamp_ns_with_source(&metadata, &guard, MessageTimestamp::System).is_ok());
        metadata.global_timestamp_us = None;
        assert!(stamp_ns_with_source(&metadata, &guard, MessageTimestamp::Global).is_err());
        metadata.global_timestamp_us = Some(0);
        assert!(stamp_ns_with_source(&metadata, &guard, MessageTimestamp::Global).is_err());
        metadata.global_timestamp_us = Some(global);
        let mut advanced = StampGuard::default();
        advanced
            .accept(sdk_system_unix_stamp(Some(global + 1)).unwrap())
            .unwrap();
        assert!(stamp_ns_with_source(&metadata, &advanced, MessageTimestamp::Global).is_err());
    }
}
