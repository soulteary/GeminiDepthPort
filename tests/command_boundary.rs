use geminidepthport::util::{LogSink, command, command_logged};
use std::sync::{Arc, Mutex};

#[test]
fn command_input_is_delivered_exactly_and_large_unterminated_logs_remain_bounded() {
    let root = tempfile::tempdir().unwrap();
    let input = b"private request\nwith a second line\0";
    assert_eq!(
        command(root.path(), &["/bin/cat".into()], 5, Some(input))
            .unwrap()
            .as_bytes(),
        input
    );
    let lines = Arc::new(Mutex::new(Vec::new()));
    let observed = lines.clone();
    let sink: LogSink = Arc::new(move |line| observed.lock().unwrap().push(line.to_owned()));
    let result = command_logged(
        root.path(),
        &[
            "/bin/sh".into(),
            "-c".into(),
            "head -c 600000 /dev/zero | tr '\\000' x; printf final >&2".into(),
        ],
        10,
        None,
        Some(sink),
    )
    .unwrap();
    assert_eq!(result.len(), 512 * 1024);
    let lines = lines.lock().unwrap();
    assert_eq!(lines.iter().map(String::len).sum::<usize>(), 600005);
    assert!(lines.iter().any(|s| s == "final"));
}

#[test]
fn timeout_kills_a_term_resistant_worker_and_empty_command_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        command(root.path(), &[], 1, None)
            .unwrap_err()
            .to_string()
            .starts_with("COMMAND:")
    );
    // --root selects the graceful worker shutdown path; this worker ignores TERM.
    let started = std::time::Instant::now();
    // Use an executable script so --root is a worker argument rather than a shell option.
    use std::os::unix::fs::PermissionsExt;
    let script = root.path().join("worker");
    geminidepthport::control::Controller::new(root.path())
        .unwrap()
        .run_with_input(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "--".into(),
                script.to_string_lossy().into(),
            ],
            5,
            b"#!/bin/sh\ntrap '' TERM\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = command(
        root.path(),
        &[script.to_string_lossy().into(), "--root".into()],
        1,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("COMMAND_TIMEOUT"), "{error}");
    assert!(started.elapsed().as_secs() < 6);
}

#[test]
fn observer_bootstrap_rejects_relative_and_incomplete_inputs_with_redacted_error() {
    for args in [
        vec![],
        vec![
            "relative-policy",
            "relative-observations",
            "private-instance",
            "private-session",
            "1",
        ],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_observer-bootstrap"))
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap().trim(),
            "OBSERVER_BOOTSTRAP_FAILED"
        );
    }
}
