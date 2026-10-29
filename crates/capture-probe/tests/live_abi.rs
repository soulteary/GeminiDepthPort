//! Actual Linux CLI acquisition through deterministic native ABI libraries.
//! No device, SDK installation, ROS process or USB access is involved.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    sdk: PathBuf,
    bridge: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn replace(source: &mut String, from: &str, to: &str) {
    assert!(
        source.contains(from),
        "review ABI fixture adaptation: {from}"
    );
    *source = source.replace(from, to);
}
fn compile(compiler: &str, source: &Path, output: &Path, include: Option<&Path>) {
    let mut cmd = Command::new(compiler);
    cmd.args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"]);
    cmd.arg(if compiler == "cc" {
        "-std=c11"
    } else {
        "-std=c++17"
    });
    if let Some(include) = include {
        cmd.arg("-I").arg(include);
    }
    let out = cmd.arg(source).arg("-o").arg(output).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gdp-live-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let sdk = root.join("sdk.so");
        let bridge = root.join("bridge.so");
        let mut c = include_str!("../../orbbec-safe/tests/mock_sdk.c").to_string();
        c.insert_str(
            0,
            "#define _DEFAULT_SOURCE\n#include <unistd.h>\nstatic unsigned long long sequence;\n",
        );
        replace(
            &mut c,
            "if(ms==1)return NULL;return make(5);",
            "usleep(1000);if(ms==1||getenv(\"GDP_ABI_TIMEOUT\"))return NULL;sequence++;return make(5);",
        );
        replace(
            &mut c,
            "return 42;",
            "return getenv(\"GDP_ABI_DUPLICATE\")?42:sequence;",
        );
        replace(
            &mut c,
            "return 1234;",
            "return sequence*33333+(getenv(\"GDP_ABI_SKEW\")&&p->stream==3?30000:0);",
        );
        replace(
            &mut c,
            "return 5678;",
            "return getenv(\"GDP_ABI_BAD_STAMP\")?0:1700000000000000ULL+sequence*33333;",
        );
        replace(
            &mut c,
            "return 0.1f;",
            "return getenv(\"GDP_ABI_SCALE\")?strtof(getenv(\"GDP_ABI_SCALE\"),NULL):0.1f;",
        );
        replace(
            &mut c,
            "obj *f=make(5);f->stream=2;return f;",
            "if(getenv(\"GDP_ABI_MISSING_COLOR\"))return NULL;obj *f=make(5);f->stream=2;return f;",
        );
        replace(
            &mut c,
            "active=0;event(\"stop\");",
            "active=0;event(\"stop\");if(getenv(\"GDP_ABI_STOP_FAILURE\"))*e=(error*)1;",
        );
        let source = root.join("sdk.c");
        fs::write(&source, c).unwrap();
        compile("cc", &source, &sdk, None);
        let mut cpp = include_str!("../../ros-output/tests/mock_bridge.cpp").to_string();
        for line in [
            "if (frame->sequence == 1 && (!frame->has_device_timestamp || frame->device_timestamp_us != 0)) return 2;",
            "if (!p.owned.empty() && p.owned[0] != 16) return 2;",
            "if (p.frame_id != \"test_optical\" || p.focal != 1.) return 2;",
        ] {
            replace(&mut cpp, line, "");
        }
        replace(
            &mut cpp,
            "if (!handle) return 1;",
            "if (!handle) return 1;\n  if(std::getenv(\"GDP_ABI_PUBLISH_FAILURE\")){event(\"publish-failed\");return 2;}",
        );
        let source = root.join("bridge.cpp");
        fs::write(&source, cpp).unwrap();
        compile(
            "c++",
            &source,
            &bridge,
            Some(&crate_root.join("../../ros/include")),
        );
        Self { root, sdk, bridge }
    }
    fn command(&self, rgbd: bool) -> Command {
        let _ = fs::remove_file(self.root.join("events"));
        let mut cmd = Command::new(if rgbd {
            env!("CARGO_BIN_EXE_rgbd-probe")
        } else {
            env!("CARGO_BIN_EXE_capture-probe")
        });
        cmd.env("GEMINIDEPTHPORT_MOCK_EVENTS", self.root.join("events"))
            .env("GDP_ROS_MOCK_EVENTS", self.root.join("events"));
        cmd
    }
    fn single(&self, depth: bool, normalize: bool, ros: bool) -> Command {
        let mut cmd = self.command(false);
        cmd.args([
            "--stream",
            if depth { "depth" } else { "color" },
            "--width",
            "2",
            "--height",
            "2",
            "--fps",
            "30",
            "--sdk-library",
            self.sdk.to_str().unwrap(),
            "--serial",
            "test-device",
            "--seconds",
            "0.08",
            "--poll-ms",
            "10",
            "--capacity",
            "2",
            "--drain-every",
            "3",
        ]);
        if normalize {
            cmd.args(["--depth-unit-policy", "normalize-mm"]);
        }
        if ros {
            let calibration = self.root.join("calibration.json");
            fs::write(&calibration,r#"{"version":1,"width":2,"height":2,"frame_id":"test_optical","distortion_model":"plumb_bob","k":[1,0,1,0,1,1,0,0,1],"d":[0,0,0,0,0],"r":[1,0,0,0,1,0,0,0,1],"p":[1,0,1,0,0,1,1,0,0,0,1,0]}"#).unwrap();
            cmd.args([
                "--ros-bridge",
                self.bridge.to_str().unwrap(),
                "--ros-namespace",
                "/test",
                "--ros-frame-id",
                "test_optical",
                "--ros-calibration",
                calibration.to_str().unwrap(),
                "--ros-stamp-source",
                "sdk-system-unix",
            ]);
        }
        cmd
    }
    fn paired(&self, ros: bool, system: bool, startup: bool) -> Command {
        let mut config = json!({"version":1,"sdk_library":self.sdk,"serial":"test-device","color":{"width":2,"height":2,"fps":30,"frame_id":"color_optical"},"depth":{"width":2,"height":2,"fps":30,"frame_id":"depth_optical","unit_policy":"normalize-mm"},"pairing":{"clock":if system{"system"}else{"device"},"max_delta_us":20000},"seconds":0.08,"poll_ms":10,"capacity":2,"startup_discovery_seconds":if startup{0.02}else{0.0}});
        if ros {
            config["ros"] =
                json!({"bridge":self.bridge,"namespace":"/test","stamp_source":"sdk-system-unix"});
        }
        let path = self.root.join("rgbd.json");
        fs::write(&path, config.to_string()).unwrap();
        let mut cmd = self.command(true);
        cmd.arg("--config").arg(path);
        cmd
    }
    fn report(&self, output: Output, ok: bool) -> Value {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.status.success(), ok, "{value}");
        assert_eq!(value["ok"], ok);
        let events = fs::read_to_string(self.root.join("events")).unwrap();
        assert!(events.lines().any(|s| s == "stop"));
        assert!(events.lines().any(|s| s == "context-delete"));
        assert!(!value.to_string().contains("test-device"));
        value
    }
}
#[test]
fn live_single_stream_preserves_raw_pixels_and_normalizes_depth_with_cleanup() {
    let f = Fixture::new();
    for (depth, normalize) in [(false, false), (true, true)] {
        let value = f.report(f.single(depth, normalize, false).output().unwrap(), true);
        assert!(value["processed"].as_u64().unwrap() > 0, "{value}");
        if normalize {
            assert!(
                value["normalization"]["frames"].as_u64().unwrap() > 0,
                "{value}"
            );
        }
    }
}
#[test]
fn live_single_stream_rejects_units_overflow_timeouts_and_native_stop_errors() {
    let f = Fixture::new();
    for (depth, normalize, key, value, expected) in [
        (
            true,
            false,
            "GDP_ABI_SCALE",
            "0.1",
            "FRAME_CONTRACT_REJECTED",
        ),
        (true, true, "GDP_ABI_SCALE", "10", "FRAME_CONTRACT_REJECTED"),
        (true, true, "GDP_ABI_SCALE", "0", "SDK_ACQUISITION_FAILED"),
        (false, false, "GDP_ABI_TIMEOUT", "1", "NO_VALID_FRAMES"),
        (false, false, "GDP_ABI_STOP_FAILURE", "1", "SDK_STOP_FAILED"),
    ] {
        let value = f.report(
            f.single(depth, normalize, false)
                .env(key, value)
                .output()
                .unwrap(),
            false,
        );
        assert_eq!(value["error"], expected, "{value}");
    }
}
#[test]
fn single_stream_ros_publication_and_failure_keep_native_cleanup() {
    let f = Fixture::new();
    let value = f.report(f.single(false, false, true).output().unwrap(), true);
    assert!(
        value["ros_output"]["publish_calls_succeeded"]
            .as_u64()
            .unwrap()
            > 0,
        "{value}"
    );
    for (key, expected) in [
        ("GDP_ABI_BAD_STAMP", "ROS_STAMP_INVALID"),
        ("GDP_ABI_PUBLISH_FAILURE", "ROS_PUBLISH_FAILED"),
    ] {
        let value = f.report(
            f.single(false, false, true).env(key, "1").output().unwrap(),
            false,
        );
        assert_eq!(value["error"], expected);
    }
}
#[test]
fn rgbd_probe_observes_both_clocks_and_discards_startup_without_false_sync_claims() {
    let f = Fixture::new();
    for system in [false, true] {
        let value = f.report(f.paired(false, system, true).output().unwrap(), true);
        assert!(value["pairs_processed"].as_u64().unwrap() > 0);
        assert!(
            value["startup_discarded"]["accepted_not_published"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(value["rgbd_exposure_sync_verified"], false);
        assert_eq!(value["physical_calibration_accuracy_verified"], false);
    }
}
#[test]
fn rgbd_probe_diagnoses_missing_skewed_duplicate_and_overflowing_pairs() {
    let f = Fixture::new();
    for key in [
        "GDP_ABI_MISSING_COLOR",
        "GDP_ABI_SKEW",
        "GDP_ABI_DUPLICATE",
        "GDP_ABI_TIMEOUT",
    ] {
        let value = f.report(
            f.paired(false, false, false)
                .env(key, "1")
                .output()
                .unwrap(),
            false,
        );
        if key == "GDP_ABI_MISSING_COLOR" {
            assert!(value["framesets_missing_color"].as_u64().unwrap() > 0);
        }
        if key == "GDP_ABI_SKEW" {
            assert!(value["pairs_skew_rejected"].as_u64().unwrap() > 0);
        }
        if key == "GDP_ABI_DUPLICATE" {
            assert!(value["pairs_monotonic_rejected"].as_u64().unwrap() > 0);
        }
        if key == "GDP_ABI_TIMEOUT" {
            assert_eq!(value["pairs_accepted"], 0);
        }
    }
    let value = f.report(
        f.paired(false, false, false)
            .env("GDP_ABI_SCALE", "10")
            .output()
            .unwrap(),
        false,
    );
    assert!(value["pairs_rejected"].as_u64().unwrap() > 0);
}
#[test]
fn rgbd_ros_publishes_calibration_and_pair_evidence_and_reports_output_failures() {
    let f = Fixture::new();
    let value = f.report(f.paired(true, false, false).output().unwrap(), true);
    assert!(
        value["ros"]["pairs_publish_calls_succeeded"]
            .as_u64()
            .unwrap()
            > 0,
        "{value}"
    );
    assert_eq!(value["ros"]["delivery_verified"], false);
    for (key, expected) in [
        ("GDP_ABI_BAD_STAMP", "ROS_STAMP_INVALID"),
        ("GDP_ABI_PUBLISH_FAILURE", "ROS_PUBLISH_FAILED"),
    ] {
        let value = f.report(
            f.paired(true, false, false).env(key, "1").output().unwrap(),
            false,
        );
        assert_eq!(value["error"], expected, "{value}");
    }
}
