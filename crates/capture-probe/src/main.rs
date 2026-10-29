//! Finite acquisition experiment with opt-in ROS output; no hardware certification.
use anyhow::{Result, ensure};
use frame_core::{
    FrameEncoding, FrameLayout, FramePacket, LatestQueue, PacketMetadata, PixelFormat, StampGuard,
    sdk_system_unix_stamp,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum DepthUnitPolicy {
    RequireMm,
    NormalizeMm,
}
impl DepthUnitPolicy {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "require-mm" => Ok(Self::RequireMm),
            "normalize-mm" => Ok(Self::NormalizeMm),
            _ => anyhow::bail!("invalid depth unit policy"),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::RequireMm => "require-mm",
            Self::NormalizeMm => "normalize-mm",
        }
    }
}
struct RosOptions {
    bridge: String,
    namespace: String,
    frame_id: String,
    calibration: String,
}
fn ros_options(values: &BTreeMap<String, String>, synthetic: bool) -> Result<Option<RosOptions>> {
    let keys = [
        "--ros-bridge",
        "--ros-namespace",
        "--ros-frame-id",
        "--ros-calibration",
        "--ros-stamp-source",
    ];
    let present = keys.iter().filter(|k| values.contains_key(**k)).count();
    if present == 0 {
        return Ok(None);
    }
    ensure!(
        present == keys.len() && !synthetic,
        "ROS output requires all arguments and actual pixels"
    );
    ensure!(
        values["--ros-stamp-source"] == "sdk-system-unix",
        "explicit SDK system Unix stamp mapping required"
    );
    ensure!(
        std::path::Path::new(&values["--ros-bridge"]).is_absolute(),
        "absolute ROS bridge required"
    );
    ensure!(
        std::path::Path::new(&values["--ros-calibration"]).is_absolute(),
        "absolute calibration path required"
    );
    let namespace = &values["--ros-namespace"];
    ensure!(
        namespace.starts_with('/')
            && namespace.len() > 1
            && !namespace.ends_with('/')
            && namespace.split('/').skip(1).all(|part| !part.is_empty()
                && part.chars().enumerate().all(|(i, c)| c == '_'
                    || c.is_ascii_alphabetic()
                    || i > 0 && c.is_ascii_digit())),
        "invalid ROS namespace"
    );
    let frame_id = &values["--ros-frame-id"];
    ensure!(
        !frame_id.is_empty()
            && !frame_id.starts_with('/')
            && frame_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '/'),
        "invalid optical frame ID"
    );
    Ok(Some(RosOptions {
        bridge: values["--ros-bridge"].clone(),
        namespace: namespace.clone(),
        frame_id: frame_id.clone(),
        calibration: values["--ros-calibration"].clone(),
    }))
}
struct Options {
    synthetic: bool,
    depth_unit_policy: DepthUnitPolicy,
    stream: String,
    width: u32,
    height: u32,
    fps: u32,
    capacity: usize,
    frames: u64,
    drain_every: u64,
    seconds: f64,
    poll_ms: u32,
    sdk: Option<String>,
    serial: Option<String>,
    ros: Option<RosOptions>,
}
fn options() -> Result<Options> {
    let mut values = BTreeMap::new();
    let mut synthetic = false;
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        if key == "--help" {
            println!(
                "capture-probe --stream color|depth --width N --height N --fps N [--synthetic --frames N] [--sdk-library PATH --serial SERIAL] [--depth-unit-policy require-mm|normalize-mm] [--seconds 5 --poll-ms 100 --capacity 4 --drain-every 1]\nSynthetic mode exercises software only. Live mode requires Linux and an unoccupied device.\nROS output requires all: --ros-bridge ABS_LIBRARY --ros-namespace /NAMESPACE --ros-frame-id OPTICAL_FRAME --ros-calibration ABS_PRIVATE_JSON --ros-stamp-source sdk-system-unix."
            );
            std::process::exit(0);
        }
        if key == "--synthetic" {
            ensure!(!synthetic, "duplicate flag");
            synthetic = true;
            continue;
        }
        ensure!(
            [
                "--stream",
                "--width",
                "--height",
                "--fps",
                "--capacity",
                "--frames",
                "--drain-every",
                "--seconds",
                "--poll-ms",
                "--sdk-library",
                "--serial",
                "--depth-unit-policy",
                "--ros-bridge",
                "--ros-namespace",
                "--ros-frame-id",
                "--ros-calibration",
                "--ros-stamp-source"
            ]
            .contains(&key.as_str()),
            "unknown argument"
        );
        let value = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("argument value required"))?;
        ensure!(values.insert(key, value).is_none(), "duplicate argument");
    }
    let required = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing {key}"))
    };
    let number = |key: &str, default: &str| -> Result<u64> {
        Ok(values
            .get(key)
            .map(String::as_str)
            .unwrap_or(default)
            .parse()?)
    };
    let stream = required("--stream")?;
    ensure!(stream == "color" || stream == "depth", "unsupported stream");
    let depth_unit_policy = DepthUnitPolicy::parse(
        values
            .get("--depth-unit-policy")
            .map(String::as_str)
            .unwrap_or("require-mm"),
    )?;
    ensure!(
        depth_unit_policy == DepthUnitPolicy::RequireMm || stream == "depth" && !synthetic,
        "normalization requires live depth pixels"
    );
    let width: u32 = required("--width")?.parse()?;
    let height: u32 = required("--height")?.parse()?;
    let fps: u32 = required("--fps")?.parse()?;
    ensure!(
        width > 0 && height > 0 && fps > 0,
        "positive exact profile required"
    );
    let capacity = usize::try_from(number("--capacity", "4")?)?;
    let frames = number("--frames", "100")?;
    let drain_every = number("--drain-every", "1")?;
    let seconds: f64 = values
        .get("--seconds")
        .map(String::as_str)
        .unwrap_or("5")
        .parse()?;
    let poll_ms = u32::try_from(number("--poll-ms", "100")?)?;
    ensure!(
        (1..=1024).contains(&capacity)
            && (1..=1_000_000).contains(&frames)
            && (1..=1_000_000).contains(&drain_every),
        "queue or frame limit invalid"
    );
    ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= 300.0 && (1..=1000).contains(&poll_ms),
        "time limit invalid"
    );
    let sdk = values.get("--sdk-library").cloned();
    let serial = values.get("--serial").cloned();
    if synthetic {
        ensure!(
            sdk.is_none() && serial.is_none(),
            "synthetic mode refuses device arguments"
        );
    } else {
        ensure!(
            sdk.as_ref().is_some_and(|v| !v.is_empty())
                && serial.as_ref().is_some_and(|v| !v.is_empty()),
            "live mode requires SDK library and exact serial"
        );
        ensure!(
            !values.contains_key("--frames"),
            "--frames is synthetic-only"
        );
    }
    let ros = ros_options(&values, synthetic)?;
    Ok(Options {
        synthetic,
        depth_unit_policy,
        stream,
        width,
        height,
        fps,
        capacity,
        frames,
        drain_every,
        seconds,
        poll_ms,
        sdk,
        serial,
        ros,
    })
}
#[derive(Default)]
struct Metrics {
    captured: u64,
    validated: u64,
    rejected: u64,
    processed: u64,
    timeouts: u64,
    waits: Vec<u64>,
    last_device: Option<u64>,
    last_system: Option<u64>,
    rejection_reasons: BTreeMap<&'static str, u64>,
    first_observed: Option<serde_json::Value>,
    index_progression: Progression,
    timestamp_progression: Progression,
    system_progression: Progression,
    first_arrival: Option<Instant>,
    last_arrival: Option<Instant>,
    normalized_frames: u64,
    normalized_pixels: u64,
    zero_samples: u64,
    clamped_samples: u64,
    output_min_mm: Option<u16>,
    output_max_mm: Option<u16>,
    scale_min_mm: Option<f32>,
    scale_max_mm: Option<f32>,
    normalization_ns: u128,
    normalization_attempts: u64,
    processed_payload_bytes: u64,
    ros_publish_calls: u64,
    ros_publish_failures: u64,
    ros_publish_ns: u128,
    packet_failures: u64,
    stamp_failures: u64,
}
enum FrameStorage {
    Raw(orbbec_safe::Frame),
    Normalized(Vec<u8>),
}
struct HeldFrame {
    storage: FrameStorage,
    enqueued_at: Instant,
}
impl HeldFrame {
    fn data(&self) -> Result<&[u8]> {
        match &self.storage {
            FrameStorage::Raw(frame) => Ok(frame.data()?),
            FrameStorage::Normalized(bytes) => Ok(bytes),
        }
    }
}
struct RosState {
    publisher: ros_output::Publisher,
    guard: StampGuard,
}
fn setup_ros(o: &Options) -> Result<Option<RosState>> {
    let Some(config) = &o.ros else {
        return Ok(None);
    };
    let calibration =
        ros_output::Calibration::from_json(&std::fs::read_to_string(&config.calibration)?)?;
    ensure!(
        calibration.width == o.width
            && calibration.height == o.height
            && calibration.frame_id == config.frame_id,
        "calibration binding mismatch"
    );
    let prefix = format!("{}/{}", config.namespace, o.stream);
    let binding = ros_output::Binding {
        node_name: "geminidepthport_capture_probe".into(),
        namespace: config.namespace.clone(),
        image_topic: format!("{prefix}/image_raw"),
        camera_info_topic: format!("{prefix}/camera_info"),
        metadata_topic: format!("{prefix}/frame_metadata"),
        frame_id: config.frame_id.clone(),
    };
    Ok(Some(RosState {
        publisher: ros_output::Publisher::load(&config.bridge, binding, calibration)?,
        guard: StampGuard::default(),
    }))
}
fn drain_live(
    queue: &mut LatestQueue<FramePacket<HeldFrame>>,
    stats: &mut Metrics,
    ros: &mut Option<RosState>,
) -> Result<()> {
    while let Some(packet) = queue.pop() {
        let data = packet.payload.data()?;
        if packet.metadata.validate(data.len()).is_err() {
            stats.packet_failures += 1;
            *stats.rejection_reasons.entry("PACKET_INVALID").or_default() += 1;
            anyhow::bail!("packet invalid");
        }
        if stats.waits.len() < 1_000_000 {
            stats.waits.push(
                packet
                    .payload
                    .enqueued_at
                    .elapsed()
                    .as_nanos()
                    .min(u64::MAX as u128) as u64,
            );
        }
        if let Some(ros) = ros {
            let stamp =
                match sdk_system_unix_stamp(packet.metadata.system_timestamp_us).and_then(|stamp| {
                    ros.guard.check(stamp)?;
                    Ok(stamp)
                }) {
                    Ok(stamp) => stamp,
                    Err(_) => {
                        stats.stamp_failures += 1;
                        *stats
                            .rejection_reasons
                            .entry("ROS_STAMP_INVALID")
                            .or_default() += 1;
                        anyhow::bail!("ROS stamp invalid");
                    }
                };
            let stamp_ns = u64::try_from(stamp.sec)?
                .checked_mul(1_000_000_000)
                .and_then(|v| v.checked_add(u64::from(stamp.nanosec)))
                .ok_or_else(|| anyhow::anyhow!("stamp overflow"))?;
            let started = Instant::now();
            let published = ros.publisher.publish(&packet.metadata, data, stamp_ns);
            stats.ros_publish_ns += started.elapsed().as_nanos();
            if published.is_err() {
                stats.ros_publish_failures += 1;
                *stats
                    .rejection_reasons
                    .entry("ROS_PUBLISH_FAILED")
                    .or_default() += 1;
                anyhow::bail!("ROS publish failed");
            }
            ros.guard.accept(stamp)?;
            stats.ros_publish_calls += 1;
        }
        stats.processed_payload_bytes = stats
            .processed_payload_bytes
            .saturating_add(data.len() as u64);
        stats.processed += 1;
    }
    Ok(())
}
#[derive(Default)]
struct Progression {
    last: Option<u64>,
    duplicates: u64,
    regressions: u64,
    gap_events: u64,
    skipped_values: u64,
}
impl Progression {
    fn observe(&mut self, value: u64, gap_threshold: u64) {
        if let Some(last) = self.last {
            if value == last {
                self.duplicates += 1;
            } else if value < last {
                self.regressions += 1;
            } else if value - last > gap_threshold {
                self.gap_events += 1;
                if gap_threshold == 1 {
                    self.skipped_values = self.skipped_values.saturating_add(value - last - 1);
                }
            }
        }
        self.last = Some(value);
    }
    fn report(&self) -> serde_json::Value {
        json!({"duplicates":self.duplicates,"regressions":self.regressions,
            "gap_events":self.gap_events,"skipped_values_if_unit_index":self.skipped_values})
    }
}
fn rejection_reasons(
    o: &Options,
    metadata: &orbbec_safe::FrameMetadata,
    actual_len: usize,
) -> Result<Vec<&'static str>> {
    let expected = if o.stream == "color" {
        orbbec_safe::Format::Yuyv
    } else {
        orbbec_safe::Format::Depth16
    };
    let mut reasons = Vec::new();
    if metadata.format != expected {
        reasons.push("FORMAT_MISMATCH");
    }
    if metadata.width != o.width || metadata.height != o.height {
        reasons.push("DIMENSION_MISMATCH");
    }
    if layout(o)?.validate(metadata.data_size).is_err() {
        reasons.push("PAYLOAD_LAYOUT");
    }
    if actual_len != metadata.data_size {
        reasons.push("DATA_SIZE_MISMATCH");
    }
    if o.stream == "depth"
        && o.depth_unit_policy == DepthUnitPolicy::RequireMm
        && !metadata.depth_scale_mm.is_some_and(|v| v == 1.0)
    {
        reasons.push("DEPTH_SCALE_NOT_ONE_MM");
    }
    Ok(reasons)
}
fn packet_metadata(
    o: &Options,
    source: &orbbec_safe::FrameMetadata,
    received_at: Instant,
) -> Result<PacketMetadata> {
    let depth = o.stream == "depth";
    Ok(PacketMetadata {
        layout: layout(o)?,
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
        normalized: depth && o.depth_unit_policy == DepthUnitPolicy::NormalizeMm,
        received_at,
    })
}
fn normalize_payload(
    o: &Options,
    bytes: &[u8],
    scale: f32,
) -> std::result::Result<frame_core::NormalizedDepth, frame_core::DepthNormalizationError> {
    let stride = usize::try_from(o.width)
        .ok()
        .and_then(|v| v.checked_mul(2))
        .ok_or(frame_core::DepthNormalizationError::Layout(
            frame_core::LayoutError::ArithmeticOverflow,
        ))?;
    let layout = FrameLayout {
        width: o.width,
        height: o.height,
        stride,
        format: if o.stream == "depth" {
            PixelFormat::Depth16
        } else {
            PixelFormat::Yuyv
        },
    };
    let converted = frame_core::normalize_depth_mm(bytes, layout, false, scale)?;
    layout
        .validate(converted.bytes.len())
        .map_err(frame_core::DepthNormalizationError::Layout)?;
    Ok(converted)
}
fn observe(m: &mut Metrics, metadata: &orbbec_safe::FrameMetadata, arrival: Instant, fps: u32) {
    if let Some(scale) = metadata.depth_scale_mm.filter(|v| v.is_finite()) {
        m.scale_min_mm = Some(m.scale_min_mm.map_or(scale, |old| old.min(scale)));
        m.scale_max_mm = Some(m.scale_max_mm.map_or(scale, |old| old.max(scale)));
    }
    m.first_arrival.get_or_insert(arrival);
    m.last_arrival = Some(arrival);
    if m.first_observed.is_none() {
        let format = match metadata.format {
            orbbec_safe::Format::Yuyv => "yuyv".to_string(),
            orbbec_safe::Format::Depth16 => "depth16".to_string(),
            orbbec_safe::Format::Other(value) => format!("other({value})"),
        };
        m.first_observed = Some(
            json!({"format":format,"width":metadata.width,"height":metadata.height,"size":metadata.data_size,"depth_scale_mm":metadata.depth_scale_mm}),
        );
    }
    m.index_progression.observe(metadata.index, 1);
    m.timestamp_progression.observe(
        metadata.timestamp_us,
        1_000_000_u64.div_ceil(u64::from(fps)) * 3 / 2,
    );
    m.system_progression.observe(
        metadata.system_timestamp_us,
        1_000_000_u64.div_ceil(u64::from(fps)) * 3 / 2,
    );
    m.last_device = Some(metadata.timestamp_us);
    m.last_system = Some(metadata.system_timestamp_us);
}
fn drain<T>(queue: &mut LatestQueue<(T, Instant)>, stats: &mut Metrics) {
    while let Some((_frame, entered)) = queue.pop() {
        stats.processed += 1;
        // Bound measurement memory independently from experiment frame rate.
        if stats.waits.len() < 1_000_000 {
            stats
                .waits
                .push(entered.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
    }
}
fn layout(o: &Options) -> Result<FrameLayout> {
    let stride = usize::try_from(o.width)?
        .checked_mul(2)
        .ok_or_else(|| anyhow::anyhow!("row overflow"))?;
    Ok(FrameLayout {
        width: o.width,
        height: o.height,
        stride,
        format: if o.stream == "color" {
            PixelFormat::Yuyv
        } else {
            PixelFormat::Depth16
        },
    })
}
fn report<T>(
    o: &Options,
    queue: &LatestQueue<T>,
    m: &mut Metrics,
    elapsed: Duration,
) -> serde_json::Value {
    m.waits.sort_unstable();
    let p99 = if m.waits.is_empty() {
        None
    } else {
        Some(m.waits[((m.waits.len() * 99).div_ceil(100) - 1).min(m.waits.len() - 1)])
    };
    let q = queue.stats();
    let active_seconds = m
        .first_arrival
        .zip(m.last_arrival)
        .map(|(first, last)| last.duration_since(first).as_secs_f64());
    let active_fps = active_seconds
        .filter(|v| *v > 0.0)
        .map(|v| m.captured.saturating_sub(1) as f64 / v);
    json!({"version":1,"kind":"capture-probe","ok":true,"mode":if o.synthetic {"synthetic"} else {"sdk-live"},
        "hardware_verified":false,"ros_published":m.ros_publish_calls > 0,"stream":o.stream,"profile":{"width":o.width,"height":o.height,"fps":o.fps},
        "captured":m.captured,"validated":m.validated,"rejected":m.rejected,"processed":m.processed,"poll_timeouts":m.timeouts,
        "queue":{"capacity":q.capacity,"high_water":q.high_water,"dropped_oldest":q.dropped,"remaining":q.len,"policy":"drop-oldest-experimental"},
        "queue_wait_ns":{"p99":p99,"samples":m.waits.len(),"sample_limit":1_000_000,"clock":"host-monotonic","scope":"application-queue-only"},
        "last_sdk_timestamp_us":m.last_device,"last_sdk_system_timestamp_us":m.last_system,"sdk_timestamps":"raw-unmapped-not-end-to-end-latency",
        "elapsed_seconds":elapsed.as_secs_f64(),
        "rejection_reasons":m.rejection_reasons,"first_observed_metadata":m.first_observed,
        "arrival_rate":{"active_window_seconds":active_seconds,"fps":active_fps,"scope":"all-observed-selected-stream-frames-first-to-last-host-arrival"},
        "progression":{"frame_index":m.index_progression.report(),"sdk_timestamp_us":m.timestamp_progression.report(),"sdk_system_timestamp_us":m.system_progression.report(),"timestamp_gap_threshold_us":1_000_000_u64.div_ceil(u64::from(o.fps)) * 3 / 2,"scope":"adjacent-observed-frames-timestamp-gaps-over-1.5-nominal-periods-index-gaps-are-not-certified-frame-drops"},
        "depth_unit":if o.stream == "depth" && !o.synthetic && m.validated > 0 && m.rejected == 0 {Some("mm")} else {None},
        "normalization":{"policy":o.depth_unit_policy.name(),"frames":m.normalized_frames,"pixels":m.normalized_pixels,"zero_input_samples":m.zero_samples,"nonzero_clamped_to_one_mm":m.clamped_samples,"output_min_mm":m.output_min_mm,"output_max_mm":m.output_max_mm,"source_scale_mm_min":m.scale_min_mm,"source_scale_mm_max":m.scale_max_mm,"attempts":m.normalization_attempts,"wall_ns":m.normalization_ns,"timing_scope":"host-monotonic-conversion-and-output-layout-validation-excludes-queue","allocation":if m.normalized_frames > 0 {Some("owned-packed-le-u16-copy-including-unit-scale-input")} else {None}},
        "processed_payload_bytes":m.processed_payload_bytes,
        "ros_output":{"enabled":o.ros.is_some(),"publish_calls_succeeded":m.ros_publish_calls,"publish_failures":m.ros_publish_failures,"publish_wall_ns":m.ros_publish_ns,"packet_failures":m.packet_failures,"stamp_failures":m.stamp_failures,"delivery_verified":false,"stamp_source":if o.ros.is_some() {Some("sdk-system-unix")} else {None},"scope":"synchronous-bridge-publish-calls-not-subscriber-delivery"},
        "depth_semantics":{"requested_unit":if o.stream == "depth" {Some("mm")} else {None},"verified_from_sdk_metadata":o.stream == "depth" && !o.synthetic && o.depth_unit_policy == DepthUnitPolicy::RequireMm && m.validated > 0 && m.rejected == 0,"normalized_output_verified":o.stream == "depth" && !o.synthetic && o.depth_unit_policy == DepthUnitPolicy::NormalizeMm && m.normalized_frames > 0 && m.rejected == 0,"source_byte_order":if !o.synthetic && o.stream == "depth" {Some("little-endian-supported-sdk-target")} else {None},"output_byte_order":if !o.synthetic && o.stream == "depth" && m.validated > 0 && m.rejected == 0 {Some("little-endian")} else {None},"physical_scale_verified":false},
        "scope":"acquisition-experiment-no-calibration-or-consumer-certification"})
}
fn synthetic(o: &Options) -> Result<serde_json::Value> {
    let l = layout(o)?;
    let len = l
        .stride
        .checked_mul(o.height as usize)
        .ok_or_else(|| anyhow::anyhow!("payload overflow"))?;
    l.validate(len)?;
    // A small ownership token avoids allocating an artificial full image for every queue entry.
    let mut queue = LatestQueue::new(o.capacity)?;
    let mut m = Metrics::default();
    let start = Instant::now();
    for sequence in 0..o.frames {
        m.captured += 1;
        l.validate(len)?;
        m.validated += 1;
        queue.push((sequence, Instant::now()));
        if (sequence + 1) % o.drain_every == 0 {
            drain(&mut queue, &mut m);
        }
    }
    drain(&mut queue, &mut m);
    Ok(report(o, &queue, &mut m, start.elapsed()))
}
fn live(o: &Options) -> Result<serde_json::Value> {
    ensure!(cfg!(target_os = "linux"), "live acquisition requires Linux");
    ensure!(
        cfg!(target_endian = "little"),
        "live SDK byte layout currently supports little-endian hosts only"
    );
    let sdk = orbbec_safe::Sdk::load(o.sdk.as_deref().unwrap())?;
    let profile = orbbec_safe::VideoProfile {
        width: o.width,
        height: o.height,
        fps: o.fps,
    };
    let (color, depth) = if o.stream == "color" {
        (Some(profile), None)
    } else {
        (None, Some(profile))
    };
    let mut ros = setup_ros(o)?;
    let mut pipeline = sdk.open(o.serial.as_deref().unwrap(), color, depth)?;
    let mut queue = LatestQueue::new(o.capacity)?;
    let mut m = Metrics::default();
    let start = Instant::now();
    let duration = Duration::from_secs_f64(o.seconds);
    let outcome = (|| -> Result<()> {
        while start.elapsed() < duration {
            let remaining = duration.saturating_sub(start.elapsed());
            let poll = o
                .poll_ms
                .min(remaining.as_millis().max(1).min(u32::MAX as u128) as u32);
            let Some(set) = pipeline.wait(poll)? else {
                m.timeouts += 1;
                continue;
            };
            let frame = if o.stream == "color" {
                set.color()?
            } else {
                set.depth()?
            };
            let Some(frame) = frame else {
                continue;
            };
            let received_at = Instant::now();
            m.captured += 1;
            let metadata = frame.metadata()?;
            observe(&mut m, &metadata, Instant::now(), o.fps);
            let reasons = rejection_reasons(o, &metadata, frame.data()?.len())?;
            if !reasons.is_empty() {
                m.rejected += 1;
                for reason in reasons {
                    *m.rejection_reasons.entry(reason).or_default() += 1;
                }
                continue;
            }
            let held = if o.stream == "depth" && o.depth_unit_policy == DepthUnitPolicy::NormalizeMm
            {
                let scale = metadata.depth_scale_mm.unwrap_or(f32::NAN);
                let started = Instant::now();
                m.normalization_attempts += 1;
                let normalized = normalize_payload(o, frame.data()?, scale);
                m.normalization_ns += started.elapsed().as_nanos();
                match normalized {
                    Ok(normalized) => {
                        m.normalized_frames += 1;
                        m.normalized_pixels +=
                            normalized.stats.zero_samples + normalized.stats.nonzero_samples;
                        m.zero_samples += normalized.stats.zero_samples;
                        m.clamped_samples += normalized.stats.clamped_samples;
                        if let Some(min) = normalized.stats.min_mm {
                            m.output_min_mm = Some(m.output_min_mm.map_or(min, |v| v.min(min)));
                        }
                        if let Some(max) = normalized.stats.max_mm {
                            m.output_max_mm = Some(m.output_max_mm.map_or(max, |v| v.max(max)));
                        }
                        FrameStorage::Normalized(normalized.bytes)
                    }
                    Err(error) => {
                        let reason = match error {
                            frame_core::DepthNormalizationError::OutputOverflow => {
                                "DEPTH_NORMALIZATION_OVERFLOW"
                            }
                            frame_core::DepthNormalizationError::InvalidScale => {
                                "DEPTH_SCALE_INVALID"
                            }
                            _ => "DEPTH_NORMALIZATION_LAYOUT",
                        };
                        m.rejected += 1;
                        *m.rejection_reasons.entry(reason).or_default() += 1;
                        continue;
                    }
                }
            } else {
                FrameStorage::Raw(frame)
            };
            m.validated += 1;
            let packet_metadata = packet_metadata(o, &metadata, received_at)?;
            queue.push(FramePacket {
                metadata: packet_metadata,
                payload: HeldFrame {
                    storage: held,
                    enqueued_at: Instant::now(),
                },
            });
            if m.validated % o.drain_every == 0 {
                drain_live(&mut queue, &mut m, &mut ros)?;
            }
        }
        drain_live(&mut queue, &mut m, &mut ros)?;
        Ok(())
    })();
    // Release queued SDK references before stopping the pipeline, including on errors.
    // Once publication or stamp validation fails, release remaining packets without
    // attempting later frames from the failed batch.
    let final_drain = if outcome.is_err() {
        while queue.pop().is_some() {}
        Ok(())
    } else {
        drain_live(&mut queue, &mut m, &mut ros)
    };
    let stopped = pipeline.stop();
    let error = if m.stamp_failures > 0 {
        Some("ROS_STAMP_INVALID")
    } else if m.ros_publish_failures > 0 {
        Some("ROS_PUBLISH_FAILED")
    } else if m.packet_failures > 0 {
        Some("PACKET_INVALID")
    } else if outcome.is_err() || final_drain.is_err() {
        Some("SDK_ACQUISITION_FAILED")
    } else if stopped.is_err() {
        Some("SDK_STOP_FAILED")
    } else if m.rejected > 0 {
        Some("FRAME_CONTRACT_REJECTED")
    } else if m.validated == 0 {
        Some("NO_VALID_FRAMES")
    } else {
        None
    };
    let mut value = report(o, &queue, &mut m, start.elapsed());
    value["sdk_version"] = sdk.version().into();
    if let Some(code) = error {
        value["ok"] = false.into();
        value["error"] = code.into();
    }
    Ok(value)
}
fn main() {
    let result = (|| -> Result<_> {
        let o = options()?;
        if o.synthetic { synthetic(&o) } else { live(&o) }
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            if value["ok"] != true {
                std::process::exit(1);
            }
        }
        Err(_) => {
            // SDK diagnostics can contain serials or paths; keep stdout/stderr report private-safe.
            eprintln!(
                "{}",
                json!({"version":1,"kind":"capture-probe","ok":false,"error":"CAPTURE_PROBE_FAILED","hint":"Check arguments, platform, SDK compatibility, device ownership and exact profile; no device identifiers are included."})
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    fn options() -> Options {
        Options {
            synthetic: false,
            depth_unit_policy: DepthUnitPolicy::RequireMm,
            stream: "depth".into(),
            width: 640,
            height: 480,
            fps: 30,
            capacity: 4,
            frames: 10,
            drain_every: 1,
            seconds: 1.0,
            poll_ms: 100,
            sdk: None,
            serial: None,
            ros: None,
        }
    }
    fn metadata() -> orbbec_safe::FrameMetadata {
        orbbec_safe::FrameMetadata {
            format: orbbec_safe::Format::Depth16,
            width: 640,
            height: 480,
            index: 1,
            timestamp_us: 100,
            system_timestamp_us: 200,
            data_size: 640 * 480 * 2,
            depth_scale_mm: Some(1.0),
        }
    }
    #[test]
    fn normalized_pixels_are_owned_queued_and_consumed() {
        let mut o = options();
        o.width = 2;
        o.height = 1;
        o.depth_unit_policy = DepthUnitPolicy::NormalizeMm;
        let source = [0, 0, 100, 0];
        let result = normalize_payload(&o, &source, 0.1).unwrap();
        assert_eq!(result.bytes, vec![0, 0, 10, 0]);
        assert_eq!(result.stats.zero_samples, 1);
        let mut queue = LatestQueue::new(1).unwrap();
        queue.push(FramePacket {
            metadata: packet_metadata(
                &o,
                &orbbec_safe::FrameMetadata {
                    width: 2,
                    height: 1,
                    data_size: 4,
                    depth_scale_mm: Some(0.1),
                    ..metadata()
                },
                Instant::now(),
            )
            .unwrap(),
            payload: HeldFrame {
                storage: FrameStorage::Normalized(result.bytes),
                enqueued_at: Instant::now(),
            },
        });
        let mut metrics = Metrics::default();
        drain_live(&mut queue, &mut metrics, &mut None).unwrap();
        assert_eq!(metrics.processed, 1);
        assert_eq!(metrics.processed_payload_bytes, 4);
    }
    #[test]
    fn normalized_packet_retains_source_identity_through_eviction() {
        let mut o = options();
        o.width = 2;
        o.height = 1;
        o.depth_unit_policy = DepthUnitPolicy::NormalizeMm;
        let arrival = Instant::now();
        let mut source = metadata();
        source.width = 2;
        source.height = 1;
        source.data_size = 4;
        source.depth_scale_mm = Some(0.1);
        let mut queue = LatestQueue::new(1).unwrap();
        for sequence in [101, 102] {
            source.index = sequence;
            let output = normalize_payload(&o, &[0, 0, 100, 0], 0.1).unwrap();
            let packet = FramePacket {
                metadata: packet_metadata(&o, &source, arrival).unwrap(),
                payload: HeldFrame {
                    storage: FrameStorage::Normalized(output.bytes),
                    enqueued_at: Instant::now(),
                },
            };
            let evicted = queue.push(packet);
            if sequence == 102 {
                assert_eq!(evicted.unwrap().metadata.sequence, 101);
            }
        }
        let packet = queue.pop().unwrap();
        assert_eq!(packet.metadata.sequence, 102);
        assert_eq!(packet.metadata.device_timestamp_us, Some(100));
        assert_eq!(packet.metadata.system_timestamp_us, Some(200));
        assert_eq!(packet.metadata.source_depth_scale_mm, Some(0.1));
        assert_eq!(packet.metadata.output_depth_scale_mm, Some(1.0));
        assert_eq!(packet.metadata.received_at, arrival);
        assert!(packet.metadata.normalized);
        assert_eq!(packet.payload.data().unwrap(), &[0, 0, 10, 0]);
        packet
            .metadata
            .validate(packet.payload.data().unwrap().len())
            .unwrap();
    }
    #[test]
    fn ros_argument_group_is_explicit_and_complete() {
        let mut values = BTreeMap::new();
        assert!(ros_options(&values, false).unwrap().is_none());
        values.insert("--ros-bridge".into(), "/private/bridge.so".into());
        assert!(ros_options(&values, false).is_err());
        for (key, value) in [
            ("--ros-namespace", "/test_camera1"),
            ("--ros-frame-id", "test_depth_optical_frame"),
            ("--ros-calibration", "/private/calibration.json"),
            ("--ros-stamp-source", "sdk-system-unix"),
        ] {
            values.insert(key.into(), value.into());
        }
        assert!(ros_options(&values, false).unwrap().is_some());
        assert!(ros_options(&values, true).is_err());
        values.insert("--ros-stamp-source".into(), "device".into());
        assert!(ros_options(&values, false).is_err());
    }
    #[test]
    fn normalization_rejects_overflow_or_invalid_scale() {
        let mut o = options();
        o.width = 1;
        o.height = 1;
        assert!(matches!(
            normalize_payload(&o, &[255, 255], 2.0),
            Err(frame_core::DepthNormalizationError::OutputOverflow)
        ));
        assert!(matches!(
            normalize_payload(&o, &[1, 0], 0.0),
            Err(frame_core::DepthNormalizationError::InvalidScale)
        ));
    }
    #[test]
    fn normalization_policy_does_not_bypass_layout_validation() {
        let mut o = options();
        o.depth_unit_policy = DepthUnitPolicy::NormalizeMm;
        let mut m = metadata();
        m.depth_scale_mm = Some(0.1);
        assert!(rejection_reasons(&o, &m, m.data_size).unwrap().is_empty());
        m.data_size -= 1;
        assert_eq!(
            rejection_reasons(&o, &m, m.data_size).unwrap(),
            vec!["PAYLOAD_LAYOUT"]
        );
    }
    #[test]
    fn differing_scale_is_identified_without_weakening_contract() {
        let o = options();
        let mut m = metadata();
        m.depth_scale_mm = Some(0.1);
        assert_eq!(
            rejection_reasons(&o, &m, m.data_size).unwrap(),
            vec!["DEPTH_SCALE_NOT_ONE_MM"]
        );
        m.depth_scale_mm = Some(1.0);
        assert!(rejection_reasons(&o, &m, m.data_size).unwrap().is_empty());
        m.depth_scale_mm = None;
        assert_eq!(
            rejection_reasons(&o, &m, m.data_size).unwrap(),
            vec!["DEPTH_SCALE_NOT_ONE_MM"]
        );
    }
    #[test]
    fn layout_and_size_failures_are_distinct() {
        let o = options();
        let mut m = metadata();
        m.data_size -= 1;
        assert_eq!(
            rejection_reasons(&o, &m, m.data_size).unwrap(),
            vec!["PAYLOAD_LAYOUT"]
        );
        m = metadata();
        assert_eq!(
            rejection_reasons(&o, &m, m.data_size - 1).unwrap(),
            vec!["DATA_SIZE_MISMATCH"]
        );
    }
    #[test]
    fn progression_separates_duplicates_regressions_and_index_gaps() {
        let mut p = Progression::default();
        for n in [10, 10, 9, 13] {
            p.observe(n, 1);
        }
        assert_eq!(
            (p.duplicates, p.regressions, p.gap_events, p.skipped_values),
            (1, 1, 1, 3)
        );
    }
    #[test]
    fn rejected_depth_does_not_claim_verified_units() {
        let o = options();
        let q: LatestQueue<u8> = LatestQueue::new(1).unwrap();
        let mut m = Metrics {
            captured: 1,
            rejected: 1,
            ..Metrics::default()
        };
        let report = report(&o, &q, &mut m, Duration::from_secs(1));
        assert!(report["depth_unit"].is_null());
        assert_eq!(
            report["depth_semantics"]["verified_from_sdk_metadata"],
            false
        );
    }
    #[test]
    fn normalization_rejects_color_layouts_without_converting_their_pixels() {
        let mut o = options();
        o.stream = "color".into();
        o.width = 2;
        o.height = 1;
        let bytes = [16, 128, 32, 128];
        assert!(normalize_payload(&o, &bytes, 1.).is_err());
        assert_eq!(bytes, [16, 128, 32, 128]);
    }
}
