//! Real loopback HTTP responses around synthetic, session-bound producers.
use super::*;
use crate::management_session_tests::Fixture;
use std::{
    io::{Read, Write},
    net::TcpStream,
};
fn manager(f: &Fixture) -> Arc<Manager> {
    Arc::new(Manager {
        ctl: f.ctl.clone(),
        token: "test-token".into(),
        busy: AtomicBool::new(false),
        active_job: AtomicBool::new(false),
        last_seen: Mutex::new(Instant::now()),
        job: Mutex::new(Value::Null),
    })
}
fn request(
    m: &Arc<Manager>,
    method: &str,
    path: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let worker = m.clone();
    let handle = thread::spawn(move || super::handle(server.recv().unwrap(), &worker, addr.port()));
    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        client,
        "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for (name, value) in [
        ("Host", format!("127.0.0.1:{}", addr.port())),
        ("Content-Type", "application/json".into()),
        ("X-CSRF-Token", "test-token".into()),
    ] {
        if !extra.iter().any(|(key, _)| key.eq_ignore_ascii_case(name)) {
            write!(client, "{name}: {value}\r\n").unwrap();
        }
    }
    for (name, value) in extra {
        write!(client, "{name}: {value}\r\n").unwrap();
    }
    write!(client, "\r\n{body}").unwrap();
    let mut bytes = vec![];
    client.read_to_end(&mut bytes).unwrap();
    handle.join().unwrap();
    let index = bytes.windows(4).position(|s| s == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8(bytes[..index].to_vec()).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, headers, bytes[index + 4..].to_vec())
}
fn get(m: &Arc<Manager>, path: &str) -> (u16, Value) {
    let (status, headers, body) = request(m, "GET", path, "", &[]);
    assert!(headers.contains("Cache-Control: no-store"));
    (status, serde_json::from_slice(&body).unwrap())
}
fn encoded(path: &str, key: &str, value: &Value) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(key, &value.to_string())
        .finish();
    format!("{path}?{query}")
}
#[test]
fn live_quality_http_routes_bind_cursor_context_and_bounded_batch_window() {
    let f = Fixture::new(false, 1);
    let m = manager(&f);
    let (status, current) = get(&m, "/api/quality/current?camera=camera_a");
    assert_eq!(status, 200);
    assert_eq!(current["worker_ready"], true);
    let cursor = json!({"origin_session_id":current["session_id"],"after_sequence":0});
    let path = format!(
        "{}&camera=camera_a&limit=1",
        encoded("/api/quality/events", "cursor", &cursor)
    );
    let (status, page) = get(&m, &path);
    assert_eq!(status, 200);
    assert_eq!(
        page["result"]["page"]["events"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        get(&m, "/api/quality/events?camera=camera_a&limit=bad").0,
        400
    );
    let mut expected = json!({});
    for key in ["generation_id", "instance_id", "session_id", "snapshot_id"] {
        expected[key] = current[key].clone();
    }
    let batch =
        json!({"version":1,"cameras":[{"camera":"camera_a","expected":expected,"limit":1}]});
    let (status, value) = get(&m, &encoded("/api/quality/batch", "request", &batch));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["atomic_group_snapshot"], false);
    let window = json!({"version":1,"camera":"camera_a","expected":expected,"from_cursor":cursor,"to_sequence":1,"limit":1});
    let (status, value) = get(&m, &encoded("/api/quality/window", "request", &window));
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        get(&m, "/api/quality/session-summary?camera=camera_a").0,
        200
    );
    assert_eq!(
        get(
            &m,
            "/api/quality/session-summary?camera=camera_a&generation=bad&session=bad"
        )
        .0,
        400
    );
    assert_eq!(get(&m, "/api/usbfs").0, 200);
    let (status, context) = get(&m, "/api/recording-context?require_strong=true");
    assert_eq!(status, 200);
    assert_eq!(context["protection"]["native_gate_ack_verified"], false);
}
#[test]
fn http_rejects_invalid_quality_inputs_and_observer_strong_guarantees() {
    let f = Fixture::new(true, 1);
    let m = manager(&f);
    let (status, context) = get(&m, "/api/recording-context");
    assert_eq!(status, 200);
    assert_eq!(context["protection"]["native_restart_gate"], false);
    let (status, value) = get(&m, "/api/recording-context?require_strong=true");
    assert_eq!(status, 400);
    assert_eq!(value["error"], "RECORDING_STRONG_GUARANTEE_UNSUPPORTED");
    assert_eq!(get(&m, "/api/quality/current?camera=missing").0, 400);
    assert_eq!(
        get(&m, "/api/quality/events?camera=camera_a&cursor=%7Bbad").0,
        400
    );
    let (status, value) = get(&m, "/api/quality/batch?request=%7B%7D");
    assert_eq!(status, 400);
    assert!(text(&value["error"]).starts_with("QUALITY_REQUEST_VERSION"));
}
#[test]
fn frame_routes_serve_only_requested_fresh_streams_and_static_assets_have_headers() {
    let f = Fixture::new(false, 1);
    let m = manager(&f);
    assert_eq!(get(&m, "/api/frame?role=camera_a&stream=color").0, 404);
    assert_eq!(get(&m, "/api/frame?role=camera_a&stream=unknown").0, 400);
    let image = f.ctl.root.join("work/web-preview/camera_a/out/color.jpg");
    private_dir(image.parent().unwrap()).unwrap();
    fs::write(&image, [0xff, 0xd8, 0xff, 0xd9]).unwrap();
    let (status, headers, body) =
        request(&m, "GET", "/api/frame?role=camera_a&stream=color", "", &[]);
    assert_eq!(status, 200);
    assert_eq!(body, [0xff, 0xd8, 0xff, 0xd9]);
    assert!(headers.contains("image/jpeg"));
    fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10))
        .unwrap();
    assert_eq!(get(&m, "/api/frame?role=camera_a&stream=color").0, 404);
    assert_eq!(get(&m, "/api/preview/logs?role=camera_a").1["log"], "");
    private_dir(&f.ctl.root.join("web")).unwrap();
    for (file, path, mime) in [
        ("index.html", "/", "text/html"),
        ("app.js", "/app.js", "text/javascript"),
        ("style.css", "/style.css", "text/css"),
    ] {
        fs::write(f.ctl.root.join("web").join(file), "__CSRF__").unwrap();
        let (status, headers, body) = request(&m, "GET", path, "", &[]);
        assert_eq!(status, 200);
        assert!(headers.contains(mime));
        assert!(headers.contains("Content-Security-Policy"));
        assert_eq!(
            body,
            if path == "/" {
                b"test-token".as_slice()
            } else {
                b"__CSRF__".as_slice()
            }
        );
    }
}
#[test]
fn http_security_and_request_shape_reject_before_mutation() {
    let f = Fixture::new(false, 1);
    let m = manager(&f);
    let before = fs::read(f.ctl.root.join("inventory.json")).unwrap();
    for (headers, expected) in [
        (vec![("Host", "evil.example")], 403),
        (vec![("Origin", "https://evil.example")], 403),
        (vec![("X-CSRF-Token", "wrong")], 403),
        (vec![("Content-Type", "text/plain")], 400),
    ] {
        assert_eq!(
            request(&m, "POST", "/api/config", "{}", &headers).0,
            expected
        );
    }
    for body in ["[]", "not-json", ""] {
        assert_eq!(request(&m, "POST", "/api/config", body, &[]).0, 400);
    }
    assert_eq!(request(&m, "DELETE", "/healthz", "", &[]).0, 405);
    assert_eq!(get(&m, "/unknown").0, 404);
    m.busy.store(true, Ordering::Release);
    assert_eq!(
        request(
            &m,
            "POST",
            "/api/preview/stop",
            r#"{"role":"camera_a"}"#,
            &[]
        )
        .0,
        400
    );
    m.busy.store(false, Ordering::Release);
    assert_eq!(
        request(
            &m,
            "POST",
            "/api/preview/stop",
            r#"{"role":"camera_a"}"#,
            &[]
        )
        .0,
        200
    );
    assert_eq!(fs::read(f.ctl.root.join("inventory.json")).unwrap(), before);
}
#[test]
fn abandoned_job_is_failed_while_state_renews_preview_lease() {
    let f = Fixture::new(false, 1);
    let m = manager(&f);
    let lease = f.ctl.root.join("work/web-preview/camera_a/config/lease");
    atomic(&lease, b"").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&lease)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH)
        .unwrap();
    persist_job(
        &f.ctl,
        &json!({"id":"a".repeat(12),"status":"running","log":[]}),
    )
    .unwrap();
    let (status, value) = get(&m, "/api/state");
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["job"]["status"], "failed");
    assert_eq!(value["generated_current"], true);
    assert!(value["docker_error"].is_null());
    assert!(
        fs::metadata(lease)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            < Duration::from_secs(2)
    );
}
#[test]
fn job_retention_preserves_latest_result_and_only_removes_old_json_jobs() {
    let f = Fixture::new(false, 1);
    let dir = f.ctl.root.join("state/jobs");
    private_dir(&dir).unwrap();
    fs::write(dir.join("notes.txt"), "keep").unwrap();
    for i in 0..55 {
        persist_job(
            &f.ctl,
            &json!({"id":format!("{i:012x}"),"status":"passed","log":[]}),
        )
        .unwrap();
    }
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "json"))
            .count(),
        50
    );
    assert_eq!(
        read_json(&f.ctl.root.join("state/latest-job.json")).unwrap()["id"],
        format!("{:012x}", 54)
    );
    assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "keep");
    assert!(persist_job(&f.ctl, &json!({"id":"../escape"})).is_err());
}

#[test]
fn preview_start_uses_bound_private_config_and_refuses_a_running_formal_group() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(true, 1);
    f.prepare_host();
    assert!(
        start_preview(&f.ctl, "camera_a")
            .unwrap_err()
            .to_string()
            .starts_with("CAMERAS_RUNNING")
    );
    f.ctl.stop("camera", true).unwrap();
    start_preview(&f.ctl, "camera_a").unwrap();
    let folder = f.ctl.root.join("work/web-preview/camera_a/config");
    for file in [
        "camera.yaml",
        "health.json",
        "binding.json",
        "revision",
        "lease",
    ] {
        assert!(folder.join(file).is_file());
        assert_eq!(
            fs::metadata(folder.join(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::read_to_string(folder.join("revision")).unwrap(),
        config::camera_digest(&f.inventory, &f.inventory["cameras"][0])
    );
    let config = read_json(&folder.join("camera.yaml")).unwrap();
    assert_eq!(config["namespace"], Value::Null);
    assert_eq!(
        config["camera_name"],
        f.inventory["cameras"][0]["frame_prefix"]
    );
}
