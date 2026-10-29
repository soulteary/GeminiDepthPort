//! Linux-only black-box process supervision. The native ABI fixtures never open
//! USB, a real SDK device or ROS. macOS deliberately rejects live workers.
#![cfg(target_os = "linux")]
use ring::digest;
use serde_json::{Value, json};
use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    config: PathBuf,
    status: PathBuf,
    events: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn compile(compiler: &str, source: &Path, output: &Path, include: Option<&Path>) {
    let mut command = Command::new(compiler);
    command.args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"]);
    command.arg(if compiler == "cc" {
        "-std=c11"
    } else {
        "-std=c++17"
    });
    if let Some(include) = include {
        command.arg("-I").arg(include);
    }
    let out = command
        .arg(source)
        .arg("-o")
        .arg(output)
        .output()
        .expect("C/C++ compiler required for process ABI fixture");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn replace(source: &mut String, old: &str, new: &str) {
    assert!(
        source.contains(old),
        "ABI fixture changed; review service adaptation"
    );
    *source = source.replace(old, new);
}
fn fixture() -> Fixture {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = std::env::temp_dir().join(format!(
        "gdp-supervisor-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let sdk = root.join("mock-sdk.so");
    let bridge = root.join("mock-bridge.so");
    let mut sdk_source =
        fs::read_to_string(crate_root.join("../orbbec-safe/tests/mock_sdk.c")).unwrap();
    sdk_source.insert_str(0,"#define _DEFAULT_SOURCE\n#include <unistd.h>\n#include <signal.h>\nstatic unsigned long long service_sequence;\n");
    replace(
        &mut sdk_source,
        "if(ms==1)return NULL;return make(5);",
        "if(ms==1)return NULL;usleep(10000);service_sequence++;return make(5);",
    );
    replace(
        &mut sdk_source,
        "(void)d;(void)e;return make(3);",
        "(void)d;(void)e;const char *helper_path=getenv(\"GDP_MOCK_HELPER_PID\");if(helper_path){pid_t helper=fork();if(helper==0){signal(SIGTERM,SIG_IGN);signal(SIGINT,SIG_IGN);for(;;)pause();}if(helper>0){FILE *f=fopen(helper_path,\"w\");if(f){fprintf(f,\"%ld\",(long)helper);fclose(f);}}}const char *delay=getenv(\"GDP_MOCK_OPEN_DELAY_MS\");if(delay)usleep((useconds_t)(strtoul(delay,NULL,10)*1000));return make(3);",
    );
    replace(&mut sdk_source, "return 42;", "return service_sequence;");
    replace(
        &mut sdk_source,
        "return 1234;",
        "return service_sequence*33333;",
    );
    replace(
        &mut sdk_source,
        "return 5678;",
        "return 1700000000000000ULL+service_sequence*33333;",
    );
    replace(
        &mut sdk_source,
        "if(ms==1)return NULL;usleep(10000);service_sequence++;return make(5);",
        "usleep(10000);if(getenv(\"GDP_MOCK_WAIT_ERROR\")){*e=(error*)1;return NULL;}if(ms==1||getenv(\"GDP_MOCK_TIMEOUT\")||(getenv(\"GDP_MOCK_STALL\")&&service_sequence>3))return NULL;service_sequence++;return make(5);",
    );
    replace(
        &mut sdk_source,
        "return service_sequence;",
        "return getenv(\"GDP_MOCK_DUPLICATE\")&&service_sequence>2?2:service_sequence;",
    );
    replace(
        &mut sdk_source,
        "return service_sequence*33333;",
        "return service_sequence*33333+(getenv(\"GDP_MOCK_SKEW\")&&p->stream==3?30000:0);",
    );
    replace(
        &mut sdk_source,
        "return 0.1f;",
        "return getenv(\"GDP_MOCK_SCALE\")?strtof(getenv(\"GDP_MOCK_SCALE\"),NULL):0.1f;",
    );
    replace(
        &mut sdk_source,
        "obj *f=make(5);f->stream=2;return f;",
        "if(getenv(\"GDP_MOCK_MISSING_COLOR\"))return NULL;obj *f=make(5);f->stream=2;return f;",
    );
    replace(
        &mut sdk_source,
        "(void)d;(void)e;const char *helper_path",
        "(void)d;(void)e;if(getenv(\"GDP_MOCK_OPEN_ERROR\")){*e=(error*)1;return NULL;}const char *helper_path",
    );
    let sdk_c = root.join("mock-sdk.c");
    fs::write(&sdk_c, sdk_source).unwrap();
    compile("cc", &sdk_c, &sdk, None);
    let mut bridge_source =
        fs::read_to_string(crate_root.join("../ros-output/tests/mock_bridge.cpp")).unwrap();
    replace(
        &mut bridge_source,
        "if (frame->sequence == 1 && (!frame->has_device_timestamp || frame->device_timestamp_us != 0)) return 2;",
        "",
    );
    replace(
        &mut bridge_source,
        "if (frame->sequence == 999) { event(\"publish-failed\"); return 2; }",
        "",
    );
    replace(
        &mut bridge_source,
        "if (!p.owned.empty() && p.owned[0] != 16) return 2;",
        "",
    );
    replace(
        &mut bridge_source,
        "if (p.frame_id != \"test_optical\" || p.focal != 1.) return 2;",
        "",
    );
    replace(
        &mut bridge_source,
        "if(pair->pair_sequence==999){event(\"aux-publish-failed\");return 2;}",
        "",
    );
    // Supervision exercises the healthy publication path. The shared bridge
    // fixture's pair-2 evidence fault belongs to its dedicated bridge tests.
    replace(
        &mut bridge_source,
        "if(p.last.pair_sequence==2){event(\"evidence-failed\");return 2;}",
        "",
    );
    replace(
        &mut bridge_source,
        "auto &p = *static_cast<Mock *>(handle);",
        "auto &p = *static_cast<Mock *>(handle); if(std::getenv(\"GDP_MOCK_DEPTH_PUBLISH_ERROR\") && p.frame_id==\"depth_optical\")return 2;",
    );
    replace(
        &mut bridge_source,
        "if(!handle||!gdp_ros::pair_valid(pair))return 1;",
        "if(!handle||!gdp_ros::pair_valid(pair))return 1;\n  if(std::getenv(\"GDP_MOCK_PAIR_PUBLISH_ERROR\"))return 2;",
    );
    let bridge_cpp = root.join("mock-bridge.cpp");
    fs::write(&bridge_cpp, bridge_source).unwrap();
    compile(
        "c++",
        &bridge_cpp,
        &bridge,
        Some(&crate_root.join("../../ros/include")),
    );
    let status = root.join("status.json");
    let config = root.join("config.json");
    let events = root.join("events");
    fs::write(&config,json!({"version":1,"sdk_library":sdk,"serial":"test-device","color":{"width":2,"height":2,"fps":30,"frame_id":"color_optical"},"depth":{"width":2,"height":2,"fps":30,"frame_id":"depth_optical","unit_policy":"normalize-mm"},"pairing":{"clock":"device","max_delta_us":20000},"poll_ms":10,"capacity":2,"ros":{"bridge":bridge,"namespace":"/service_test","stamp_source":"sdk-system-unix"},"service":{"status_path":status,"generation":"fixture-generation","idle_timeout_ms":500,"startup_timeout_ms":5000,"heartbeat_ms":50,"heartbeat_timeout_ms":500,"max_restarts":1,"restart_backoff_ms":100,"shutdown_timeout_ms":300,"ready_min_pairs":2}}).to_string()).unwrap();
    Fixture {
        root,
        config,
        status,
        events,
    }
}
fn launch(f: &Fixture) -> Child {
    Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--config")
        .arg(&f.config)
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
fn status(f: &Fixture) -> Option<Value> {
    fs::read_to_string(&f.status)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}
fn wait_status(f: &Fixture, child: &mut Child, predicate: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        if let Some(value) = status(f) {
            if predicate(&value) {
                return value;
            }
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "service exited before expected state: {:?}",
            status(f)
        );
        if start.elapsed() > Duration::from_secs(8) {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let _ = child.wait();
            panic!("status deadline: {:?}", status(f));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn alive(pid: u64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
fn stop(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let started = Instant::now();
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success());
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stopped_worker_is_bounded_killed_then_new_session_recovers_without_config_change() {
    let f = fixture();
    let original = fs::read(&f.config).unwrap();
    let mut supervisor = launch(&f);
    let first = wait_status(&f, &mut supervisor, |v| v["state"] == "ready");
    let old_pid = first["pid"].as_u64().unwrap();
    let old_session = first["session_id"].as_str().unwrap().to_owned();
    unsafe {
        assert_eq!(libc::kill(old_pid as i32, libc::SIGSTOP), 0);
    }
    let stopped = Instant::now();
    thread::sleep(Duration::from_millis(180));
    // Atomic status is a snapshot, not a live readiness lease. A stopped worker
    // may still have ready=true before watchdog expiry, but heartbeat is stale.
    let stale = status(&f).unwrap();
    assert_eq!(stale["session_id"], old_session);
    assert_eq!(stale["ready"], true);
    let age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - stale["updated_unix_ms"].as_u64().unwrap();
    assert!(age >= 100, "health must reject stale ready snapshot");
    let second = wait_status(&f, &mut supervisor, |v| {
        v["state"] == "ready" && v["session_id"].as_str().is_some_and(|s| s != old_session)
    });
    assert!(stopped.elapsed() < Duration::from_secs(4));
    assert!(!alive(old_pid), "old stopped worker was not reaped");
    assert_ne!(second["pid"].as_u64().unwrap(), old_pid);
    assert_eq!(second["restart_attempt"], 1);
    assert_eq!(fs::read(&f.config).unwrap(), original);
    let new_pid = second["pid"].as_u64().unwrap();
    stop(&mut supervisor);
    assert!(!alive(new_pid));
    assert_eq!(status(&f).unwrap()["state"], "stopped");
    let events = fs::read_to_string(&f.events).unwrap();
    assert_eq!(events.lines().filter(|l| *l == "start").count(), 2);
    assert!(events.lines().any(|l| l == "stop"));
}

#[test]
fn sigterm_cleans_worker_native_handles_and_child_process() {
    let f = fixture();
    let mut supervisor = launch(&f);
    let ready = wait_status(&f, &mut supervisor, |v| v["state"] == "ready");
    let worker = ready["pid"].as_u64().unwrap();
    stop(&mut supervisor);
    assert!(!alive(worker));
    let events = fs::read_to_string(&f.events).unwrap();
    for event in [
        "stop",
        "pipeline-delete",
        "device-delete",
        "context-delete",
        "aux-destroy",
        "destroy",
    ] {
        assert!(
            events.lines().any(|l| l == event),
            "missing native cleanup {event}"
        );
    }
    assert_eq!(status(&f).unwrap()["reason"], "SIGNAL");
}

#[test]
fn wrong_runtime_hash_fails_before_any_sdk_start() {
    let f = fixture();
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["runtime_contract"] = json!({"sdk_sha256":"0".repeat(64),"bridge_sha256":"0".repeat(64),"service_sha256":"0".repeat(64)});
    config["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, config.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--validate-config")
        .arg(&f.config)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("test-device"));
    let mut supervisor = launch(&f);
    let start = Instant::now();
    loop {
        if let Some(exit) = supervisor.try_wait().unwrap() {
            assert!(!exit.success());
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(4));
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fs::read_to_string(&f.events).unwrap_or_default().is_empty());
    assert_eq!(status(&f).unwrap()["ready"], false);
}

#[test]
fn config_changed_during_failure_cannot_be_adopted_by_restart() {
    let f = fixture();
    let mut supervisor = launch(&f);
    let ready = wait_status(&f, &mut supervisor, |v| v["state"] == "ready");
    let old_pid = ready["pid"].as_u64().unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["service"]["ready_min_pairs"] = json!(3);
    fs::write(&f.config, value.to_string()).unwrap();
    unsafe {
        libc::kill(old_pid as i32, libc::SIGSTOP);
    }
    let start = Instant::now();
    loop {
        if let Some(exit) = supervisor.try_wait().unwrap() {
            assert!(!exit.success());
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(4));
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(old_pid));
    assert_eq!(
        fs::read_to_string(&f.events)
            .unwrap()
            .lines()
            .filter(|l| *l == "start")
            .count(),
        1
    );
    assert_eq!(status(&f).unwrap()["ready"], false);
}

#[test]
fn correct_artifact_hashes_validate_without_loading_native_libraries() {
    let f = fixture();
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    let hash = |path: &Path| {
        digest::digest(&digest::SHA256, &fs::read(path).unwrap())
            .as_ref()
            .iter()
            .fold(String::new(), |mut output, byte| {
                write!(&mut output, "{byte:02x}").unwrap();
                output
            })
    };
    value["runtime_contract"] = json!({"sdk_sha256":hash(Path::new(value["sdk_library"].as_str().unwrap())),"bridge_sha256":hash(Path::new(value["ros"]["bridge"].as_str().unwrap())),"service_sha256":hash(Path::new(env!("CARGO_BIN_EXE_capture-service")))});
    fs::write(&f.config, value.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--validate-config")
        .arg(&f.config)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(fs::read_to_string(&f.events).unwrap_or_default().is_empty());
}

#[test]
fn sigkill_supervisor_cannot_leave_running_sdk_worker_orphan() {
    let f = fixture();
    let mut supervisor = launch(&f);
    let ready = wait_status(&f, &mut supervisor, |v| v["state"] == "ready");
    let worker_pid = ready["pid"].as_u64().unwrap();
    unsafe {
        assert_eq!(libc::kill(supervisor.id() as i32, libc::SIGKILL), 0);
    }
    let exit = supervisor.wait().unwrap();
    assert!(!exit.success());
    let started = Instant::now();
    while alive(worker_pid) {
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "SDK worker survived parent SIGKILL"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // No process can refresh the old ready snapshot now. Health must reject its
    // timestamp; the process death signal does not pretend to perform SDK stop.
    let old = status(&f).unwrap();
    assert_eq!(old["session_id"], ready["session_id"]);
}

#[test]
fn worker_rejects_unrelated_or_already_dead_expected_parent_before_sdk_access() {
    let f = fixture();
    let hash = digest::digest(&digest::SHA256, &fs::read(&f.config).unwrap())
        .as_ref()
        .iter()
        .fold(String::new(), |mut output, byte| {
            write!(&mut output, "{byte:02x}").unwrap();
            output
        });
    let out = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--worker")
        .arg("--config")
        .arg(&f.config)
        .args([
            "--expected-config-sha",
            &hash,
            "--restart-attempt",
            "0",
            "--supervisor-pid",
            "2147483647",
        ])
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!f.status.exists());
    assert!(!f.events.exists());
}

#[test]
fn slow_sdk_open_cannot_extend_worker_readiness_startup_budget() {
    let f = fixture();
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["service"]["startup_timeout_ms"] = json!(1000);
    value["service"]["heartbeat_timeout_ms"] = json!(4000);
    value["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--config")
        .arg(&f.config)
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .env("GDP_MOCK_OPEN_DELAY_MS", "2000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if status(&f).is_some_and(|v| v["ready"] == true) {
            stop(&mut supervisor);
            panic!("slow SDK open incorrectly received a fresh readiness budget");
        }
        if let Some(exit) = supervisor.try_wait().unwrap() {
            assert!(!exit.success());
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            stop(&mut supervisor);
            panic!("slow open test exceeded bounded watchdog");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(start.elapsed() >= Duration::from_secs(2));
    let value = status(&f).unwrap();
    assert_eq!(value["ready"], false);
    assert_eq!(value["pairs_published"], 0);
    let events = fs::read_to_string(&f.events).unwrap();
    assert_eq!(events.lines().filter(|l| *l == "start").count(), 1);
    assert_eq!(events.lines().filter(|l| *l == "stop").count(), 1);
    assert!(!events.lines().any(|l| l == "publish-copied"));
}

#[test]
fn protected_worker_failure_cannot_reopen_sdk_until_persistent_gate_is_removed() {
    let f = fixture();
    let gate = f.root.join("recording");
    fs::create_dir(&gate).unwrap();
    let path = gate.join("control.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["service"]["recording_control_path"] = json!(path);
    config["service"]["generation"] = json!("d".repeat(32));
    // Snapshot creation is opt-in. Use an explicit report-only policy because
    // the native ABI mock has fixed historical timestamps, not a real clock.
    config["quality_policy"] = json!({"version":1,"max_source_age_ns":500_000_000_u64,
        "max_future_ns":100_000_000_u64,"max_pair_delta_us":20_000,
        "min_valid_fraction":0.0,"max_zero_fraction":1.0,"action":"report-only"});
    fs::write(&f.config, config.to_string()).unwrap();
    let mut supervisor = launch(&f);
    let first = wait_status(&f, &mut supervisor, |v| v["state"] == "ready");
    let old_pid = first["pid"].as_u64().unwrap();
    let snapshot_path = first["snapshot_path"]
        .as_str()
        .expect("ready gate fixture must publish an actual snapshot path");
    let snapshot: capture_contract::snapshot::SnapshotEnvelope = serde_json::from_value(
        capture_contract::parse_strict(&fs::read(snapshot_path).unwrap()).unwrap(),
    )
    .unwrap();
    snapshot.verify(&snapshot.content.binding).unwrap();
    snapshot.require_runtime().unwrap();
    assert_eq!(first["snapshot_id"], snapshot.snapshot_id.as_str());
    assert_eq!(
        first["session_id"],
        snapshot.content.binding.session_id.as_str()
    );
    assert_eq!(
        first["instance_id"],
        snapshot.content.binding.instance_id.as_str()
    );
    assert_eq!(
        first["generation"],
        snapshot.content.binding.generation_id.as_str()
    );
    assert_eq!(
        snapshot.content.configuration["runtime_config_sha256"],
        first["config_sha256"]
    );
    for role in ["color", "depth"] {
        let model = snapshot
            .content
            .calibration
            .models
            .get(role)
            .expect("actual mock SDK profile required");
        assert_eq!((model.width, model.height), (2, 2));
    }
    let request = capture_contract::recording::GateRequest {
        version: 1,
        phase: capture_contract::recording::GatePhase::Preparing,
        request_id: capture_contract::SessionId::parse("a".repeat(32)).unwrap(),
        binding: snapshot.content.binding.clone(),
        snapshot_id: snapshot.snapshot_id.clone(),
        runtime_config_sha256: capture_contract::ConfigurationId::parse(
            first["config_sha256"].as_str().unwrap(),
        )
        .unwrap(),
        token_sha256: capture_contract::ConfigurationId::parse("0".repeat(64)).unwrap(),
    };
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !gate.join("ack.json").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let ack: capture_contract::recording::GateAck = serde_json::from_value(
        capture_contract::parse_strict(&fs::read(gate.join("ack.json")).unwrap()).unwrap(),
    )
    .unwrap();
    request
        .verify_ack(&ack, u32::try_from(old_pid).unwrap())
        .unwrap();
    assert_eq!(
        ack.session_id.as_str(),
        first["session_id"].as_str().unwrap()
    );
    unsafe {
        assert_eq!(libc::kill(old_pid as i32, libc::SIGSTOP), 0);
    }
    wait_status(&f, &mut supervisor, |v| {
        v["reason"] == "RECORDING_PROTECTED_RECOVERY_BLOCKED"
    });
    thread::sleep(Duration::from_millis(400));
    assert!(!alive(old_pid));
    assert_eq!(
        fs::read_to_string(&f.events)
            .unwrap()
            .lines()
            .filter(|l| *l == "start")
            .count(),
        1,
        "SDK reopened under protection"
    );
    assert_eq!(status(&f).unwrap()["session_id"], first["session_id"]);
    fs::remove_file(&path).unwrap(); // Authenticated manager release is tested separately.
    let second = wait_status(&f, &mut supervisor, |v| {
        v["state"] == "ready" && v["session_id"] != first["session_id"]
    });
    assert_eq!(second["restart_attempt"], 1);
    stop(&mut supervisor);
}

struct GroupRegressionGuard {
    supervisor: Child,
    helpers: Vec<std::os::fd::OwnedFd>,
    unrelated: Child,
}
impl Drop for GroupRegressionGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // pidfds retain identity even if an assertion fails after a PID exited.
        // Never send a cleanup signal using a stale test PID/PGID.
        for helper in &self.helpers {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    helper.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        if self.supervisor.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.supervisor.id() as i32, libc::SIGTERM);
            }
            let started = Instant::now();
            while self.supervisor.try_wait().ok().flatten().is_none()
                && started.elapsed() < Duration::from_secs(4)
            {
                thread::sleep(Duration::from_millis(10));
            }
            if self.supervisor.try_wait().ok().flatten().is_none() {
                let _ = self.supervisor.kill();
                let _ = self.supervisor.wait();
            }
        }
        if self.unrelated.try_wait().ok().flatten().is_none() {
            let _ = self.unrelated.kill();
            let _ = self.unrelated.wait();
        }
    }
}
fn helper_pidfd(pid: u64) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    assert!(
        fd >= 0,
        "Linux pidfd support is required for safe lifecycle fixture cleanup"
    );
    unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) }
}
#[test]
fn killed_worker_group_helpers_are_reaped_before_reopen_and_unrelated_groups_survive() {
    use std::os::unix::process::CommandExt;
    let f = fixture();
    let original = fs::read(&f.config).unwrap();
    let helper_file = f.root.join("helper.pid");
    let unrelated = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let supervisor = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .arg("--config")
        .arg(&f.config)
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .env("GDP_MOCK_HELPER_PID", &helper_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut processes = GroupRegressionGuard {
        supervisor,
        helpers: vec![],
        unrelated,
    };
    let first = wait_status(&f, &mut processes.supervisor, |v| v["state"] == "ready");
    let old_helper = fs::read_to_string(&helper_file)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    processes.helpers.push(helper_pidfd(old_helper));
    let old_worker = first["pid"].as_u64().unwrap();
    assert_eq!(
        unsafe { libc::getpgid(old_helper as i32) },
        old_worker as i32
    );
    unsafe {
        assert_eq!(libc::kill(old_worker as i32, libc::SIGKILL), 0);
    }
    let second = wait_status(&f, &mut processes.supervisor, |v| {
        v["state"] == "ready" && v["session_id"] != first["session_id"]
    });
    assert!(
        !alive(old_helper),
        "native helper must be killed and reaped before a new SDK worker opens"
    );
    assert!(!alive(old_worker));
    assert_eq!(second["restart_attempt"], 1);
    assert!(
        processes.unrelated.try_wait().unwrap().is_none(),
        "cleanup must not target unrelated process groups"
    );
    let new_helper = fs::read_to_string(&helper_file)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    processes.helpers.push(helper_pidfd(new_helper));
    stop(&mut processes.supervisor);
    assert!(
        !alive(new_helper),
        "graceful supervisor shutdown must also reap TERM-ignoring helpers"
    );
    assert!(processes.unrelated.try_wait().unwrap().is_none());
    assert_eq!(fs::read(&f.config).unwrap(), original);
}

fn configure_usb_recovery(f: &Fixture, fail: bool) {
    use std::os::unix::fs::PermissionsExt;
    let helper = f.root.join("usb-recovery");
    let script = if fail {
        "#!/bin/sh\ncat >/dev/null\nprintf 'usb-recovery\n' >>\"$GEMINIDEPTHPORT_MOCK_EVENTS\"\nexit 1\n"
    } else {
        "#!/bin/sh\n[ \"$#\" = 2 ] || exit 2\ns=$(cat)\n[ \"$s\" = test-device ] || exit 3\nprintf 'usb-recovery\n' >>\"$GEMINIDEPTHPORT_MOCK_EVENTS\"\nprintf '{\"version\":1,\"complete\":true,\"interfaces\":2,\"endpoints\":2}'\n"
    };
    fs::write(&helper, script).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let bytes = digest::digest(&digest::SHA256, &fs::read(&helper).unwrap());
    let mut sha = String::new();
    for b in bytes.as_ref() {
        use std::fmt::Write;
        write!(&mut sha, "{b:02x}").unwrap();
    }
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["usb_recovery"] = json!({"helper":helper,"sha256":sha});
    value["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, value.to_string()).unwrap();
}
#[test]
fn usb_recovery_is_private_verified_and_precedes_sdk_start() {
    let f = fixture();
    configure_usb_recovery(&f, false);
    let mut supervisor = launch(&f);
    let value = wait_status(&f, &mut supervisor, |v| v["ready"] == true);
    assert_eq!(value["usb_recovery"]["state"], "complete");
    let events = fs::read_to_string(&f.events).unwrap();
    assert!(events.find("usb-recovery").unwrap() < events.find("start").unwrap());
    stop(&mut supervisor);
}
#[test]
fn usb_recovery_failure_cannot_open_sdk_or_claim_ready() {
    let f = fixture();
    configure_usb_recovery(&f, true);
    let mut supervisor = launch(&f);
    let deadline = Instant::now() + Duration::from_secs(5);
    while supervisor.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(status(&f).unwrap()["ready"], true);
    assert_eq!(
        status(&f).unwrap()["last_failure_reason"],
        "USB_STREAM_RECOVERY_FAILED"
    );
    assert_eq!(fs::read_to_string(&f.events).unwrap(), "usb-recovery\n");
}
#[test]
fn persistent_recording_gate_blocks_usb_recovery_before_any_device_access() {
    let f = fixture();
    configure_usb_recovery(&f, false);
    let path = f.root.join("control.json");
    fs::write(&path, "malformed").unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["service"]["recording_control_path"] = json!(path);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut supervisor = launch(&f);
    wait_status(&f, &mut supervisor, |v| {
        v["reason"] == "RECORDING_PROTECTED_RECOVERY_BLOCKED"
    });
    thread::sleep(Duration::from_millis(150));
    assert!(!f.events.exists());
    assert!(path.exists());
    stop(&mut supervisor);
}

fn configure_runtime_quality(f: &Fixture, raw_pairs: u64, max_bytes: u64) {
    let quality = capture_contract::QualityPolicy {
        version: 1,
        max_source_age_ns: 1_000_000_000,
        max_future_ns: 100_000_000,
        max_pair_delta_us: 20_000,
        min_valid_fraction: 0.,
        max_zero_fraction: 1.,
        action: capture_contract::QualityAction::ReportOnly,
    };
    let policy = quality_core::RuntimeQualityPolicy {
        version: 1,
        quality: quality.clone(),
        stale_after_ns: 1_000_000_000,
        event_capacity: 64,
        window_capacity: 64,
        window_horizon_ns: 10_000_000_000,
        summary_capacity: 4,
        max_sessions: 4,
        rules: quality_core::FrameViolation::ALL
            .into_iter()
            .map(|v| {
                (
                    v,
                    quality_core::RulePolicy {
                        open_after: 1,
                        recover_after: 2,
                        severity: quality_core::QualityState::Degraded,
                    },
                )
            })
            .collect(),
    };
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["quality_policy"] = serde_json::to_value(quality).unwrap();
    value["quality_runtime"] = serde_json::to_value(policy).unwrap();
    value["raw_recording"] = json!({"directory":f.root.join("raw"),"name":"session","queue_capacity":4,"max_pairs":raw_pairs,"max_bytes":max_bytes});
    value["service"]["generation"] = json!("d".repeat(32));
    value["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, value.to_string()).unwrap();
}
#[test]
fn quality_bound_worker_commits_native_recording_and_finalizes_observed_quality() {
    let f = fixture();
    configure_runtime_quality(&f, 3, 1_000_000);
    let mut supervisor = launch(&f);
    let ready = wait_status(&f, &mut supervisor, |v| {
        v["ready"] == true && v["raw_recording"]["state"] == "complete"
    });
    assert_eq!(ready["raw_recording"]["written_pairs"], 3);
    assert_ne!(
        ready["raw_recording"]["native_snapshot_id"],
        ready["raw_recording"]["ros_snapshot_id"]
    );
    assert_eq!(
        ready["raw_recording"]["recording_protection_released"],
        false
    );
    assert!(ready["raw_recording"]["business_validity"].is_null());
    let job = PathBuf::from(ready["raw_recording"]["job_path"].as_str().unwrap());
    assert!(job.is_dir());
    assert!(!Path::new(ready["raw_recording"]["partial_path"].as_str().unwrap()).exists());
    let mut recording = offline_depth::recording::RecordingDirectorySource::open(&job).unwrap();
    assert!(
        matches!(recording.snapshot().content.depth,capture_contract::DepthRepresentation::SdkCounts{scale_mm} if scale_mm==f64::from(0.1f32))
    );
    for _ in 0..3 {
        let frame = recording.next_frame().unwrap().unwrap();
        assert_eq!(frame.depth.len(), 8);
        assert!(matches!(
            frame.evidence,
            offline_depth::recording::TaggedEvidence::SdkBoundPair { .. }
        ));
    }
    assert!(recording.next_frame().unwrap().is_none());
    let quality_path = PathBuf::from(ready["quality"]["current_path"].as_str().unwrap());
    let current: Value = serde_json::from_slice(&fs::read(&quality_path).unwrap()).unwrap();
    assert_eq!(current["session_id"], ready["session_id"]);
    assert_eq!(current["snapshot_id"], ready["snapshot_id"]);
    assert!(current["publication_calls"].as_u64().unwrap() > 0);
    stop(&mut supervisor);
    let summary: Value =
        serde_json::from_slice(&fs::read(quality_path.with_file_name("summary.json")).unwrap())
            .unwrap();
    assert_eq!(summary["summary"]["finalized"], true);
}
#[test]
fn bounded_raw_storage_failure_is_reported_without_claiming_completed_recording() {
    let f = fixture();
    configure_runtime_quality(&f, 3, 1);
    let mut supervisor = launch(&f);
    let failed = wait_status(&f, &mut supervisor, |v| {
        v["raw_recording"]["state"] == "failed"
    });
    assert_eq!(failed["raw_recording"]["reason"], "RAW_BYTE_LIMIT");
    assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
    assert!(failed["raw_recording"]["business_validity"].is_null());
    stop(&mut supervisor);
}
#[test]
fn interrupted_raw_capture_never_commits_an_incomplete_pair_target() {
    let f = fixture();
    configure_runtime_quality(&f, 1_000_000, 100_000_000);
    let mut supervisor = launch(&f);
    let ready = wait_status(&f, &mut supervisor, |v| {
        v["ready"] == true
            && v["raw_recording"]["written_pairs"]
                .as_u64()
                .is_some_and(|n| n > 0)
    });
    stop(&mut supervisor);
    assert!(!Path::new(ready["raw_recording"]["job_path"].as_str().unwrap()).exists());
    assert_eq!(status(&f).unwrap()["ready"], false);
}

fn quality_only(f: &Fixture) {
    configure_runtime_quality(f, 10, 1_000_000);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("raw_recording");
    value["service"]["startup_timeout_ms"] = json!(2000);
    fs::write(&f.config, value.to_string()).unwrap();
}
fn expect_terminal_failure(f: &Fixture, key: &str, value: &str) -> Value {
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env(key, value)
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let exit = loop {
        if let Some(exit) = supervisor.try_wait().unwrap() {
            break exit;
        }
        if started.elapsed() > Duration::from_secs(7) {
            supervisor.kill().unwrap();
            supervisor.wait().unwrap();
            panic!("failure deadline: {:?}", status(f));
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!exit.success());
    let terminal = status(f).unwrap();
    assert_eq!(terminal["ready"], false);
    assert_eq!(terminal["restart_attempt"], 0);
    let summary = fs::read_dir(f.root.join("quality"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("summary.json");
    let finalized: Value = serde_json::from_slice(&fs::read(summary).unwrap()).unwrap();
    assert_eq!(finalized["summary"]["finalized"], true);
    terminal
}
#[test]
fn quality_runtime_tracks_missing_skewed_regressing_and_invalid_pixel_failures() {
    for (key, value, metric) in [
        ("GDP_MOCK_MISSING_COLOR", "1", "missing_pairs"),
        ("GDP_MOCK_SKEW", "1", "skew_rejections"),
        ("GDP_MOCK_DUPLICATE", "1", "monotonic_rejections"),
        ("GDP_MOCK_SCALE", "10", "pairs_rejected"),
        ("GDP_MOCK_TIMEOUT", "1", "poll_timeouts"),
        ("GDP_MOCK_STALL", "1", "poll_timeouts"),
    ] {
        let f = fixture();
        quality_only(&f);
        let terminal = expect_terminal_failure(&f, key, value);
        assert!(terminal[metric].as_u64().unwrap() > 0, "{key}: {terminal}");
    }
}
#[test]
fn disconnected_or_failed_publishers_finalize_bound_quality_without_false_readiness() {
    for key in [
        "GDP_MOCK_OPEN_ERROR",
        "GDP_MOCK_WAIT_ERROR",
        "GDP_MOCK_DEPTH_PUBLISH_ERROR",
        "GDP_MOCK_PAIR_PUBLISH_ERROR",
    ] {
        let f = fixture();
        quality_only(&f);
        let terminal = expect_terminal_failure(&f, key, "1");
        if key.contains("PUBLISH") {
            assert!(terminal["publish_failures"].as_u64().unwrap() > 0);
            assert!(terminal["individual_publish_calls"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(terminal["pairs_accepted"], 0);
        }
    }
}
#[test]
fn unit_scale_quality_measurement_and_startup_discard_do_not_require_normalization_or_raw_output() {
    let f = fixture();
    quality_only(&f);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["depth"]["unit_policy"] = json!("require-mm");
    value["startup_discovery_seconds"] = json!(0.05);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_SCALE", "1")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = wait_status(&f, &mut supervisor, |v| v["ready"] == true);
    assert!(ready["snapshot_id"].as_str().is_some());
    assert!(ready["raw_recording"].is_null());
    let current: Value = serde_json::from_slice(
        &fs::read(ready["quality"]["current_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(current["last_observed"]["depth"]["pixel_measurement"].is_object());
    stop(&mut supervisor);
}
