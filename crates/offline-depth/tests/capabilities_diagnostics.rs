mod support;
use offline_depth::{DirectoryFrameSource, Operation, RunOptions, job_control::JobControl, run};
use serde_json::{Value, json};
use std::{fs, process::Command};
#[test]
fn offline_discovery_and_negotiation_never_need_an_input() {
    let output = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .arg("--capabilities")
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["build"]["component"], "offline-depth");
    assert_eq!(value["worker_ready"], false);
    let wanted = json!({"protocols":{"offline_input":2,"offline_diagnostics":1}}).to_string();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_offline-depth"))
            .args(["--capabilities", "--require", &wanted])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_offline-depth"))
            .args([
                "--capabilities",
                "--require",
                "{\"protocols\":{\"unknown\":1}}"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn d2c_preserves_build_identity_per_frame_counters_and_progress() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), false, 1.);
    let output = tempfile::tempdir().unwrap();
    let progress = output.path().join("progress.jsonl");
    let result = support::cli(
        source.path(),
        &[
            "--d2c",
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "warp",
            "--mode",
            "both",
            "--progress",
            progress.to_str().unwrap(),
        ],
    );
    assert!(result.status.success(), "{:?}", result);
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["d2c_diagnostics"]["valid_output"], 1);
    let target = output.path().join("warp");
    let build: Value =
        serde_json::from_slice(&fs::read(target.join("build-info.json")).unwrap()).unwrap();
    assert_eq!(build, report["build"]);
    assert_eq!(build["executable_sha256"].as_str().unwrap().len(), 64);
    let stats: Value = serde_json::from_slice(
        &fs::read(target.join("d2c-diagnostics-00000000000000000000.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["counters"]["source_pixels"], 2);
    assert_eq!(stats["counters"]["zero_depth"], 1);
    assert_eq!(stats["pair_sequence"], 1);
    assert_eq!(stats["algorithm_version"], 1);
    assert_eq!(
        support::decoded(&target.join("frames/00000000000000000000.png")),
        [2000, 0]
    );
    let events = fs::read_to_string(progress)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.first().unwrap()["state"], "running");
    assert_eq!(events.last().unwrap()["state"], "complete");
    assert_eq!(events.last().unwrap()["processed_pairs"], 1);
}
#[test]
fn overflow_failure_stays_private_and_is_not_a_complete_job() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), false, 100.);
    let output = tempfile::tempdir().unwrap();
    let progress = output.path().join("progress.jsonl");
    let result = support::cli(
        source.path(),
        &[
            "--d2c",
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "bad",
            "--progress",
            progress.to_str().unwrap(),
        ],
    );
    assert!(!result.status.success());
    assert!(!output.path().join("bad").exists());
    assert!(!output.path().join("bad.partial/COMPLETE").exists());
    let failure: Value =
        serde_json::from_slice(&fs::read(output.path().join("bad.partial/FAILED.json")).unwrap())
            .unwrap();
    assert_eq!(failure["detail_code"], "D2C_OUTPUT_OVERFLOW");
    assert_eq!(failure["last_observed_pair_sequence"], 1);
    assert!(!String::from_utf8_lossy(&result.stdout).contains(source.path().to_str().unwrap()));
}
#[test]
fn cancellation_and_progress_paths_cannot_overwrite_source_or_publish_complete() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), false, 0.);
    let output = tempfile::tempdir().unwrap();
    let progress = output.path().join("progress.jsonl");
    let control = JobControl::new(Some(&progress)).unwrap();
    let mut source = DirectoryFrameSource::open(source.path(), None).unwrap();
    struct CancelOnRead<'a> {
        source: &'a mut DirectoryFrameSource,
        control: JobControl,
    }
    impl offline_depth::FrameSource for CancelOnRead<'_> {
        fn snapshot(&self) -> &capture_contract::snapshot::SnapshotEnvelope {
            offline_depth::FrameSource::snapshot(self.source)
        }
        fn next_frame(&mut self) -> offline_depth::Result<Option<offline_depth::SourceFrame>> {
            let frame = offline_depth::FrameSource::next_frame(self.source)?;
            self.control.cancel();
            Ok(frame)
        }
    }
    let mut source = CancelOnRead {
        source: &mut source,
        control: control.clone(),
    };
    let error = control
        .execute(|| {
            run(
                &mut source,
                output.path(),
                "cancel",
                RunOptions {
                    operation: Operation::Original {
                        normalize_mm: false,
                    },
                    mode: depth_output::Mode::Both,
                    queue_capacity: 1,
                },
            )
        })
        .unwrap_err();
    assert_eq!(error.code(), "JOB_CANCELLED");
    assert!(!output.path().join("cancel").exists());
    assert!(!output.path().join("cancel.partial/COMPLETE").exists());
    assert!(JobControl::new(Some(&progress)).is_err());
    assert!(JobControl::new(Some(std::path::Path::new("relative"))).is_err());
    let events = fs::read_to_string(progress).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(events.lines().last().unwrap()).unwrap()["state"],
        "cancelled"
    );
}

#[cfg(unix)]
#[test]
fn actual_sigterm_is_a_cooperative_cancel_not_a_success() {
    use std::{
        io::Read,
        time::{Duration, Instant},
    };
    let input = tempfile::tempdir().unwrap();
    let mut manifest = support::fixture(input.path(), false, 0.);
    let pair: Value =
        serde_json::from_slice(&fs::read(input.path().join("pair.json")).unwrap()).unwrap();
    let template = manifest.pairs[0].clone();
    manifest.pairs.clear();
    for i in 1..=1024u64 {
        let mut pair = pair.clone();
        pair["pair_sequence"] = json!(i);
        for stream in ["color", "depth"] {
            pair[stream]["context"]["sequence"] = json!(i);
            pair[stream]["context"]["device_timestamp_us"] = json!(i * 1000);
            pair[stream]["context"]["system_timestamp_us"] = json!(i * 10000);
            pair[stream]["context"]["received_mono_offset_ns"] = json!(i * 100);
            pair[format!("{stream}_stamp_us")] = json!(i * 1000);
        }
        let evidence = serde_json::to_vec(&pair).unwrap();
        let mut entry = template.clone();
        entry.pair_sequence = i;
        entry.evidence = format!("p{i}.json");
        entry.depth = format!("d{i}.raw16");
        entry.evidence_sha256 = offline_depth::sha256(&evidence);
        fs::write(input.path().join(&entry.evidence), evidence).unwrap();
        fs::write(input.path().join(&entry.depth), 1000u32.to_le_bytes()).unwrap();
        entry.depth_sha256 = offline_depth::sha256(&1000u32.to_le_bytes());
        manifest.pairs.push(entry);
    }
    fs::write(
        input.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let output = tempfile::tempdir().unwrap();
    let progress = output.path().join("progress.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .args([
            "--input",
            input.path().to_str().unwrap(),
            "--output",
            output.path().to_str().unwrap(),
            "--job",
            "signal",
            "--progress",
            progress.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&progress).is_ok_and(|text| text.contains("running")) {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("no progress")
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cancel deadline")
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(!status.success());
    let mut raw = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut raw)
        .unwrap();
    let report: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(report["code"], "JOB_CANCELLED");
    assert!(!output.path().join("signal").exists());
    assert!(!output.path().join("signal.partial/COMPLETE").exists());
}

#[test]
fn public_error_categories_keep_backward_compatible_codes_and_specific_failure_details() {
    use offline_depth::OfflineError;
    let io = fs::File::open("/nonexistent-gdp-error-source").unwrap_err();
    assert_eq!(OfflineError::from(io).code(), "INPUT_OUTPUT_IO");
    let geometry = d2c_core::CameraModel::new(
        d2c_core::CameraIntrinsics {
            width: 0,
            height: 1,
            fx: 1.,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            skew: 0.,
        },
        d2c_core::DistortionModel::PinholeZero,
    )
    .unwrap_err();
    assert_eq!(OfflineError::from(geometry).code(), "GEOMETRY_INVALID");
    let warp = d2c_core::DepthScale::millimeters_per_count(0.).unwrap_err();
    assert_eq!(OfflineError::from(warp).code(), "WARP_FAILED");
    let tmp = tempfile::tempdir().unwrap();
    let id = capture_contract::SnapshotId::parse("a".repeat(64)).unwrap();
    let output =
        depth_output::Job::create(tmp.path(), "../escape", depth_output::Mode::Directory, id)
            .err()
            .unwrap();
    assert_eq!(OfflineError::from(output).code(), "OUTPUT_FAILED");
    let uncertain = depth_output::OutputError::CommitUncertain {
        path: tmp.path().join("committed"),
        source: std::io::Error::other("parent durability was not confirmed"),
    };
    assert_eq!(
        OfflineError::from(uncertain).code(),
        "OUTPUT_COMMIT_UNCERTAIN"
    );
    assert_eq!(OfflineError::Worker.code(), "OUTPUT_WORKER_FAILED");
}

#[test]
fn job_control_is_single_use_across_clones_and_failed_runs() {
    let control = JobControl::default();
    let alias = control.clone();
    assert_eq!(control.execute(|| Ok(7)).unwrap(), 7);
    assert_eq!(
        alias.execute(|| Ok(8)).unwrap_err().detail_code(),
        "JOB_CONTROL_ALREADY_USED"
    );
    let failed = JobControl::default();
    assert!(
        failed
            .execute(|| Err::<(), _>(offline_depth::OfflineError::Invalid("TEST_FAILURE")))
            .is_err()
    );
    assert_eq!(
        failed.execute(|| Ok(())).unwrap_err().detail_code(),
        "JOB_CONTROL_ALREADY_USED"
    );
}
