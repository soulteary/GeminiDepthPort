use offline_depth::*;
use std::path::{Path, PathBuf};
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_some_and(|v| v == "--capabilities" || v == "--version")
    {
        let response: Result<serde_json::Value> = (|| {
            let component = "offline-depth";
            let version = env!("CARGO_PKG_VERSION");
            if args == ["--version"] {
                return Ok(build_info());
            }
            if args == ["--capabilities"] {
                return Ok(capture_contract::capabilities::describe(component, version));
            }
            if args.len() == 3 && args[0] == "--capabilities" && args[1] == "--require" {
                let request = capture_contract::parse_strict(args[2].as_bytes())?;
                return Ok(capture_contract::capabilities::negotiate(
                    component, version, &request,
                )?);
            }
            Err(OfflineError::Invalid("CAPABILITY_ARGUMENT"))
        })();
        match response {
            Ok(value) => println!("{value}"),
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"version":1,"complete":false,"code":error.code(),"detail_code":error.detail_code()})
                );
                std::process::exit(1)
            }
        }
        return;
    }
    if args
        .first()
        .is_some_and(|a| a == "--recover-recording-prefix")
    {
        let result = (|| -> Result<serde_json::Value> {
            if args.len() != 2 || !Path::new(&args[1]).is_absolute() {
                return Err(OfflineError::Invalid("RECOVERY_ARGUMENT"));
            }
            let mut source =
                recording::RecordingDirectorySource::open_recovered(Path::new(&args[1]))?;
            let mut pairs = 0u64;
            while source.validate_next()? {
                pairs += 1;
            }
            Ok(
                serde_json::json!({"version":1,"complete":false,"recovered_prefix_valid":true,
                "validated_pairs":pairs,"business_validity":null,"source_modified":false}),
            )
        })();
        match result {
            Ok(report) => println!("{report}"),
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"complete":false,"recovered_prefix_valid":false,"code":error.code()})
                );
                std::process::exit(1);
            }
        }
        return;
    }
    let progress = args
        .iter()
        .position(|v| v == "--progress")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let mut control = match job_control::JobControl::new(progress.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"version":1,"complete":false,"code":e.code(),"detail_code":e.detail_code()})
            );
            std::process::exit(1)
        }
    };
    control.install_cli_signals();
    match control.execute(execute) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string(&{
                    let mut value = serde_json::to_value(&report).unwrap();
                    value["build"] = build_info();
                    value["d2c_diagnostics"] = control.diagnostics();
                    value
                })
                .unwrap_or_else(|_| "{\"complete\":false,\"code\":\"REPORT_FAILED\"}".into())
            );
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"version":1,"complete":false,
                "code":e.code(),"detail_code":e.detail_code()})
            );
            std::process::exit(1)
        }
    }
}
fn execute() -> Result<RunReport> {
    let mut args = std::env::args().skip(1);
    let mut input = None;
    let mut output = None;
    let mut name = None;
    let mut selected = None;
    let mut capacity = 2;
    let mut mode = depth_output::Mode::Directory;
    let mut validate_only = false;
    let mut d2c = false;
    let mut normalize_mm = false;
    let mut recording_v2 = false;
    let mut preserve_counts = false;
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = args.next() {
        if !seen.insert(arg.clone()) {
            return Err(OfflineError::Invalid("DUPLICATE_ARGUMENT"));
        }
        match arg.as_str() {
            "--input" => {
                input = Some(PathBuf::from(
                    args.next().ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?,
                ))
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?,
                ))
            }
            "--job" => name = Some(args.next().ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?),
            "--pair" => {
                selected = Some(
                    args.next()
                        .ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?
                        .parse::<u64>()
                        .map_err(|_| OfflineError::Invalid("PAIR_ARGUMENT"))?,
                )
            }
            "--capacity" => {
                capacity = args
                    .next()
                    .ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?
                    .parse::<usize>()
                    .map_err(|_| OfflineError::Invalid("QUEUE_CAPACITY"))?
            }
            "--mode" => {
                mode = match args.next().as_deref() {
                    Some("png") => depth_output::Mode::Directory,
                    Some("tar") => depth_output::Mode::Tar,
                    Some("both") => depth_output::Mode::Both,
                    _ => return Err(OfflineError::Invalid("MODE_ARGUMENT")),
                }
            }
            "--progress" => {
                args.next().ok_or(OfflineError::Invalid("ARGUMENT_VALUE"))?;
            }
            "--validate-only" => validate_only = true,
            "--d2c" => d2c = true,
            "--normalize-mm" => normalize_mm = true,
            "--recording-v2" | "--recording" => recording_v2 = true,
            "--preserve-counts" => preserve_counts = true,
            _ => return Err(OfflineError::Invalid("UNKNOWN_ARGUMENT")),
        }
    }
    if !(1..=16).contains(&capacity) {
        return Err(OfflineError::Invalid("QUEUE_CAPACITY"));
    }
    if preserve_counts && (d2c || normalize_mm) {
        return Err(OfflineError::Invalid("COUNTS_OPERATION_CONFLICT"));
    }
    if d2c && normalize_mm {
        return Err(OfflineError::Invalid("D2C_NORMALIZATION_ALREADY_EXPLICIT"));
    }
    let input = input.ok_or(OfflineError::Invalid("INPUT_REQUIRED"))?;
    if !input.is_absolute() {
        return Err(OfflineError::Invalid("ABSOLUTE_INPUT_REQUIRED"));
    }
    let operation = if preserve_counts {
        Operation::PreserveCounts
    } else if d2c {
        Operation::DepthToColor
    } else {
        Operation::Original { normalize_mm }
    };
    if recording_v2 {
        if selected.is_some() {
            return Err(OfflineError::Invalid("V2_PAIR_SELECTION_UNSUPPORTED"));
        }
        let mut source = recording::RecordingDirectorySource::open(&input)?;
        if validate_only {
            if output.is_some() || name.is_some() {
                return Err(OfflineError::Invalid("VALIDATE_OUTPUT_ARGUMENT"));
            }
            return recording::process(&mut source, None, operation);
        }
        let output = output.ok_or(OfflineError::Invalid("OUTPUT_REQUIRED"))?;
        if !output.is_absolute() {
            return Err(OfflineError::Invalid("ABSOLUTE_OUTPUT_REQUIRED"));
        }
        return recording::process(
            &mut source,
            Some((
                &output,
                &name.ok_or(OfflineError::Invalid("JOB_REQUIRED"))?,
                mode,
            )),
            operation,
        );
    }
    let mut source = DirectoryFrameSource::open(&input, selected)?;
    if validate_only {
        if output.is_some() || name.is_some() {
            return Err(OfflineError::Invalid("VALIDATE_OUTPUT_ARGUMENT"));
        }
        validate(&mut source, operation)
    } else {
        let output = output.ok_or(OfflineError::Invalid("OUTPUT_REQUIRED"))?;
        if !output.is_absolute() {
            return Err(OfflineError::Invalid("ABSOLUTE_OUTPUT_REQUIRED"));
        }
        run(
            &mut source,
            Path::new(&output),
            &name.ok_or(OfflineError::Invalid("JOB_REQUIRED"))?,
            RunOptions {
                operation,
                mode,
                queue_capacity: capacity,
            },
        )
    }
}
