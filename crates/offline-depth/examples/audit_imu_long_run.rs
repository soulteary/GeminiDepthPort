//! Read-only JSONL export for matching real DDS observations to verified native data.
use offline_depth::{
    OfflineError,
    recording::{RecordingDirectorySource, TaggedEvidence},
};
use serde_json::json;
use std::{
    io::{self, Write},
    path::Path,
};
/// Independent acceptance reference; deliberately does not call the capture normalizer.
/// The sealed SDK scale has already been promoted from f32 to f64 by the writer.
pub(crate) fn reference_depth_mm(
    bytes: &[u8],
    stored: &offline_depth::recording::StoredImage,
    scale: f64,
    normalize: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use offline_depth::StorageEndian;
    if !scale.is_finite() || scale <= 0. || (!normalize && scale != 1.) {
        return Err("invalid ROS depth scale/policy".into());
    }
    let row_bytes = (stored.width as usize)
        .checked_mul(2)
        .ok_or("depth row overflow")?;
    let size = stored
        .stride
        .checked_mul(stored.height as usize)
        .ok_or("depth size overflow")?;
    if row_bytes == 0 || stored.height == 0 || stored.stride < row_bytes || bytes.len() != size {
        return Err("invalid native depth layout".into());
    }
    if !normalize && (!matches!(stored.endian, StorageEndian::Little) || stored.stride != row_bytes)
    {
        return Err("require-mm needs packed little-endian source".into());
    }
    let mut output = Vec::with_capacity(row_bytes * stored.height as usize);
    for row in bytes.chunks_exact(stored.stride) {
        for pixel in row[..row_bytes].chunks_exact(2) {
            let count = match stored.endian {
                StorageEndian::Little => u16::from_le_bytes([pixel[0], pixel[1]]),
                StorageEndian::Big => u16::from_be_bytes([pixel[0], pixel[1]]),
            };
            let millimetres = if count == 0 {
                0
            } else {
                let value = (f64::from(count) * scale + 0.5).floor();
                if value > 65535. {
                    return Err("ROS depth overflow".into());
                }
                value.max(1.) as u16
            };
            output.extend_from_slice(&millimetres.to_le_bytes());
        }
    }
    Ok(output)
}
pub(crate) fn audit(
    root: &Path,
    recovered: bool,
    mut out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let open = || {
        if recovered {
            RecordingDirectorySource::open_recovered(root)
        } else {
            RecordingDirectorySource::open(root)
        }
    };
    let mut source = open()?;
    let configuration = &source.snapshot().content.configuration;
    let capture = configuration.get("capture").unwrap_or(configuration);
    let normalize = match capture["depth"]["unit_policy"].as_str() {
        Some("normalize-mm") => true,
        Some("require-mm") => false,
        _ => return Err("explicit ROS depth unit policy required in sealed snapshot".into()),
    };
    let mut pairs = 0u64;
    let mut samples = [0u64; 2];
    while let Some(frame) = source.next_frame()? {
        let TaggedEvidence::SdkBoundPair { pair, .. } = frame.evidence else {
            return Err("SDK bound evidence required".into());
        };
        let image = |stored: &offline_depth::recording::StoredImage,
                     context: &capture_contract::FrameContext|
         -> Result<_, OfflineError> {
            frame_core::sdk_system_unix_stamp(context.global_timestamp_us)
                .map_err(|_| OfflineError::Invalid("GLOBAL_REQUIRED"))?;
            let stamp = context
                .global_timestamp_us
                .and_then(|v| v.checked_mul(1000))
                .ok_or(OfflineError::Invalid("GLOBAL_REQUIRED"))?;
            Ok(
                json!({"stamp_ns":stamp,"sequence":context.sequence,"device_us":context.device_timestamp_us,"system_us":context.system_timestamp_us,"sha256":stored.file.sha256,"frame_id":stored.frame_id,"width":stored.width,"height":stored.height,"step":stored.stride}),
            )
        };
        let capture_contract::DepthRepresentation::SdkCounts { scale_mm } =
            frame.record.storage_representation
        else {
            return Err("native SDK counts required".into());
        };
        // Keep the counts hash separate from the expected ROS millimetre bytes.
        if offline_depth::sha256(&frame.depth) != frame.record.depth.file.sha256 {
            return Err("native counts hash mismatch".into());
        }
        let expected_ros =
            reference_depth_mm(&frame.depth, &frame.record.depth, scale_mm, normalize)?;
        let mut depth = image(&frame.record.depth, &pair.depth.context)?;
        depth["ros_sha256"] = json!(offline_depth::sha256(&expected_ros));
        depth["source_depth_scale_mm"] = json!(scale_mm);
        depth["ros_normalized"] = json!(normalize);
        depth["ros_step"] = json!(frame.record.depth.width as u64 * 2);
        serde_json::to_writer(
            &mut out,
            &json!({"kind":"pair","sequence":frame.record.sequence,"color":image(&frame.record.color,&pair.color.context)?,"depth":depth}),
        )?;
        writeln!(out)?;
        for sample in frame.record.imu {
            frame_core::sdk_system_unix_stamp(sample.global_timestamp_us)?;
            let stamp = sample
                .global_timestamp_us
                .and_then(|v| v.checked_mul(1000))
                .ok_or("IMU global required")?;
            samples[sample.kind.slot()] += 1;
            serde_json::to_writer(
                &mut out,
                &json!({"kind":"imu","stamp_ns":stamp,"sample":sample}),
            )?;
            writeln!(out)?;
        }
        pairs += 1;
    }
    // A new reader performs fresh actual I/O and hashes; no result/cache is reused.
    let mut second = open()?;
    let mut count = 0;
    while second.validate_next()? {
        count += 1;
    }
    if count != pairs {
        return Err("readback count changed".into());
    }
    serde_json::to_writer(
        &mut out,
        &json!({"kind":"summary","audit_version":2,"native_hashes_verified":true,"ok":true,"recovered":recovered,"pairs":pairs,"imu":samples,"independent_audit_passes":2,"production_passes_certified":false}),
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.is_empty() || args.len() > 2 || (args.len() == 2 && args[1] != "--recovered") {
        return Err("usage: audit_imu_long_run RECORDING [--recovered]".into());
    }
    audit(
        Path::new(&args[0]),
        args.len() == 2,
        &mut io::BufWriter::new(io::stdout().lock()),
    )
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
