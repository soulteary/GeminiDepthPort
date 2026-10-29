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
    probe_failure_camera: Option<String>,
    stop_failure: bool,
    calibration_request: Option<Value>,
    calibration_fault: Option<&'static str>,
}
pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    pub(crate) ctl: Controller,
    pub(crate) inventory: Value,
    pub(crate) generation: Value,
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
    pub(crate) fn new(official: bool, count: usize) -> Self {
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
            let container = json!({"Id":format!("{:064x}",i+100),"Config":{"Labels":{"com.docker.compose.project":"orbbec-multicam","com.docker.compose.oneoff":"False","orbbec.role":"camera","orbbec.generation":"d".repeat(32),"orbbec.camera":name}},"State":{"Running":true,"Status":"running","Health":{"Status":"healthy"}},"Mounts":[{"Destination":"/recording","Source":gate,"RW":true}]});
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
        let patch: Value =
            serde_json::from_str(include_str!("../patches/public-orbbec/manifest.json")).unwrap();
        atomic_json(&root.join("patches/public-orbbec/manifest.json"), &patch).unwrap();
        let image = json!([{"Config":{"Labels":{"org.opencontainers.image.revision":inventory["upstream_commit"],"org.geminidepthport.explicit-bindings":"1","org.geminidepthport.native-yuyv":"1","org.geminidepthport.native-yuyv-patch":patch["patch_sha256"],"org.geminidepthport.communication-profile":config::COMMUNICATION,"org.geminidepthport.official-observer":"v1"}}}]);
        let state = docker.clone();
        let command_root = root.clone();
        let mut ctl = Controller::new(&root).unwrap();
        ctl.command_runner = Some(Arc::new(move |args, input| {
            let mut d = state.lock().unwrap();
            d.calls.push(args.to_vec());
            match args.get(1).map(String::as_str) {
                Some("ps")
                    if args
                        .iter()
                        .any(|a| a.contains("orbbec.web.preview") || a == "--format") =>
                {
                    Ok(String::new())
                }
                Some("ps") => Ok(d
                    .cameras
                    .iter()
                    .map(|c| text(&c.container["Id"]))
                    .collect::<Vec<_>>()
                    .join("\n")),
                Some("inspect") => Ok(serde_json::to_string(
                    &d.cameras
                        .iter()
                        .filter(|c| args[2..].iter().any(|id| c.container["Id"] == *id))
                        .map(|c| &c.container)
                        .collect::<Vec<_>>(),
                )?),
                Some("image") => Ok(image.to_string()),
                Some("info") | Some("build") | Some("run") => Ok("synthetic success".into()),
                Some("logs") => Ok("synthetic log".into()),
                Some("stop") => {
                    if d.stop_failure {
                        bail!("SYNTHETIC_STOP_FAILURE");
                    }
                    for c in &mut d.cameras {
                        if args.last().is_some_and(|id| c.container["Id"] == *id) {
                            c.container["State"]["Running"] = json!(false);
                        }
                    }
                    Ok(String::new())
                }
                Some("rm") => {
                    d.cameras
                        .retain(|c| !args.last().is_some_and(|id| c.container["Id"] == *id));
                    Ok(String::new())
                }
                Some("compose") if args.iter().any(|a| a == "/app/health.py") => {
                    if d.probe_failure
                        || d.probe_failure_camera
                            .as_ref()
                            .is_some_and(|name| args.iter().any(|a| a == &format!("camera_{name}")))
                    {
                        bail!("SYNTHETIC_PROBE_FAILURE");
                    }
                    Ok(String::new())
                }
                Some("compose") => {
                    let i = d
                        .cameras
                        .iter()
                        .position(|c| args.iter().any(|a| a == &format!("camera_{}", c.name)));
                    if args.iter().any(|a| a == "ps") {
                        return Ok(i
                            .map(|i| text(&d.cameras[i].container["Id"]).to_string())
                            .unwrap_or_default());
                    }
                    if args.iter().any(|a| a == "up") {
                        d.cameras[i.expect("known service")].container["State"]["Running"] =
                            json!(true);
                    }
                    Ok(String::new())
                }
                Some("exec") if args.iter().any(|a| a == "/app/calibration_export.py") => {
                    let request: Value = serde_json::from_slice(
                        input.expect("calibration request must be private stdin"),
                    )?;
                    assert_eq!(request["kind"], crate::calibration::KIND);
                    d.calibration_request = Some(request);
                    if d.calibration_fault == Some("collect") {
                        bail!("synthetic collector failed");
                    }
                    Ok(String::new())
                }
                Some("exec") if args.get(3).is_some_and(|a| a == "cat" || a == "rm") => {
                    if args[3] == "rm" {
                        return Ok(String::new());
                    }
                    if d.calibration_fault == Some("read") {
                        bail!("synthetic evidence unreadable");
                    }
                    if d.calibration_fault == Some("malformed") {
                        return Ok("bad json".into());
                    }
                    let mut evidence =
                        calibration_observations(d.calibration_request.as_ref().unwrap());
                    if d.calibration_fault == Some("binding") {
                        evidence["request_sha256"] = json!("0".repeat(64));
                    }
                    if d.calibration_fault == Some("identity") {
                        d.cameras[0].container["Id"] = json!("9".repeat(64));
                    }
                    if d.calibration_fault == Some("group") {
                        d.cameras[0].container["State"]["Running"] = json!(false);
                    }
                    Ok(evidence.to_string())
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
    pub(crate) fn install_rust_runtime(&mut self) -> Value {
        let manifest = json!({"version":1,"upstream_commit":crate::capture_backend::SOURCE_COMMIT,"sdk_version":"2.10.6","communication_profile":config::COMMUNICATION,"sdk_sha256":"a".repeat(64),"bridge_sha256":"b".repeat(64),"service_sha256":"c".repeat(64),"files":{"/usr/local/bin/capture-service":"c".repeat(64),"/opt/orbbec-sdk/lib/libOrbbecSDK.so":"a".repeat(64),"/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so":"b".repeat(64),"/opt/gdp-ros/bin/gdp-usb-recover":"e".repeat(64)},"usb_recovery":{"helper":"/opt/gdp-ros/bin/gdp-usb-recover","sha256":"e".repeat(64)},"sdk_source":{"repository":"https://github.com/orbbec/OrbbecSDK_ROS2","commit":crate::capture_backend::SOURCE_COMMIT,"license_files":{"/opt/third-party/orbbec/LICENSE":"f".repeat(64)}}});
        let image = json!([{"Id":format!("sha256:{}","a".repeat(64)),"Config":{"Labels":{"org.opencontainers.image.revision":self.inventory["upstream_commit"],"org.geminidepthport.capture-backend":"rust-v1","org.geminidepthport.explicit-bindings":"1","org.geminidepthport.communication-profile":config::COMMUNICATION}}}]);
        let raw = manifest.to_string();
        let previous = self.ctl.command_runner.clone().unwrap();
        self.ctl.command_runner = Some(Arc::new(move |args, input| {
            if args.get(1).is_some_and(|a| a == "image") {
                return Ok(image.to_string());
            }
            if args.get(1).is_some_and(|a| a == "run")
                && args
                    .last()
                    .is_some_and(|a| a == "/opt/gdp-capture/runtime.json")
            {
                return Ok(raw.clone());
            }
            previous(args, input)
        }));
        let runtime = crate::capture_backend::runtime(&self.ctl, &self.inventory).unwrap();
        self.generation["capture_runtime"] = runtime.clone();
        let path = self
            .ctl
            .root
            .join("state/generations")
            .join(text(&self.generation["generation"]))
            .join("manifest.json");
        atomic_json(&path, &self.generation).unwrap();
        atomic_json(&self.ctl.root.join("state/current-generation.json"),&json!({"generation":self.generation["generation"],"manifest_sha256":file_hash(&path).unwrap()})).unwrap();
        runtime
    }
    pub(crate) fn prepare_host(&mut self) {
        let host = self.ctl.root.join("host");
        for path in ["dev/bus/usb", "run/udev", "etc/udev/rules.d"] {
            private_dir(&host.join(path)).unwrap();
        }
        atomic(
            &host.join("etc/udev/rules.d/99-obsensor-libusb.rules"),
            b"# synthetic",
        )
        .unwrap();
        for (i, camera) in config::active(&self.inventory).iter().enumerate() {
            let path = host.join("sys/bus/usb/devices").join(format!("1-{i}"));
            private_dir(&path).unwrap();
            for (key, value) in [
                ("idVendor", "2bc5"),
                ("serial", text(&camera["serial_number"])),
                (
                    "product",
                    text(&config::resolved(&self.inventory, camera)["model"]),
                ),
            ] {
                atomic(&path.join(key), value.as_bytes()).unwrap();
            }
        }
        self.ctl.host_root = Some(host);
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

#[test]
fn controlled_stop_finishes_every_owned_instance_and_persists_partial_failures() {
    let f = Fixture::new(false, 2);
    assert_eq!(f.ctl.stop("camera", true).unwrap()["status"], "stopped");
    assert!(f.docker.lock().unwrap().cameras.is_empty());
    let f = Fixture::new(false, 2);
    f.edit(|d| d.stop_failure = true);
    error(f.ctl.stop("camera", false), "STOP_FAILED");
    let state = read_json(&f.ctl.root.join("state/lifecycle.json")).unwrap();
    assert_eq!(state["status"], "failed");
    assert_eq!(
        f.docker
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|a| a.get(1).is_some_and(|v| v == "stop"))
            .count(),
        2
    );
}
#[test]
fn health_checks_require_a_real_owned_running_healthy_instance() {
    let f = Fixture::new(false, 1);
    f.ctl.healthy("camera_camera_a", 1).unwrap();
    f.edit(|d| {
        d.cameras[0].container["State"]["Status"] = json!("exited");
        d.cameras[0].container["State"]["Running"] = json!(false);
    });
    error(f.ctl.healthy("camera_camera_a", 1), "CAMERA_HEALTH_FAILED");
    error(f.ctl.healthy("camera_missing", 1), "CAMERA_MISSING");
}
#[test]
fn migration_requires_exact_plan_and_preserves_a_private_byte_identical_backup() {
    let temp = tempfile::tempdir().unwrap();
    let mut ctl = Controller::new(temp.path()).unwrap();
    let original = include_str!("../tests/fixtures/inventory.v1.json");
    atomic(&ctl.root.join("inventory.json"), original.as_bytes()).unwrap();
    ctl.command_runner = Some(Arc::new(|a, _| {
        assert_eq!(a[1], "ps");
        Ok(String::new())
    }));
    let plan = ctl.migrate(&Value::Null, false, "").unwrap();
    assert_eq!(plan["applied"], false);
    assert_eq!(
        fs::read(ctl.root.join("inventory.json")).unwrap(),
        original.as_bytes()
    );
    error(ctl.migrate(&Value::Null, true, "wrong"), "MIGRATION_STALE");
    let applied = ctl
        .migrate(&Value::Null, true, text(&plan["plan_digest"]))
        .unwrap();
    assert_eq!(applied["backup_created"], true);
    assert_eq!(ctl.load().unwrap()["schema_version"], 2);
    let backup = fs::read_dir(ctl.root.join("state/migrations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(fs::read(backup).unwrap(), original.as_bytes());
}
#[test]
fn image_contract_rejects_revision_binding_and_builds_without_device_access() {
    let f = Fixture::new(true, 1);
    f.ctl.image_contract(&f.inventory).unwrap();
    let mut wrong = f.inventory.clone();
    wrong["upstream_commit"] = json!("0".repeat(40));
    error(f.ctl.image_contract(&wrong), "IMAGE_REVISION");
    let mut rust = f.inventory.clone();
    rust["capture_backend"] = json!("rust");
    error(f.ctl.image_contract(&rust), "IMAGE_CONTRACT_UNSUPPORTED");
    assert_eq!(
        f.ctl.build(&f.inventory).unwrap()["log"],
        "synthetic success"
    );
}
#[cfg(target_os = "linux")]
#[test]
fn group_start_is_sequential_reuses_healthy_instances_and_protects_the_observed_session() {
    let mut f = Fixture::new(true, 3);
    f.prepare_host();
    f.edit(|d| {
        for c in &mut d.cameras {
            c.container["State"]["Running"] = json!(false)
        }
    });
    assert_eq!(f.ctl.scan().unwrap().as_array().unwrap().len(), 3);
    let started = f.ctl.start(&f.inventory).unwrap();
    assert_eq!(started["ready"], true);
    assert_eq!(started["reused"], false);
    let calls = f.docker.lock().unwrap().calls.clone();
    let ups = calls
        .iter()
        .filter(|a| a.iter().any(|v| v == "up"))
        .collect::<Vec<_>>();
    assert_eq!(ups.len(), 3);
    for (i, call) in ups.iter().enumerate() {
        assert_eq!(
            call.last().unwrap(),
            &format!("camera_camera_{}", char::from(b'a' + i as u8))
        );
    }
    assert_eq!(f.ctl.ensure_started(&f.inventory).unwrap()["reused"], true);
    assert_eq!(
        f.docker
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|a| a.iter().any(|v| v == "up"))
            .count(),
        3
    );
    let protection = f.ctl.protect().unwrap();
    assert_eq!(protection["native_restart_gate"], false);
    assert_eq!(protection["guard_version"], 1);
    f.ctl.release(text(&protection["token"])).unwrap();
}
#[cfg(target_os = "linux")]
#[test]
fn failed_middle_camera_never_starts_later_camera_or_clears_diagnostic_instances() {
    let mut f = Fixture::new(true, 3);
    f.prepare_host();
    f.edit(|d| {
        for c in &mut d.cameras {
            c.container["State"]["Running"] = json!(false)
        }
        d.probe_failure_camera = Some("camera_b".into());
    });
    assert!(f.ctl.start(&f.inventory).is_err());
    let state = read_json(&f.ctl.root.join("state/lifecycle.json")).unwrap();
    assert_eq!(state["ready"], false);
    assert_eq!(state["status"], "failed");
    assert_eq!(state["cameras"][0]["status"], "healthy");
    assert_eq!(state["cameras"][1]["status"], "failed");
    assert_eq!(state["cameras"][2]["status"], "not_started");
    let d = f.docker.lock().unwrap();
    assert_eq!(d.cameras[0].container["State"]["Running"], true);
    assert_eq!(d.cameras[1].container["State"]["Running"], true);
    assert_eq!(d.cameras[2].container["State"]["Running"], false);
}
#[cfg(target_os = "linux")]
#[test]
fn preflight_refuses_missing_udev_rules_and_missing_bound_device() {
    let mut f = Fixture::new(true, 1);
    f.prepare_host();
    fs::remove_file(
        f.ctl
            .host_root
            .as_ref()
            .unwrap()
            .join("etc/udev/rules.d/99-obsensor-libusb.rules"),
    )
    .unwrap();
    error(f.ctl.preflight(&f.inventory), "DEVICE_ACCESS");
    let mut f = Fixture::new(true, 1);
    f.prepare_host();
    fs::remove_dir_all(
        f.ctl
            .host_root
            .as_ref()
            .unwrap()
            .join("sys/bus/usb/devices"),
    )
    .unwrap();
    error(f.ctl.preflight(&f.inventory), "DEVICE_UNAVAILABLE");
}

fn calibration_observations(request: &Value) -> Value {
    let mut cameras = json!({});
    for camera in request["cameras"].as_array().unwrap() {
        let mut info = json!({});
        for (stream, expected) in camera["streams"].as_object().unwrap() {
            info[stream] = json!({"frame_id":expected["frame_id"],"width":expected["width"],"height":expected["height"],"k":[500.,0.,320.,0.,500.,240.,0.,0.,1.],"r":[1.,0.,0.,0.,1.,0.,0.,0.,1.],"p":[500.,0.,320.,0.,0.,500.,240.,0.,0.,0.,1.,0.],"d":[0.,0.,0.,0.,0.],"distortion_model":"plumb_bob"});
        }
        info["depth_to_color"] = json!({"source_frame":camera["streams"]["depth"]["frame_id"],"target_frame":camera["streams"]["color"]["frame_id"],"translation_unit":"m","rotation_representation":"quaternion_xyzw","translation":[0.02,0.,0.],"rotation":[0.,0.,0.,1.]});
        cameras[text(&camera["name"])] = info;
    }
    json!({"source":"offline-evidence","request_sha256":crate::calibration::digest(request),"cameras":cameras})
}
#[test]
fn calibration_export_binds_private_request_to_all_cameras_and_always_cleans_remote_evidence() {
    let f = Fixture::new(false, 2);
    let result = crate::calibration::export(&f.ctl).unwrap();
    assert_eq!(result["camera_count"], 2);
    assert_eq!(result["physical_accuracy_verified"], false);
    let file = f
        .ctl
        .root
        .join("state/calibration")
        .join(format!("{}.json", text(&result["snapshot_sha256"])));
    let record = read_json(&file).unwrap();
    assert_eq!(record["generation"], f.generation["generation"]);
    assert_eq!(record["cameras"].as_array().unwrap().len(), 2);
    let d = f.docker.lock().unwrap();
    let request = d.calibration_request.as_ref().unwrap();
    assert_eq!(
        record["request_sha256"],
        crate::calibration::digest(request)
    );
    let collect = d
        .calls
        .iter()
        .find(|a| a.iter().any(|a| a == "/app/calibration_export.py"))
        .unwrap();
    let cleanup = d
        .calls
        .iter()
        .find(|a| a.get(3).is_some_and(|a| a == "rm"))
        .unwrap();
    assert_eq!(collect.last(), cleanup.last());
}
#[test]
fn calibration_collection_read_binding_and_identity_failures_never_publish_a_snapshot() {
    for fault in [
        "collect",
        "read",
        "malformed",
        "binding",
        "identity",
        "group",
    ] {
        let f = Fixture::new(false, 2);
        f.edit(|d| d.calibration_fault = Some(fault));
        assert!(crate::calibration::export(&f.ctl).is_err(), "{fault}");
        assert!(!f.ctl.root.join("state/calibration").exists());
        assert!(
            f.docker
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|a| a.get(3).is_some_and(|a| a == "rm")),
            "cleanup missing after {fault}"
        );
    }
}
#[test]
fn calibration_requires_complete_group_before_collecting_any_evidence() {
    let f = Fixture::new(false, 2);
    f.edit(|d| d.cameras[0].container["State"]["Running"] = json!(false));
    error(crate::calibration::export(&f.ctl), "GROUP_INCOMPLETE");
    assert!(f.docker.lock().unwrap().calibration_request.is_none());
}
#[test]
fn calibration_rejects_cropped_unrectified_and_invalid_distortion_models() {
    let f = Fixture::new(false, 1);
    let request =
        crate::calibration::request(&f.inventory, text(&f.generation["generation"])).unwrap();
    for (key, value) in [
        ("binning_x", json!(2)),
        ("roi", json!({"x_offset":1})),
        ("r", json!([1., 0., 0., 0., -1., 0., 0., 0., 1.])),
        ("k", json!([0., 0., 0., 0., 0., 0., 0., 0., 1.])),
        ("distortion_model", json!("unsupported")),
        ("d", json!([0.])),
    ] {
        let mut observed = calibration_observations(&request);
        observed["cameras"]["camera_a"]["color"][key] = value;
        assert!(
            crate::calibration::snapshot(
                &f.inventory,
                text(&f.generation["generation"]),
                &observed
            )
            .is_err(),
            "{key}"
        );
    }
}

#[test]
fn native_image_contract_and_service_configuration_bind_the_usb_recovery_artifact() {
    let mut f = Fixture::new(false, 1);
    let runtime = f.install_rust_runtime();
    f.ctl.image_contract(&f.inventory).unwrap();
    let camera = config::resolved(&f.inventory, &f.inventory["cameras"][0]);
    let service = crate::capture_backend::service_config(
        &f.inventory,
        &camera,
        text(&f.generation["generation"]),
        &runtime,
    );
    assert_eq!(service["usb_recovery"], runtime["manifest"]["usb_recovery"]);
    assert_eq!(
        service["runtime_contract"]["sdk_sha256"],
        runtime["manifest"]["sdk_sha256"]
    );
}
#[cfg(target_os = "linux")]
#[test]
fn native_preflight_requires_the_frozen_runtime_instead_of_mutable_image_labels() {
    let mut f = Fixture::new(false, 1);
    f.prepare_host();
    f.install_rust_runtime();
    f.ctl.preflight(&f.inventory).unwrap();
    f.generation["capture_runtime"]["image_id"] = json!(format!("sha256:{}", "0".repeat(64)));
    let path = f
        .ctl
        .root
        .join("state/generations")
        .join(text(&f.generation["generation"]))
        .join("manifest.json");
    atomic_json(&path, &f.generation).unwrap();
    atomic_json(&f.ctl.root.join("state/current-generation.json"),&json!({"generation":f.generation["generation"],"manifest_sha256":file_hash(&path).unwrap()})).unwrap();
    error(f.ctl.preflight(&f.inventory), "RUST_RUNTIME_DRIFT");
}

#[test]
fn ensure_started_persists_preparation_failure_before_attempting_start() {
    let f = Fixture::new(true, 1);
    let pointer = f.ctl.root.join("state/current-generation.json");
    let original = fs::read(&pointer).unwrap();
    fs::remove_file(&pointer).unwrap();
    let error = f.ctl.ensure_started(&f.inventory).unwrap_err().to_string();
    let result = read_json(&f.ctl.root.join("state/lifecycle.json")).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"], error.split(':').next().unwrap());
    atomic(&pointer, &original).unwrap();
}
