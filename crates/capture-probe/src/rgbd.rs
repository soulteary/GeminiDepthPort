//! Single-Pipeline RGB-D experiment. Frameset association is not exposure sync.
use anyhow::{Context, Result, ensure};
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
    pub(crate) sdk_library: String,
    pub(crate) serial: String,
    pub(crate) quality_policy: Option<capture_contract::QualityPolicy>,
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
            &["ros", "startup_discovery_seconds", "quality_policy"],
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
                text(ros, "stamp_source")? == "sdk-system-unix",
                "explicit system Unix mapping required"
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
        Ok(Self {
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
    pub(crate) bound_evidence: Option<frame_core::BoundPairEvidence>,
}
#[derive(Default)]
pub(crate) struct Metrics {
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
    pub(crate) waits: Vec<u64>,
    pub(crate) reasons: BTreeMap<String, u64>,
}
impl Metrics {
    pub(crate) fn reject(&mut self, reason: impl Into<String>) {
        self.rejected += 1;
        *self.reasons.entry(reason.into()).or_default() += 1;
    }
}
pub(crate) fn packet(
    frame: Frame,
    config: &StreamConfig,
    depth: bool,
    normalize: bool,
    received_at: Instant,
    metrics: &mut Metrics,
) -> Result<FramePacket<Storage>> {
    Ok(packet_measured(frame, config, depth, normalize, received_at, metrics, false)?.packet)
}
pub(crate) struct MeasuredPacket {
    pub packet: FramePacket<Storage>,
    pub source_pixels: Option<frame_core::DepthSampleStats>,
    pub output_pixels: Option<frame_core::DepthSampleStats>,
}
pub(crate) fn packet_measured(
    frame: Frame,
    config: &StreamConfig,
    depth: bool,
    normalize: bool,
    received_at: Instant,
    metrics: &mut Metrics,
    measure: bool,
) -> Result<MeasuredPacket> {
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
    let metadata = make_metadata(source, layout, depth, normalize, received_at);
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
    }))
}
pub(crate) fn stamp_ns(
    metadata: &PacketMetadata,
    guard: &StampGuard,
) -> Result<(frame_core::UnixStamp, u64)> {
    let stamp = sdk_system_unix_stamp(metadata.system_timestamp_us)?;
    guard.check(stamp)?;
    let ns = u64::try_from(stamp.sec)?
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(u64::from(stamp.nanosec)))
        .context("stamp overflow")?;
    Ok((stamp, ns))
}
/// Publish an available stream without manufacturing RGB-D pair evidence.
/// The caller must drain older pairs first to preserve per-stream stamp order.
pub(crate) fn publish_unpaired(
    frame: Frame,
    config: &StreamConfig,
    depth: bool,
    normalize: bool,
    received: Instant,
    ros: &mut Option<Ros>,
    metrics: &mut Metrics,
) -> Result<()> {
    let packet = packet(frame, config, depth, normalize, received, metrics)?;
    let bytes = packet.payload.data()?;
    packet.metadata.validate(bytes.len())?;
    if let Some(ros) = ros {
        let (publisher, guard) = if depth {
            (&mut ros.depth, &mut ros.depth_guard)
        } else {
            (&mut ros.color, &mut ros.color_guard)
        };
        let (stamp, ns) = stamp_ns(&packet.metadata, guard).inspect_err(|_| {
            metrics.stamp_failures += 1;
        })?;
        publisher
            .publish(&packet.metadata, bytes, ns)
            .inspect_err(|_| {
                metrics.publish_failures += 1;
            })?;
        guard.accept(stamp)?;
        metrics.publish_calls += 1;
        metrics.unpaired_images_published += 1;
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
        if metrics.waits.len() < 1_000_000 {
            metrics
                .waits
                .push(item.enqueued_at.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        if let Some(ros) = ros {
            let stamps = (|| -> Result<_> {
                Ok((
                    stamp_ns(&pair.color.metadata, &ros.color_guard)?,
                    stamp_ns(&pair.depth.metadata, &ros.depth_guard)?,
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
                ros.color.publish(&pair.color.metadata, color, color_ns)?;
                metrics.publish_calls += 1;
                ros.depth.publish(&pair.depth.metadata, depth, depth_ns)?;
                metrics.publish_calls += 1;
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
            ros.color_guard.accept(color_stamp)?;
            ros.depth_guard.accept(depth_stamp)?;
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
    let mut pipeline = sdk.open(
        &config.serial,
        Some(config.color.profile),
        Some(config.depth.profile),
    )?;
    let mut ros = None;
    let mut queue = LatestQueue::new(config.capacity)?;
    let mut guard = PairGuard::default();
    let mut metrics = Metrics::default();
    let mut startup_metrics = Metrics::default();
    let mut frameset_sequence = 0_u64;
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
            let Some(set) = pipeline.wait(poll)? else {
                if started.elapsed() < warmup {
                    startup_metrics.timeouts += 1;
                } else {
                    metrics.timeouts += 1;
                }
                continue;
            };
            let received_at = Instant::now();
            let startup = started.elapsed() < warmup;
            let metrics = if startup {
                &mut startup_metrics
            } else {
                &mut metrics
            };
            frameset_sequence += 1;
            metrics.framesets += 1;
            let color = set.color()?;
            let depth = set.depth()?;
            if color.is_none() || depth.is_none() {
                let missing_color = color.is_none();
                let missing_depth = depth.is_none();
                if !startup {
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
                                Instant::now(),
                                &mut ros,
                                metrics,
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
            let packets = (|| -> Result<_> {
                Ok((
                    packet(
                        color.unwrap(),
                        &config.color,
                        false,
                        false,
                        received_at,
                        metrics,
                    )?,
                    packet(
                        depth.unwrap(),
                        &config.depth,
                        true,
                        config.normalize,
                        received_at,
                        metrics,
                    )?,
                ))
            })();
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
                pair,
                enqueued_at: Instant::now(),
            });
            drain(&mut queue, &mut ros, metrics)?;
        }
        drain(&mut queue, &mut ros, &mut metrics)?;
        Ok(())
    })();
    while queue.pop().is_some() {}
    let stopped = pipeline.stop();
    metrics.waits.sort_unstable();
    let p99 = metrics
        .waits
        .get((metrics.waits.len() * 99).div_ceil(100).saturating_sub(1))
        .copied();
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
        "pairing":{"clock":match config.policy.clock {PairClock::Device=>"device",PairClock::System=>"system"},"max_delta_us":config.policy.max_delta_us,"source":"single-sdk-pipeline-single-frameset","hardware_sync_verified":false,"independent_stream_indices_allowed":true},
        "queue":{"capacity":queue.stats().capacity,"high_water":queue.stats().high_water,"pairs_dropped_oldest":queue.stats().dropped,"policy":"whole-pair-drop-oldest","wait_p99_ns":p99},
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
}
