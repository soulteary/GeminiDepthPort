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
    let mut pairs = 0u64;
    let mut samples = [0u64; 2];
    while let Some(frame) = source.next_frame()? {
        let TaggedEvidence::SdkBoundPair { pair, .. } = frame.evidence else {
            return Err("SDK bound evidence required".into());
        };
        let image = |stored: &offline_depth::recording::StoredImage,
                     context: &capture_contract::FrameContext|
         -> Result<_, OfflineError> {
            let stamp = context
                .global_timestamp_us
                .and_then(|v| v.checked_mul(1000))
                .ok_or(OfflineError::Invalid("GLOBAL_REQUIRED"))?;
            Ok(
                json!({"stamp_ns":stamp,"sequence":context.sequence,"device_us":context.device_timestamp_us,"system_us":context.system_timestamp_us,"sha256":stored.file.sha256,"frame_id":stored.frame_id,"width":stored.width,"height":stored.height,"step":stored.stride}),
            )
        };
        serde_json::to_writer(
            &mut out,
            &json!({"kind":"pair","sequence":frame.record.sequence,"color":image(&frame.record.color,&pair.color.context)?,"depth":image(&frame.record.depth,&pair.depth.context)?}),
        )?;
        writeln!(out)?;
        for sample in frame.record.imu {
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
        &json!({"kind":"summary","ok":true,"recovered":recovered,"pairs":pairs,"imu":samples,"independent_audit_passes":2,"production_passes_certified":false}),
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
