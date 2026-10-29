//! Offline orchestration contracts: real generation hashes, snapshots, guards,
//! ACKs and stores; only Docker's command boundary is replaced per Controller.
use crate::{
    capture_snapshot, config, control::Controller, generation, managed_quality, recording, util::*,
};
use anyhow::{Result, bail};
use capture_contract::{
    content::*,
    geometry::*,
    recording::{GateAck, GatePhase, GateRequest},
    snapshot::*,
    store::SnapshotStore,
    *,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Camera {
    name: String,
    container: Value,
    status: Value,
    snapshot: SnapshotEnvelope,
    bundle: Value,
    status_reads: usize,
    quality_reads: usize,
}
#[derive(Default)]
struct Docker {
    cameras: Vec<Camera>,
    calls: Vec<Vec<String>>,
    status_drift_at: Option<usize>,
    quality_drift: bool,
    inventory_on_snapshot: bool,
    inventory_on_quality: bool,
    snapshot_read_failure: bool,
    quality_read_failure: bool,
    probe_failure: bool,
}
struct Fixture {
    _temp: tempfile::TempDir,
    ctl: Controller,
    inventory: Value,
    generation: Value,
    docker: Arc<Mutex<Docker>>,
}
fn snapshot(name: &str, config_hash: &str, official: bool, index: usize) -> SnapshotEnvelope {
    let session = SessionId::parse(format!("{:032x}", index + 1)).unwrap();
    let instance = InstanceId::parse(format!("{:064x}", index + 10)).unwrap();
    let calibration = CalibrationRecord {
        version: 1,
        source: if official {
            CalibrationSource::RosCameraInfoTf
        } else {
            CalibrationSource::SdkActiveProfiles
        },
        actual_runtime_observed: true,
        source_session_id: Some(session.clone()),
        source_instance_id: Some(instance.clone()),
        observed_unix_ns: Some(1),
        models: BTreeMap::from_iter(["color", "depth"].map(|role| {
            (
                role.into(),
                CameraModel {
                    frame_id: format!("{name}_{role}_optical_frame"),
                    width: 640,
                    height: 480,
                    k: [500., 0., 320., 0., 500., 240., 0., 0., 1.],
                    distortion_model: DistortionModel::None,
                    distortion_coefficients: vec![],
                },
            )
        })),
        transforms: vec![RigidTransform {
            source_frame: format!("{name}_depth_optical_frame"),
            target_frame: format!("{name}_color_optical_frame"),
            rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            translation_m: [0.02, 0., 0.],
        }],
        physical_accuracy_verified: false,
        legacy_source_sha256: None,
    };
    let configuration = json!({"runtime_config_sha256":config_hash});
    CaptureSnapshot {
        version: 1,
        binding: SnapshotBinding {
            instance_id: instance,
            session_id: session,
            generation_id: GenerationId::parse("d".repeat(32)).unwrap(),
            configuration_id: ConfigurationId::parse(
                content_digest(
                    ContentKind::Configuration,
                    &serde_json::to_vec(&configuration).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
            calibration_id: calibration.id().unwrap(),
        },
        configuration,
        calibration,
        depth: DepthRepresentation::Millimetres {
            quantization: QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
        },
        quality_policy: QualityPolicy {
            version: 1,
            max_source_age_ns: 1,
            max_future_ns: 0,
            max_pair_delta_us: 1,
            min_valid_fraction: 0.,
            max_zero_fraction: 1.,
            action: QualityAction::ReportOnly,
        },
    }
    .seal()
    .unwrap()
}
impl Fixture {
    fn new(official: bool, count: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut inventory: Value =
            serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
        if !official {
            inventory["capture_backend"] = json!("rust");
            inventory["rust_capture"] = json!({"depth_unit_policy":"normalize-mm","pairing":{"clock":"device","max_delta_us":20000},"poll_ms":100,"capacity":4,"startup_discovery_seconds":2,"service":{"idle_timeout_ms":2000,"startup_timeout_ms":15000,"heartbeat_ms":500,"heartbeat_timeout_ms":10000,"max_restarts":3,"restart_backoff_ms":1000,"shutdown_timeout_ms":5000,"ready_min_pairs":5}});
        }
        for i in 0..count {
            let c = &mut inventory["cameras"][i];
            c["enabled"] = json!(true);
            c["serial_number"] = json!(format!("SYNTHETIC_{i}"));
            c["timestamp_source"] = json!("host");
            c["health"] = json!({"discovery_seconds":1,"sample_seconds":2,"freshness_seconds":2,"min_fps_ratio":0.8});
            for role in ["color", "depth"] {
                for (key, v) in [("width", 640), ("height", 480), ("fps", 30)] {
                    c["parameters"][format!("{role}_{key}")] = json!(v);
                }
            }
            c["profiles_verified"] = json!(true);
            let digest = config::camera_digest(&inventory, &inventory["cameras"][i]);
            let c = &mut inventory["cameras"][i];
            c["verification"] = json!({"method":"runtime-probe","configuration_digest":digest});
            if !official {
                c["verification"]["runtime_digest"] = json!("a".repeat(64));
            }
        }
        config::validate(&inventory, false).unwrap();
        atomic_json(&root.join("inventory.json"), &inventory).unwrap();
        let folder = root.join("state/generations").join("d".repeat(32));
        let mut files = json!({});
        let mut cameras = vec![];
        let mut summaries = vec![];
        for i in 0..count {
            let name = text(&inventory["cameras"][i]["name"]).to_string();
            let gate = root.join("state/recording-gates").join(&name);
            private_dir(&gate).unwrap();
            let config_file = if official {
                "observer-policy.json"
            } else {
                "capture.json"
            };
            let runtime = json!({"camera":name,"service":{"recording_control_path":"/recording/control.json"}});
            let mut entries = vec![
                (config_file, runtime.clone()),
                ("camera.yaml", json!({})),
                ("health.json", json!({})),
                ("binding.json", json!({})),
            ];
            if !official {
                entries.push(("runtime.json", json!({"version":1})));
            }
            for (file, value) in entries {
                let relative = format!("config/{name}/{file}");
                atomic_json(&folder.join(&relative), &value).unwrap();
                files[&relative] = json!(file_hash(&folder.join(&relative)).unwrap());
            }
            let config_hash =
                file_hash(&folder.join(format!("config/{name}/{config_file}"))).unwrap();
            let snapshot = snapshot(&name, &config_hash, official, i);
            let binding = &snapshot.content.binding;
            let base = if official {
                "/tmp/capture-observer"
            } else {
                "/tmp/capture"
            };
            let status = json!({"version":1,"kind":if official{"official-observer-status"}else{"capture-service-status"},"instance_id":binding.instance_id,"session_id":binding.session_id,"generation":"d".repeat(32),"config_sha256":config_hash,"snapshot_id":snapshot.snapshot_id,"snapshot_path":format!("{base}/snapshots/{}.json",snapshot.snapshot_id),"pid":42+i,"ready":true,"state":"ready","updated_unix_ms":now()*1000.,"last_pair_age_ms":1,"session_kind":if official{"observer"}else{"sdk-worker"},"native_session_verified":!official,"received_counts":{"color":3,"depth":3}});
            let quality = json!({"version":1,"session_id":binding.session_id,"origin_session_id":binding.session_id,"at":{"session_id":binding.session_id,"offset_ns":10},"active_episodes":[],"finalized_sessions":[],"current_session_finalized":false,"latest_event_sequence":1});
            let bundle = json!({"status":status,"processes_alive":true,"current":{"version":1,"instance_id":binding.instance_id,"session_id":binding.session_id,"snapshot_id":snapshot.snapshot_id,"quality":quality},"events":{"version":1,"origin_session_id":binding.session_id,"latest_sequence":1,"gap":null,"events":[{"version":1,"sequence":1,"scope":{"kind":"recorder"},"fact":"synthetic"}],"has_more":false,"next_cursor":{"origin_session_id":binding.session_id,"after_sequence":1}},"summary":null});
            let container = json!({"Id":format!("{:064x}",i+100),"Config":{"Labels":{"com.docker.compose.project":"orbbec-multicam","com.docker.compose.oneoff":"False","orbbec.role":"camera","orbbec.generation":"d".repeat(32),"orbbec.camera":name}},"State":{"Running":true},"Mounts":[{"Destination":"/recording","Source":gate,"RW":true}]});
            summaries.push(json!({"name":name,"service":format!("camera_{name}")}));
            cameras.push(Camera {
                name,
                container,
                status,
                snapshot,
                bundle,
                status_reads: 0,
                quality_reads: 0,
            });
        }
        atomic_json(&folder.join("compose.yaml"), &json!({"services":{}})).unwrap();
        files["compose.yaml"] = json!(file_hash(&folder.join("compose.yaml")).unwrap());
        let mut generation = json!({"kind":generation::KIND,"generation":"d".repeat(32),"draft":false,"schema_version":2,"inventory_sha256":file_hash(&root.join("inventory.json")).unwrap(),"communication_profile":config::COMMUNICATION,"cameras":summaries,"files":files,"quality_evidence_version":1});
        if official {
            generation["observer_backend"] = json!("ros-observed-v1");
        } else {
            generation["capture_backend"] = json!("rust");
            generation["capture_runtime"] =
                json!({"manifest":{"version":1},"image_id":format!("sha256:{}","a".repeat(64))});
        }
        atomic_json(&folder.join("manifest.json"), &generation).unwrap();
        atomic_json(&root.join("state/current-generation.json"), &json!({"generation":generation["generation"],"manifest_sha256":file_hash(&folder.join("manifest.json")).unwrap()})).unwrap();
        let docker = Arc::new(Mutex::new(Docker {
            cameras,
            ..Default::default()
        }));
        let state = docker.clone();
        let command_root = root.clone();
        let mut ctl = Controller::new(&root).unwrap();
        ctl.command_runner = Some(Arc::new(move |args| {
            let mut d = state.lock().unwrap();
            d.calls.push(args.to_vec());
            match args.get(1).map(String::as_str) {
                Some("ps") => Ok(d
                    .cameras
                    .iter()
                    .map(|c| text(&c.container["Id"]))
                    .collect::<Vec<_>>()
                    .join("\n")),
                Some("inspect") => Ok(serde_json::to_string(
                    &d.cameras.iter().map(|c| &c.container).collect::<Vec<_>>(),
                )?),
                Some("compose") if args.iter().any(|a| a == "/app/health.py") => {
                    if d.probe_failure {
                        bail!("SYNTHETIC_PROBE_FAILURE");
                    }
                    Ok(String::new())
                }
                Some("exec") => {
                    let index = d
                        .cameras
                        .iter()
                        .position(|c| c.container["Id"].as_str() == args.get(2).map(String::as_str))
                        .expect("known container");
                    let last = args.last().unwrap();
                    if ["0", "1"].contains(&last.as_str()) {
                        d.cameras[index].status_reads += 1;
                        let mut status = d.cameras[index].status.clone();
                        if d.status_drift_at
                            .is_some_and(|n| d.cameras[index].status_reads >= n)
                        {
                            status["session_id"] = json!("f".repeat(32));
                        }
                        Ok(serde_json::to_string(&status)?)
                    } else if ["rust", "official"].contains(&last.as_str()) {
                        if d.quality_read_failure {
                            bail!("private path and credentials must not escape");
                        }
                        d.cameras[index].quality_reads += 1;
                        let mut bundle = d.cameras[index].bundle.clone();
                        if d.quality_drift && d.cameras[index].quality_reads >= 2 {
                            bundle["status"]["session_id"] = json!("f".repeat(32));
                        }
                        if d.inventory_on_quality {
                            atomic_json(
                                &command_root.join("inventory.json"),
                                &json!({"changed":true}),
                            )?;
                        }
                        Ok(serde_json::to_string(&bundle)?)
                    } else {
                        assert_eq!(last, d.cameras[index].snapshot.snapshot_id.as_str());
                        if d.snapshot_read_failure {
                            bail!("private snapshot read failed");
                        }
                        if d.inventory_on_snapshot {
                            atomic_json(
                                &command_root.join("inventory.json"),
                                &json!({"changed":true}),
                            )?;
                        }
                        Ok(serde_json::to_string(&d.cameras[index].snapshot)?)
                    }
                }
                _ => panic!("unexpected external command: {args:?}"),
            }
        }));
        assert_eq!(ctl.check_render(&inventory).unwrap(), generation);
        Self {
            _temp: temp,
            ctl,
            inventory,
            generation,
            docker,
        }
    }
    fn edit(&self, f: impl FnOnce(&mut Docker)) {
        f(&mut self.docker.lock().unwrap());
    }
    fn acquire(&self) -> Result<Vec<capture_snapshot::Witness>> {
        capture_snapshot::acquire(&self.ctl, &self.inventory, &self.generation)
    }
    fn query(&self, kind: &str) -> Result<Value> {
        managed_quality::query(&self.ctl, "camera_a", kind, None, 10)
    }
    fn gate(&self, name: &str) -> PathBuf {
        self.ctl.root.join("state/recording-gates").join(name)
    }
    fn guard(&self) -> Value {
        read_json(&self.ctl.root.join("state/recording-guard.json")).unwrap()
    }
    fn assert_retained(&self) {
        assert_eq!(self.guard()["state"], "preparing");
        assert!(self.ctl.unprotected().is_err());
        assert!(
            self.ctl
                .root
                .join("state/recording-recovery-credential")
                .is_file()
        );
        for c in &self.docker.lock().unwrap().cameras {
            assert!(self.gate(&c.name).join("control.json").is_file());
        }
    }
    fn release(&self) {
        let credential =
            fs::read_to_string(self.ctl.root.join("state/recording-recovery-credential")).unwrap();
        self.ctl.release(&credential).unwrap();
        assert!(self.ctl.unprotected().is_ok());
        for c in &self.docker.lock().unwrap().cameras {
            assert!(!self.gate(&c.name).join("control.json").exists());
            assert!(!self.gate(&c.name).join("ack.json").exists());
        }
    }
}
fn error<T>(result: Result<T>, code: &str) {
    let message = result.err().expect("must reject").to_string();
    assert!(message.starts_with(code), "expected {code}, got {message}");
}

#[derive(Clone, Copy)]
enum AckMode {
    All,
    Missing,
    WrongRequest,
    WrongSession,
    WrongWorker,
    FirstCameraOnly,
    PreparingOnly,
    ChangeInventory,
    ChangeSession,
    ChangeInventoryOnActive,
    ChangeSessionOnActive,
}
struct Responder {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    phases: Arc<Mutex<Vec<(String, GatePhase)>>>,
}
impl Responder {
    fn start(f: &Fixture, mode: AckMode) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let phases = Arc::new(Mutex::new(vec![]));
        let observed = phases.clone();
        let root = f.ctl.root.clone();
        let docker = f.docker.clone();
        let names = docker
            .lock()
            .unwrap()
            .cameras
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>();
        let handle = thread::spawn(move || {
            let mut seen = BTreeMap::new();
            while !done.load(Ordering::Acquire) {
                for (i, name) in names.iter().enumerate() {
                    let gate = root.join("state/recording-gates").join(name);
                    let Ok(raw) = fs::read(gate.join("control.json")) else {
                        continue;
                    };
                    if seen.get(name) == Some(&raw) {
                        continue;
                    }
                    let request: GateRequest = serde_json::from_slice(&raw).unwrap();
                    observed.lock().unwrap().push((name.clone(), request.phase));
                    seen.insert(name.clone(), raw);
                    if matches!(mode, AckMode::Missing)
                        || (matches!(mode, AckMode::FirstCameraOnly) && i > 0)
                        || (matches!(mode, AckMode::PreparingOnly)
                            && request.phase == GatePhase::Active)
                    {
                        continue;
                    }
                    if i == 0 {
                        if (request.phase == GatePhase::Preparing
                            && matches!(mode, AckMode::ChangeInventory))
                            || (request.phase == GatePhase::Active
                                && matches!(mode, AckMode::ChangeInventoryOnActive))
                        {
                            atomic_json(&root.join("inventory.json"), &json!({"changed":true}))
                                .unwrap();
                        }
                        if (request.phase == GatePhase::Preparing
                            && matches!(mode, AckMode::ChangeSession))
                            || (request.phase == GatePhase::Active
                                && matches!(mode, AckMode::ChangeSessionOnActive))
                        {
                            docker.lock().unwrap().cameras[0].status["session_id"] =
                                json!("f".repeat(32));
                        }
                    }
                    let mut ack = GateAck {
                        version: 1,
                        request_id: request.request_id.clone(),
                        request_sha256: request.digest().unwrap(),
                        worker_pid: 42 + i as u32,
                        supervisor_pid: 1,
                        session_id: request.binding.session_id.clone(),
                    };
                    if matches!(mode, AckMode::WrongRequest) {
                        ack.request_sha256 = ConfigurationId::parse("0".repeat(64)).unwrap();
                    }
                    if matches!(mode, AckMode::WrongSession) {
                        ack.session_id = SessionId::parse("f".repeat(32)).unwrap();
                    }
                    if matches!(mode, AckMode::WrongWorker) {
                        ack.worker_pid += 100;
                    }
                    atomic_json(&gate.join("ack.json"), &serde_json::to_value(ack).unwrap())
                        .unwrap();
                }
                thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            stop,
            handle: Some(handle),
            phases,
        }
    }
}
impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}

#[test]
fn snapshot_acquires_and_persists_both_native_and_observer_evidence() {
    for official in [false, true] {
        let f = Fixture::new(official, 2);
        let witnesses = f.acquire().unwrap();
        assert_eq!(witnesses.len(), 2);
        let store = SnapshotStore::open(&f.ctl.root.join("state/capture-snapshots")).unwrap();
        for w in &witnesses {
            let snapshot = w.snapshot.as_ref().unwrap();
            assert_eq!(w.snapshot_id(), &snapshot.snapshot_id);
            assert_eq!(
                store
                    .read(&snapshot.snapshot_id, &snapshot.content.binding)
                    .unwrap(),
                *snapshot
            );
        }
        assert_eq!(
            capture_snapshot::machine_summary(&witnesses)["snapshots"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            f.docker
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|a| a.get(1).is_some_and(|a| a == "exec"))
                .all(|a| a.iter().any(|arg| arg.contains(if official {
                    "/tmp/capture-observer"
                } else {
                    "/tmp/capture"
                })))
        );
    }
}
#[test]
fn live_snapshot_rejects_stale_future_and_mismatched_status() {
    for official in [false, true] {
        for (key, value) in [
            ("updated_unix_ms", json!(now() * 1000. - 60000.)),
            ("updated_unix_ms", json!(now() * 1000. + 60000.)),
            ("generation", json!("e".repeat(32))),
            ("config_sha256", json!("e".repeat(64))),
            ("ready", json!(false)),
            ("state", json!("opening")),
            ("kind", json!("wrong")),
        ] {
            let f = Fixture::new(official, 1);
            f.edit(|d| d.cameras[0].status[key] = value);
            error(f.acquire(), "SNAPSHOT_SESSION_NOT_READY");
        }
    }
}
#[test]
fn snapshot_rejects_invalid_pid_path_and_backend_readiness() {
    for pid in [json!(0), json!(-1), json!(u64::MAX), Value::Null] {
        let f = Fixture::new(false, 1);
        f.edit(|d| d.cameras[0].status["pid"] = pid);
        error(f.acquire(), "SNAPSHOT_WORKER_PID");
    }
    for official in [false, true] {
        let f = Fixture::new(official, 1);
        f.edit(|d| d.cameras[0].status["snapshot_path"] = json!("/tmp/escape.json"));
        error(
            f.acquire(),
            if official {
                "SNAPSHOT_OBSERVER_PATH_INVALID"
            } else {
                "SNAPSHOT_SDK_PATH_INVALID"
            },
        );
        let f = Fixture::new(official, 1);
        f.edit(|d| {
            if official {
                d.cameras[0].status["received_counts"]["depth"] = json!(2)
            } else {
                d.cameras[0].status["last_pair_age_ms"] = json!(2001)
            }
        });
        error(f.acquire(), "SNAPSHOT_SESSION_NOT_READY");
    }
}
#[test]
fn terminal_witness_is_readable_without_claiming_live_readiness() {
    let f = Fixture::new(false, 1);
    f.edit(|d| {
        d.cameras[0].status["ready"] = json!(false);
        d.cameras[0].status["state"] = json!("degraded");
    });
    capture_snapshot::status_witness(
        &f.ctl,
        &f.inventory,
        &f.generation,
        &f.inventory["cameras"][0],
    )
    .unwrap();
    error(f.acquire(), "SNAPSHOT_SESSION_NOT_READY");
    let calls = &f.docker.lock().unwrap().calls;
    assert!(calls.iter().any(|a| a.last().is_some_and(|s| s == "0")));
}
#[test]
fn snapshot_rechecks_each_session_and_the_complete_collection() {
    for read in [2, 3] {
        let f = Fixture::new(false, 2);
        f.edit(|d| d.status_drift_at = Some(read));
        error(f.acquire(), "SNAPSHOT_SESSION_CHANGED");
    }
    let f = Fixture::new(false, 1);
    f.edit(|d| d.inventory_on_snapshot = true);
    error(f.acquire(), "SNAPSHOT_CONFIGURATION_CHANGED");
}
#[test]
fn snapshot_private_read_failure_does_not_publish_evidence() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.snapshot_read_failure = true);
    error(f.acquire(), "SNAPSHOT_SDK_PRIVATE_READ_FAILED");
    let path = f.ctl.root.join("state/capture-snapshots");
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}
#[test]
fn snapshot_rejects_missing_container_and_unavailable_observer() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.cameras.clear());
    error(f.acquire(), "SNAPSHOT_INSTANCE_MISSING");
    let f = Fixture::new(true, 1);
    let mut g = f.generation.clone();
    g["observer_backend"] = Value::Null;
    error(
        capture_snapshot::acquire(&f.ctl, &f.inventory, &g),
        "OFFICIAL_OBSERVER_UNAVAILABLE",
    );
}

#[test]
fn protection_requires_both_ack_phases_for_every_camera_and_can_release() {
    let f = Fixture::new(false, 2);
    let responder = Responder::start(&f, AckMode::All);
    let result = recording::protect(&f.ctl, &f.inventory, &f.generation).unwrap();
    assert_eq!(result["state"], "active");
    assert_eq!(result["snapshot_count"], 2);
    assert_eq!(result["runtime_session_bound"], true);
    assert_eq!(f.guard()["state"], "active");
    for name in ["camera_a", "camera_b"] {
        for phase in [GatePhase::Preparing, GatePhase::Active] {
            assert!(
                responder
                    .phases
                    .lock()
                    .unwrap()
                    .contains(&(name.into(), phase))
            );
        }
    }
    let context = managed_quality::recording_context(&f.ctl, true).unwrap();
    assert_eq!(context["protection"]["session_bound"], true);
    assert_eq!(context["protection"]["native_gate_ack_verified"], true);
    error(f.ctl.release("wrong"), "GUARD_CREDENTIAL");
    assert!(f.ctl.unprotected().is_err());
    assert!(
        !recording::status(&f.ctl)
            .to_string()
            .contains(text(&result["token"]))
    );
    drop(responder);
    f.release();
}
fn failed_handshake(mode: AckMode) {
    let f = Fixture::new(false, 2);
    let responder = Responder::start(&f, mode);
    error(
        recording::protect(&f.ctl, &f.inventory, &f.generation),
        "RECORDING_PREPARE_FAILED",
    );
    f.assert_retained();
    drop(responder);
    f.release();
}
#[test]
fn protection_retains_every_gate_when_no_ack_arrives() {
    failed_handshake(AckMode::Missing);
}
#[test]
fn protection_rejects_ack_for_another_request() {
    failed_handshake(AckMode::WrongRequest);
}
#[test]
fn protection_rejects_ack_for_another_session() {
    failed_handshake(AckMode::WrongSession);
}
#[test]
fn protection_rejects_ack_from_another_worker() {
    failed_handshake(AckMode::WrongWorker);
}
#[test]
fn protection_retains_both_gates_when_only_first_camera_acknowledges() {
    failed_handshake(AckMode::FirstCameraOnly);
}
#[test]
fn protection_does_not_promote_preparing_ack_to_active_ack() {
    failed_handshake(AckMode::PreparingOnly);
}
#[test]
fn protection_rechecks_inventory_after_preparing_ack() {
    failed_handshake(AckMode::ChangeInventory);
}
#[test]
fn protection_rechecks_session_after_preparing_ack() {
    failed_handshake(AckMode::ChangeSession);
}
#[test]
fn protection_retains_gates_when_health_probe_fails_after_preparing() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.probe_failure = true);
    let responder = Responder::start(&f, AckMode::All);
    error(
        recording::protect(&f.ctl, &f.inventory, &f.generation),
        "RECORDING_PREPARE_FAILED",
    );
    f.assert_retained();
    assert!(
        responder
            .phases
            .lock()
            .unwrap()
            .iter()
            .all(|(_, phase)| *phase == GatePhase::Preparing)
    );
    drop(responder);
    f.release();
}
#[test]
fn protection_refuses_wrong_or_read_only_gate_mount_before_writing_guard() {
    for (key, value) in [
        ("RW", json!(false)),
        ("Source", json!("/unowned")),
        ("Destination", json!("/wrong")),
    ] {
        let f = Fixture::new(false, 1);
        f.edit(|d| d.cameras[0].container["Mounts"][0][key] = value);
        error(
            recording::protect(&f.ctl, &f.inventory, &f.generation),
            "RECORDING_PERSISTENT_GATE_REQUIRED",
        );
        assert!(f.ctl.unprotected().is_ok());
        assert!(!f.gate("camera_a").join("control.json").exists());
    }
}

#[test]
fn quality_queries_return_current_events_and_finalized_summary_for_both_backends() {
    for official in [false, true] {
        let f = Fixture::new(official, 1);
        let current = f.query("current").unwrap();
        assert_eq!(current["worker_ready"], true);
        assert_eq!(
            current["session_kind"],
            if official { "observer" } else { "sdk-worker" }
        );
        assert_eq!(current["result"]["quality"]["version"], 1);
        let events = f.query("events").unwrap();
        assert_eq!(
            events["result"]["page"]["events"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            f.query("session-summary").unwrap()["result"]["available"],
            false
        );
        f.edit(|d| {
            let b = &mut d.cameras[0].bundle;
            b["status"]["ready"] = json!(false);
            let summary =
                json!({"version":1,"session_id":b["status"]["session_id"],"finalized":true});
            b["current"]["quality"]["current_session_finalized"] = json!(true);
            b["current"]["quality"]["finalized_sessions"] = json!([summary]);
            b["summary"] =
                json!({"version":1,"instance_id":b["status"]["instance_id"],"summary":summary});
        });
        let result = f.query("session-summary").unwrap();
        assert_eq!(result["result"]["available"], true);
        assert_eq!(result["worker_ready"], false);
    }
}
#[test]
fn quality_rejects_session_or_inventory_changes_between_reads() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.quality_drift = true);
    error(f.query("current"), "QUALITY_SESSION_CHANGED");
    let f = Fixture::new(false, 1);
    f.edit(|d| d.inventory_on_quality = true);
    error(f.query("current"), "QUALITY_SESSION_CHANGED");
}
#[test]
fn quality_rejects_wrong_identity_and_inconsistent_event_revisions() {
    for (key, value) in [
        ("generation", json!("e".repeat(32))),
        ("config_sha256", json!("e".repeat(64))),
        ("pid", json!(0)),
        ("updated_unix_ms", json!(now() * 1000. + 60000.)),
    ] {
        let f = Fixture::new(false, 1);
        f.edit(|d| d.cameras[0].bundle["status"][key] = value);
        error(
            f.query("current"),
            if key == "updated_unix_ms" {
                "QUALITY_STATUS_CLOCK_INVALID"
            } else {
                "QUALITY_BINDING_MISMATCH"
            },
        );
    }
    let f = Fixture::new(false, 1);
    f.edit(|d| d.cameras[0].bundle["current"]["session_id"] = json!("f".repeat(32)));
    error(f.query("current"), "QUALITY_BINDING_MISMATCH");
    let f = Fixture::new(false, 1);
    f.edit(|d| d.cameras[0].bundle["events"]["latest_sequence"] = json!(2));
    error(f.query("events"), "QUALITY_EVIDENCE_CHANGED");
}
#[test]
fn quality_stale_heartbeat_frames_or_dead_processes_cannot_report_ready() {
    for official in [false, true] {
        let f = Fixture::new(official, 1);
        f.edit(|d| {
            d.cameras[0].bundle["status"]["updated_unix_ms"] = json!(now() * 1000. - 60000.)
        });
        assert_eq!(f.query("current").unwrap()["worker_ready"], false);
        let f = Fixture::new(official, 1);
        f.edit(|d| d.cameras[0].bundle["processes_alive"] = json!(false));
        assert_eq!(f.query("current").unwrap()["worker_ready"], false);
    }
    for age in [json!(-1), json!(2001), Value::Null] {
        let f = Fixture::new(false, 1);
        f.edit(|d| d.cameras[0].bundle["status"]["last_pair_age_ms"] = age);
        assert_eq!(f.query("current").unwrap()["worker_ready"], false);
    }
}
#[test]
fn quality_private_read_errors_are_sanitized_and_invalid_queries_rejected() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.quality_read_failure = true);
    assert_eq!(
        f.query("current").unwrap_err().to_string(),
        "QUALITY_PRIVATE_EVIDENCE_UNAVAILABLE"
    );
    let f = Fixture::new(false, 1);
    error(f.query("unknown"), "QUALITY_QUERY_KIND");
    error(
        managed_quality::query(&f.ctl, "missing", "current", None, 1),
        "QUALITY_CAMERA_UNAVAILABLE",
    );
}
#[test]
fn recording_context_distinguishes_configured_capability_from_active_protection() {
    let f = Fixture::new(false, 1);
    let context = managed_quality::recording_context(&f.ctl, true).unwrap();
    assert_eq!(context["protection"]["strong_guarantee_available"], true);
    assert_eq!(context["protection"]["session_bound"], false);
    assert_eq!(context["protection"]["native_gate_ack_verified"], false);
    assert_eq!(context["physical_accuracy_verified"], false);
    f.edit(|d| d.cameras[0].container["Mounts"][0]["RW"] = json!(false));
    error(
        managed_quality::recording_context(&f.ctl, true),
        "RECORDING_NATIVE_GATE_UNAVAILABLE",
    );
    assert_eq!(
        managed_quality::recording_context(&f.ctl, false).unwrap()["protection"]["strong_guarantee_available"],
        false
    );
    let f = Fixture::new(true, 1);
    error(
        managed_quality::recording_context(&f.ctl, true),
        "RECORDING_STRONG_GUARANTEE_UNSUPPORTED",
    );
    let context = managed_quality::recording_context(&f.ctl, false).unwrap();
    assert_eq!(context["protection"]["native_restart_gate"], false);
    assert_eq!(context["cameras"][0]["session_kind"], "observer");
}
#[test]
fn recording_context_rechecks_session_after_collecting_evidence() {
    let f = Fixture::new(false, 1);
    f.edit(|d| d.status_drift_at = Some(4));
    error(
        managed_quality::recording_context(&f.ctl, true),
        "RECORDING_SESSION_CHANGED",
    );
}

#[test]
fn protection_rechecks_inventory_after_active_ack_before_persisting_active_guard() {
    failed_handshake(AckMode::ChangeInventoryOnActive);
}
#[test]
fn protection_rechecks_session_after_active_ack_before_persisting_active_guard() {
    failed_handshake(AckMode::ChangeSessionOnActive);
}
