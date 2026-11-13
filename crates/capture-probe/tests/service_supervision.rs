//! Linux-only black-box process supervision. The native ABI fixtures never open
//! USB, a real SDK device or ROS. macOS deliberately rejects live workers.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../offline-depth/examples/audit_imu_long_run.rs"]
mod recording_auditor;
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
        "ABI fixture changed; review service adaptation: {old}"
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
        "const char *wait_ms=getenv(\"GDP_MOCK_WAIT_DELAY_MS\");usleep(wait_ms?(useconds_t)(strtoul(wait_ms,NULL,10)*1000):10000);if(getenv(\"GDP_MOCK_WAIT_ERROR\")){*e=(error*)1;return NULL;}if(ms==1||getenv(\"GDP_MOCK_TIMEOUT\")||(getenv(\"GDP_MOCK_STALL\")&&service_sequence>3))return NULL;service_sequence++;return make(5);",
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
    sdk_source.insert_str(0, "static int fault_active(void);\n");
    sdk_source.push_str("\nstatic int fault_active(void){const char *gate=getenv(\"GDP_MOCK_FAULT_ARMED\");if(gate&&access(gate,F_OK)!=0)return 0;const char *n=getenv(\"GDP_MOCK_FAULT_AFTER\");return !n||service_sequence>strtoul(n,NULL,10); }\n");
    replace(
        &mut sdk_source,
        "if(getenv(\"GDP_MOCK_MISSING_COLOR\"))return NULL;",
        "if(getenv(\"GDP_MOCK_MISSING_COLOR\")&&fault_active())return NULL;",
    );
    replace(
        &mut sdk_source,
        "obj *f=make(5);f->stream=3;return f;",
        "if(getenv(\"GDP_MOCK_MISSING_DEPTH\")&&fault_active())return NULL;obj *f=make(5);f->stream=3;return f;",
    );
    replace(
        &mut sdk_source,
        "return sizeof(bytes);",
        "return getenv(\"GDP_MOCK_BAD_PAYLOAD\")&&fault_active()?3:sizeof(bytes);",
    );
    replace(
        &mut sdk_source,
        "return p->stream==3?8:0;",
        "return getenv(\"GDP_MOCK_BAD_FORMAT\")&&fault_active()?17:(p->stream==3?8:0);",
    );
    replace(
        &mut sdk_source,
        "return getenv(\"GDP_MOCK_SCALE\")?strtof",
        "return getenv(\"GDP_MOCK_SCALE\")&&fault_active()?strtof",
    );
    replace(
        &mut sdk_source,
        "int kind, refs, stream;",
        "int kind, refs, stream, size_reads, width_reads, global_reads;",
    );
    replace(
        &mut sdk_source,
        "o->stream=0;",
        "o->stream=0;o->size_reads=0;o->width_reads=0;o->global_reads=0;",
    );
    replace(
        &mut sdk_source,
        "return getenv(\"GDP_MOCK_BAD_PAYLOAD\")&&fault_active()?3:sizeof(bytes);",
        "return getenv(\"GDP_MOCK_MUTATE_PAYLOAD\")&&fault_active()&&++((obj*)p)->size_reads>(getenv(\"GDP_MOCK_MUTATE_AFTER\")?atoi(getenv(\"GDP_MOCK_MUTATE_AFTER\")):3)?3:(getenv(\"GDP_MOCK_BAD_PAYLOAD\")&&fault_active()?3:sizeof(bytes));",
    );
    replace(
        &mut sdk_source,
        "uint32_t ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;return 2;}",
        "uint32_t ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;if(getenv(\"GDP_MOCK_WIDTH_AFTER\")&&fault_active()&&++((obj*)p)->width_reads>atoi(getenv(\"GDP_MOCK_WIDTH_AFTER\"))){return 0;}return getenv(\"GDP_MOCK_BAD_WIDTH\")&&fault_active()?3:2;}",
    );
    replace(
        &mut sdk_source,
        "return 1700000000000000ULL+service_sequence*33333;",
        "return getenv(\"GDP_MOCK_ZERO_SYSTEM\")&&fault_active()?0:1700000000000000ULL+service_sequence*33333;",
    );
    replace(
        &mut sdk_source,
        ":0.1f;",
        ":(getenv(\"GDP_MOCK_BASE_SCALE\")?strtof(getenv(\"GDP_MOCK_BASE_SCALE\"),NULL):0.1f);",
    );
    // Independent source periods reproduce an unpaired depth at a phase slip.
    // Only the test clock starts near the slip; the service itself is unchanged.
    sdk_source.insert_str(0, "static unsigned long long drift_index(int stream);\n");
    sdk_source.push_str("\nstatic unsigned long long drift_index(int stream){unsigned long long n=service_sequence+870;return stream==2?(n*33308+16663)/33327:n;}\n");
    replace(
        &mut sdk_source,
        "if(getenv(\"GDP_MOCK_MISSING_COLOR\")&&fault_active())return NULL;",
        "if(getenv(\"GDP_MOCK_DRIFT\")&&drift_index(2)==((service_sequence+869)*33308+16663)/33327)return NULL;if(getenv(\"GDP_MOCK_MISSING_COLOR\")&&fault_active())return NULL;",
    );
    replace(
        &mut sdk_source,
        "return getenv(\"GDP_MOCK_DUPLICATE\")&&service_sequence>2?2:service_sequence;",
        "if(getenv(\"GDP_MOCK_DRIFT\"))return drift_index(p->stream);return getenv(\"GDP_MOCK_DUPLICATE\")&&service_sequence>2?2:service_sequence;",
    );
    replace(
        &mut sdk_source,
        "return service_sequence*33333+(getenv(\"GDP_MOCK_SKEW\")&&p->stream==3?30000:0);",
        "if(getenv(\"GDP_MOCK_DRIFT\"))return drift_index(p->stream)*(p->stream==2?33327:33308);return service_sequence*33333+(getenv(\"GDP_MOCK_SKEW\")&&p->stream==3?30000:0);",
    );
    replace(
        &mut sdk_source,
        "return bytes;",
        "static unsigned char zero[sizeof(bytes)];return getenv(\"GDP_MOCK_ZERO_PIXELS\")?zero:bytes;",
    );
    replace(
        &mut sdk_source,
        "return global_enabled?5000:0;",
        "if(getenv(\"GDP_MOCK_GLOBAL_LATE_LOST\")&&fault_active()&&++((obj*)f)->global_reads>1){return 0;}if(getenv(\"GDP_MOCK_GLOBAL_DEPTH_ZERO\")&&f->stream==3){return 0;}const char *warmup=getenv(\"GDP_MOCK_GLOBAL_WARMUP\");if(warmup&&service_sequence<=strtoul(warmup,NULL,10))return 0;if(getenv(\"GDP_MOCK_GLOBAL_LOST\")&&fault_active())return 0;if(getenv(\"GDP_MOCK_GLOBAL_INVALID\")&&fault_active())return 2147483648000000ULL;return global_enabled?1700000000000000ULL+service_sequence*33333-2000:0;",
    );
    replace(
        &mut sdk_source,
        "obj *f=make(5);f->stream=2;return f;",
        "if(getenv(\"GDP_MOCK_ALTERNATING\")&&service_sequence%2==0)return NULL;obj *f=make(5);f->stream=2;return f;",
    );
    replace(
        &mut sdk_source,
        "obj *f=make(5);f->stream=3;return f;",
        "if(getenv(\"GDP_MOCK_ALTERNATING\")&&service_sequence%2!=0)return NULL;obj *f=make(5);f->stream=3;return f;",
    );
    replace(
        &mut sdk_source,
        "return getenv(\"GDP_MOCK_MUTATE_PAYLOAD\")",
        "if(getenv(\"GDP_MOCK_BAD_DEPTH_ONLY\")&&p->stream==3)return 3;return getenv(\"GDP_MOCK_MUTATE_PAYLOAD\")",
    );
    let sdk_c = root.join("mock-sdk.c");
    fs::write(&sdk_c, sdk_source).unwrap();
    compile("cc", &sdk_c, &sdk, None);
    let mut bridge_source =
        fs::read_to_string(crate_root.join("../ros-output/tests/mock_bridge.cpp")).unwrap();
    bridge_source.insert_str(0, "#include <unistd.h>\n");
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
        "auto &p = *static_cast<Mock *>(handle); const char *pub_ms=std::getenv(\"GDP_MOCK_PUBLISH_DELAY_MS\"); if(pub_ms)usleep(static_cast<useconds_t>(std::strtoul(pub_ms,nullptr,10)*1000)); if(std::getenv(\"GDP_MOCK_DEPTH_PUBLISH_ERROR\") && p.frame_id==\"depth_optical\")return 2;",
    );
    replace(
        &mut bridge_source,
        "if(!handle||!gdp_ros::pair_valid(pair))return 1;",
        "if(!handle||!gdp_ros::pair_valid(pair))return 1;\n  if(std::getenv(\"GDP_MOCK_PAIR_PUBLISH_ERROR\"))return 2;",
    );
    replace(
        &mut bridge_source,
        "p.calls++; event(\"publish-copied\");",
        "p.calls++; event(\"publish-copied\"); char record[160]; std::snprintf(record,sizeof(record),\"image:%s:%llu\",p.frame_id.c_str(),static_cast<unsigned long long>(frame->sequence)); event(record);",
    );
    replace(
        &mut bridge_source,
        "event(source == 2 ? \"publish-global\" : \"publish-system-with-global\");",
        "event(source == 2 ? \"publish-global\" : \"publish-system-with-global\"); if(std::getenv(\"GDP_ROS_CAPTURE_BYTES\")){char payload[512]; int used=std::snprintf(payload,sizeof(payload),\"payload:%s:%llu:%llu:\",p.frame_id.c_str(),static_cast<unsigned long long>(frame->sequence),static_cast<unsigned long long>(frame->stamp_sec)*1000000000ULL+frame->stamp_nanosec); for(size_t i=0;i<frame->data_len && i<64;i++)used+=std::snprintf(payload+used,sizeof(payload)-used,\"%02x\",frame->data[i]); event(payload);} char record[160]; std::snprintf(record,sizeof(record),\"image:%s:%llu\",p.frame_id.c_str(),static_cast<unsigned long long>(frame->sequence)); event(record);",
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
    wait_status_for(f, child, predicate, Duration::from_secs(8))
}
fn wait_status_for(
    f: &Fixture,
    child: &mut Child,
    predicate: impl Fn(&Value) -> bool,
    timeout: Duration,
) -> Value {
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
        if start.elapsed() > timeout {
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
    value["raw_recording"] = json!({"directory":f.root.join("raw"),"name":"session","queue_capacity":4,"max_pairs":raw_pairs,"max_bytes":max_bytes,"storage_format":"chunked-v3"});
    value["service"]["generation"] = json!("d".repeat(32));
    value["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, value.to_string()).unwrap();
}
fn configure_property_sdk(f: &Fixture, failure: i32) {
    let source = f.root.join("mock-sdk.c");
    let original = fs::read_to_string(&source).unwrap();
    fs::write(
        &source,
        format!("#define MOCK_DEVICE_FAILURE {failure}\n{original}"),
    )
    .unwrap();
    compile("cc", &source, &f.root.join("mock-sdk.so"), None);
}
#[test]
fn quality_bound_worker_commits_native_recording_and_finalizes_observed_quality() {
    for (failure, gain) in [(0, 24), (18, 103), (36, 103)] {
        let f = fixture();
        if failure != 0 {
            configure_property_sdk(&f, failure);
        }
        configure_runtime_quality(&f, 3, 1_000_000);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["device_controls"] = json!({"enable_color_auto_exposure":false,"color_exposure":104,"color_gain":gain,"enable_ir_auto_exposure":false,"ir_exposure":204,"ir_gain":28});
        fs::write(&f.config, config.to_string()).unwrap();
        let mut supervisor = launch(&f);
        let ready = wait_status(&f, &mut supervisor, |v| {
            v["ready"] == true && v["raw_recording"]["state"] == "complete"
        });
        assert_eq!(ready["raw_recording"]["written_pairs"], 3);
        assert_eq!(ready["device_capabilities"]["info"]["firmware"], "1.4.98");
        let properties = ready["device_capabilities"]["properties"]
            .as_array()
            .unwrap();
        assert_eq!(properties.len(), 6);
        assert!(properties.iter().all(|p| p["readback_verified"] == true));
        assert_eq!(properties[3]["observed"], gain);
        if failure == 18 {
            assert_eq!(properties[3]["range_current"], 100);
            assert_eq!(properties[3]["observed_before"], 102);
        }
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
        assert_eq!(
            recording.snapshot().content.configuration["device_capabilities"],
            ready["device_capabilities"]
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
}
#[test]
fn device_setting_failure_never_publishes_or_commits_native_capture() {
    for (failure, reason) in [
        (3, "DEVICE_PROPERTY_UNSUPPORTED"),
        (8, "DEVICE_PROPERTY_READBACK_MISMATCH"),
        (18, "DEVICE_PROPERTY_OUT_OF_RANGE"),
        (20, "DEVICE_PROPERTY_RANGE_SCHEMA"),
        (22, "DEVICE_PROPERTY_VALUE_RANGE"),
        (24, "DEVICE_PROPERTY_SUPPORT_QUERY"),
        (30, "DEVICE_PROPERTY_VALUE_RANGE"),
        (32, "DEVICE_PROPERTY_VALUE_RANGE"),
        (33, "DEVICE_PROPERTY_RANGE_QUERY"),
        (34, "DEVICE_PROPERTY_VALUE_RANGE"),
        (35, "DEVICE_PROPERTY_UVC_READ_FAILED"),
    ] {
        let f = fixture();
        configure_runtime_quality(&f, 3, 1_000_000);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["device_controls"] =
            json!({"enable_color_auto_exposure":false,"color_gain":if failure==18 {257} else {24}});
        // This test classifies a property fault, not the supervisor's 500 ms
        // deadline fixture. Keep host scheduling/fsync delays from winning the
        // race before the SDK injection is reached. Production budgets and the
        // dedicated heartbeat deadline tests are unchanged.
        config["service"]["startup_timeout_ms"] = json!(4000);
        config["service"]["idle_timeout_ms"] = json!(1500);
        config["service"]["heartbeat_timeout_ms"] = json!(5000);
        fs::write(&f.config, config.to_string()).unwrap();
        configure_property_sdk(&f, failure);
        let mut child = launch(&f);
        let started = Instant::now();
        let exit = loop {
            if let Some(exit) = child.try_wait().unwrap() {
                break exit;
            }
            if started.elapsed() > Duration::from_secs(8) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("property failure not finalized");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!exit.success());
        let terminal = status(&f).unwrap();
        assert_eq!(terminal["ready"], false);
        assert_eq!(terminal["worker_reason"], "SESSION_FAILED", "{terminal}");
        let diagnostic = f.status.parent().unwrap().join(format!(
            "failure-{}.json",
            terminal["session_id"].as_str().unwrap()
        ));
        let detail: Value = serde_json::from_slice(&fs::read(diagnostic).unwrap()).unwrap();
        assert!(
            detail["error_chain"].as_str().unwrap().contains(reason),
            "{detail}"
        );
        let diagnostic = &detail["device_property_failure"];
        assert_eq!(diagnostic["version"], 1);
        assert_eq!(
            diagnostic["property"]["id"],
            if failure == 3 { 2000 } else { 2002 }
        );
        assert_eq!(detail["details_truncated"], false);
        if failure >= 30 {
            assert_eq!(diagnostic["property"]["getter_attempted"], true);
            assert_eq!(diagnostic["property"]["getter_uvc"]["backend"], "v4l2");
            assert_eq!(
                diagnostic["property"]["getter_uvc"]["events"][0]["selector"],
                2002
            );
            if failure == 30 {
                assert_eq!(diagnostic["property"]["range_current"], 256);
                assert_eq!(diagnostic["property"]["getter_returned"], 101);
            }
            if failure == 32 || failure == 34 {
                assert!(diagnostic["property"]["getter_error"].is_string());
                assert!(diagnostic["property"]["observed_before"].is_null());
            }
        }
        if failure != 8 {
            assert!(diagnostic["rollback_verified"].is_null());
        }
        if failure == 18 {
            assert_eq!(diagnostic["property"]["range_current"], 100);
            assert_eq!(diagnostic["property"]["observed_before"], 102);
            assert_eq!(diagnostic["property"]["requested"], 257);
            assert_eq!(diagnostic["auto_exposure_before"]["color"], 1);
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(f.status.parent().unwrap().join(format!(
                "failure-{}.json",
                terminal["session_id"].as_str().unwrap()
            )))
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
        assert_eq!(terminal["pairs_published"], 0);
        assert!(!f.root.join("raw/session/COMPLETE").exists());
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(!events.contains("wait\n"));
        if failure != 8 {
            assert!(!events.contains("property-write"));
        }
        assert!(!events.contains("publish"));
    }
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
    // These fixtures classify frame/quality faults. Keep their deadlines
    // separate from the intentionally tight supervision-budget tests.
    value["service"]["startup_timeout_ms"] = json!(4000);
    value["service"]["idle_timeout_ms"] = json!(1500);
    value["service"]["heartbeat_timeout_ms"] = json!(5000);
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

#[test]
fn faults_after_readiness_abort_raw_output_and_never_publish_invalid_frames() {
    for key in [
        "GDP_MOCK_MISSING_COLOR",
        "GDP_MOCK_MISSING_DEPTH",
        "GDP_MOCK_BAD_PAYLOAD",
        "GDP_MOCK_BAD_FORMAT",
        "GDP_MOCK_MUTATE_PAYLOAD",
        "GDP_MOCK_SCALE",
    ] {
        let f = fixture();
        configure_runtime_quality(&f, 1_000_000, 100_000_000);
        // This test measures frame rejection, not scheduler latency during
        // snapshot initialization on a shared CI runner.
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["service"]["heartbeat_timeout_ms"] = json!(3000);
        config["service"]["idle_timeout_ms"] = json!(3000);
        fs::write(&f.config, config.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
            .args(["--config", f.config.to_str().unwrap()])
            .env(key, if key == "GDP_MOCK_SCALE" { "0" } else { "1" })
            .env("GDP_MOCK_FAULT_AFTER", "20")
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
        assert!(ready["pairs_published"].as_u64().unwrap() > 0);
        // The recording worker reports failure asynchronously; wait for the
        // supervisor's next status transition as well as the recorder result.
        let failed = wait_status(&f, &mut child, |v| {
            v["raw_recording"]["state"] == "failed" && v["state"] != "ready"
        });
        assert_ne!(failed["state"], "ready");
        assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
        stop(&mut child);
    }
}

#[test]
fn finite_rgbd_probe_reports_timeout_missing_and_sdk_failures_with_no_raw_output() {
    let f = fixture();
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("service");
    config.as_object_mut().unwrap().remove("ros");
    config["seconds"] = json!(0.1);
    config["startup_discovery_seconds"] = json!(0.03);
    fs::write(&f.config, config.to_string()).unwrap();
    for key in [
        "NONE",
        "GDP_MOCK_MISSING_DEPTH",
        "GDP_MOCK_MISSING_COLOR",
        "GDP_MOCK_TIMEOUT",
        "GDP_MOCK_WAIT_ERROR",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"))
            .args(["--config", f.config.to_str().unwrap()])
            .env(key, "1")
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        if key == "NONE" {
            assert_eq!(value["ok"], true);
            assert_eq!(value["ros"]["enabled"], false);
        } else {
            assert_eq!(value["ok"], false);
            assert!(value["error"].is_string());
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_MISSING_DEPTH", "1")
        .env("GDP_MOCK_MISSING_COLOR", "1")
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["framesets_missing_depth"].as_u64().unwrap() > 0);
    assert!(value["framesets_missing_color"].as_u64().unwrap() > 0);
}

#[test]
fn finite_single_stream_probe_rejects_invalid_metadata_and_depth_conversion_before_publication() {
    let f = fixture();
    let config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    let sdk = config["sdk_library"].as_str().unwrap();
    for (stream, key, value) in [
        ("color", "NONE", "1"),
        ("depth", "NONE", "1"),
        ("color", "GDP_MOCK_BAD_FORMAT", "1"),
        ("depth", "GDP_MOCK_SCALE", "10"),
        ("depth", "GDP_MOCK_BAD_PAYLOAD", "1"),
        ("color", "GDP_MOCK_BAD_WIDTH", "1"),
        ("color", "GDP_MOCK_MUTATE_PAYLOAD", "1"),
        ("depth", "GDP_MOCK_MUTATE_PAYLOAD", "1"),
        ("depth", "GDP_MOCK_ZERO_PIXELS", "1"),
        ("color", "GDP_MOCK_MISSING_COLOR", "1"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_capture-probe"));
        command.args([
            "--stream",
            stream,
            "--width",
            "2",
            "--height",
            "2",
            "--fps",
            "30",
            "--sdk-library",
            sdk,
            "--serial",
            "test-device",
            "--seconds",
            "0.1",
            "--poll-ms",
            "10",
        ]);
        if stream == "depth" {
            command.args(["--depth-unit-policy", "normalize-mm"]);
        }
        let output = command
            .env(key, value)
            .env("GDP_MOCK_MUTATE_AFTER", "2")
            .output()
            .unwrap();
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result["ok"],
            key == "NONE" || key == "GDP_MOCK_ZERO_PIXELS",
            "{key}: {result}"
        );
        if key == "GDP_MOCK_ZERO_PIXELS" {
            assert_eq!(result["normalization"]["output_min_mm"], 0);
            assert_eq!(result["normalization"]["output_max_mm"], 0);
        }
    }
}

#[test]
fn worker_usb_recovery_rechecks_recording_control_before_and_after_the_helper() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    for install_before in [true, false] {
        let f = fixture();
        quality_only(&f);
        let folder = f.root.join("recording");
        fs::create_dir(&folder).unwrap();
        let control = folder.join("control.json");
        let helper = f.root.join("recovery-helper");
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nprintf protected > '{}'\nprintf '%s' '{{\"version\":1,\"complete\":true,\"interfaces\":1,\"endpoints\":1}}'\n",
            control.display()
        );
        let mut writer = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\"", "--"])
            .arg(&helper)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        if install_before {
            fs::write(&control, b"protected").unwrap();
        }
        let mut cfg: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        cfg["service"]["recording_control_path"] = json!(control);
        cfg["usb_recovery"] =
            json!({"helper":helper,"sha256":offline_depth::sha256(script.as_bytes())});
        fs::write(&f.config, cfg.to_string()).unwrap();
        let hash = offline_depth::sha256(&fs::read(&f.config).unwrap());
        let result = Command::new(env!("CARGO_BIN_EXE_capture-service"))
            .args([
                "--worker",
                "--config",
                f.config.to_str().unwrap(),
                "--expected-config-sha",
                &hash,
                "--restart-attempt",
                "0",
                "--supervisor-pid",
                &std::process::id().to_string(),
            ])
            .env("GDP_CAPTURE_INSTANCE_ID", "b".repeat(64))
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let value = status(&f).unwrap();
        assert_eq!(value["ready"], false);
        assert_eq!(value["pairs_accepted"], 0);
        assert!(control.exists());
        let detail: Value = serde_json::from_slice(
            &fs::read(f.root.join(format!(
                "failure-{}.json",
                value["session_id"].as_str().unwrap()
            )))
            .unwrap(),
        )
        .unwrap();
        assert!(detail.to_string().contains("protect"));
        assert!(
            !fs::read_to_string(&f.events)
                .unwrap_or_default()
                .lines()
                .any(|line| line == "start")
        );
    }
}

#[test]
fn zero_system_clock_after_readiness_rejects_pairs_and_keeps_raw_output_incomplete() {
    let f = fixture();
    configure_runtime_quality(&f, 1_000_000, 100_000_000);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["pairing"]["clock"] = json!("system");
    // Exercise the injected clock fault, not shared-runner scheduling/fsync stalls.
    value["service"]["heartbeat_timeout_ms"] = json!(3000);
    value["service"]["idle_timeout_ms"] = json!(3000);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_ZERO_SYSTEM", "1")
        .env("GDP_MOCK_FAULT_AFTER", "20")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_status(&f, &mut child, |v| v["ready"] == true);
    let failed = wait_status(&f, &mut child, |v| {
        v["last_failure_reason"] == "PAIR_CONTRACT_REJECTED"
    });
    assert!(failed["pairs_rejected"].as_u64().unwrap() > 0);
    assert_eq!(failed["raw_recording"]["state"], "failed");
    assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
    stop(&mut child);
}

#[test]
fn startup_discovery_frames_are_excluded_from_the_raw_target_before_readiness() {
    let f = fixture();
    configure_runtime_quality(&f, 2, 1_000_000);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    // This case checks discovery exclusion, not worker scheduler latency.
    value["service"]["heartbeat_timeout_ms"] = json!(3000);
    value["startup_discovery_seconds"] = json!(0.08);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut child = launch(&f);
    let ready = wait_status(&f, &mut child, |v| {
        v["ready"] == true && v["raw_recording"]["state"] == "complete"
    });
    assert!(
        ready["raw_recording"]["startup_framesets_excluded"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(ready["raw_recording"]["written_pairs"], 2);
    stop(&mut child);
}

#[test]
fn unbound_worker_disconnects_do_not_need_quality_or_raw_recording() {
    let f = fixture();
    for key in ["GDP_MOCK_OPEN_ERROR", "GDP_MOCK_WAIT_ERROR"] {
        let out = Command::new(env!("CARGO_BIN_EXE_capture-service"))
            .args([
                "--worker",
                "--config",
                f.config.to_str().unwrap(),
                "--expected-config-sha",
                &offline_depth::sha256(&fs::read(&f.config).unwrap()),
                "--restart-attempt",
                "0",
                "--supervisor-pid",
                &std::process::id().to_string(),
            ])
            .env("GDP_CAPTURE_INSTANCE_ID", "a".repeat(64))
            .env(key, "1")
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .output()
            .unwrap();
        assert!(!out.status.success());
        let value = status(&f).unwrap();
        assert_eq!(value["ready"], false);
        assert!(value["quality"].is_null());
        assert!(value["raw_recording"].is_null());
    }
}

#[test]
fn millimeter_service_binds_quality_without_raw_recording_and_rejects_bad_frames() {
    for fault in [
        "NONE",
        "GDP_MOCK_BAD_PAYLOAD",
        "GDP_MOCK_DUPLICATE",
        "GDP_MOCK_MISSING_COLOR",
    ] {
        let f = fixture();
        quality_only(&f);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["depth"]["unit_policy"] = json!("require-mm");
        config["service"]["heartbeat_timeout_ms"] = json!(3000);
        fs::write(&f.config, config.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
            .args(["--config", f.config.to_str().unwrap()])
            .env("GDP_MOCK_SCALE", "1")
            .env("GDP_MOCK_BASE_SCALE", "1")
            .env(fault, "1")
            .env("GDP_MOCK_FAULT_AFTER", "30")
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
        assert!(ready["snapshot_id"].is_string());
        assert!(ready["raw_recording"].is_null());
        if fault != "NONE" {
            let bad = wait_status(&f, &mut child, |v| {
                v["pairs_rejected"].as_u64().unwrap_or(0) > 0
            });
            assert_eq!(bad["ready"], false);
        }
        stop(&mut child);
    }
}

#[test]
fn missing_color_with_invalid_depth_is_a_quality_diagnostic_not_a_pixel_measurement() {
    let f = fixture();
    quality_only(&f);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, config.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_MISSING_COLOR", "1")
        .env("GDP_MOCK_BAD_FORMAT", "1")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let bad = wait_status(&f, &mut child, |v| {
        v["missing_pairs"].as_u64().unwrap_or(0) > 0
    });
    assert_eq!(bad["pairs_published"], 0);
    assert_eq!(bad["ready"], false);
    stop(&mut child);
}

#[test]
fn continuous_source_depth_is_published_even_when_sdk_color_pairing_slips() {
    let f = fixture();
    quality_only(&f);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_DRIFT", "1")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_status(&f, &mut child, |v| v["ready"] == true);
    let sampled = wait_status(&f, &mut child, |v| {
        v["framesets_observed"].as_u64().unwrap_or(0) >= 30
    });
    assert_eq!(sampled["missing_pairs"], 1);
    assert_eq!(sampled["unpaired_images_published"], 1);
    assert_eq!(sampled["publish_failures"], 0);
    assert_eq!(
        sampled["individual_publish_calls"].as_u64().unwrap(),
        sampled["pairs_published"].as_u64().unwrap() * 4 + 1
    );
    assert!(sampled["pairs_published"].as_u64().unwrap() > 0);
    let current: Value = serde_json::from_slice(
        &fs::read(sampled["quality"]["current_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(
        current["quality"]["streams"]["depth"]["frame_count"]
            .as_u64()
            .unwrap()
            > current["quality"]["streams"]["color"]["frame_count"]
                .as_u64()
                .unwrap()
    );
    stop(&mut child);
    let events = fs::read_to_string(&f.events).unwrap();
    let depth: Vec<u64> = events
        .lines()
        .filter_map(|line| {
            line.strip_prefix("image:depth_optical:")
                .map(|n| n.parse().unwrap())
        })
        .collect();
    assert!(depth.len() >= 30);
    assert!(
        depth.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{depth:?}"
    );
}

#[path = "../../../tests/support/io_faults.rs"]
mod io_faults;
#[test]
fn native_recording_reports_thread_exhaustion_queue_backpressure_and_snapshot_link_failure() {
    for (op, suffix, delay, expected) in [
        (
            "pthread_create",
            "raw-writer",
            false,
            "RAW_INITIALIZATION_FAILED",
        ),
        ("fsync", "/snapshot.json", true, "RAW_QUEUE_OVERFLOW"),
        ("linkat", ".json", false, ""),
    ] {
        let f = fixture();
        configure_runtime_quality(&f, 1_000_000, 100_000_000);
        let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        // Allow the deliberately stalled fsync to reach the queue-overflow fault.
        value["service"]["idle_timeout_ms"] = json!(3000);
        value["service"]["heartbeat_timeout_ms"] = json!(3000);
        value["service"]["shutdown_timeout_ms"] = json!(3000);
        fs::write(&f.config, value.to_string()).unwrap();
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("injected");
        let mut command = Command::new(env!("CARGO_BIN_EXE_capture-service"));
        command
            .args(["--config", f.config.to_str().unwrap()])
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        io_faults::inject(&mut command, &library, op, suffix, &marker);
        if op == "pthread_create" {
            // The status writer is created first; target the native raw writer.
            command.env("GDP_IO_FAIL_NTH", "2");
        }
        if delay {
            command.env("GDP_IO_DELAY_MS", "1500");
        }
        let mut child = command.spawn().unwrap();
        if expected.is_empty() {
            let started = Instant::now();
            while child.try_wait().unwrap().is_none() {
                assert!(started.elapsed() < Duration::from_secs(8));
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(status(&f).unwrap()["ready"], false);
        } else {
            let failed = wait_status(&f, &mut child, |v| v["raw_recording"]["state"] == "failed");
            assert_eq!(failed["raw_recording"]["reason"], expected);
            assert!(
                !Path::new(failed["raw_recording"]["job_path"].as_str().unwrap())
                    .join("COMPLETE")
                    .exists()
            );
            stop(&mut child);
        }
        io_faults::assert_injected(&marker, op);
    }
}

#[test]
fn a_normally_exiting_worker_is_distinct_from_a_supervisor_shutdown() {
    let f = fixture();
    quality_only(&f);
    let mut child = launch(&f);
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    unsafe {
        libc::kill(ready["pid"].as_u64().unwrap() as i32, libc::SIGTERM);
    }
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status(&f).unwrap()["reason"], "WORKER_STOPPED");
    assert_eq!(status(&f).unwrap()["ready"], false);
}

#[test]
fn valid_depth_scale_changes_cannot_silently_rebind_an_existing_capture_snapshot() {
    let f = fixture();
    quality_only(&f);
    let mut cfg: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    cfg["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, cfg.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_SCALE", "0.2")
        .env("GDP_MOCK_FAULT_AFTER", "30")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    let failed = wait_status(&f, &mut child, |v| {
        v["last_failure_reason"] == "PAIR_CONTRACT_REJECTED"
    });
    assert_eq!(failed["snapshot_id"], ready["snapshot_id"]);
    assert_eq!(failed["ready"], false);
    assert!(failed["pairs_rejected"].as_u64().unwrap() > 0);
    stop(&mut child);
}

#[test]
fn duplicate_supervisors_cannot_replace_the_live_worker_or_acknowledge_a_directory() {
    let f = fixture();
    quality_only(&f);
    let control = f.root.join("recording/control.json");
    fs::create_dir(control.parent().unwrap()).unwrap();
    let mut cfg: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    cfg["service"]["recording_control_path"] = json!(control);
    cfg["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, cfg.to_string()).unwrap();
    let mut child = launch(&f);
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    let other = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!other.status.success());
    fs::create_dir(&control).unwrap();
    let later = wait_status(&f, &mut child, |v| {
        v["heartbeat_sequence"].as_u64().unwrap_or(0)
            > ready["heartbeat_sequence"].as_u64().unwrap() + 5
    });
    assert_eq!(later["pid"], ready["pid"]);
    assert!(!f.root.join("recording/ack.json").exists());
    stop(&mut child);
}

#[test]
fn missing_kernel_supervision_guarantees_fail_before_the_sdk_can_start() {
    for mode in ["pdeath", "subreaper"] {
        let f = fixture();
        let binary = f.root.join("deny-prctl");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/deny_prctl.c");
        let out = Command::new("cc")
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["service"]["max_restarts"] = json!(0);
        fs::write(&f.config, config.to_string()).unwrap();
        let marker = f.root.join("installed");
        let mut child = Command::new(binary)
            .arg(mode)
            .arg(env!("CARGO_BIN_EXE_capture-service"))
            .args(["--config", f.config.to_str().unwrap()])
            .env("GDP_SECCOMP_MARKER", &marker)
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let exit = loop {
            if let Some(exit) = child.try_wait().unwrap() {
                break exit;
            }
            if start.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("supervision syscall failure did not stop setup");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(marker.is_file(), "sandbox setup failed with {exit}");
        assert!(!exit.success());
        assert_ne!(exit.code(), Some(127));
        assert!(
            !fs::read_to_string(&f.events)
                .unwrap_or_default()
                .lines()
                .any(|line| line == "start")
        );
    }
}

#[test]
fn interrupted_worker_exit_checks_do_not_turn_a_healthy_worker_into_a_failure() {
    let f = fixture();
    quality_only(&f);
    let mut cfg: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    cfg["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, cfg.to_string()).unwrap();
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("injected");
    let mut command = Command::new(env!("CARGO_BIN_EXE_capture-service"));
    command
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_IO_ERRNO", "EINTR")
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    io_faults::inject(&mut command, &library, "waitid", "worker", &marker);
    let mut child = command.spawn().unwrap();
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    io_faults::assert_injected(&marker, "waitid");
    assert_eq!(ready["restart_attempt"], 0);
    stop(&mut child);
}

#[test]
fn finite_rgbd_preserves_each_available_stream_without_claiming_pairs() {
    let f = fixture();
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("service");
    config["seconds"] = json!(0.12);
    fs::write(&f.config, config.to_string()).unwrap();
    for missing in ["GDP_MOCK_MISSING_COLOR", "GDP_MOCK_MISSING_DEPTH"] {
        for fault in [
            "NONE",
            "GDP_MOCK_ZERO_SYSTEM",
            "GDP_MOCK_DEPTH_PUBLISH_ERROR",
            "GDP_MOCK_BAD_PAYLOAD",
        ] {
            let _ = fs::remove_file(&f.events);
            let output = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"))
                .args(["--config", f.config.to_str().unwrap()])
                .env(missing, "1")
                .env(fault, "1")
                .env("GDP_ROS_MOCK_EVENTS", &f.events)
                .output()
                .unwrap();
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ros"]["pairs_publish_calls_succeeded"], 0);
            let events = fs::read_to_string(&f.events).unwrap_or_default();
            assert!(!events.lines().any(|line| line == "aux-publish"));
            let succeeds = fault == "NONE"
                || (fault == "GDP_MOCK_DEPTH_PUBLISH_ERROR" && missing == "GDP_MOCK_MISSING_DEPTH");
            if succeeds {
                assert!(
                    value["ros"]["unpaired_images_published"].as_u64().unwrap() > 0,
                    "{value}"
                );
                assert!(events.lines().any(|line| line.starts_with("image:")));
            } else {
                assert_eq!(value["ros"]["unpaired_images_published"], 0, "{value}");
                assert!(!events.lines().any(|line| line.starts_with("image:")));
                assert!(value["error"].is_string());
            }
        }
    }
}

#[test]
fn initially_incomplete_streams_publish_images_but_never_claim_pair_readiness() {
    for missing in ["GDP_MOCK_MISSING_COLOR", "GDP_MOCK_MISSING_DEPTH"] {
        let f = fixture();
        let mut child = Command::new(env!("CARGO_BIN_EXE_capture-service"))
            .args(["--config", f.config.to_str().unwrap()])
            .env(missing, "1")
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let status = wait_status(&f, &mut child, |v| {
            // Image counters can precede the supervisor's asynchronous state
            // transition; observe that transition before asserting its value.
            v["unpaired_images_published"].as_u64().unwrap_or(0) >= 3 && v["state"] == "degraded"
        });
        assert_eq!(status["ready"], false);
        assert_eq!(status["state"], "degraded");
        assert_eq!(status["pairs_published"], 0);
        assert_eq!(status["pairs_accepted"], 0);
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(!events.lines().any(|line| line == "aux-publish"));
        assert_eq!(
            events
                .lines()
                .filter(|line| *line == "session-bound")
                .count(),
            2
        );
        let expected = if missing == "GDP_MOCK_MISSING_COLOR" {
            "image:depth_optical:"
        } else {
            "image:color_optical:"
        };
        assert!(
            events
                .lines()
                .filter(|line| line.starts_with(expected))
                .count()
                >= 3
        );
    }
}

#[test]
fn unpaired_depth_still_requires_explicit_millimeter_units() {
    let f = fixture();
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("service");
    config["seconds"] = json!(0.1);
    config["depth"]["unit_policy"] = json!("require-mm");
    fs::write(&f.config, config.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GDP_MOCK_MISSING_COLOR", "1")
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"], "SDK_OR_OUTPUT_FAILED");
    assert_eq!(value["normalization"]["enabled"], false);
    assert_eq!(value["ros"]["unpaired_images_published"], 0);
}

#[test]
fn raw_byte_limit_after_successful_appends_never_commits_a_partial_target() {
    let f = fixture();
    // Include the bounded startup property evidence in snapshot metadata;
    // leave room for successful appends before the byte limit is reached.
    configure_runtime_quality(&f, 100, 40_000);
    // Exercise the byte budget after a successful append, not throughput.
    // Dedicated overflow cases retain their full-rate producers and FIFO caps.
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .env("GDP_MOCK_WAIT_DELAY_MS", "100")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let failed = wait_status(&f, &mut supervisor, |v| {
        v["raw_recording"]["state"] == "failed"
    });
    assert_eq!(failed["raw_recording"]["reason"], "RAW_BYTE_LIMIT");
    assert!(
        failed["raw_recording"]["written_pairs"].as_u64().unwrap() > 0,
        "{failed}"
    );
    assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
    stop(&mut supervisor);
}

fn acquisition(f: &Fixture, preset: &str, global: bool) {
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["acquisition"] = if global {
        json!({"preset":preset})
    } else {
        json!({"preset":preset,"advanced":{"message_timestamp":"system"}})
    };
    value["ros"]["stamp_source"] = json!(if global {
        "sdk-global-unix"
    } else {
        "sdk-system-unix"
    });
    value["service"]["startup_timeout_ms"] = json!(4000);
    value["service"]["heartbeat_timeout_ms"] = json!(5000);
    fs::write(&f.config, value.to_string()).unwrap();
}
fn acquisition_child(f: &Fixture, environment: &[(&str, &str)]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_capture-service"))
        .args(["--config", f.config.to_str().unwrap()])
        .envs(environment.iter().copied())
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
#[test]
fn acquisition_complete_and_strict_apply_sdk_controls_and_global_without_duplicate_images() {
    for preset in ["complete", "strict-rgbd"] {
        let f = fixture();
        acquisition(&f, preset, true);
        let mut child = acquisition_child(&f, &[]);
        let ready = wait_status(&f, &mut child, |v| {
            v["ready"] == true && v["pairs_published"].as_u64().unwrap_or(0) >= 3
        });
        assert_eq!(ready["message_timestamp"], "global");
        assert_eq!(ready["physical_sync_verified"], false);
        assert_eq!(
            ready["acquisition_effective"]["sdk_frame_sync"],
            preset == "strict-rgbd"
        );
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        let sync = if preset == "complete" {
            "sync-off"
        } else {
            "sync-on"
        };
        assert!(events.find(sync).unwrap() < events.find("start").unwrap());
        assert!(events.find("aggregate-any").unwrap() < events.find("start").unwrap());
        assert!(events.find("global-on").unwrap() < events.find("start").unwrap());
        assert!(events.lines().any(|l| l == "publish-global"));
        let images: Vec<_> = events.lines().filter(|l| l.starts_with("image:")).collect();
        let unique: std::collections::BTreeSet<_> = images.iter().copied().collect();
        assert_eq!(
            images.len(),
            unique.len(),
            "an accepted pair must not republish independent images"
        );
    }
}
#[test]
fn acquisition_complete_is_ready_with_independent_streams_while_strict_excludes_them() {
    for preset in ["complete", "strict-rgbd"] {
        let f = fixture();
        acquisition(&f, preset, true);
        let mut child = acquisition_child(&f, &[("GDP_MOCK_ALTERNATING", "1")]);
        let sampled = wait_status(&f, &mut child, |v| {
            v["framesets_observed"].as_u64().unwrap_or(0) >= 8
        });
        assert_eq!(sampled["ready"], preset == "complete");
        assert_eq!(sampled["pairs_published"], 0);
        assert_eq!(sampled["pairs_accepted"], 0);
        assert_eq!(
            sampled["readiness_basis"],
            if preset == "complete" {
                "both-streams-published"
            } else {
                "accepted-rgbd-pairs"
            }
        );
        if preset == "complete" {
            assert!(sampled["unpaired_images_published"].as_u64().unwrap() >= 8);
        } else {
            assert_eq!(sampled["individual_publish_calls"], 0);
        }
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(!events.lines().any(|l| l == "aux-publish"));
    }
    // One healthy stream must never hide absence of the other stream.
    let f = fixture();
    acquisition(&f, "complete", false);
    let mut child = acquisition_child(&f, &[("GDP_MOCK_MISSING_COLOR", "1")]);
    let sampled = wait_status(&f, &mut child, |v| {
        v["unpaired_images_published"].as_u64().unwrap_or(0) >= 5
    });
    assert_eq!(sampled["ready"], false);
    stop(&mut child);
}

#[test]
fn global_bound_evidence_with_imu_publishes_in_complete_and_strict_workers() {
    for preset in ["complete", "strict-rgbd"] {
        let f = fixture();
        quality_only(&f);
        enable_imu(&f);
        acquisition(&f, preset, true);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["startup_discovery_seconds"] = json!(0.3);
        assert_eq!(config["pairing"]["clock"], "device");
        fs::write(&f.config, config.to_string()).unwrap();
        let mut child = acquisition_child(&f, &[]);
        let ready = wait_status(&f, &mut child, |v| {
            v["ready"] == true
                && v["pairs_published"].as_u64().unwrap_or(0) >= 3
                && v["imu"]["published"][0].as_u64().unwrap_or(0) > 0
                && v["imu"]["published"][1].as_u64().unwrap_or(0) > 0
        });
        assert!(
            ready["snapshot_id"].is_string(),
            "quality must bind evidence"
        );
        assert_eq!(ready["message_timestamp"], "global");
        assert_eq!(ready["publish_failures"], 0);
        assert_eq!(ready["stamp_failures"], 0);
        assert!(ready["imu"]["error"].is_null());
        assert!(ready["raw_recording"].is_null());
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(
            events
                .lines()
                .filter(|line| *line == "evidence-copied")
                .count()
                >= 3
        );
        assert!(events.lines().any(|line| line == "publish-global"));
        let images: Vec<_> = events
            .lines()
            .filter(|line| line.starts_with("image:"))
            .collect();
        let unique: std::collections::BTreeSet<_> = images.iter().copied().collect();
        assert_eq!(
            images.len(),
            unique.len(),
            "Image submissions must stay unique"
        );
    }
}
#[test]
fn acquisition_complete_preserves_valid_images_when_pair_skew_rejects_metadata() {
    let f = fixture();
    acquisition(&f, "complete", false);
    let mut child = acquisition_child(&f, &[("GDP_MOCK_SKEW", "1")]);
    let sampled = wait_status(&f, &mut child, |v| {
        v["ready"] == true && v["skew_rejections"].as_u64().unwrap_or(0) >= 3
    });
    assert_eq!(sampled["pairs_accepted"], 0);
    assert_eq!(sampled["pairs_published"], 0);
    assert!(sampled["individual_publish_calls"].as_u64().unwrap() >= 6);
    stop(&mut child);
    let events = fs::read_to_string(&f.events).unwrap();
    assert!(!events.lines().any(|l| l == "aux-publish"));
}
#[test]
fn acquisition_global_initial_mapping_waits_but_loss_after_readiness_never_falls_back() {
    let f = fixture();
    acquisition(&f, "complete", true);
    let fault_arm = f.root.join("global-loss-armed");
    let mut child = acquisition_child(
        &f,
        &[
            ("GDP_MOCK_GLOBAL_WARMUP", "5"),
            ("GDP_MOCK_GLOBAL_LOST", "1"),
            ("GDP_MOCK_FAULT_AFTER", "30"),
            ("GDP_MOCK_FAULT_ARMED", fault_arm.to_str().unwrap()),
        ],
    );
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    assert!(ready["pairs_rejected"].as_u64().unwrap() >= 5);
    fs::write(&fault_arm, b"1").unwrap();
    let failed = wait_status(&f, &mut child, |v| {
        v["ready"] == false
            && v["rejection_reasons"]["GLOBAL_TIMESTAMP_UNAVAILABLE"]
                .as_u64()
                .unwrap_or(0)
                >= 6
    });
    assert_eq!(failed["ready"], false);
    assert!(failed["pairs_rejected"].as_u64().unwrap() >= 6);
    stop(&mut child);
    let events = fs::read_to_string(&f.events).unwrap();
    assert!(events.lines().any(|l| l == "publish-global"));
    assert!(!events.lines().any(|l| l == "publish-copied"));
}

#[test]
fn acquisition_complete_preserves_valid_peer_when_other_payload_or_global_is_invalid() {
    for (global, fault) in [
        (false, "GDP_MOCK_BAD_DEPTH_ONLY"),
        (true, "GDP_MOCK_GLOBAL_DEPTH_ZERO"),
    ] {
        let f = fixture();
        acquisition(&f, "complete", global);
        let mut child = acquisition_child(&f, &[(fault, "1")]);
        let sampled = wait_status(&f, &mut child, |v| {
            v["individual_publish_calls"].as_u64().unwrap_or(0) >= 5
        });
        assert_eq!(sampled["ready"], false);
        assert_eq!(sampled["pairs_published"], 0);
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(
            events
                .lines()
                .filter(|l| l.starts_with("image:color_optical:"))
                .count()
                >= 5
        );
        assert!(
            !events
                .lines()
                .any(|l| l.starts_with("image:depth_optical:"))
        );
    }
}
#[test]
fn acquisition_independent_streams_bind_actual_calibration_snapshot_before_readiness() {
    let f = fixture();
    quality_only(&f);
    acquisition(&f, "complete", true);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["acquisition"]["advanced"] = json!({"aggregate":"disabled"});
    fs::write(&f.config, value.to_string()).unwrap();
    let mut child = acquisition_child(&f, &[("GDP_MOCK_ALTERNATING", "1")]);
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    assert!(ready["snapshot_id"].is_string(), "{ready}");
    let snapshot: Value =
        serde_json::from_slice(&fs::read(ready["snapshot_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(
        snapshot["content"]["configuration"]["source_depth_scale_mm"],
        0.1_f32 as f64
    );
    assert_eq!(ready["pairs_published"], 0);
    stop(&mut child);
    let events = fs::read_to_string(&f.events).unwrap();
    assert!(events.lines().any(|l| l == "aggregate-disabled"));
}

#[test]
fn finite_acquisition_complete_and_strict_validate_global_mapping_and_stream_preservation() {
    for (preset, global, environment, expected_pairs, expected_color, expected_depth) in [
        ("complete", true, vec![], true, true, true),
        ("complete", false, vec![], true, true, true),
        (
            "complete",
            true,
            vec![("GDP_MOCK_GLOBAL_WARMUP", "2")],
            true,
            true,
            true,
        ),
        (
            "strict-rgbd",
            true,
            vec![("GDP_MOCK_GLOBAL_WARMUP", "2")],
            true,
            true,
            true,
        ),
        (
            "complete",
            true,
            vec![("GDP_MOCK_GLOBAL_DEPTH_ZERO", "1")],
            false,
            true,
            false,
        ),
        (
            "complete",
            true,
            vec![("GDP_MOCK_ALTERNATING", "1")],
            false,
            true,
            true,
        ),
        (
            "complete",
            false,
            vec![("GDP_MOCK_BAD_DEPTH_ONLY", "1")],
            false,
            true,
            false,
        ),
        (
            "complete",
            false,
            vec![("GDP_MOCK_SKEW", "1")],
            false,
            true,
            true,
        ),
        (
            "complete",
            true,
            vec![("GDP_MOCK_GLOBAL_LOST", "1"), ("GDP_MOCK_FAULT_AFTER", "3")],
            true,
            true,
            true,
        ),
        (
            "complete",
            false,
            vec![("GDP_MOCK_DEPTH_PUBLISH_ERROR", "1")],
            false,
            true,
            false,
        ),
    ] {
        let f = fixture();
        acquisition(&f, preset, global);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config.as_object_mut().unwrap().remove("service");
        config["seconds"] = json!(0.15);
        fs::write(&f.config, config.to_string()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"))
            .args(["--config", f.config.to_str().unwrap()])
            .envs(environment.iter().copied())
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{e}; stderr:{}", String::from_utf8_lossy(&output.stderr)));
        assert_eq!(
            report["message_timestamp"],
            if global { "global" } else { "system" }
        );
        assert_eq!(
            report["acquisition"]["sdk_frame_sync"],
            preset == "strict-rgbd"
        );
        assert_eq!(
            report["ros"]["pairs_publish_calls_succeeded"]
                .as_u64()
                .unwrap()
                > 0,
            expected_pairs,
            "{environment:?}: {report}"
        );
        let events = fs::read_to_string(&f.events).unwrap();
        let images: Vec<_> = events.lines().filter(|l| l.starts_with("image:")).collect();
        let unique: std::collections::BTreeSet<_> = images.iter().copied().collect();
        assert_eq!(
            images.len(),
            unique.len(),
            "{environment:?}: duplicate image publication"
        );
        assert_eq!(
            images.iter().any(|l| l.starts_with("image:color_optical:")),
            expected_color,
            "{environment:?}: {report}"
        );
        assert_eq!(
            images.iter().any(|l| l.starts_with("image:depth_optical:")),
            expected_depth,
            "{environment:?}: {report}"
        );
        if global {
            assert!(!events.lines().any(|l| l == "publish-copied"));
        }
        if environment.is_empty() {
            assert!(output.status.success(), "{report}");
        } else {
            assert!(
                !output.status.success(),
                "probe must report rejected source/pair/output observations: {report}"
            );
        }
    }
}

#[test]
fn stage_metrics_attribute_native_publish_and_sdk_wait_delays_separately() {
    for (environment, stage) in [
        ("GDP_MOCK_PUBLISH_DELAY_MS", "ros_image_publish_call"),
        ("GDP_MOCK_WAIT_DELAY_MS", "sdk_wait_call"),
    ] {
        let f = fixture();
        acquisition(&f, "complete", false);
        let mut child = acquisition_child(&f, &[(environment, "25")]);
        let ready = wait_status(&f, &mut child, |v| {
            v["ready"] == true && v["individual_publish_calls"].as_u64().unwrap_or(0) >= 4
        });
        let timing = &ready["pipeline_timing"];
        let measured = if stage == "sdk_wait_call" {
            &timing[stage]
        } else {
            &timing["streams"]["color"][stage]
        };
        assert!(
            measured["minimum_ns"].as_u64().unwrap() >= 20_000_000,
            "{ready}"
        );
        assert!(
            timing["streams"]["color"]["sdk_return_to_publish_start"]["samples"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(timing["sdk_internal_queue"]["current"].is_null());
        assert!(timing["derived_pair_queue"]["current"].as_u64().unwrap() <= 2);
        stop(&mut child);
        let events = fs::read_to_string(&f.events).unwrap();
        assert!(events.lines().any(|v| v == "sdk-return-observed"));
        assert!(events.lines().any(|v| v == "sdk-system-clock-verified"));
        assert_eq!(status(&f).unwrap()["state"], "stopped");
    }
}
#[test]
fn alternating_partial_diagnostics_accumulate_without_per_frame_status_fsync() {
    let f = fixture();
    acquisition(&f, "complete", false);
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["service"]["heartbeat_ms"] = json!(200);
    fs::write(&f.config, value.to_string()).unwrap();
    let mut child = acquisition_child(&f, &[("GDP_MOCK_ALTERNATING", "1")]);
    let observed = wait_status(&f, &mut child, |v| {
        v["framesets_observed"].as_u64().unwrap_or(0) >= 45
    });
    assert_eq!(observed["missing_pairs"], observed["framesets_observed"]);
    assert!(
        observed["status_persistence"]["coalesced_partial_updates"]
            .as_u64()
            .unwrap()
            > 20,
        "{observed}"
    );
    assert!(
        observed["heartbeat_sequence"].as_u64().unwrap() < 15,
        "{observed}"
    );
    assert!(observed["pipeline_timing"]["quality_tick_wall"]["samples"].is_number());
    stop(&mut child);
    assert_eq!(status(&f).unwrap()["state"], "stopped");
}

fn status_io_fixture(f: &Fixture) {
    quality_only(f);
    acquisition(f, "complete", false);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["service"]["max_restarts"] = json!(0);
    config["service"]["shutdown_timeout_ms"] = json!(3000);
    fs::write(&f.config, config.to_string()).unwrap();
}
fn status_io_command(f: &Fixture, binary: &Path, library: &Path, marker: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .args(["--config", f.config.to_str().unwrap()])
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .env("GDP_IO_ARMED", f.root.join("armed"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    io_faults::inject(
        &mut command,
        library,
        "fsync",
        f.root.to_str().unwrap(),
        marker,
    );
    command
}
#[test]
fn stalled_status_fsync_keeps_images_advancing_and_shutdown_persists_final_quality() {
    let f = fixture();
    status_io_fixture(&f);
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("injected");
    let mut command = status_io_command(
        &f,
        Path::new(env!("CARGO_BIN_EXE_capture-service")),
        &library,
        &marker,
    );
    // Hold fsync until publication progress is observed. A synchronous status
    // regression cannot pass merely because a shared runner slept longer.
    let release = f.root.join("release-status-sync");
    command
        .env("GDP_IO_DELAY_MS", "0")
        .env("GDP_IO_RELEASE", &release);
    let mut child = command.spawn().unwrap();
    wait_status(&f, &mut child, |v| v["ready"] == true);
    fs::write(f.root.join("armed"), b"1").unwrap();
    let started = Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(5));
    }
    let count = || {
        fs::read_to_string(&f.events)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("image:depth_optical:"))
            .count()
    };
    let before = count();
    let started = Instant::now();
    while count() <= before {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "capture blocked behind status fsync"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!release.exists());
    fs::write(&release, b"1").unwrap();
    stop(&mut child);
    io_faults::assert_injected(&marker, "fsync");
    let final_status = status(&f).unwrap();
    assert_eq!(final_status["state"], "stopped");
    assert_eq!(final_status["ready"], false);
    assert!(final_status["quality"]["summary_path"].is_string());
    let summary: Value = serde_json::from_slice(
        &fs::read(final_status["quality"]["summary_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(summary["summary"].is_object());
    let current: Value = serde_json::from_slice(
        &fs::read(final_status["quality"]["current_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(current["session_id"], final_status["session_id"]);
    assert!(
        final_status["status_persistence"]["pending_snapshots"]
            .as_u64()
            .unwrap()
            <= 1
    );
}
#[test]
fn background_status_fsync_failure_terminates_without_false_readiness() {
    let f = fixture();
    status_io_fixture(&f);
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("injected");
    let mut child = status_io_command(
        &f,
        Path::new(env!("CARGO_BIN_EXE_capture-service")),
        &library,
        &marker,
    )
    .spawn()
    .unwrap();
    wait_status(&f, &mut child, |v| v["ready"] == true);
    fs::write(f.root.join("armed"), b"1").unwrap();
    let started = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!exit.success());
    io_faults::assert_injected(&marker, "fsync");
    assert_eq!(status(&f).unwrap()["ready"], false);
    assert_eq!(status(&f).unwrap()["restart_attempt"], 0);
}

/// Explicit paired A/B experiment. Both binaries use identical ABI fixtures and
/// configs; delays apply only to status-directory fsync after readiness.
#[test]
#[ignore = "run explicitly with GDP_STATUS_BENCH_BEFORE and GDP_STATUS_BENCH_AFTER release binaries"]
fn status_persistence_ab_benchmark() {
    let before = std::env::var("GDP_STATUS_BENCH_BEFORE").unwrap();
    let after = std::env::var("GDP_STATUS_BENCH_AFTER").unwrap();
    let mut rows = Vec::new();
    for repeat in 0..3 {
        for delay in [0, 25, 100] {
            // Alternate order to reduce build/thermal/order effects.
            let binaries = if repeat % 2 == 0 {
                [("before", &before), ("after", &after)]
            } else {
                [("after", &after), ("before", &before)]
            };
            for (variant, binary) in binaries {
                let f = fixture();
                status_io_fixture(&f);
                let library = io_faults::compile(&f.root);
                let marker = f.root.join("injected");
                let mut command = status_io_command(&f, Path::new(binary), &library, &marker);
                command
                    .env("GDP_IO_DELAY_MS", delay.to_string())
                    .env("GDP_IO_REPEAT", "1");
                let mut child = command.spawn().unwrap();
                wait_status(&f, &mut child, |v| v["ready"] == true);
                let count = || {
                    fs::read_to_string(&f.events)
                        .unwrap()
                        .lines()
                        .filter(|l| l.starts_with("image:depth_optical:"))
                        .count()
                };
                let initial = count();
                fs::write(f.root.join("armed"), b"1").unwrap();
                let started = Instant::now();
                thread::sleep(Duration::from_secs(2));
                let images = count() - initial;
                let elapsed = started.elapsed().as_secs_f64();
                stop(&mut child);
                io_faults::assert_injected(&marker, "fsync");
                let final_status = status(&f).unwrap();
                assert_eq!(final_status["state"], "stopped");
                rows.push(json!({"variant":variant,"repeat":repeat,"delay_ms":delay,"elapsed_seconds":elapsed,
                    "images_in_window":images,"images_per_second":images as f64/elapsed,
                    "pipeline_timing":final_status["pipeline_timing"],"status_persistence":final_status["status_persistence"]}));
            }
        }
    }
    let report = json!({"kind":"status-persistence-ab","sdk":"synthetic 2x2 ABI fixture, 10ms wait; not hardware FPS",
        "build":"release","heartbeat_ms":50,"window_seconds":2,"repeats":3,
        "injection":"status-directory fsync delay only; enabled after readiness; no USB/ROS/disk-throughput claim", "rows":rows});
    fs::write(
        std::env::var("GDP_STATUS_BENCH_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}

#[test]
fn status_writer_thread_exhaustion_fails_before_sdk_access() {
    let f = fixture();
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("injected");
    let mut command = Command::new(env!("CARGO_BIN_EXE_capture-service"));
    command
        .args(["--config", f.config.to_str().unwrap()])
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    io_faults::inject(
        &mut command,
        &library,
        "pthread_create",
        "raw-writer",
        &marker,
    );
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!exit.success());
    io_faults::assert_injected(&marker, "pthread_create");
    assert_eq!(status(&f).unwrap()["ready"], false);
    let events = fs::read_to_string(&f.events).unwrap_or_default();
    assert!(!events.lines().any(|line| line == "start"));
}

// Functional tests allow shared-runner snapshot fsync stalls. SDK overflow
// tests and the explicit performance driver retain their independent 200 Hz load.
fn imu_policy() -> Value {
    json!({"enabled":true,"accel_rate_hz":25,"gyro_rate_hz":25,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"})
}
fn enable_imu(f: &Fixture) {
    acquisition(f, "strict-rgbd", false);
    let mut v: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    v["imu"] = imu_policy();
    v["service"]["idle_timeout_ms"] = json!(3000);
    v["service"]["max_restarts"] = json!(0);
    fs::write(&f.config, v.to_string()).unwrap();
}
#[test]
fn imu_service_publishes_both_streams_and_preserves_samples_in_native_recording() {
    let f = fixture();
    // This verifies ROS/IMU persistence, not shared-runner checkpoint throughput.
    // Dedicated recording tests exercise cross-chunk checkpoints/backpressure.
    configure_runtime_quality(&f, 31, 2_000_000);
    enable_imu(&f);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    // At 25 Hz, a 50 ms window can elapse before the first scheduled callback.
    // Allow multiple callback periods while retaining the startup exclusion assertion.
    config["startup_discovery_seconds"] = json!(0.3);
    config["service"]["heartbeat_timeout_ms"] = json!(3000);
    fs::write(&f.config, config.to_string()).unwrap();
    let mut child = acquisition_child(&f, &[("GDP_MOCK_WAIT_DELAY_MS", "33")]);
    let done = wait_status(&f, &mut child, |v| {
        v["ready"] == true && v["raw_recording"]["state"] == "complete"
    });
    assert_eq!(done["imu"]["ready"], true);
    assert_eq!(done["imu"]["shared_ros_context"], true);
    assert!(done["imu"]["startup_samples_excluded"].as_u64().unwrap() > 0);
    assert!(done["imu"]["published"][0].as_u64().unwrap() > 0);
    assert!(done["imu"]["published"][1].as_u64().unwrap() > 0);
    let path = Path::new(done["raw_recording"]["job_path"].as_str().unwrap());
    let mut source = offline_depth::recording::RecordingDirectorySource::open(path).unwrap();
    let mut samples = 0;
    while let Some(frame) = source.next_frame().unwrap() {
        samples += frame.record.imu.len();
    }
    assert!(samples > 20);
    assert_eq!(
        done["raw_recording"]["disk_stages"]["validation_imu_samples"],
        samples * 2
    );
    assert_eq!(done["raw_recording"]["imu_samples_submitted"], samples);
    stop(&mut child);
    let events = fs::read_to_string(&f.events).unwrap();
    assert!(events.contains("imu-accel-publish"));
    assert!(events.contains("imu-gyro-publish"));
    assert_eq!(events.lines().filter(|l| *l == "imu-stop").count(), 2);
}
#[test]
fn equal_system_imu_survives_global_shared_context_and_native_readback() {
    for pipeline in [false, true] {
        let f = fixture();
        configure_runtime_quality(&f, 31, 2_000_000);
        enable_imu(&f);
        acquisition(&f, "strict-rgbd", true);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["startup_discovery_seconds"] = json!(0.3);
        config["raw_recording"]["checkpoint_pipeline"] = json!(pipeline);
        config["raw_recording"]["queue_capacity"] = json!(16);
        fs::write(&f.config, config.to_string()).unwrap();
        let mut child = acquisition_child(
            &f,
            &[
                ("GDP_MOCK_WAIT_DELAY_MS", "33"),
                ("GDP_MOCK_IMU_EQUAL_SYSTEM", "1"),
                ("GDP_ROS_CAPTURE_BYTES", "1"),
            ],
        );
        let done = wait_status(&f, &mut child, |v| {
            v["ready"] == true && v["raw_recording"]["state"] == "complete"
        });
        assert_eq!(done["message_timestamp"], "global");
        assert_eq!(done["imu"]["shared_ros_context"], true);
        assert_eq!(done["publish_failures"], 0);
        assert_eq!(done["stamp_failures"], 0);
        assert!(done["imu"]["error"].is_null());
        let path = Path::new(done["raw_recording"]["job_path"].as_str().unwrap());
        let mut source = offline_depth::recording::RecordingDirectorySource::open(path).unwrap();
        let mut previous = [None, None];
        let mut repeated = [false, false];
        let mut samples = 0;
        while let Some(frame) = source.next_frame().unwrap() {
            let offline_depth::recording::TaggedEvidence::SdkBoundPair { pair, .. } =
                &frame.evidence
            else {
                panic!("SDK evidence required")
            };
            for f in [&pair.color, &pair.depth] {
                assert_eq!(
                    f.context.global_timestamp_us,
                    Some(1_700_000_000_000_000 + f.context.sequence.unwrap() * 33333 - 2000)
                );
            }
            for sample in frame.record.imu {
                let slot = sample.kind.slot();
                let system = 1_700_000_000_000_000 + (((sample.sequence - 1) / 2) * 2 + 1) * 40_000;
                assert_eq!(sample.system_timestamp_us, system);
                assert_eq!(
                    sample.global_timestamp_us,
                    Some(1_700_000_000_000_000 + sample.sequence * 40_000 - 2000)
                );
                if let Some((seq, device, system, global)) = previous[slot] {
                    assert!(sample.sequence > seq);
                    assert!(sample.device_timestamp_us > device);
                    assert!(sample.system_timestamp_us >= system);
                    assert!(sample.global_timestamp_us.unwrap() > global);
                    repeated[slot] |= sample.system_timestamp_us == system;
                }
                previous[slot] = Some((
                    sample.sequence,
                    sample.device_timestamp_us,
                    sample.system_timestamp_us,
                    sample.global_timestamp_us.unwrap(),
                ));
                samples += 1;
            }
        }
        assert!(samples > 20);
        assert_eq!(repeated, [true, true]);
        assert_eq!(done["raw_recording"]["imu_samples_submitted"], samples);
        assert_eq!(
            done["raw_recording"]["disk_stages"]["validation_imu_samples"],
            samples * 2
        );
        stop(&mut child);
        let stopped = status(&f).unwrap();
        assert_eq!(stopped["state"], "stopped");
        assert_eq!(stopped["raw_recording"]["state"], "complete");
        let events = fs::read_to_string(&f.events).unwrap();
        let mut audit = Vec::new();
        recording_auditor::audit(path, false, &mut audit).unwrap();
        let rows: Vec<Value> = String::from_utf8(audit)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut pairs = 0;
        for row in rows.iter().filter(|v| v["kind"] == "pair") {
            pairs += 1;
            for (stream, frame_id) in [("color", "color_optical"), ("depth", "depth_optical")] {
                let data = &row[stream];
                let prefix = format!(
                    "payload:{frame_id}:{}:{}:",
                    data["sequence"], data["stamp_ns"]
                );
                let hex = events
                    .lines()
                    .find_map(|l| l.strip_prefix(&prefix))
                    .expect("actual C ABI publication must match native Global header");
                let payload: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect();
                let digest = offline_depth::sha256(&payload);
                assert_eq!(
                    digest,
                    data[if stream == "depth" {
                        "ros_sha256"
                    } else {
                        "sha256"
                    }]
                );
                if stream == "depth" {
                    assert_ne!(data["sha256"], data["ros_sha256"]);
                }
            }
        }
        assert_eq!(pairs, 31);
        assert_eq!(rows.last().unwrap()["independent_audit_passes"], 2);

        assert!(events.contains("evidence-copied"));
        assert!(events.contains("imu-accel-publish"));
        assert!(events.contains("imu-gyro-publish"));
    }
}
#[test]
fn native_global_capture_rejects_missing_invalid_and_changed_sdk_metadata_without_false_complete() {
    for (fault, value) in [
        ("GDP_MOCK_GLOBAL_LOST", "1"),
        ("GDP_MOCK_GLOBAL_INVALID", "1"),
        ("GDP_MOCK_GLOBAL_LATE_LOST", "1"),
        ("GDP_MOCK_WIDTH_AFTER", "2"),
        ("GDP_MOCK_MUTATE_PAYLOAD", "1"),
    ] {
        let f = fixture();
        configure_runtime_quality(&f, 1000, 20_000_000);
        acquisition(&f, "strict-rgbd", true);
        let gate = f.root.join("arm-fault");
        // Arm complete framesets, not individual property reads. Otherwise the
        // parent can create the file after native capture validated this frame
        // but before ROS reads its Global stamp, exercising a different fault.
        let source_path = f.root.join("mock-sdk.c");
        let mut source = fs::read_to_string(&source_path).unwrap();
        source.insert_str(
            0,
            "static int requested_frame_fault(void);\nstatic int native_frame_fault;\n",
        );
        replace(
            &mut source,
            "static int fault_active(void){const char *gate",
            "static int requested_frame_fault(void){const char *gate",
        );
        assert_eq!(
            source.matches("service_sequence++;return make(5);").count(),
            1
        );
        replace(
            &mut source,
            "service_sequence++;return make(5);",
            "service_sequence++;native_frame_fault=requested_frame_fault();return make(5);",
        );
        source.push_str("\nstatic int fault_active(void){return native_frame_fault;}\n");
        fs::write(&source_path, source).unwrap();
        compile("cc", &source_path, &f.root.join("mock-sdk.so"), None);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        // This verifies injected SDK corruption and recovery, not the 500 ms
        // supervision deadline. Instrumented CI status fsync can exceed it
        // before the fault is armed; keep the data-fault assertions unchanged.
        config["service"]["idle_timeout_ms"] = json!(3000);
        config["raw_recording"]["queue_capacity"] = json!(16);
        fs::write(&f.config, config.to_string()).unwrap();
        let mut child = acquisition_child(
            &f,
            &[
                (fault, value),
                ("GDP_MOCK_FAULT_ARMED", gate.to_str().unwrap()),
                ("GDP_MOCK_WAIT_DELAY_MS", "33"),
                ("GDP_MOCK_MUTATE_AFTER", "4"),
            ],
        );
        let ready = wait_status(&f, &mut child, |v| {
            v["ready"] == true && v["raw_recording"]["written_pairs"].as_u64().unwrap_or(0) >= 40
        });
        fs::write(&gate, b"armed").unwrap();
        let failed = wait_status(&f, &mut child, |v| v["raw_recording"]["state"] == "failed");
        assert_eq!(
            failed["raw_recording"]["reason"],
            if fault == "GDP_MOCK_GLOBAL_LOST" {
                "RAW_SDK_OR_SESSION_ERROR"
            } else {
                "RAW_NATIVE_PAIR_REJECTED"
            },
            "{fault}: {failed}"
        );
        assert!(!Path::new(ready["raw_recording"]["job_path"].as_str().unwrap()).exists());
        let deadline = Instant::now() + Duration::from_secs(7);
        let exit = loop {
            if let Some(exit) = child.try_wait().unwrap() {
                break exit;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("fault must terminate unhealthy session: {fault}");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!exit.success());
        let partial = Path::new(ready["raw_recording"]["partial_path"].as_str().unwrap());
        assert!(!partial.join("COMPLETE").exists());
        let mut out = Vec::new();
        recording_auditor::audit(partial, true, &mut out).unwrap();
        let rows: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert!(rows.last().unwrap()["pairs"].as_u64().unwrap() >= 32);
        assert_eq!(rows.last().unwrap()["recovered"], true);
        assert_eq!(rows.last().unwrap()["independent_audit_passes"], 2);
    }
}
#[test]
fn requested_imu_failures_are_terminal_and_never_complete_a_native_recording() {
    for fault in [
        "GDP_MOCK_IMU_UNSUPPORTED",
        "GDP_MOCK_IMU_PROFILE_ERROR",
        "GDP_MOCK_IMU_START_ERROR",
        "GDP_MOCK_IMU_NAN",
        "GDP_MOCK_IMU_REGRESS",
        "GDP_MOCK_IMU_GYRO_STALL",
        "GDP_ROS_IMU_PUBLISH_ERROR",
    ] {
        let f = fixture();
        configure_runtime_quality(&f, 1000, 10_000_000);
        enable_imu(&f);
        let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        value["imu"]["stale_timeout_ms"] = json!(100);
        fs::write(&f.config, value.to_string()).unwrap();
        let mut child = acquisition_child(&f, &[(fault, "1")]);
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "{fault}");
            thread::sleep(Duration::from_millis(5));
        }
        let state = status(&f).unwrap();
        assert_eq!(state["ready"], false, "{fault}: {state}");
        assert_ne!(state["raw_recording"]["state"], "complete");
        assert!(
            !Path::new(state["raw_recording"]["job_path"].as_str().unwrap())
                .join("COMPLETE")
                .exists()
        );
    }
}

fn large_imu_benchmark_fixture() -> Fixture {
    let f = fixture();
    let path = f.root.join("mock-sdk.c");
    let mut source = fs::read_to_string(&path).unwrap();
    replace(
        &mut source,
        "static unsigned char bytes[] = {16,128,32,128,64,90,80,240};",
        "static unsigned char bytes[640*400*2];\n__attribute__((constructor)) static void fill_pixels(void){for(size_t i=0;i<sizeof(bytes);i+=2){bytes[i]=10;bytes[i+1]=0;}} ",
    );
    replace(
        &mut source,
        "w!=2||h!=2||fps!=30",
        "w!=640||h!=400||fps!=30",
    );
    replace(
        &mut source,
        "ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;if(getenv(\"GDP_MOCK_WIDTH_AFTER\")&&fault_active()&&++((obj*)p)->width_reads>atoi(getenv(\"GDP_MOCK_WIDTH_AFTER\"))){return 0;}return getenv(\"GDP_MOCK_BAD_WIDTH\")&&fault_active()?3:2;}",
        "ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;return 640;}",
    );
    replace(
        &mut source,
        "ob_video_frame_get_height(const obj *p,error **e){(void)p;(void)e;return 2;}",
        "ob_video_frame_get_height(const obj *p,error **e){(void)p;(void)e;return 400;}",
    );
    replace(
        &mut source,
        "ob_video_stream_profile_get_width(const profile *p,error **e){(void)p;(void)e;return 2;}",
        "ob_video_stream_profile_get_width(const profile *p,error **e){(void)p;(void)e;return 640;}",
    );
    replace(
        &mut source,
        "ob_video_stream_profile_get_height(const profile *p,error **e){(void)p;(void)e;return 2;}",
        "ob_video_stream_profile_get_height(const profile *p,error **e){(void)p;(void)e;return 400;}",
    );
    replace(
        &mut source,
        "(intrinsic){1,2,.5f,.5f,2,2}",
        "(intrinsic){1,2,.5f,.5f,640,400}",
    );
    fs::write(&path, source).unwrap();
    let mut v: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    compile(
        "cc",
        &path,
        Path::new(v["sdk_library"].as_str().unwrap()),
        None,
    );
    for stream in ["color", "depth"] {
        v[stream]["width"] = json!(640);
        v[stream]["height"] = json!(400);
    }
    if let Some(bridge) = std::env::var_os("GDP_IMU_BENCH_REAL_BRIDGE") {
        v["ros"]["bridge"] = json!(bridge.to_str().unwrap());
    }
    v["poll_ms"] = json!(100);
    v["service"]["heartbeat_ms"] = json!(100);
    v["service"]["idle_timeout_ms"] = json!(3000);
    fs::write(&f.config, v.to_string()).unwrap();
    f
}
fn process_measurement(pid: u32) -> (u64, u64) {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = text
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let cpu = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let peak = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (cpu, peak)
}
#[test]
#[ignore = "explicit release IMU on/off performance experiment; no noisy CI thresholds"]
fn imu_capture_ab_benchmark() {
    let baseline = std::env::var_os("GDP_IMU_BENCH_BASELINE_SERVICE");
    let target_pairs = if baseline.is_some() { 300 } else { 60 };
    let window_seconds = if baseline.is_some() { 10 } else { 2 };
    let mut rows = Vec::new();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    for repeat in 0..5 {
        for recording in [false, true] {
            for candidate in if repeat % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let enabled = baseline.is_some() || candidate;
                let service = if !candidate {
                    baseline
                        .as_deref()
                        .unwrap_or(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_capture-service")))
                } else {
                    std::ffi::OsStr::new(env!("CARGO_BIN_EXE_capture-service"))
                };
                let f = large_imu_benchmark_fixture();
                if recording {
                    configure_runtime_quality(&f, target_pairs, 1024 * 1024 * 1024);
                }
                enable_imu(&f);
                let mut value: Value =
                    serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
                value["imu"]["enabled"] = json!(enabled);
                value["imu"]["accel_rate_hz"] = json!(200);
                value["imu"]["gyro_rate_hz"] = json!(200);
                fs::write(&f.config, value.to_string()).unwrap();
                let mut child = Command::new(service)
                    .args(["--config", f.config.to_str().unwrap()])
                    .env("GDP_MOCK_WAIT_DELAY_MS", "33")
                    .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
                    .env("GDP_ROS_MOCK_EVENTS", &f.events)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
                let worker = ready["pid"].as_u64().unwrap() as u32;
                let parent_start = process_measurement(child.id());
                let worker_start = process_measurement(worker);
                let started = Instant::now();
                let final_status = if recording {
                    wait_status_for(
                        &f,
                        &mut child,
                        |v| v["raw_recording"]["state"] == "complete",
                        Duration::from_secs(30),
                    )
                } else {
                    thread::sleep(Duration::from_secs(window_seconds));
                    status(&f).unwrap()
                };
                let wall = started.elapsed().as_secs_f64();
                let parent_end = process_measurement(child.id());
                let worker_end = process_measurement(worker);
                let frames = final_status["pairs_published"].as_u64().unwrap()
                    - ready["pairs_published"].as_u64().unwrap();
                assert_eq!(final_status["pairs_rejected"], 0);
                assert_eq!(final_status["missing_pairs"], 0);
                if enabled {
                    assert_eq!(final_status["imu"]["ready"], true);
                    assert!(final_status["imu"]["queue_high_water"].as_u64().unwrap() <= 128);
                }
                if recording {
                    assert_eq!(final_status["raw_recording"]["written_pairs"], target_pairs);
                    let samples = final_status["raw_recording"]["imu_samples_submitted"]
                        .as_u64()
                        .unwrap();
                    assert_eq!(
                        final_status["raw_recording"]["disk_stages"]["validation_imu_samples"],
                        samples * 2
                    );
                    if enabled {
                        assert!(samples > 100);
                    }
                }
                let row = json!({"service_variant":if baseline.is_some() {if candidate {"after"}else{"before"}}else{"current"},"repeat":repeat,"recording":recording,"imu_enabled":enabled,"wall_seconds":wall,"published_pairs_in_window":frames,"observed_pairs_per_second":frames as f64/wall,"monitored_cpu_cores":((parent_end.0-parent_start.0)+(worker_end.0-worker_start.0)) as f64/hz/wall,"worker_peak_rss_kib":worker_end.1,"supervisor_peak_rss_kib":parent_end.1,"sum_process_peak_rss_kib":worker_end.1+parent_end.1,"pipeline_timing":final_status["pipeline_timing"],"imu":final_status["imu"],"raw_recording":final_status["raw_recording"]});
                stop(&mut child);
                rows.push(row);
            }
        }
    }
    fs::write(std::env::var_os("GDP_IMU_BENCH_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({"bridge_backend":if std::env::var_os("GDP_IMU_BENCH_REAL_BRIDGE").is_some(){"real-humble-dds"}else{"synthetic-c-abi"},"kind":if baseline.is_some(){"imu-cache-before-after-alternating"}else{"imu-on-off-synthetic-capture"},"native_target_pairs":target_pairs,"ros_window_seconds":window_seconds,"runs":20,"repeats":5,"width":640,"height":400,"nominal_fps":30,"imu_rate_hz_per_stream":200,"rows":rows,"limits":if std::env::var_os("GDP_IMU_BENCH_REAL_BRIDGE").is_some(){"synthetic C SDK with real Humble/Fast DDS publishers and no downstream image subscribers; no USB; architecture may be emulated; CPU sums capture worker and supervisor; RSS sums separate process high-water marks; native recording serial chunked-v3; target/window in report metadata"}else{"synthetic C SDK and bridge; includes producer callback thread overhead, excludes USB and actual DDS costs; CPU sums capture worker and supervisor; RSS sums process high-water marks, not simultaneous or anonymous RSS; native recording uses serial chunked-v3; target/window in report metadata"}})).unwrap()).unwrap();
}

#[test]
fn finite_imu_sdk_only_capture_needs_both_streams_without_requiring_ros() {
    for stalled in [false, true] {
        let f = fixture();
        enable_imu(&f);
        let mut v: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("service");
        v.as_object_mut().unwrap().remove("ros");
        v["seconds"] = json!(0.05);
        fs::write(&f.config, v.to_string()).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_rgbd-probe"));
        c.args(["--config", f.config.to_str().unwrap()]);
        if stalled {
            c.env("GDP_MOCK_IMU_STALL", "1");
        }
        let output = c.output().unwrap();
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["ok"], !stalled, "{result}");
        assert_eq!(result["imu"]["ready"], !stalled);
        assert_eq!(result["imu"]["published"], json!([0, 0]));
        assert_eq!(result["imu"]["ros_publish_wall"]["samples"], 0);
        assert!(result["imu"]["drain_wall"]["samples"].as_u64().unwrap() > 0);
    }
}

#[test]
fn imu_is_published_even_while_video_framesets_are_incomplete() {
    let f = fixture();
    enable_imu(&f);
    acquisition(&f, "complete", false);
    let mut child = acquisition_child(&f, &[("GDP_MOCK_MISSING_COLOR", "1")]);
    let status = wait_status(&f, &mut child, |v| {
        v["unpaired_images_published"].as_u64().unwrap_or(0) >= 5 && v["imu"]["ready"] == true
    });
    assert_eq!(status["ready"], false);
    assert_eq!(status["pairs_accepted"], 0);
    assert!(status["imu"]["published"][0].as_u64().unwrap() > 0);
    assert!(status["imu"]["published"][1].as_u64().unwrap() > 0);
    stop(&mut child);
}

#[test]
fn imu_timing_reports_drain_and_ros_batches_separately_from_sdk_wait() {
    let f = fixture();
    enable_imu(&f);
    let mut child = acquisition_child(&f, &[("GDP_ROS_IMU_FIRST_PUBLISH_DELAY_MS", "150")]);
    let observed = wait_status(&f, &mut child, |v| {
        v["ready"] == true && v["imu"]["published"][0].as_u64().unwrap_or(0) > 10
    });
    let imu = &observed["imu"];
    assert!(imu["ros_publish_wall"]["maximum_ns"].as_u64().unwrap() >= 150_000_000);
    let total = |key: &str| {
        imu[key]["total_ns"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap()
    };
    assert!(total("pump_wall") >= total("drain_wall") + total("ros_publish_wall"));
    assert!(
        imu["drain_wall"]["samples"].as_u64().unwrap()
            >= imu["ros_publish_wall"]["samples"].as_u64().unwrap()
    );
    let sdk = &observed["pipeline_timing"];
    assert!(sdk["sdk_wait_call"]["samples"].as_u64().unwrap() > 0);
    assert!(
        sdk["sdk_wait_scope"]
            .as_str()
            .unwrap()
            .contains("pipeline.wait only")
    );
    assert!(
        imu["timing_scope"]["ros_publish_wall"]
            .as_str()
            .unwrap()
            .contains("batch")
    );
    stop(&mut child);
}

#[test]
fn old_imu_bridges_keep_isolated_contexts_without_losing_capture_readiness() {
    let f = fixture();
    enable_imu(&f);
    let source = f.root.join("mock-bridge.cpp");
    let old = fs::read_to_string(&source).unwrap();
    fs::write(&source, format!("#define MOCK_NO_CONTEXT\n{old}")).unwrap();
    let config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    compile(
        "c++",
        &source,
        Path::new(config["ros"]["bridge"].as_str().unwrap()),
        Some(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ros/include")),
    );
    let mut child = launch(&f);
    let ready = wait_status(&f, &mut child, |v| {
        v["ready"] == true && v["imu"]["ready"] == true
    });
    assert_eq!(ready["imu"]["shared_ros_context"], false);
    stop(&mut child);
    assert!(
        !fs::read_to_string(&f.events)
            .unwrap()
            .lines()
            .any(|v| v == "context-create")
    );
}

#[test]
#[ignore = "explicit release checkpoint instrumentation/sampler overhead experiment"]
fn checkpoint_observation_benchmark() {
    let mut rows = Vec::new();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    for repeat in 0..5 {
        let modes = if repeat % 2 == 0 {
            [0, 1, 2, 3]
        } else {
            [3, 2, 1, 0]
        };
        for mode in modes {
            let f = large_imu_benchmark_fixture();
            configure_runtime_quality(&f, 180, 1024 * 1024 * 1024);
            enable_imu(&f);
            let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
            value["imu"]["accel_rate_hz"] = json!(200);
            value["imu"]["gyro_rate_hz"] = json!(200);
            value["raw_recording"]["checkpoint_pipeline"] = json!(true);
            if mode > 0 {
                value["raw_recording"]["checkpoint_observation"] = json!(mode >= 2);
            }
            fs::write(&f.config, value.to_string()).unwrap();
            let service = if mode == 0 {
                std::env::var_os("GDP_CHECKPOINT_BEFORE").unwrap()
            } else {
                env!("CARGO_BIN_EXE_capture-service").into()
            };
            let mut child = Command::new(service)
                .args(["--config", f.config.to_str().unwrap()])
                .env("GDP_MOCK_WAIT_DELAY_MS", "33")
                .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
                .env("GDP_ROS_MOCK_EVENTS", &f.events)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
            let worker = ready["pid"].as_u64().unwrap() as u32;
            let out = f.config.parent().unwrap().join("probe-output");
            let mut sampler = if mode == 3 {
                Some(
                    Command::new("python3")
                        .args([
                            "/work/scripts/validation/checkpoint_probe.py",
                            "--host-status",
                            f.status.to_str().unwrap(),
                            "--pid",
                            &worker.to_string(),
                            "--seconds",
                            "60",
                            "--output",
                            out.to_str().unwrap(),
                        ])
                        .stdout(Stdio::null())
                        .spawn()
                        .unwrap(),
                )
            } else {
                None
            };
            let a = process_measurement(child.id());
            let b = process_measurement(worker);
            let started = Instant::now();
            let final_status = wait_status_for(
                &f,
                &mut child,
                |v| v["raw_recording"]["state"] == "complete",
                Duration::from_secs(30),
            );
            let wall = started.elapsed().as_secs_f64();
            let c = process_measurement(child.id());
            let d = process_measurement(worker);
            let sample = if let Some(ref mut sampler) = sampler {
                unsafe {
                    libc::kill(sampler.id() as i32, libc::SIGINT);
                };
                assert!(sampler.wait().unwrap().success());
                serde_json::from_slice::<Value>(&fs::read(out.join("summary.json")).unwrap())
                    .unwrap()
            } else {
                Value::Null
            };
            assert_eq!(final_status["raw_recording"]["written_pairs"], 180);
            assert_eq!(
                final_status["raw_recording"]["disk_stages"]["validation_imu_samples"]
                    .as_u64()
                    .unwrap(),
                final_status["raw_recording"]["imu_samples_submitted"]
                    .as_u64()
                    .unwrap()
                    * 2
            );
            assert_eq!(final_status["pairs_rejected"], 0);
            rows.push(json!({"mode":mode,"repeat":repeat,"wall_seconds":wall,"cpu_cores":((c.0-a.0)+(d.0-b.0)) as f64/hz/wall,"worker_peak_rss_kib":d.1,"raw_recording":final_status["raw_recording"],"sampler":sample}));
            stop(&mut child);
        }
    }
    fs::write(std::env::var_os("GDP_CHECKPOINT_BENCH_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({"modes":["baseline","observation_disabled","observation_enabled","enabled_plus_sampler"],"pairs":180,"repeats":5,"synthetic_sdk":true,"synthetic_ros_bridge":true,"width":640,"height":400,"imu_hz":200,"rows":rows})).unwrap()).unwrap();
}

#[test]
#[ignore = "explicit release status report lock A/B; no noisy CI thresholds"]
fn status_report_lock_ab_benchmark() {
    let before = std::env::var_os("GDP_STATUS_LOCK_BEFORE").unwrap();
    let mut rows = Vec::new();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    for repeat in 0..5 {
        for observation in [false, true] {
            for candidate in if repeat % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let f = large_imu_benchmark_fixture();
                configure_runtime_quality(&f, 240, 1024 * 1024 * 1024);
                enable_imu(&f);
                let mut config: Value =
                    serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
                config["imu"]["accel_rate_hz"] = json!(200);
                config["imu"]["gyro_rate_hz"] = json!(200);
                config["raw_recording"]["checkpoint_pipeline"] = json!(true);
                config["raw_recording"]["checkpoint_observation"] = json!(observation);
                config["service"]["heartbeat_ms"] = json!(50);
                assert_eq!(config["service"]["heartbeat_ms"], 50);
                fs::write(&f.config, config.to_string()).unwrap();
                let service = if candidate {
                    std::ffi::OsStr::new(env!("CARGO_BIN_EXE_capture-service"))
                } else {
                    before.as_os_str()
                };
                let mut child = Command::new(service)
                    .args(["--config", f.config.to_str().unwrap()])
                    .env("GDP_MOCK_WAIT_DELAY_MS", "33")
                    .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
                    .env("GDP_ROS_MOCK_EVENTS", &f.events)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
                let worker = ready["pid"].as_u64().unwrap() as u32;
                let a = process_measurement(child.id());
                let b = process_measurement(worker);
                let started = Instant::now();
                let tracking = std::cell::RefCell::new((
                    ready["heartbeat_sequence"].as_u64().unwrap(),
                    Instant::now(),
                    Vec::new(),
                ));
                let final_status = wait_status_for(
                    &f,
                    &mut child,
                    |v| {
                        let seq = v["heartbeat_sequence"].as_u64().unwrap();
                        let mut tracking = tracking.borrow_mut();
                        if seq != tracking.0 {
                            let elapsed = tracking.1.elapsed().as_secs_f64() * 1000.;
                            tracking.2.push(elapsed);
                            tracking.1 = Instant::now();
                            tracking.0 = seq;
                        }
                        v["raw_recording"]["state"] == "complete"
                    },
                    Duration::from_secs(30),
                );
                let wall = started.elapsed().as_secs_f64();
                let c = process_measurement(child.id());
                let d = process_measurement(worker);
                let gaps = tracking.into_inner().2;
                let raw = &final_status["raw_recording"];
                assert_eq!(raw["written_pairs"], 240);
                assert_eq!(raw["whole_pair_queue_overflows"], 0);
                assert_eq!(final_status["pairs_rejected"], 0);
                assert_eq!(final_status["missing_pairs"], 0);
                assert_eq!(
                    raw["disk_stages"]["validation_image_bytes"]
                        .as_u64()
                        .unwrap(),
                    raw["payload_bytes_submitted"].as_u64().unwrap() * 2
                );
                assert_eq!(
                    raw["disk_stages"]["validation_imu_samples"]
                        .as_u64()
                        .unwrap(),
                    raw["imu_samples_submitted"].as_u64().unwrap() * 2
                );
                assert!(final_status["imu"]["queue_high_water"].as_u64().unwrap() <= 128);
                stop(&mut child);
                let stopped = status(&f).unwrap();
                assert_eq!(stopped["state"], "stopped");
                assert_eq!(stopped["raw_recording"]["state"], "complete");
                rows.push(json!({"variant":if candidate {"after"}else{"before"},"observation":observation,"repeat":repeat,
                    "wall_seconds":wall,"cpu_cores":((c.0-a.0)+(d.0-b.0)) as f64/hz/wall,"worker_peak_rss_kib":d.1,"supervisor_peak_rss_kib":c.1,
                    "heartbeat_observed_gaps_ms":gaps,"pipeline_timing":final_status["pipeline_timing"],"status_persistence":stopped["status_persistence"],"raw_recording":raw,"imu":final_status["imu"]}));
            }
        }
    }
    fs::write(std::env::var_os("GDP_STATUS_LOCK_OUTPUT").unwrap(),serde_json::to_vec_pretty(&json!({"kind":"status-report-lock-ab","repeats":5,"runs":20,"target_pairs":240,"width":640,"height":400,"heartbeat_ms":50,"sdk_wait_delay_ms":33,"imu_requested_hz_per_stream":200,"rows":rows,"limits":"synthetic SDK/C ABI bridge; no USB/real DDS/actual disk latency; worker+supervisor CPU from readiness to recording complete; VmHWM separate process peaks; heartbeat gaps are polling observations, not exact write intervals"})).unwrap()).unwrap();
}

#[test]
fn checkpoint_sync_tail_is_absorbed_with_pipeline_but_unbounded_stalls_still_overflow() {
    for (pipeline, delay_ms, succeeds) in
        [(false, 800, false), (true, 800, true), (true, 1800, false)]
    {
        let f = fixture();
        configure_runtime_quality(&f, 64, 10_000_000);
        let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
        config["raw_recording"]["checkpoint_pipeline"] = json!(pipeline);
        config["raw_recording"]["queue_capacity"] = json!(16);
        for key in [
            "idle_timeout_ms",
            "heartbeat_timeout_ms",
            "shutdown_timeout_ms",
        ] {
            config["service"][key] = json!(4000);
        }
        fs::write(&f.config, config.to_string()).unwrap();
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("sync-injected");
        let mut command = Command::new(env!("CARGO_BIN_EXE_capture-service"));
        command
            .args(["--config", f.config.to_str().unwrap()])
            .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
            .env("GDP_ROS_MOCK_EVENTS", &f.events)
            .env("GDP_MOCK_WAIT_DELAY_MS", "33")
            .env("GDP_IO_DELAY_MS", delay_ms.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        io_faults::inject(
            &mut command,
            &library,
            "fsync",
            "/payload-00000000000000000001.bin",
            &marker,
        );
        let mut child = command.spawn().unwrap();
        let final_status = wait_status(&f, &mut child, |v| {
            v["raw_recording"]["state"] == if succeeds { "complete" } else { "failed" }
        });
        if succeeds {
            assert_eq!(final_status["raw_recording"]["written_pairs"], 64);
            assert_eq!(
                final_status["raw_recording"]["whole_pair_queue_overflows"],
                0
            );
            assert_eq!(
                final_status["raw_recording"]["disk_stages"]["durable_pairs"],
                64
            );
            assert!(
                final_status["raw_recording"]["disk_stages"]["validation_image_bytes"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            let path = final_status["raw_recording"]["job_path"].as_str().unwrap();
            let mut reader =
                offline_depth::recording::RecordingDirectorySource::open(Path::new(path)).unwrap();
            let mut count = 0;
            while reader.next_frame().unwrap().is_some() {
                count += 1;
            }
            assert_eq!(count, 64);
        } else {
            assert_eq!(
                final_status["raw_recording"]["reason"],
                "RAW_QUEUE_OVERFLOW"
            );
            assert_eq!(
                final_status["raw_recording"]["whole_pair_queue_overflows"],
                1
            );
            let path = Path::new(final_status["raw_recording"]["job_path"].as_str().unwrap());
            assert!(!path.join("COMPLETE").exists());
        }
        stop(&mut child);
        io_faults::assert_injected(&marker, "fsync");
    }
}

fn cleanup_fault_fixture() -> Fixture {
    let f = fixture();
    configure_runtime_quality(&f, 31, 2_000_000);
    enable_imu(&f);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["service"]["heartbeat_timeout_ms"] = json!(3000);
    config["raw_recording"]["checkpoint_pipeline"] = json!(true);
    config["raw_recording"]["checkpoint_observation"] = json!(true);
    config["raw_recording"]["queue_capacity"] = json!(16);
    fs::write(&f.config, config.to_string()).unwrap();
    f
}
fn cleanup_fault_command(f: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_capture-service"));
    command
        .args(["--config", f.config.to_str().unwrap()])
        .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
        .env("GDP_ROS_MOCK_EVENTS", &f.events)
        .env("GDP_MOCK_WAIT_DELAY_MS", "33")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
fn await_fault_marker(marker: &Path, child: &mut Child) {
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker exited before injection"
        );
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "injection not reached"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn await_failed_exit(child: &mut Child) {
    let start = Instant::now();
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(!exit.success());
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "failed worker did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn slow_critical_draining_status_exposes_imu_overflow_and_persistence_wait() {
    let f = cleanup_fault_fixture();
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("draining-fsync");
    let release = f.root.join("release-final-reader");
    let mut command = cleanup_fault_command(&f);
    io_faults::inject(&mut command, &library, "fsync", ".tmp", &marker);
    command
        .env("GDP_IO_HOLD_READ_SUFFIX", "/manifest.json")
        .env("GDP_IO_HOLD_MARKER", f.root.join("reader-held"))
        .env("GDP_IO_HOLD_RELEASE", &release)
        .env("GDP_IO_REQUIRE_CONTENT", "\"state\":\"draining\"")
        .env("GDP_IO_DELAY_MS", "800")
        // Begin the deterministic callback burst only while the real critical
        // draining status fsync is blocked. Startup runs at the normal rate.
        .env("GDP_MOCK_IMU_BURST_AFTER_FILE", &marker);
    let mut child = command.spawn().unwrap();
    await_fault_marker(&marker, &mut child);
    let failed = wait_status(&f, &mut child, |v| {
        v["imu"]["error"] == "IMU_QUEUE_OVERFLOW"
    });
    assert_eq!(failed["ready"], false);
    assert_eq!(failed["reason"], "SESSION_FAILED");
    assert_eq!(
        failed["imu"]["failure_observation"]["first_error_queue_len"],
        128
    );
    assert_eq!(failed["imu"]["failure_observation"]["queue_capacity"], 128);
    assert!(failed["imu"]["failure_observation"]["since_last_drain_ns"].is_number());
    // The intentional unbounded burst still reaches the real queue cap even
    // while a fence pumps. Do not require the old 800ms IMU starvation bug.
    assert!(failed["imu"]["drain_interval"]["maximum_ns"].is_number());
    let events = failed["status_persistence"]["critical_events"]
        .as_array()
        .unwrap();
    assert!(events.iter().any(|v| v["target"]["raw_state"] == "draining"
        && v["wall_ns"].as_u64().unwrap() >= 800_000_000
        && v["ok"] == true
        && v["pump_failed"] == true));
    assert!(f.root.join("reader-held").exists());
    assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
    fs::write(&release, b"1").unwrap();
    await_failed_exit(&mut child);
    let partial = Path::new(failed["raw_recording"]["partial_path"].as_str().unwrap());
    for _ in 0..2 {
        let mut reader =
            offline_depth::recording::RecordingDirectorySource::open_recovered(partial).unwrap();
        let mut count = 0;
        while reader.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 31);
    }
    let end = status(&f).unwrap();
    assert_eq!(end["ready"], false);
    assert_eq!(end["imu"]["error"], "IMU_QUEUE_OVERFLOW");
    assert_eq!(end["worker_reason"], "SESSION_FAILED");
    io_faults::assert_injected(&marker, "fsync");
}
#[test]
fn imu_failure_is_durable_before_blocked_final_readback_and_never_commits() {
    for release_readback in [true, false] {
        let f = cleanup_fault_fixture();
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("final-readback");
        let release = f.root.join("release-readback");
        let mut command = cleanup_fault_command(&f);
        io_faults::inject(&mut command, &library, "read", "/manifest.json", &marker);
        command
            .env("GDP_IO_RELEASE", &release)
            .env("GDP_IO_DELAY_MS", "0")
            .env("GDP_MOCK_IMU_NULL_AFTER_FILE", &marker);
        let mut child = command.spawn().unwrap();
        await_fault_marker(&marker, &mut child);
        let failed = wait_status(&f, &mut child, |v| {
            v["cleanup"]["active"]["phase"] == "recorder_join"
                && v["imu"]["error"] == "IMU_CALLBACK_FAILED"
        });
        assert_eq!(failed["ready"], false);
        assert_eq!(failed["reason"], "SESSION_FAILED");
        assert_eq!(failed["raw_recording"]["state"], "failed");
        assert_eq!(
            failed["raw_recording"]["reason"],
            "RAW_SDK_OR_SESSION_ERROR"
        );
        assert!(
            failed["raw_recording"]["disk_stages"]["checkpoint_observation"]["active"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["phase"] == "final_readback")
        );
        assert!(!release.exists());
        assert!(child.try_wait().unwrap().is_none());
        let partial = PathBuf::from(failed["raw_recording"]["partial_path"].as_str().unwrap());
        let job = PathBuf::from(failed["raw_recording"]["job_path"].as_str().unwrap());
        assert!(!job.exists());
        assert!(!partial.join("COMPLETE").exists());
        if release_readback {
            fs::write(&release, b"1").unwrap();
        }
        await_failed_exit(&mut child);
        let end = status(&f).unwrap();
        assert_eq!(end["imu"]["error"], "IMU_CALLBACK_FAILED");
        assert_eq!(end["worker_reason"], "SESSION_FAILED");
        if !release_readback {
            assert_eq!(end["reason"], "NATIVE_HEARTBEAT_TIMEOUT");
        }
        assert!(!job.exists());
        assert!(!partial.join("COMPLETE").exists());
        for _ in 0..2 {
            let mut recovered =
                offline_depth::recording::RecordingDirectorySource::open_recovered(&partial)
                    .unwrap();
            let mut count = 0;
            while recovered.next_frame().unwrap().is_some() {
                count += 1;
            }
            assert_eq!(count, 31);
        }
        io_faults::assert_injected(&marker, "read");
    }
}

fn normal_rate_wait_fixture(target: u64) -> Fixture {
    let f = cleanup_fault_fixture();
    let mut value: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    value["raw_recording"]["max_pairs"] = json!(target);
    value["raw_recording"]["max_bytes"] = json!(20_000_000);
    value["imu"]["accel_rate_hz"] = json!(200);
    value["imu"]["gyro_rate_hz"] = json!(200);
    value["startup_discovery_seconds"] = json!(0.2);
    fs::write(&f.config, value.to_string()).unwrap();
    f
}
fn imu_publish_count(f: &Fixture) -> usize {
    fs::read_to_string(&f.events)
        .unwrap_or_default()
        .lines()
        .filter(|line| matches!(*line, "imu-accel-publish" | "imu-gyro-publish"))
        .count()
}
fn assert_pump_progress_while_fenced(f: &Fixture, child: &mut Child, marker: &Path) -> usize {
    await_fault_marker(marker, child);
    let before = imu_publish_count(f);
    let start = Instant::now();
    while imu_publish_count(f) < before + 40 {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "IMU publication stalled behind critical fsync"
        );
        assert!(child.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(5));
    }
    // This is a controlled barrier: no durability acknowledgment can occur
    // until the parent creates the release file, regardless of scheduler speed.
    thread::sleep(Duration::from_millis(800).saturating_sub(start.elapsed()));
    imu_publish_count(f) - before
}
#[test]
fn normal_200hz_imu_keeps_draining_and_publishing_during_critical_draining_fsync() {
    for permanent in [false, true] {
        let f = normal_rate_wait_fixture(31);
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("draining-critical");
        let release = f.root.join("release-critical");
        let release_reader = f.root.join("release-final");
        let mut command = cleanup_fault_command(&f);
        io_faults::inject(&mut command, &library, "fsync", ".tmp", &marker);
        command
            .env("GDP_IO_REQUIRE_CONTENT", "\"state\":\"draining\"")
            .env("GDP_IO_RELEASE", &release)
            .env("GDP_IO_DELAY_MS", "0")
            .env("GDP_IO_HOLD_READ_SUFFIX", "/manifest.json")
            .env("GDP_IO_HOLD_MARKER", f.root.join("final-held"))
            .env("GDP_IO_HOLD_RELEASE", &release_reader);
        let mut child = command.spawn().unwrap();
        let progress = assert_pump_progress_while_fenced(&f, &mut child, &marker);
        let before = status(&f).unwrap();
        assert_ne!(
            before["raw_recording"]["state"], "draining",
            "critical snapshot became visible before fsync release"
        );
        assert!(
            !Path::new(before["raw_recording"]["job_path"].as_str().unwrap())
                .join("COMPLETE")
                .exists()
        );
        if permanent {
            await_failed_exit(&mut child);
            let end = status(&f).unwrap();
            assert_eq!(end["reason"], "NATIVE_HEARTBEAT_TIMEOUT");
            assert_eq!(end["ready"], false);
            let partial = Path::new(end["raw_recording"]["partial_path"].as_str().unwrap());
            assert!(!partial.join("COMPLETE").exists());
            for _ in 0..2 {
                let mut reader =
                    offline_depth::recording::RecordingDirectorySource::open_recovered(partial)
                        .unwrap();
                let mut count = 0;
                while reader.next_frame().unwrap().is_some() {
                    count += 1;
                }
                assert_eq!(count, 31);
            }
        } else {
            fs::write(&release, b"1").unwrap();
            let observed = wait_status(&f, &mut child, |v| {
                v["status_persistence"]["critical_events"]
                    .as_array()
                    .is_some_and(|events| {
                        events.iter().any(|event| {
                            event["target"]["raw_state"] == "draining"
                                && event["pump_calls"].as_u64().unwrap_or(0) > 0
                        })
                    })
            });
            assert!(observed["imu"]["error"].is_null());
            assert!(observed["imu"]["queue_high_water"].as_u64().unwrap() <= 128);
            let event = observed["status_persistence"]["critical_events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["target"]["raw_state"] == "draining")
                .unwrap();
            assert_eq!(event["pump_failed"], false);
            assert!(event["wall_ns"].as_u64().unwrap() >= 800_000_000);
            println!(
                "critical-draining-pump: {}",
                json!({"publish_progress_while_fenced":progress,"critical":event,"max_drain_ns":observed["imu"]["drain_interval"]["maximum_ns"],"imu_queue_peak":observed["imu"]["queue_high_water"]})
            );
            fs::write(&release_reader, b"1").unwrap();
            let complete = wait_status(&f, &mut child, |v| {
                v["raw_recording"]["state"] == "complete"
            });
            assert!(complete["imu"]["error"].is_null());
            assert_eq!(
                complete["raw_recording"]["disk_stages"]["validation_imu_samples"]
                    .as_u64()
                    .unwrap(),
                2 * complete["raw_recording"]["imu_samples_submitted"]
                    .as_u64()
                    .unwrap()
            );
            assert_eq!(
                complete["raw_recording"]["disk_stages"]["validation_image_bytes"]
                    .as_u64()
                    .unwrap(),
                2 * complete["raw_recording"]["payload_bytes_submitted"]
                    .as_u64()
                    .unwrap()
            );
            stop(&mut child);
        }
        io_faults::assert_injected(&marker, "fsync");
    }
}
struct RawOverflowFaultChild<'a> {
    child: &'a mut Child,
    critical_release: PathBuf,
    checkpoint_release: PathBuf,
}
impl std::ops::Deref for RawOverflowFaultChild<'_> {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        self.child
    }
}
impl std::ops::DerefMut for RawOverflowFaultChild<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child
    }
}
impl Drop for RawOverflowFaultChild<'_> {
    fn drop(&mut self) {
        // Assertions below can fail while either real IO barrier is held.
        // Release only this fixture's files before reaping its owned child.
        let _ = fs::write(&self.critical_release, b"1");
        let _ = fs::write(&self.checkpoint_release, b"1");
        let _ = fs::write(self.checkpoint_release.with_extension("first"), b"1");
        // Child caches a reaped exit. Never signal a numeric PID after it has
        // been reaped, nor after an error leaves child identity unconfirmed.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn raw_overflow_fault_assertion_releases_both_barriers_and_reaps_only_its_child() {
    let f = fixture();
    let critical_release = f.root.join("release-critical");
    let checkpoint_release = f.root.join("release-checkpoint");
    let mut process = Command::new("sleep").arg("30").spawn().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = RawOverflowFaultChild {
            child: &mut process,
            critical_release: critical_release.clone(),
            checkpoint_release: checkpoint_release.clone(),
        };
        panic!("controlled assertion while both raw overflow IO barriers are held");
    }));
    assert!(result.is_err());
    assert_eq!(fs::read(&critical_release).unwrap(), b"1");
    assert_eq!(fs::read(&checkpoint_release).unwrap(), b"1");
    assert!(process.try_wait().unwrap().is_some());
    assert!(!process.wait().unwrap().success());
    // Drop after a normal wait uses Child's cached status and remains harmless.
    {
        let _guard = RawOverflowFaultChild {
            child: &mut process,
            critical_release,
            checkpoint_release,
        };
    }
    assert!(process.try_wait().unwrap().is_some());
}

fn raw_overflow_test_monotonic_ns() -> u64 {
    let mut value: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
        0
    );
    (value.tv_sec as u64) * 1_000_000_000 + value.tv_nsec as u64
}

fn assert_raw_overflow_context(value: &Value, held_phase: &str, observed_ns: u64) -> Value {
    let overflow = &value["raw_recording"]["overflow_observation"];
    assert_eq!(overflow["version"], 1, "{overflow}");
    assert_eq!(overflow["reason"], "RAW_QUEUE_OVERFLOW");
    assert_eq!(overflow["clock"], "CLOCK_MONOTONIC");
    let queue = &overflow["queue_at_detection"];
    assert_eq!(queue["capacity_pairs"], 16);
    assert_eq!(queue["queued_pairs"], 16);
    assert_eq!(queue["inflight_pairs"], 1);
    assert_eq!(queue["high_water_pairs"], 16);
    assert_eq!(queue["closed"], false);
    assert_eq!(queue["discarded_pairs"], 0);
    assert!(queue["oldest_queued_age_ns"].as_u64().unwrap() > 0);
    let rejected = &overflow["rejected_pair"];
    // The SDK fixture supplies every sequence. The failed seventeenth queued
    // pair must immediately follow the full queue's last admitted pair.
    for key in ["pair_sequence", "color_sequence", "depth_sequence"] {
        assert_eq!(
            rejected[key].as_u64().unwrap(),
            queue["back_pair"][key].as_u64().unwrap() + 1,
            "{key}: {overflow}"
        );
        assert_eq!(
            queue["front_pair"][key].as_u64().unwrap(),
            queue["inflight_pair"][key].as_u64().unwrap() + 1,
            "{key}: {overflow}"
        );
    }
    assert_eq!(
        queue["back_pair"]["pair_sequence"].as_u64().unwrap()
            - queue["front_pair"]["pair_sequence"].as_u64().unwrap(),
        15
    );
    let detected = overflow["detected_monotonic_ns"].as_u64().unwrap();
    let started = overflow["writer_context_capture_started_monotonic_ns"]
        .as_u64()
        .unwrap();
    let finished = overflow["writer_context_capture_finished_monotonic_ns"]
        .as_u64()
        .unwrap();
    let context = &overflow["writer_context"];
    let context_started = context["capture_started_monotonic_ns"].as_u64().unwrap();
    let context_finished = context["capture_finished_monotonic_ns"].as_u64().unwrap();
    assert!(detected > 0);
    assert!(detected <= started && started <= context_started);
    assert!(context_started <= context_finished && context_finished <= finished);
    assert!(finished <= observed_ns);
    assert_eq!(context["observation_enabled"], true);
    assert_eq!(context["clock"], "CLOCK_MONOTONIC");
    assert_eq!(context["active_append_capacity"], 1);
    assert_eq!(context["active_checkpoint_capacity"], 4);
    assert_eq!(context["active_append_limit_hits"], 0);
    assert_eq!(context["active_checkpoint_limit_hits"], 0);
    let append = &context["active_append"];
    assert!(append["append_id"].as_u64().unwrap() > 0);
    assert_eq!(append["pid"], value["pid"]);
    assert!(append["tid"].as_u64().unwrap() > 0);
    assert_eq!(
        append["pair_sequence"],
        queue["inflight_pair"]["pair_sequence"]
    );
    assert!(append["start_monotonic_ns"].as_u64().unwrap() <= detected);
    let spans = context["active_checkpoint_spans"].as_array().unwrap();
    assert!(spans.len() <= 4);
    let held = spans
        .iter()
        .find(|span| span["phase"] == held_phase)
        .unwrap();
    let waiting = spans
        .iter()
        .find(|span| {
            span["phase"] == "verify_wait"
                && span["info"]["checkpoint_id"] == held["info"]["checkpoint_id"]
        })
        .unwrap();
    assert!(held["start_monotonic_ns"].as_u64().unwrap() <= detected);
    assert!(waiting["start_monotonic_ns"].as_u64().unwrap() <= detected);
    assert_eq!(waiting["tid"], append["tid"]);
    assert_ne!(held["tid"], append["tid"]);
    assert_eq!(held["pid"], value["pid"]);
    assert_eq!(held["info"]["asynchronous"], true);
    assert!(
        append["pair_sequence"].as_u64().unwrap() > held["info"]["prefix_pairs"].as_u64().unwrap()
    );
    // A busy checkpoint no longer makes the half-credit chunk target block.
    // Tiny pairs consume the full 32-pair budget before the next admission waits.
    assert_eq!(waiting["wait_reason"], "global_pair_credit");
    let credits = &waiting["credit_at_wait"];
    assert_eq!(credits["pair_limit"], 32);
    assert_eq!(credits["payload_limit_bytes"], 16 * 1024 * 1024);
    assert_eq!(
        credits["pending_pairs"].as_u64().unwrap() + credits["active_pairs"].as_u64().unwrap(),
        32
    );
    assert_eq!(
        value["raw_recording"]["disk_stages"]["uncommitted_pairs_peak"],
        32
    );
    assert!(
        append["pair_sequence"].as_u64().unwrap() - held["info"]["prefix_pairs"].as_u64().unwrap()
            <= 32
    );
    overflow.clone()
}

#[test]
fn raw_overflow_critical_fsync_does_not_create_a_secondary_imu_overflow() {
    assert_raw_overflow_context_frozen_during_critical_fsync("readback");
}

#[test]
fn raw_overflow_payload_sync_freezes_queue_and_checkpoint_wait_before_durable_failure() {
    assert_raw_overflow_context_frozen_during_critical_fsync("payload_sync");
}

fn assert_raw_overflow_context_frozen_during_critical_fsync(held_phase: &str) {
    run_raw_overflow_fault(held_phase, None);
}

#[test]
fn three_writers_consecutive_payload_fences_keep_local_credits_and_recover_prefixes() {
    three_writer_fences("payload_sync");
}
#[test]
fn three_writers_consecutive_index_sync_tails_keep_queue_history_and_recover_prefixes() {
    three_writer_fences("index_sync");
}
fn three_writer_fences(phase: &str) {
    // Retain three concurrent writers, avoid running two six-worker LLVM
    // stress groups against each other's test-only parent deadlines.
    static GROUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _group = GROUP.lock().unwrap_or_else(|e| e.into_inner());
    let ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    thread::scope(|scope| {
        let children: Vec<_> = (0..3)
            .map(|_| {
                let ready = ready.clone();
                scope.spawn(move || run_raw_overflow_fault(phase, Some(ready)))
            })
            .collect();
        for child in children {
            child.join().unwrap();
        }
    });
}

fn run_raw_overflow_fault(
    held_phase: &str,
    group: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
) {
    let f = normal_rate_wait_fixture(1000);
    // This fixture isolates checkpoint/status fencing, not IMU throughput.
    // Keep callbacks active during the fence without making LLVM-instrumented
    // shared-runner scheduling an unrelated 200 Hz queue-overflow stimulus.
    // The independent normal-rate wait tests retain both 200 Hz streams.
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["imu"]["accel_rate_hz"] = json!(50);
    config["imu"]["gyro_rate_hz"] = json!(50);
    fs::write(&f.config, config.to_string()).unwrap();
    if group.is_some() {
        // Bound production during the first fence by frame progress, so a
        // delayed parent cannot accidentally overflow before releasing it.
        // Returning an SDK timeout keeps heartbeat/IMU pumping live.
        let source = f.root.join("mock-sdk.c");
        let mut sdk = fs::read_to_string(&source).unwrap();
        replace(
            &mut sdk,
            "obj *ob_pipeline_wait_for_frameset(obj *p,uint32_t ms,error **e){",
            r#"obj *ob_pipeline_wait_for_frameset(obj *p,uint32_t ms,error **e){
        static unsigned after_first_fence;
        const char *first_marker=getenv("GDP_IO_FIRST_MARKER"),*first_release=getenv("GDP_IO_FIRST_RELEASE");
        if(first_marker && first_release && access(first_marker,F_OK)==0 && access(first_release,F_OK)!=0
           && ++after_first_fence>24){usleep(1000);return NULL;}
        "#,
        );
        fs::write(&source, sdk).unwrap();
        compile("cc", &source, &f.root.join("mock-sdk.so"), None);
    }
    let library = io_faults::compile(&f.root);
    // Scope the deliberate readback stall to an already healthy recording.
    // GDP_IO_ARMED governs fail(), not the independent read() hold hook.
    let hook_source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/io_faults.c");
    let mut hook = fs::read_to_string(hook_source).unwrap();
    assert_eq!(hook.matches("if(hold) {").count(), 1);
    replace(
        &mut hook,
        "if(hold) {",
        "const char *hold_armed=getenv(\"GDP_IO_HOLD_ARMED\");if(hold && hold_armed && access(hold_armed,F_OK)==0) {",
    );
    if matches!(held_phase, "payload_sync" | "index_sync") {
        // A second, independent barrier holds the real payload fsync. The
        // existing fail_fd hook still gates only the raw-failed status fsync.
        replace(
            &mut hook,
            "int fsync(int fd) {",
            r#"int fsync(int fd) {
    const char *armed=getenv("GDP_IO_HOLD_ARMED");
    if(armed && access(armed,F_OK)==0 && payload_fd(fd)) {
        const char *marker=getenv("GDP_IO_HOLD_MARKER"),*release=getenv("GDP_IO_HOLD_RELEASE");
        if(marker && release) {
            int out=open(marker,O_WRONLY|O_CREAT|O_EXCL,0600);
            if(out>=0){ssize_t (*real_write)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real_write(out,"fsync",5);close(out);}
            unsigned waited=0;while(access(release,F_OK)!=0&&waited++<10000)usleep(1000);
        }
    }"#,
        );
    }
    if group.is_some() {
        // Two real fsync barriers in successive payload transactions. The first
        // is released by queue progress, never by a timing guess; the second
        // remains fenced until the bounded FIFO reports overflow.
        replace(
            &mut hook,
            "if(armed && access(armed,F_OK)==0 && payload_fd(fd)) {",
            r#"if(armed && access(armed,F_OK)==0 && payload_fd(fd)) {
        static _Atomic unsigned payload_fences;
        if(atomic_fetch_add(&payload_fences,1)==0) {
            const char *marker=getenv("GDP_IO_FIRST_MARKER"),*release=getenv("GDP_IO_FIRST_RELEASE");
            if(marker && release) {
                int out=open(marker,O_WRONLY|O_CREAT|O_EXCL,0600);if(out>=0)close(out);
                unsigned waited=0;while(access(release,F_OK)!=0&&waited++<10000)usleep(1000);
            }
            int (*first_sync)(int)=dlsym(RTLD_NEXT,"fsync");return first_sync(fd);
        }"#,
        );
    }
    if held_phase == "index_sync" {
        replace(
            &mut hook,
            "int fsync(int fd) {",
            r#"static int index_sync_fd(int fd) {
            char link[64],path[4096];snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
            ssize_t n=readlink(link,path,sizeof(path)-1);if(n<0)return 0;path[n]=0;
            return strstr(path,"/records.jsonl")!=NULL;
        }
        int fsync(int fd) {"#,
        );
        replace(
            &mut hook,
            "access(armed,F_OK)==0 && payload_fd(fd)",
            "access(armed,F_OK)==0 && index_sync_fd(fd)",
        );
        replace(
            &mut hook,
            "const char *marker=getenv(\"GDP_IO_HOLD_MARKER\"),*release=getenv(\"GDP_IO_HOLD_RELEASE\");",
            "usleep(400000); const char *marker=getenv(\"GDP_IO_HOLD_MARKER\"),*release=getenv(\"GDP_IO_HOLD_RELEASE\");",
        );
    }
    let local_hook = f.root.join("armed-checkpoint-hook.c");
    fs::write(&local_hook, hook).unwrap();
    let compiled = Command::new("cc")
        .args([
            "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(&local_hook)
        .args(["-ldl", "-o"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let marker = f.root.join("raw-failed-critical");
    let release = f.root.join("release-critical");
    let release_reader = f.root.join("release-checkpoint");
    let arm_reader = f.root.join("arm-checkpoint");
    let checkpoint_held = f.root.join("checkpoint-held");
    let mut command = cleanup_fault_command(&f);
    io_faults::inject(&mut command, &library, "fsync", ".tmp", &marker);
    command
        .env(
            "GDP_IO_REQUIRE_CONTENT",
            "\"reason\":\"RAW_QUEUE_OVERFLOW\"",
        )
        .env("GDP_IO_RELEASE", &release)
        .env("GDP_IO_DELAY_MS", "0")
        .env("GDP_IO_HOLD_MARKER", &checkpoint_held)
        .env("GDP_IO_HOLD_RELEASE", &release_reader)
        .env("GDP_IO_HOLD_ARMED", &arm_reader);
    if group.is_some() {
        command
            .env("GDP_IO_FIRST_MARKER", f.root.join("first-payload-held"))
            .env(
                "GDP_IO_FIRST_RELEASE",
                release_reader.with_extension("first"),
            );
    }
    if held_phase == "readback" {
        command.env("GDP_IO_HOLD_READ_SUFFIX", ".bin");
    }
    let mut process = command.spawn().unwrap();
    let mut child = RawOverflowFaultChild {
        child: &mut process,
        critical_release: release.clone(),
        checkpoint_release: release_reader.clone(),
    };
    let wait_phase = |child: &mut Child, phase: &str, done: &dyn Fn(&Option<Value>) -> bool| {
        let started = Instant::now();
        loop {
            let latest = status(&f);
            if done(&latest) {
                break latest;
            }
            let exit = child.try_wait().unwrap();
            if exit.is_some() || started.elapsed() >= Duration::from_secs(8) {
                let failures: Vec<_> = fs::read_dir(&f.root)
                    .unwrap()
                    .filter_map(|entry| {
                        let path = entry.unwrap().path();
                        if path.file_name().unwrap().to_str().unwrap().starts_with("failure-") {
                            Some(json!({"path":path,"private_failure":fs::read_to_string(&path).ok()}))
                        } else {
                            None
                        }
                    })
                    .collect();
                // Release only this test's barriers before terminating a still
                // live child; never leave a gated worker after a failed assert.
                fs::write(&release, b"1").unwrap();
                fs::write(&release_reader, b"1").unwrap();
                fs::write(release_reader.with_extension("first"), b"1").unwrap();
                if exit.is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let latest = latest.map(|mut v| {
                    if let Some(events) =
                        v["raw_recording"]["writer_queue"]["history"]["events"].as_array_mut()
                    {
                        if events.len() > 4 {
                            events.drain(..events.len() - 4);
                        }
                    }
                    v
                });
                panic!(
                    "{phase}: exit={exit:?}; evidence={}",
                    json!({"status":latest,"private_failures":failures})
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    let healthy = wait_phase(
        &mut child,
        "healthy recording before checkpoint fault",
        &|value| {
            value.as_ref().is_some_and(|v| {
                v["ready"] == true
                    && v["raw_recording"]["state"] == "recording"
                    && v["imu"]["ready"] == true
                    && v["imu"]["error"].is_null()
                    && v["raw_recording"]["disk_stages"]["checkpoints"]
                        .as_u64()
                        .unwrap_or(0)
                        > 0
            })
        },
    )
    .unwrap();
    assert_eq!(healthy["raw_recording"]["whole_pair_queue_overflows"], 0);
    assert!(
        !checkpoint_held.exists(),
        "readback hook ran before its arm gate"
    );
    if let Some(ready) = group.as_ref() {
        ready.fetch_add(1, Ordering::SeqCst);
        let began = Instant::now();
        while ready.load(Ordering::SeqCst) < 3 {
            assert!(
                began.elapsed() < Duration::from_secs(8),
                "three-writer readiness barrier"
            );
            assert!(child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(5));
        }
    }
    fs::write(&arm_reader, b"1").unwrap();
    if group.is_some() {
        let first = wait_phase(&mut child, "first consecutive payload fence", &|v| {
            f.root.join("first-payload-held").exists()
                && v.as_ref().is_some_and(|v| {
                    v["raw_recording"]["writer_queue"]["queued_pairs"]
                        .as_u64()
                        .unwrap_or(0)
                        >= 4
                })
        })
        .unwrap();
        assert_eq!(first["raw_recording"]["whole_pair_queue_overflows"], 0);
        fs::write(release_reader.with_extension("first"), b"1").unwrap();
    }
    let _ = wait_phase(&mut child, "armed checkpoint IO", &|_| {
        checkpoint_held.exists()
    });
    let _ = wait_phase(&mut child, "RAW_QUEUE_OVERFLOW critical fsync", &|_| {
        marker.exists()
    });
    let progress = assert_pump_progress_while_fenced(&f, &mut child, &marker);
    assert_ne!(
        status(&f).unwrap()["raw_recording"]["reason"],
        "RAW_QUEUE_OVERFLOW",
        "failed snapshot acknowledged before critical fsync release"
    );
    // The atomic queue failure has already frozen its evidence before the
    // critical snapshot can be acknowledged. Inspect only this fixture's
    // pending status file while its real fsync is gated.
    let pending = fs::read_dir(&f.root)
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("tmp") {
                return None;
            }
            let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).ok()?;
            (value["raw_recording"]["reason"] == "RAW_QUEUE_OVERFLOW").then_some(value)
        })
        .expect("critical status tempfile must retain the first overflow before fsync release");
    let frozen =
        assert_raw_overflow_context(&pending, held_phase, raw_overflow_test_monotonic_ns());
    let history = &pending["raw_recording"]["writer_queue"]["history"];
    assert_eq!(history["enabled"], true);
    let events = history["events"].as_array().unwrap();
    assert!(events.len() <= 512);
    assert!(events.iter().any(|e| e[2] == "overflow" && e[4] == 16));
    assert!(
        events
            .windows(2)
            .all(|e| e[0][0].as_u64().unwrap() + 1 == e[1][0].as_u64().unwrap())
    );

    assert!(
        !Path::new(pending["raw_recording"]["partial_path"].as_str().unwrap())
            .join("COMPLETE")
            .exists()
    );
    fs::write(&release, b"1").unwrap();
    let observed = wait_status(&f, &mut child, |v| {
        v["status_persistence"]["critical_events"]
            .as_array()
            .is_some_and(|events| {
                events.iter().any(|event| {
                    event["target"]["raw_state"] == "failed"
                        && event["pump_calls"].as_u64().unwrap_or(0) > 0
                })
            })
    });
    assert_eq!(observed["raw_recording"]["reason"], "RAW_QUEUE_OVERFLOW");
    assert_eq!(
        assert_raw_overflow_context(&observed, held_phase, raw_overflow_test_monotonic_ns()),
        frozen,
        "durable failure changed the first overflow evidence"
    );
    assert!(observed["imu"]["error"].is_null());
    assert!(observed["imu"]["queue_high_water"].as_u64().unwrap() <= 128);
    let event = observed["status_persistence"]["critical_events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["target"]["raw_state"] == "failed")
        .unwrap();
    assert!(event["wall_ns"].as_u64().unwrap() >= 800_000_000);
    assert_eq!(event["pump_failed"], false);
    println!(
        "critical-raw-failed-pump: {}",
        json!({"held_phase":held_phase,"overflow":frozen,"publish_progress_while_fenced":progress,"critical":event,"max_drain_ns":observed["imu"]["drain_interval"]["maximum_ns"],"imu_queue_peak":observed["imu"]["queue_high_water"]})
    );
    fs::write(&release_reader, b"1").unwrap();
    // Do not race the test's own blocked readback/Drop work against the
    // unchanged 300ms SDK shutdown deadline. The worker queue guard abandons
    // its credits only after recorder Drop and checkpoint joins have returned.
    let drained = wait_phase(
        &mut child,
        "released checkpoint writer quiescent",
        &|value| {
            value.as_ref().is_some_and(|v| {
                v["raw_recording"]["reason"] == "RAW_QUEUE_OVERFLOW"
                    && v["imu"]["ready"] == true
                    && v["imu"]["error"].is_null()
                    && v["raw_recording"]["writer_queue"]["closed"] == true
                    && v["raw_recording"]["writer_queue"]["queued_pairs"] == 0
                    && v["raw_recording"]["writer_queue"]["inflight_pairs"] == 0
                    && v["raw_recording"]["disk_stages"]["checkpoint_observation"]["active"]
                        .as_array()
                        .is_some_and(|active| active.is_empty())
            })
        },
    )
    .unwrap();
    assert_eq!(drained["raw_recording"]["overflow_observation"], frozen);
    stop(&mut child);
    let end = status(&f).unwrap();
    assert_eq!(end["raw_recording"]["overflow_observation"], frozen);
    assert_eq!(end["raw_recording"]["reason"], "RAW_QUEUE_OVERFLOW");
    assert!(end["imu"]["error"].is_null());
    let partial = Path::new(end["raw_recording"]["partial_path"].as_str().unwrap());
    assert!(!Path::new(end["raw_recording"]["job_path"].as_str().unwrap()).exists());
    assert!(!partial.join("COMPLETE").exists());
    let checkpoint_bytes = fs::read(partial.join("checkpoint.json")).unwrap();
    let checkpoint: Value = serde_json::from_slice(&checkpoint_bytes).unwrap();
    assert!(checkpoint["record_count"].as_u64().unwrap() > 0);
    for _ in 0..2 {
        let mut reader =
            offline_depth::recording::RecordingDirectorySource::open_recovered(partial).unwrap();
        let mut count = 0;
        while reader.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, checkpoint["record_count"].as_u64().unwrap());
    }
    assert_eq!(
        fs::read(partial.join("checkpoint.json")).unwrap(),
        checkpoint_bytes
    );
    assert!(!partial.join("COMPLETE").exists());
    assert_eq!(
        status(&f).unwrap()["raw_recording"]["overflow_observation"],
        frozen
    );
    io_faults::assert_injected(&marker, "fsync");
}
#[test]
fn invalid_imu_timestamps_during_a_critical_fence_remain_terminal() {
    let f = normal_rate_wait_fixture(31);
    let source = f.root.join("mock-sdk.c");
    let mut text = fs::read_to_string(&source).unwrap();
    replace(
        &mut text,
        "getenv(\"GDP_MOCK_IMU_REGRESS\")&&sequence>2",
        "getenv(\"GDP_MOCK_IMU_REGRESS\")&&sequence>2&&fault_active()",
    );
    fs::write(&source, text).unwrap();
    compile("cc", &source, &f.root.join("mock-sdk.so"), None);
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("critical");
    let release = f.root.join("release");
    let release_reader = f.root.join("release-reader");
    let mut command = cleanup_fault_command(&f);
    io_faults::inject(&mut command, &library, "fsync", ".tmp", &marker);
    command
        .env("GDP_IO_REQUIRE_CONTENT", "\"state\":\"draining\"")
        .env("GDP_IO_RELEASE", &release)
        .env("GDP_IO_DELAY_MS", "0")
        .env("GDP_IO_HOLD_READ_SUFFIX", "/manifest.json")
        .env("GDP_IO_HOLD_MARKER", f.root.join("reader-held"))
        .env("GDP_IO_HOLD_RELEASE", &release_reader)
        .env("GDP_MOCK_FAULT_ARMED", &marker)
        .env("GDP_MOCK_IMU_REGRESS", "1");
    let diagnostic = f.root.join("critical-worker.stderr");
    command.stderr(fs::File::create(&diagnostic).unwrap());
    let mut child = command.spawn().unwrap();
    let injection_start = Instant::now();
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker exited before injection: status={:?}; stderr={:?}",
            fs::read_to_string(&f.status),
            fs::read_to_string(&diagnostic)
        );
        assert!(injection_start.elapsed() < Duration::from_secs(8));
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(800));
    fs::write(&release, b"1").unwrap();
    let failed = wait_status(&f, &mut child, |v| {
        v["imu"]["error"] == "IMU_INVALID_OR_REGRESSING" && v["ready"] == false
    });
    assert_eq!(failed["reason"], "SESSION_FAILED");
    assert_eq!(failed["status_persistence"]["submission_errors"], 0);
    assert_eq!(failed["status_persistence"]["errors"], 0);
    assert!(
        failed["status_persistence"]["pump_errors"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(failed["status_persistence"]["critical_events"].as_array().unwrap().iter().any(|event|event["target"]["raw_state"]=="draining" && event["pump_failed"]==true));
    fs::write(&release_reader, b"1").unwrap();
    await_failed_exit(&mut child);
    let partial = Path::new(failed["raw_recording"]["partial_path"].as_str().unwrap());
    assert!(!Path::new(failed["raw_recording"]["job_path"].as_str().unwrap()).exists());
    for _ in 0..2 {
        let mut reader =
            offline_depth::recording::RecordingDirectorySource::open_recovered(partial).unwrap();
        let mut count = 0;
        while reader.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 31);
    }
}

#[test]
fn recording_critical_fence_retains_the_real_pending_imu_limit() {
    let f = normal_rate_wait_fixture(1000);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    config["startup_discovery_seconds"] = json!(0);
    fs::write(&f.config, config.to_string()).unwrap();
    let marker = f.root.join("ready-critical");
    let release = f.root.join("release-critical");
    // Keep both SDK sources running at their normal 200Hz throughout startup.
    // Pausing them while waiting for readiness would itself create a stale
    // stream under a slow runner, rather than exercise the recorder's bound.
    let library = io_faults::compile(&f.root);
    let mut command = cleanup_fault_command(&f);
    io_faults::inject(&mut command, &library, "fsync", ".tmp", &marker);
    command
        .env("GDP_IO_REQUIRE_CONTENT", "\"state\":\"ready\"")
        .env("GDP_IO_RELEASE", &release)
        .env("GDP_IO_DELAY_MS", "0");
    let mut child = command.spawn().unwrap();
    await_fault_marker(&marker, &mut child);
    let held_since = Instant::now();
    let held: Value = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().and_then(|v| v.to_str()) == Some("tmp"))
                .then(|| fs::read(path).ok())
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        })
        .find(|value| value["kind"] == "capture-service-status" && value["ready"] == true)
        .expect("ready status must remain in its pre-fsync temporary file");
    assert_eq!(held["raw_recording"]["state"], "recording");
    assert_eq!(held["raw_recording"]["imu_samples_pending"], 0);
    let submitted = held["raw_recording"]["imu_samples_submitted"]
        .as_u64()
        .unwrap();
    let published: u64 = held["imu"]["published"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    let excluded_before = held["raw_recording"]["imu_samples_outside_recording_excluded"]
        .as_u64()
        .unwrap();
    assert_eq!(published, submitted + excluded_before);
    assert_eq!(held["raw_recording"]["queue_capacity"], 16);
    assert_ne!(status(&f).unwrap()["ready"], true);
    let start = Instant::now();
    // A real batch must attempt sample 129 while no video poll can consume the
    // recorder's pending list. The batch is rejected whole, so its retained
    // pending length may be below 128; no sample is discarded to make it fit.
    while imu_publish_count(&f) as u64 <= published + 128 {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "normal SDK source did not reach the pending bound; published={}, submitted={submitted}, status={:?}",
            imu_publish_count(&f),
            status(&f)
        );
        assert!(child.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(800).saturating_sub(held_since.elapsed()));
    assert_ne!(status(&f).unwrap()["ready"], true);
    fs::write(&release, b"1").unwrap();
    await_failed_exit(&mut child);
    let end = status(&f).unwrap();
    assert_eq!(end["ready"], false);
    assert_eq!(end["worker_reason"], "SESSION_FAILED");
    assert_eq!(end["raw_recording"]["state"], "failed");
    assert_eq!(end["raw_recording"]["reason"], "RAW_IMU_QUEUE_OVERFLOW");
    let pending = end["raw_recording"]["imu_samples_pending"]
        .as_u64()
        .unwrap();
    assert!(pending > 0 && pending <= 128);
    let published: u64 = end["imu"]["published"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .sum();
    let excluded = end["raw_recording"]["imu_samples_outside_recording_excluded"]
        .as_u64()
        .unwrap();
    assert!(
        published > submitted + excluded + 128,
        "a complete real batch must exceed the unchanged pending bound"
    );
    assert!(
        end["imu"]["error"].is_null() || end["imu"]["error"] == "IMU_QUEUE_OVERFLOW",
        "{end}"
    );
    assert_eq!(end["raw_recording"]["submitted_pairs"], 2);
    assert_eq!(end["raw_recording"]["imu_samples_submitted"], submitted);
    assert_eq!(end["status_persistence"]["errors"], 0);
    assert_eq!(end["status_persistence"]["submission_errors"], 0);
    let event = end["status_persistence"]["critical_events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["target"]["raw_state"] == "recording" && event["pump_failed"] == true)
        .expect("the recording critical fence must report its real pump failure");
    assert_eq!(event["ok"], true);
    assert!(event["wall_ns"].as_u64().unwrap() >= 800_000_000);
    assert!(event["pump_calls"].as_u64().unwrap() > 1);
    let partial = Path::new(end["raw_recording"]["partial_path"].as_str().unwrap());
    assert!(!Path::new(end["raw_recording"]["job_path"].as_str().unwrap()).exists());
    assert!(!partial.join("COMPLETE").exists());
    let checkpoint: Value =
        serde_json::from_slice(&fs::read(partial.join("checkpoint.json")).unwrap()).unwrap();
    for _ in 0..2 {
        let mut reader =
            offline_depth::recording::RecordingDirectorySource::open_recovered(partial).unwrap();
        let mut count = 0;
        while reader.next_frame().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, checkpoint["record_count"].as_u64().unwrap());
    }
    println!(
        "critical-recording-pending-limit: {}",
        json!({"critical":event,"raw_reason":end["raw_recording"]["reason"],"pending_imu":end["raw_recording"]["imu_samples_pending"],"imu_error":end["imu"]["error"]})
    );
    io_faults::assert_injected(&marker, "fsync");
}

fn gate_skew_at_armed_file(f: &Fixture) {
    let source_path = f.root.join("mock-sdk.c");
    let mut source = fs::read_to_string(&source_path).unwrap();
    replace(
        &mut source,
        "getenv(\"GDP_MOCK_SKEW\")&&p->stream==3",
        "getenv(\"GDP_MOCK_SKEW\")&&fault_active()&&p->stream==3",
    );
    fs::write(&source_path, source).unwrap();
    compile("cc", &source_path, &f.root.join("mock-sdk.so"), None);
}
fn quality_persistence_fault_fixture() -> Fixture {
    let f = normal_rate_wait_fixture(1000);
    let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
    // Filesystem contract tests isolate the injected fault from runner startup
    // stalls. The dedicated 200Hz pump/128-limit tests remain unchanged above.
    config["imu"]["accel_rate_hz"] = json!(25);
    config["imu"]["gyro_rate_hz"] = json!(25);
    fs::write(&f.config, config.to_string()).unwrap();
    f
}
#[test]
fn quality_directory_bootstrap_is_durable_before_any_sdk_access() {
    for succeeds in [false, true] {
        let f = quality_persistence_fault_fixture();
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("quality-bootstrap");
        let release = f.root.join("release-bootstrap");
        let mut command = cleanup_fault_command(&f);
        io_faults::inject(&mut command, &library, "fsync", "/quality", &marker);
        command.env("GDP_IO_FAIL_FD_TYPE", "directory");
        if succeeds {
            command
                .env("GDP_IO_RELEASE", &release)
                .env("GDP_IO_DELAY_MS", "0");
        }
        let mut child = command.spawn().unwrap();
        await_fault_marker(&marker, &mut child);
        assert!(
            !f.events.exists(),
            "no SDK context/callback may open before the intermediate directory barrier"
        );
        let quality = f.root.join("quality");
        assert!(
            fs::read_dir(&quality).unwrap().all(|entry| !entry
                .unwrap()
                .path()
                .join("events.json")
                .exists())
        );
        if succeeds {
            fs::write(&release, b"1").unwrap();
            wait_status(&f, &mut child, |v| v["ready"] == true);
            stop(&mut child);
        } else {
            await_failed_exit(&mut child);
            assert_eq!(status(&f).unwrap()["ready"], false);
            assert!(!f.events.exists());
        }
        io_faults::assert_injected(&marker, "fsync");
    }
}
#[test]
fn quality_file_rename_and_group_directory_failures_never_acknowledge_success() {
    for (operation, suffix, fd_type) in [
        ("fsync", ".tmp", "file"),
        ("rename", "/events.json", "file"),
        ("fsync", "", "directory"),
    ] {
        let f = quality_persistence_fault_fixture();
        gate_skew_at_armed_file(&f);
        let library = io_faults::compile(&f.root);
        let marker = f.root.join("quality-failure");
        let armed = f.root.join("armed");
        let counts = f.root.join("sync-counts");
        let mut command = cleanup_fault_command(&f);
        io_faults::inject(&mut command, &library, operation, suffix, &marker);
        command
            .env("GDP_IO_FAIL_CONTAINS", "/quality/")
            .env("GDP_IO_FAIL_FD_TYPE", fd_type)
            .env("GDP_IO_ARMED", &armed)
            .env("GDP_IO_SYNC_COUNTS", &counts)
            .env("GDP_MOCK_FAULT_ARMED", &armed)
            .env("GDP_MOCK_SKEW", "1");
        let mut child = command.spawn().unwrap();
        let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
        fs::write(&armed, b"1").unwrap();
        await_fault_marker(&marker, &mut child);
        await_failed_exit(&mut child);
        let end = status(&f).unwrap();
        assert_eq!(end["ready"], false);
        assert_eq!(end["restart_attempt"], 0);
        assert!(!Path::new(ready["raw_recording"]["job_path"].as_str().unwrap()).exists());
        assert!(
            !Path::new(ready["raw_recording"]["partial_path"].as_str().unwrap())
                .join("COMPLETE")
                .exists()
        );
        let counter: Value = serde_json::from_slice(
            &fs::read(format!(
                "{}.{}.json",
                counts.display(),
                ready["pid"].as_u64().unwrap()
            ))
            .unwrap(),
        )
        .unwrap();
        if operation == "fsync" {
            let role = if fd_type == "directory" {
                "quality_directory"
            } else {
                "quality_file"
            };
            assert_eq!(counter["roles"][role]["injected_error"], 1);
        }
        io_faults::assert_injected(&marker, operation);
    }
}

#[test]
fn critical_quality_directory_fence_precedes_status_and_keeps_imu_pumping() {
    let f = quality_persistence_fault_fixture();
    gate_skew_at_armed_file(&f);
    let library = io_faults::compile(&f.root);
    let marker = f.root.join("quality-directory-held");
    let release = f.root.join("release-quality");
    let armed = f.root.join("armed");
    let mut command = cleanup_fault_command(&f);
    io_faults::inject(&mut command, &library, "fsync", "", &marker);
    command
        .env("GDP_IO_FAIL_CONTAINS", "/quality/")
        .env("GDP_IO_FAIL_FD_TYPE", "directory")
        .env("GDP_IO_ARMED", &armed)
        .env("GDP_IO_RELEASE", &release)
        .env("GDP_IO_DELAY_MS", "0")
        .env("GDP_MOCK_FAULT_ARMED", &armed)
        .env("GDP_MOCK_SKEW", "1");
    let mut child = command.spawn().unwrap();
    let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
    fs::write(&armed, b"1").unwrap();
    await_fault_marker(&marker, &mut child);
    let before = status(&f).unwrap()["heartbeat_sequence"].clone();
    let published = imu_publish_count(&f);
    let start = Instant::now();
    while imu_publish_count(&f) < published + 40 {
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(child.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(status(&f).unwrap()["heartbeat_sequence"], before);
    assert!(!release.exists());
    fs::write(&release, b"1").unwrap();
    let changed = wait_status(&f, &mut child, |v| {
        v["quality"]["latest_event_sequence"].as_u64().unwrap_or(0)
            > ready["quality"]["latest_event_sequence"].as_u64().unwrap()
    });
    assert!(
        changed["quality"]["latest_event_sequence"]
            .as_u64()
            .unwrap()
            > ready["quality"]["latest_event_sequence"].as_u64().unwrap()
    );
    let current: Value = serde_json::from_slice(
        &fs::read(changed["quality"]["current_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(current["quality"]["state"], "degraded");
    assert!(changed["imu"]["error"].is_null());
    stop(&mut child);
    io_faults::assert_injected(&marker, "fsync");
}

#[test]
#[ignore = "release paired quality persistence experiment; synthetic ABI, no camera/DDS claim"]
fn quality_persistence_ab_benchmark() {
    let baseline =
        std::env::var_os("GDP_QUALITY_BASELINE_SERVICE").expect("baseline binary required");
    let output = std::env::var_os("GDP_QUALITY_BENCH_OUTPUT").expect("fresh output path required");
    let mut rows = Vec::new();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    for repeat in 0..3 {
        for candidate in if repeat % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let f = large_imu_benchmark_fixture();
            let source_path = f.root.join("mock-sdk.c");
            let mut source = fs::read_to_string(&source_path).unwrap();
            replace(&mut source, "bytes[640*400*2]", "bytes[640*480*2]");
            replace(
                &mut source,
                "w!=640||h!=400||fps!=30",
                "w!=640||h!=(s==2?480:400)||fps!=30",
            );
            replace(
                &mut source,
                "ob_video_frame_get_height(const obj *p,error **e){(void)p;(void)e;return 400;}",
                "ob_video_frame_get_height(const obj *p,error **e){(void)e;return p->stream==2?480:400;}",
            );
            replace(
                &mut source,
                "ob_video_stream_profile_get_height(const profile *p,error **e){(void)p;(void)e;return 400;}",
                "ob_video_stream_profile_get_height(const profile *p,error **e){(void)e;return p->stream==2?480:400;}",
            );
            replace(
                &mut source,
                "(intrinsic){1,2,.5f,.5f,640,400}",
                "(intrinsic){1,2,.5f,.5f,640,p->stream==2?480:400}",
            );
            replace(
                &mut source,
                "uint32_t ob_frame_get_data_size(const obj *p,error **e){(void)p;(void)e;",
                "uint32_t ob_frame_get_data_size(const obj *p,error **e){(void)e;if(p->stream==3)return 640*400*2;",
            );
            replace(
                &mut source,
                "getenv(\"GDP_MOCK_ZERO_PIXELS\")?zero:bytes",
                "getenv(\"GDP_MOCK_ZERO_PIXELS\")&&fault_active()?zero:bytes",
            );
            fs::write(&source_path, source).unwrap();
            compile("cc", &source_path, &f.root.join("mock-sdk.so"), None);
            configure_runtime_quality(&f, 90, 200 * 1024 * 1024);
            enable_imu(&f);
            let mut config: Value = serde_json::from_slice(&fs::read(&f.config).unwrap()).unwrap();
            config["color"]["height"] = json!(480);
            config["quality_policy"]["max_zero_fraction"] = json!(0.5);
            config["quality_runtime"]["quality"]["max_zero_fraction"] = json!(0.5);
            config["imu"]["accel_rate_hz"] = json!(200);
            config["imu"]["gyro_rate_hz"] = json!(200);
            config["service"]["heartbeat_ms"] = json!(500);
            config["service"]["heartbeat_timeout_ms"] = json!(10000);
            config["service"]["shutdown_timeout_ms"] = json!(5000);
            config["raw_recording"]["checkpoint_pipeline"] = json!(true);
            config["raw_recording"]["checkpoint_observation"] = json!(true);
            config["raw_recording"]["queue_capacity"] = json!(16);
            fs::write(&f.config, config.to_string()).unwrap();
            let library = io_faults::compile(&f.root);
            let counter = f.root.join("sync-counts");
            let armed = f.root.join("zero-pixels-armed");
            let mut command = Command::new(if candidate {
                std::ffi::OsStr::new(env!("CARGO_BIN_EXE_capture-service"))
            } else {
                baseline.as_os_str()
            });
            command
                .args(["--config", f.config.to_str().unwrap()])
                .env("GEMINIDEPTHPORT_MOCK_EVENTS", &f.events)
                .env("GDP_ROS_MOCK_EVENTS", &f.events)
                .env("GDP_MOCK_WAIT_DELAY_MS", "33")
                .env("GDP_MOCK_ZERO_PIXELS", "1")
                .env("GDP_MOCK_FAULT_ARMED", &armed)
                .env("LD_PRELOAD", &library)
                .env("GDP_IO_SYNC_COUNTS", &counter)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().unwrap();
            let ready = wait_status(&f, &mut child, |v| v["ready"] == true);
            let worker = ready["pid"].as_u64().unwrap() as u32;
            let start = process_measurement(worker);
            let begun = Instant::now();
            let observed = std::cell::RefCell::new((
                ready["heartbeat_sequence"].as_u64().unwrap(),
                ready["quality"]["latest_event_sequence"].as_u64().unwrap(),
                0,
                0,
            ));
            let complete = wait_status_for(
                &f,
                &mut child,
                |v| {
                    if begun.elapsed() >= Duration::from_secs(1) && !armed.exists() {
                        fs::write(&armed, b"1").unwrap();
                    }
                    let heartbeat = v["heartbeat_sequence"].as_u64().unwrap();
                    let event = v["quality"]["latest_event_sequence"].as_u64().unwrap();
                    let mut observation = observed.borrow_mut();
                    if heartbeat != observation.0 {
                        if event == observation.1 {
                            observation.2 += 1;
                        } else {
                            observation.3 += 1;
                        }
                        observation.0 = heartbeat;
                        observation.1 = event;
                    }
                    v["raw_recording"]["state"] == "complete"
                },
                Duration::from_secs(30),
            );
            let end = process_measurement(worker);
            let wall = begun.elapsed().as_secs_f64();
            let raw = &complete["raw_recording"];
            assert_eq!(raw["written_pairs"], 90);
            assert_eq!(
                raw["disk_stages"]["validation_image_bytes"]
                    .as_u64()
                    .unwrap(),
                2 * raw["payload_bytes_submitted"].as_u64().unwrap()
            );
            assert_eq!(
                raw["disk_stages"]["validation_imu_samples"]
                    .as_u64()
                    .unwrap(),
                2 * raw["imu_samples_submitted"].as_u64().unwrap()
            );
            assert!(complete["imu"]["error"].is_null());
            let observation = observed.into_inner();
            assert!(
                observation.2 > 0 && observation.3 > 0,
                "benchmark must observe stable and changed quality events"
            );
            let job = Path::new(raw["job_path"].as_str().unwrap());
            let mut reader = offline_depth::recording::RecordingDirectorySource::open(job).unwrap();
            let mut count = 0;
            while reader.next_frame().unwrap().is_some() {
                count += 1;
            }
            assert_eq!(count, 90);
            let artifacts: Value = [
                "records.jsonl",
                "checkpoint.json",
                "manifest.json",
                "COMPLETE",
            ]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    json!(
                        digest::digest(&digest::SHA256, &fs::read(job.join(name)).unwrap())
                            .as_ref()
                            .iter()
                            .fold(String::with_capacity(64), |mut hex, byte| {
                                write!(&mut hex, "{byte:02x}").unwrap();
                                hex
                            })
                    ),
                )
            })
            .collect();
            stop(&mut child);
            let counts: Value = serde_json::from_slice(
                &fs::read(format!("{}.{}.json", counter.display(), worker)).unwrap(),
            )
            .unwrap();
            rows.push(json!({"candidate":candidate,"repeat":repeat,"workers":1,"window_seconds":wall,"worker_cpu_cores":(end.0-start.0) as f64/hz/wall,"worker_peak_rss_kib":end.1,"actual_fsync_counts":counts,"heartbeat_start":ready["heartbeat_sequence"],"heartbeat_end":complete["heartbeat_sequence"],"observed_stable_event_heartbeats":observation.2,"observed_changed_event_heartbeats":observation.3,"quality_event_start":ready["quality"]["latest_event_sequence"],"quality_event_end":complete["quality"]["latest_event_sequence"],"raw_recording":raw,"imu":complete["imu"],"artifact_sha256":artifacts,"independent_pairs_read_back":count}));
        }
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .unwrap();
    serde_json::to_writer_pretty(file, &json!({"kind":"quality-durable-writer-ab","repeats":3,"color_profile":[640,480,30],"depth_profile":[640,400,30],"rows":rows,"scope":"single writer, 30FPS synthetic SDK ABI with 200Hz accel+gyro, real filesystem fsync; no USB/real DDS/multiwriter inference; counters separate real success/error from injected failures; worker CPU readiness-to-COMPLETE; VmHWM before independent materialized readback; new-directory bootstrap syncs included in whole-worker counts; 6/5 fsync statement describes steady-state batches"})).unwrap();
}
