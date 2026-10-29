mod common;
use serde_json::json;
use std::{
    io::Write,
    process::{Command, Stdio},
};
#[test]
fn persistent_cli_recovers_after_invalid_and_oversized_input_without_private_echo() {
    let path = std::env::temp_dir().join(format!("observer-config-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec(&common::config()).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-observer"))
        .args(["--config", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"{\"version\":1,\"version\":1}\n").unwrap();
    input.write_all(&vec![b'x'; 65537]).unwrap();
    input.write_all(b"\n").unwrap();
    writeln!(
        input,
        "{}",
        json!({"version":1,"command":{"kind":"tick","at":common::at(200),"received_unix_ns":null}})
    )
    .unwrap();
    writeln!(input,"{}",json!({"version":1,"command":{"kind":"query","request":{"version":1,"query":{"kind":"snapshot"}}}})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let lines = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["error"]["code"], "INPUT_INVALID");
    assert_eq!(lines[1]["error"]["code"], "INPUT_LIMIT");
    assert_eq!(lines[2]["ok"], true);
    assert_eq!(lines[3]["kind"], "query");
    assert!(!text.contains("synthetic_color_optical"));
    assert!(!text.contains("synthetic_depth_optical"));
}
#[test]
fn cli_interruption_command_survives_finalization_as_a_bounded_reasoned_event() {
    let path =
        std::env::temp_dir().join(format!("observer-interruption-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec(&common::config()).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-observer"))
        .args(["--config", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for command in [
        json!({"kind":"interrupt","at":common::at(1),"reason":"driver-exited"}),
        json!({"kind":"finalize"}),
        json!({"kind":"query","request":{"version":1,"query":{"kind":"snapshot"}}}),
        json!({"kind":"query","request":{"version":1,"query":{"kind":"events","cursor":null,"limit":16}}}),
    ] {
        writeln!(input, "{}", json!({"version":1,"command":command})).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success());
    let lines = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines[0]["kind"], "interrupted");
    let snapshot = &lines[2]["result"]["quality"]["result"]["snapshot"];
    assert_eq!(snapshot["state"], "failed");
    assert_eq!(snapshot["interruption_reason"], "driver-exited");
    assert_eq!(snapshot["streams"]["color"]["frame_count"], 0);
    assert_eq!(
        snapshot["finalized_sessions"][0]["interruption_reason"],
        "driver-exited"
    );
    let events = lines[3]["result"]["quality"]["result"]["page"]["events"]
        .as_array()
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["violation"], "OBSERVER_INTERRUPTED");
}

#[test]
fn command_line_config_limits_and_metadata_dispatch_do_not_accept_unsupported_versions() {
    for args in [
        vec!["--help"],
        vec![],
        vec!["--config", "/nonexistent-observer-config"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_capture-observer"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(out.status.success(), args == ["--help"]);
    }
    let path =
        std::env::temp_dir().join(format!("gdp-observer-limits-{}.json", std::process::id()));
    std::fs::write(&path, vec![b' '; 65537]).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_capture-observer"))
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("CONFIG_LIMIT"));
    std::fs::write(&path, serde_json::to_vec(&common::config()).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-observer"))
        .arg("--config")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for v in [
        json!({"version":2,"command":{"kind":"finalize"}}),
        json!({"version":1,"command":{"kind":"metadata","stream":"color","at":common::at(0),"metadata":{"stamp":{"sec":1,"nanosec":0},"data":"invalid"}}}),
    ] {
        writeln!(input, "{v}").unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("OBSERVER_REQUEST_VERSION"));
    assert!(text.contains("update"));
    std::fs::remove_file(path).unwrap();
}
