use geminidepthport::quality_batch::execute_with;
use serde_json::{Value, json};
use std::{fs, process::Command};
fn binding() -> Value {
    json!({"generation_id":"a".repeat(32),"instance_id":"b".repeat(64),"session_id":"c".repeat(32),"snapshot_id":"d".repeat(64)})
}
fn envelope(kind: &str) -> Value {
    let mut value = binding();
    value["version"] = json!(1);
    value["backend"] = json!("rust");
    value["result"] = if kind == "events" {
        json!({"page":{"origin_session_id":"c".repeat(32),"latest_sequence":4,"gap":null,
        "events":[{"sequence":2,"scope":{"kind":"future-scope"}},{"sequence":3,"scope":{"kind":"recorder"}},{"sequence":4}],
        "next_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":4},"has_more":false}})
    } else {
        json!({"quality":{"active_episodes":[]}})
    };
    value
}
#[test]
fn management_discovery_is_read_only_without_inventory_or_docker() {
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("absent");
    for command in ["capabilities", "build-info"] {
        let output = Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
            .args(["--root", absent.to_str().unwrap(), command])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let build = if command == "capabilities" {
            &value["build"]
        } else {
            &value
        };
        assert_eq!(build["component"], "geminidepthport");
        assert_eq!(build["identity_attested"], false);
        assert_eq!(build["executable_sha256"].as_str().unwrap().len(), 64);
        assert!(!absent.exists());
    }
    let wanted = json!({"protocols":{"quality_batch":1}}).to_string();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
            .args(["capabilities", "--require", &wanted])
            .output()
            .unwrap()
            .status
            .success()
    );
    let bad = json!({"protocols":{"quality_batch":99}}).to_string();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
            .args(["capabilities", "--require", &bad])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}
#[test]
fn bounded_batch_rechecks_every_session_and_keeps_unknown_facts() {
    let request = json!({"version":1,"cameras":[{"camera":"camera_a","expected":binding()},{"camera":"camera_b","expected":binding()}]});
    let mut calls = 0;
    let value = execute_with("quality-batch", &request, |_, kind, _, _| {
        calls += 1;
        Ok(envelope(kind))
    })
    .unwrap();
    assert_eq!(calls, 6);
    assert_eq!(value["atomic_group_snapshot"], false);
    assert!(value["business_validity"].is_null());
    assert_eq!(
        value["cameras"][0]["events"]["result"]["page"]["events"][0]["scope"]["kind"],
        "future-scope"
    );
    for count in [0, 4] {
        let invalid = json!({"version":1,"cameras":vec![request["cameras"][0].clone();count]});
        assert!(
            execute_with("quality-batch", &invalid, |_, _, _, _| panic!(
                "invalid group queried a device"
            ))
            .is_err()
        );
    }
    let mut calls = 0;
    assert!(
        execute_with("quality-batch", &request, |_, kind, _, _| {
            calls += 1;
            let mut value = envelope(kind);
            if calls == 3 {
                value["session_id"] = json!("e".repeat(32));
            }
            Ok(value)
        })
        .is_err()
    );
    let duplicate = json!({"version":1,"cameras":[request["cameras"][0].clone(),request["cameras"][0].clone()]});
    assert!(
        execute_with("quality-batch", &duplicate, |_, kind, _, _| Ok(envelope(
            kind
        )))
        .is_err()
    );
}
#[test]
fn windows_are_bounded_explicitly_paged_and_do_not_hide_gaps() {
    let mut request = json!({"version":1,"camera":"camera_a","expected":binding(),"from_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":1},"to_sequence":3,"limit":2});
    let value = execute_with("quality-window", &request, |_, kind, _, _| {
        Ok(envelope(kind))
    })
    .unwrap();
    assert_eq!(value["page"]["events"].as_array().unwrap().len(), 2);
    assert_eq!(value["window_complete"], true);
    assert_eq!(value["page"]["next_cursor"]["after_sequence"], 3);
    let value = execute_with("quality-window", &request, |_, kind, _, _| {
        let mut v = envelope(kind);
        v["result"]["page"]["events"] = json!([{"sequence":2}]);
        Ok(v)
    })
    .unwrap();
    assert_eq!(value["window_complete"], false);
    assert_eq!(value["page"]["has_more"], true);
    let value = execute_with("quality-window", &request, |_, kind, _, _| {
        let mut v = envelope(kind);
        v["result"]["page"]["gap"] = json!({"code":"EVENT_GAP"});
        Ok(v)
    })
    .unwrap();
    assert_eq!(value["window_complete"], false);
    assert_eq!(value["page"]["gap"]["code"], "EVENT_GAP");
    let mut broken = request.clone();
    broken["to_sequence"] = json!(4);
    assert!(
        execute_with("quality-window", &broken, |_, kind, _, _| {
            let mut v = envelope(kind);
            v["result"]["page"]["events"] = json!([{"sequence":2},{"sequence":4}]);
            Ok(v)
        })
        .is_err()
    );
    request["to_sequence"] = json!(5);
    assert!(
        execute_with("quality-window", &request, |_, kind, _, _| Ok(envelope(
            kind
        )))
        .is_err()
    );
    request["to_sequence"] = json!(3);
    request["limit"] = json!(1025);
    assert!(
        execute_with("quality-window", &request, |_, kind, _, _| Ok(envelope(
            kind
        )))
        .is_err()
    );
}
#[test]
fn terminal_no_snapshot_facts_stay_unbound() {
    let mut expected = binding();
    expected["snapshot_id"] = Value::Null;
    let request = json!({"version":1,"cameras":[{"camera":"camera_a","expected":expected}]});
    let value = execute_with("quality-batch", &request, |_, kind, _, _| {
        let mut v = envelope(kind);
        v["snapshot_id"] = Value::Null;
        Ok(v)
    })
    .unwrap();
    assert!(value["cameras"][0]["before"]["snapshot_id"].is_null());
    assert!(value["business_validity"].is_null());
}

#[test]
fn actual_http_capabilities_and_invalid_window_are_read_only() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpStream,
    };
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("inventory.json"),
        include_bytes!("../inventory.v2.example.json"),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_geminidepthport"))
        .args([
            "--root",
            temp.path().to_str().unwrap(),
            "serve",
            "--port",
            "0",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    struct Cleanup(std::process::Child);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Cleanup(child);
    let mut line = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = line
        .trim()
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let get = |path: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let response = get("/api/capabilities");
    assert!(response.starts_with("HTTP/1.1 200"));
    let value: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(value["worker_ready"], false);
    let request = url::form_urlencoded::byte_serialize(b"{\"protocols\":{\"offline_input\":2}}")
        .collect::<String>();
    assert!(get(&format!("/api/capabilities?require={request}")).starts_with("HTTP/1.1 200"));
    assert!(get("/api/build-info").contains("executable_sha256"));
    let bad = url::form_urlencoded::byte_serialize(b"{\"version\":99}").collect::<String>();
    assert!(get(&format!("/api/quality/window?request={bad}")).starts_with("HTTP/1.1 400"));
}

#[test]
fn evicted_windows_terminate_without_fabricating_a_cursor_or_completion() {
    for limit in [1, 3] {
        let request = json!({"version":1,"camera":"camera_a","expected":binding(),
            "from_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":0},
            "to_sequence":5,"limit":limit});
        let mut calls = 0;
        let result=execute_with("quality-window",&request,|_,kind,cursor,limit|{
            calls+=1;let mut value=envelope(kind);
            let producer=json!({"version":1,"origin_session_id":"c".repeat(32),
                "latest_sequence":12,"gap":null,"events":(10..=12).map(|s|json!({"version":1,"sequence":s,"scope":{"kind":"future-scope"}})).collect::<Vec<_>>(),
                "has_more":false,"next_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":12}});
            value["result"]["page"]=geminidepthport::managed_quality::page_events(producer,cursor,limit)?;
            Ok(value)
        }).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(result["window_complete"], false);
        assert_eq!(result["terminal_reason"], "WINDOW_EVICTED");
        assert_eq!(result["page"]["has_more"], false);
        assert_eq!(result["page"]["next_cursor"]["after_sequence"], 0);
        assert_eq!(result["page"]["gap"]["code"], "EVENT_GAP");
        assert_eq!(result["page"]["gap"]["first_retained_sequence"], 10);
        assert_eq!(result["page"]["gap"]["last_lost_sequence"], 9);
        assert!(result["page"]["events"].as_array().unwrap().is_empty());
    }
}

#[test]
fn partially_evicted_windows_progress_and_empty_windows_do_not_loop() {
    let mut request = json!({"version":1,"camera":"camera_a","expected":binding(),
        "from_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":0},"to_sequence":12,"limit":1});
    for (after, more, reason) in [
        (0, true, Value::Null),
        (10, true, Value::Null),
        (11, false, Value::Null),
        (12, false, Value::Null),
    ] {
        request["from_cursor"]["after_sequence"] = json!(after);
        let result=execute_with("quality-window",&request,|_,kind,cursor,limit|{
            let mut value=envelope(kind);
            let producer=json!({"version":1,"origin_session_id":"c".repeat(32),"latest_sequence":12,"gap":null,
                "events":(10..=12).map(|s|json!({"version":1,"sequence":s})).collect::<Vec<_>>(),"has_more":false,
                "next_cursor":{"origin_session_id":"c".repeat(32),"after_sequence":12}});
            value["result"]["page"]=geminidepthport::managed_quality::page_events(producer,cursor,limit)?;Ok(value)
        }).unwrap();
        assert_eq!(result["page"]["has_more"], more);
        assert_eq!(result["terminal_reason"], reason);
        if more {
            assert!(
                result["page"]["next_cursor"]["after_sequence"]
                    .as_u64()
                    .unwrap()
                    > after
            );
        }
    }
}

#[test]
fn unknown_expected_binding_fields_are_not_treated_as_capture_identity() {
    let mut expected = binding();
    expected["unexpected"] = json!(true);
    let request = json!({"version":1,"cameras":[{"camera":"camera_a","expected":expected}]});
    assert!(
        execute_with("quality-batch", &request, |_, kind, _, _| Ok(envelope(
            kind
        )))
        .unwrap_err()
        .to_string()
        .starts_with("QUALITY_REQUEST_SCHEMA")
    );
}
