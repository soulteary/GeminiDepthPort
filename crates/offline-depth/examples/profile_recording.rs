//! Read-only CPU breakdown for a completed recording or explicitly recovered prefix.
use offline_depth::{OfflineError, recording::RecordingDirectorySource};
fn run() -> offline_depth::Result<serde_json::Value> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.len() > 2 || (args.len() == 2 && args[1] != "--recovered") {
        return Err(OfflineError::Invalid(
            "USAGE_PROFILE_RECORDING_PATH_OPTIONAL_RECOVERED",
        ));
    }
    let mut source = if args.len() == 2 {
        RecordingDirectorySource::open_recovered(std::path::Path::new(&args[0]))?
    } else {
        RecordingDirectorySource::open(std::path::Path::new(&args[0]))?
    };
    source.enable_validation_cpu_profiling()?;
    let mut count = 0u64;
    while source.validate_next()? {
        count += 1;
    }
    Ok(
        serde_json::json!({"ok":true,"validated_pairs":count,"recovered":args.len()==2,"cpu_profile":source.validation_cpu_profile()}),
    )
}
fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{}", error.code());
            std::process::exit(1);
        }
    }
}
