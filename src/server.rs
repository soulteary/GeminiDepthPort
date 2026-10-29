//! Loopback management HTTP API, background tasks and preview leases.
//!
//! Mutating handlers coordinate the control lock and recording protection;
//! long-running operations report task results rather than imply readiness from
//! request acceptance. Preview leases expire independently of browser lifetime.
//! Keep producer quality facts distinct from downstream recording validity.
use crate::{
    config,
    control::{Controller, PROJECT},
    generation,
    util::*,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use subtle::ConstantTimeEq;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
#[cfg(test)]
type JobReadHook = Box<dyn FnOnce(&Manager) + Send>;
struct Manager {
    ctl: Controller,
    token: String,
    busy: AtomicBool,
    active_job: AtomicBool,
    last_seen: Mutex<Instant>,
    job: Mutex<Value>,
    #[cfg(test)]
    state_after_job_read: Mutex<Option<JobReadHook>>,
}
struct Busy<'a>(&'a AtomicBool);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Manager {
    fn busy(&self) -> Result<Busy<'_>> {
        require(
            self.busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "CONTROL_BUSY",
            "Another operation is running",
        )?;
        Ok(Busy(&self.busy))
    }
}
pub fn role<'a>(data: &'a Value, name: &str) -> Result<&'a Value> {
    data["cameras"]
        .as_array()
        .and_then(|v| v.iter().find(|c| c["name"] == name))
        .ok_or_else(|| anyhow::anyhow!("CAMERA_ID: Unknown camera role"))
}
fn preview_name(role: &str) -> String {
    format!("orbbec-web-preview-{role}")
}
fn stop_owned_previews(ctl: &Controller) -> Result<()> {
    for item in ctl.instances("preview")? {
        if ctl.owns_preview(&item) {
            let id = text(&item["Id"]);
            if item["State"]["Running"] == true {
                ctl.run(&["docker", "stop", "--time", "12", id])?;
            }
            ctl.run(&["docker", "rm", id])?;
        }
    }
    Ok(())
}
fn managed(ctl: &Controller, name: &str) -> Result<Option<Value>> {
    let container = preview_name(name);
    let items = ctl.instances("preview")?;
    let item = items
        .into_iter()
        .find(|v| text(&v["Name"]) == format!("/{container}"));
    if let Some(v) = item {
        require(
            ctl.owns_preview(&v),
            "INSTANCE_OWNERSHIP",
            "Preview belongs to another deployment root",
        )?;
        return Ok(Some(v));
    } // A conflicting name must never be removed or reused.
    let raw = ctl.run(&[
        "docker",
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{container}$"),
        "--format",
        "{{.Names}}",
    ])?;
    require(
        raw.trim().is_empty(),
        "INSTANCE_OWNERSHIP",
        "Container name belongs to another workload",
    )?;
    Ok(None)
}
fn stop_preview(ctl: &Controller, name: &str) -> Result<()> {
    if let Some(v) = managed(ctl, name)? {
        let id = text(&v["Id"]);
        if v["State"]["Running"] == true {
            ctl.run(&["docker", "stop", "--time", "12", id])?;
        }
        ctl.run(&["docker", "rm", id])?;
    }
    Ok(())
}
fn save_config(ctl: &Controller, payload: &Value) -> Result<Value> {
    let (mut d, revision) = ctl.load_revision()?;
    require(
        payload["revision"] == revision,
        "INVENTORY_DRIFT",
        "Configuration changed; reload before saving",
    )?;
    config::validate(&d, true)?;
    let updates = payload["cameras"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("CAMERA_FIELDS: All cameras required"))?;
    let cameras = d["cameras"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("CONFIG_SHAPE: Missing cameras"))?;
    let mut names = std::collections::BTreeSet::new();
    require(
        updates.len() == cameras.len()
            && updates.iter().all(|u| {
                u.is_object()
                    && names.insert(text(&u["name"]))
                    && cameras.iter().any(|c| c["name"] == u["name"])
            }),
        "CAMERA_FIELDS",
        "All configured cameras required",
    )?;
    let found = ctl.scan()?;
    let mut changed = vec![];
    for update in updates {
        require(update.as_object().unwrap().keys().all(|k|"name serial_number enabled namespace frame_prefix timestamp_source calibration_revision health parameters".split_whitespace().any(|v|v==k)),"CAMERA_FIELDS","Unsupported editable field")?;
        let name = text(&update["name"]);
        let old = role(&d, name)?;
        let model = config::resolved(&d, old)["model"].clone();
        let before = config::signature(old);
        let p = update.get("parameters").cloned().unwrap_or(json!({}));
        require(p.as_object().is_some_and(|m|m.keys().all(|k|"enable_color color_width color_height color_fps color_format enable_depth depth_width depth_height depth_fps depth_format enable_color_auto_exposure color_exposure color_gain enable_ir_auto_exposure ir_exposure ir_gain".split_whitespace().any(|v|v==k))),"PARAMETER_FIELDS","Unsupported editable parameter")?;
        if let Some(serial) = update.get("serial_number") {
            if serial != &old["serial_number"] && !text(serial).is_empty() {
                require(
                    found
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v["serial_number"] == *serial && v["model"] == model),
                    "CAMERA_BINDING",
                    "Detected model must match configured profile",
                )?;
            }
        }
        let c = d["cameras"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| c["name"] == name)
            .unwrap();
        for (k, v) in update.as_object().unwrap() {
            if k != "name" && k != "parameters" {
                c[k] = v.clone();
            }
        }
        c["parameters"]
            .as_object_mut()
            .unwrap()
            .extend(p.as_object().unwrap().clone());
        validate_editable(&c["parameters"])?;
        if config::signature(c) != before {
            c["profiles_verified"] = false.into();
            if c.get("verification").is_some() {
                c["verification"] = Value::Null;
            }
            changed.push(name.to_string());
        }
    }
    config::validate(&d, true)?;
    // An unchanged save does not touch preview instances or require Docker.
    if !changed.is_empty() {
        require(
            ctl.instances("preview")?
                .iter()
                .all(|item| ctl.owns_preview(item)),
            "INSTANCE_OWNERSHIP",
            "Another deployment has a preview; release it before capture",
        )?;
    }
    for name in changed {
        stop_preview(ctl, &name)?;
    }
    ctl.save_at_revision(&d, false, &revision)?;
    Ok(json!({"message":"配置已保存；启用设备需完成实际档位检查，标定与语义验收另行执行。"}))
}
fn validate_editable(p: &Value) -> Result<()> {
    for stream in ["color", "depth"] {
        for (suffix, max) in [("width", 4096), ("height", 4096), ("fps", 120)] {
            if let Some(v) = p.get(format!("{stream}_{suffix}")) {
                require(
                    v.as_u64().is_some_and(|v| v > 0 && v <= max),
                    "STREAM_PROFILE",
                    "Image size or frame rate outside editable limits",
                )?;
            }
        }
        require(
            p.get(format!("{stream}_format"))
                .is_none_or(|v| ["MJPG", "RGB", "YUYV", "Y16", "ANY"].contains(&text(v))),
            "STREAM_PROFILE",
            "Unsupported editable image format",
        )?;
    }
    for key in ["enable_color_auto_exposure", "enable_ir_auto_exposure"] {
        require(
            p.get(key).is_none_or(Value::is_boolean),
            "PARAMETER_TYPE",
            "Exposure switches must be booleans",
        )?;
    }
    for key in ["color_exposure", "color_gain", "ir_exposure", "ir_gain"] {
        require(
            p.get(key)
                .is_none_or(|v| v.as_i64().is_some_and(|v| (-1..=1_000_000).contains(&v))),
            "PARAMETER_TYPE",
            "Exposure or gain outside editable limits",
        )?;
    }
    Ok(())
}
#[cfg(test)]
#[cfg(target_os = "linux")]
pub(crate) fn test_start_preview(ctl: &Controller, name: &str) -> Result<Value> {
    start_preview(ctl, name)
}
fn start_preview(ctl: &Controller, name: &str) -> Result<Value> {
    let d = ctl.load()?;
    config::validate(&d, true)?;
    let c = config::resolved(&d, role(&d, name)?);
    require(
        d["schema_version"] != 2 || c["enabled"] == true,
        "CAMERA_UNBOUND",
        "Enable and bind camera before preview",
    )?;
    require(
        !ctl.instances("camera")?
            .iter()
            .any(|v| v["State"]["Running"] == true),
        "CAMERAS_RUNNING",
        "Stop formal service before preview",
    )?;
    require(
        ctl.instances("preview")?
            .iter()
            .all(|item| ctl.owns_preview(item)),
        "INSTANCE_OWNERSHIP",
        "Another deployment has a preview; release it before capture",
    )?;
    let found = ctl.scan()?;
    let devices: Vec<_> = found
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["serial_number"] == c["serial_number"])
        .collect();
    require(
        devices.len() == 1 && devices[0]["model"] == c["model"],
        "CAMERA_BINDING",
        "Bound device absent or model differs",
    )?;
    if d["schema_version"] == 2 {
        ctl.image_contract(&d)?;
    }
    stop_preview(ctl, name)?;
    let folder = ctl.root.join("work/web-preview").join(name);
    let cfg = folder.join("config");
    let output = folder.join("out");
    private_dir(&folder)?;
    private_dir(&cfg)?;
    private_dir(&output)?;
    for entry in fs::read_dir(&output)? {
        let p = entry?.path();
        if p.is_file() {
            fs::remove_file(p)?;
        }
    }
    let (p, h, b) = generation::camera_files(&d, &c)?;
    atomic_json(&cfg.join("camera.yaml"), &p)?;
    atomic_json(&cfg.join("health.json"), &h)?;
    atomic_json(&cfg.join("binding.json"), &b)?;
    if crate::capture_backend::selected(&d) == "rust" {
        let runtime = crate::capture_backend::runtime(ctl, &d)?;
        atomic_json(&cfg.join("runtime-binding.json"), &runtime)?;
        atomic_json(&cfg.join("runtime.json"), &runtime["manifest"])?;
        atomic_json(
            &cfg.join("capture.json"),
            &crate::capture_backend::service_config(&d, &c, &random(16), &runtime),
        )?;
    }

    atomic(
        &cfg.join("revision"),
        config::camera_digest(&d, role(&d, name)?).as_bytes(),
    )?;
    atomic(&cfg.join("lease"), b"")?;
    let mut a = args(&[
        "docker",
        "run",
        "-d",
        "--init",
        "--user",
        &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }),
        "--name",
        &preview_name(name),
        "--label",
        &format!("orbbec.web.preview={PROJECT}"),
        "--network",
        "host",
        "--ipc",
        "private",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--read-only",
        "--tmpfs",
        "/tmp:size=256m",
        "--device-cgroup-rule",
        "c 189:* rwm",
        "-v",
        "/dev/bus/usb:/dev/bus/usb:rw",
        "-v",
        "/run/udev:/run/udev:ro",
        "-v",
        &format!("{}:/config:ro", cfg.display()),
        "-v",
        &format!("{}:/out:rw", output.display()),
    ]);
    let mut env = generation::communication(&d);
    env.as_object_mut().unwrap().extend(json!({"CAMERA_SCHEMA":d["schema_version"].to_string(),"CAMERA_NAMESPACE":c["namespace"],"CAMERA_ROLE":name,"CAMERA_NAME":c["frame_prefix"],"CAMERA_MODEL":c["model"],"CAMERA_LAUNCH":c["launch_file"],"HOME":"/tmp","ROS_HOME":"/tmp/ros"}).as_object().unwrap().clone());
    for (k, v) in env.as_object().unwrap() {
        a.extend(["-e".into(), format!("{k}={}", text(v))]);
    }
    a.extend(args(&[
        text(&d["image"]),
        "python3",
        "/app/preview_session.py",
    ]));
    ctl.run_vec(&a, 30)?;
    Ok(json!({"message":"预览容器已启动，等待实际图像。"}))
}
pub fn confirm(ctl: &Controller, name: &str) -> Result<Value> {
    let (mut d, revision) = ctl.load_revision()?;
    config::validate(&d, true)?;
    let c = role(&d, name)?;
    let folder = ctl.root.join("work/web-preview").join(name);
    require(
        fs::read_to_string(folder.join("config/revision"))? == config::camera_digest(&d, c),
        "CONFIRMATION_STALE",
        "Preview differs from saved configuration",
    )?;
    let instance = managed(ctl, name)?;
    require(
        instance
            .as_ref()
            .is_some_and(|v| v["State"]["Running"] == true),
        "PREVIEW_MISSING",
        "Start live preview before confirming",
    )?;
    let container_id = text(&instance.as_ref().unwrap()["Id"]).to_string();
    fs::OpenOptions::new()
        .write(true)
        .open(folder.join("config/lease"))?
        .set_modified(SystemTime::now())?;
    let status = read_json(&folder.join("out/status.json"))?;
    for stream in ["color", "depth"] {
        if c["parameters"][format!("enable_{stream}")] == true {
            let st = &status[stream];
            require(
                st["received_at"]
                    .as_f64()
                    .is_some_and(|v| (0.0..=3.0).contains(&(now() - v)))
                    && st["frames"].as_u64().is_some_and(|v| v >= 3),
                "PREVIEW_STALE",
                "Enabled streams must be fresh",
            )?;
            require(
                st["width"] == c["parameters"][format!("{stream}_width")]
                    && st["height"] == c["parameters"][format!("{stream}_height")],
                "STREAM_PROFILE",
                "Actual geometry differs",
            )?;
        }
    }
    ctl.run(&[
        "docker",
        "exec",
        &container_id,
        "/app/entrypoint.sh",
        "python3",
        "/app/health.py",
    ])?;
    let rust_runtime_digest = if crate::capture_backend::selected(&d) == "rust" {
        let observed = read_json(&folder.join("config/runtime-binding.json"))?;
        require(
            observed == crate::capture_backend::runtime(ctl, &d)?,
            "RUST_CONFIRMATION_STALE",
            "Capture image/runtime changed during preview",
        )?;
        Some(crate::capture_backend::runtime_digest(&observed))
    } else {
        None
    };
    require(
        managed(ctl, name)?
            .is_some_and(|v| v["Id"] == container_id && v["State"]["Running"] == true),
        "CONFIRMATION_STALE",
        "Preview instance changed during the runtime probe",
    )?;
    let digest = config::camera_digest(&d, c);
    let schema = d["schema_version"].clone();
    let c = d["cameras"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|c| c["name"] == name)
        .unwrap();
    c["profiles_verified"] = true.into();
    if schema == 2 {
        c["verification"] = json!({"method":"runtime-probe","configuration_digest":digest});
        if let Some(runtime_digest) = rust_runtime_digest {
            c["verification"]["runtime_digest"] = runtime_digest.into();
        }
    }
    ctl.save_at_revision(&d, true, &revision)?;
    Ok(json!({"message":"实际图像检查通过，档位已确认。"}))
}
pub fn persist_job(ctl: &Controller, job: &Value) -> Result<()> {
    let id = text(&job["id"]);
    require(
        matches(r"^[0-9a-f]{12}$", id),
        "JOB_INVALID",
        "Invalid job identifier",
    )?;
    let folder = ctl.root.join("state/jobs");
    atomic_json(&folder.join(format!("{id}.json")), job)?;
    atomic_json(&ctl.root.join("state/latest-job.json"), job)?;
    let mut entries: Vec<_> = fs::read_dir(folder)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|v| v == "json"))
        .collect();
    entries.sort_by_key(|e| {
        std::cmp::Reverse(
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    });
    for e in entries.into_iter().skip(50) {
        fs::remove_file(e.path())?;
    }
    Ok(())
}
fn finish_job_result(job: &mut Value, result: Result<String>) {
    match result {
        Ok(out) => {
            job["status"] = "passed".into();
            if job["log"].as_array().is_some_and(|v| v.is_empty()) {
                job["log"] = json!(
                    out.lines()
                        .rev()
                        .take(1200)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                );
            }
        }
        Err(e) => {
            job["status"] = "failed".into();
            job["log"]
                .as_array_mut()
                .unwrap()
                .push(json!(e.to_string()));
        }
    }
}
fn start_job(m: &Arc<Manager>, action: &str) -> Result<Value> {
    require(
        ["build", "render", "preflight", "up", "check", "down"].contains(&action),
        "JOB_INVALID",
        "Unknown action",
    )?;
    let guard = m.busy()?;
    let control = m.ctl.lock(true)?;
    let job = json!({"id":random(6),"action":action,"status":"running","log":[],"started_at":now(),"inventory_revision":m.ctl.revision()?});
    persist_job(&m.ctl, &job)?;
    *m.job.lock().unwrap() = job.clone();
    m.active_job.store(true, Ordering::Release);
    drop(control);
    std::mem::forget(guard);
    let response = json!({"job_id":job["id"]});
    let m = Arc::clone(m);
    let action = action.to_string();
    thread::spawn(move || {
        let _busy = Busy(&m.busy);
        let mut job = job;
        let live = Arc::clone(&m);
        let sink: LogSink = Arc::new(move |line| {
            let mut current = live.job.lock().unwrap();
            if let Some(log) = current["log"].as_array_mut() {
                log.push(json!(line));
                if log.len() > 1200 {
                    let excess = log.len() - 1200;
                    log.drain(..excess);
                }
            }
        });
        let result = env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| {
                command_logged(
                    &m.ctl.root,
                    &args(&[
                        exe.to_str().unwrap(),
                        "--root",
                        m.ctl.root.to_str().unwrap(),
                        &action,
                    ]),
                    if action == "build" {
                        3600
                    } else if action == "up" {
                        2400
                    } else {
                        600
                    },
                    None,
                    Some(sink),
                )
            });
        job["log"] = m.job.lock().unwrap()["log"].clone();
        finish_job_result(&mut job, result);
        job["finished_at"] = now().into();
        let _ = persist_job(&m.ctl, &job);
        *m.job.lock().unwrap() = job;
        m.active_job.store(false, Ordering::Release);
    });
    Ok(response)
}
fn state(m: &Manager) -> Result<Value> {
    *m.last_seen.lock().unwrap() = Instant::now();
    let d = m.ctl.load()?;
    config::validate(&d, true)?;
    for c in d["cameras"].as_array().unwrap() {
        let lease = m
            .ctl
            .root
            .join("work/web-preview")
            .join(text(&c["name"]))
            .join("config/lease");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(lease) {
            let _ = file.set_modified(SystemTime::now());
        }
    }
    if !m.active_job.load(Ordering::Acquire) {
        if let Ok(mut job) = read_json(&m.ctl.root.join("state/latest-job.json")) {
            #[cfg(test)]
            if let Some(hook) = m.state_after_job_read.lock().unwrap().take() {
                hook(m);
            }
            if job["status"] == "running" {
                if let Ok(_lock) = m.ctl.lock(false) {
                    if !m.active_job.load(Ordering::Acquire) {
                        job = read_json(&m.ctl.root.join("state/latest-job.json"))?;
                        if job["status"] == "running" {
                            job["status"] = "failed".into();
                            job["finished_at"] = now().into();
                            if let Some(log) = job["log"].as_array_mut() {
                                log.push(json!("Startup worker stopped before completion; inspect actual service state."));
                            }
                            persist_job(&m.ctl, &job)?;
                        }
                    }
                }
            }
            if !m.active_job.load(Ordering::Acquire) {
                *m.job.lock().unwrap() = job;
            }
        }
    }
    let current = generation::load(&m.ctl).ok().flatten().is_some_and(|s| {
        s["draft"] == false && s["inventory_sha256"] == m.ctl.revision().unwrap_or_default()
    });
    let mut result = json!({"generated_current":current,"inventory":d,"revision":m.ctl.revision()?,"devices":m.ctl.scan()?,"platform":if cfg!(target_os="linux"){"Linux"}else if cfg!(target_os="macos"){"Darwin"}else{"Unknown"},"job":m.job.lock().unwrap().clone(),"previews":{},"services":[],"docker_error":null});
    let outcome = (|| {
        let raw = m.ctl.run_vec(
            &args(&[
                "docker",
                "ps",
                "-a",
                "--filter",
                &format!("label=com.docker.compose.project={PROJECT}"),
                "--format",
                "{{json .}}",
            ]),
            5,
        )?;
        let services: Result<Vec<Value>, _> = raw
            .lines()
            .filter(|s| !s.is_empty())
            .map(serde_json::from_str)
            .collect();
        result["services"] = json!(services?);
        for c in d["cameras"].as_array().unwrap() {
            let name = text(&c["name"]);
            let item = managed(&m.ctl, name)?;
            let status = read_json(
                &m.ctl
                    .root
                    .join("work/web-preview")
                    .join(name)
                    .join("out/status.json"),
            )
            .unwrap_or(json!({}));
            let running = item.as_ref().is_some_and(|v| v["State"]["Running"] == true);
            result["previews"][name] = json!({"running":running,"streams":status});
            if item.is_some() && !running {
                result["previews"][name]["error"] = m
                    .ctl
                    .run(&["docker", "logs", "--tail", "15", &preview_name(name)])?
                    .into();
            }
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(e) = outcome {
        result["docker_error"] = e.to_string().into();
    }
    Ok(result)
}
fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}
fn allowed(req: &Request, port: u16) -> bool {
    let host = header(req, "Host").unwrap_or("");
    let mut ports = vec![port];
    if let Ok(p) = env::var("ORBBEC_PUBLIC_PORT") {
        if let Ok(p) = p.parse() {
            ports.push(p);
        }
    }
    ports
        .into_iter()
        .any(|p| host == format!("127.0.0.1:{p}") || host == format!("localhost:{p}"))
        && header(req, "Origin").is_none_or(|o| o == format!("http://{host}"))
}
fn send(req: Request, status: u16, body: Vec<u8>, mime: &str) {
    let headers = [
        ("Content-Type", mime),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        (
            "Content-Security-Policy",
            "default-src 'self'; img-src 'self'; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ];
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    for (k, v) in headers {
        response.add_header(Header::from_bytes(k, v).unwrap());
    }
    let _ = req.respond(response);
}
fn json_response(req: Request, status: u16, v: Value) {
    send(
        req,
        status,
        serde_json::to_vec(&v).unwrap(),
        "application/json",
    )
}
fn handle(mut req: Request, m: &Arc<Manager>, port: u16) {
    if !allowed(&req, port) {
        return json_response(req, 403, json!({"error":"Invalid host/origin"}));
    }
    let result = (|| -> Result<(u16, Vec<u8>, &'static str)> {
        let parsed = url::Url::parse(&format!("http://localhost{}", req.url()))?;
        let path = parsed.path();
        let query: std::collections::HashMap<_, _> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let get = |k: &str| query.get(k).map(String::as_str).unwrap_or("");
        let json = |code, v: Value| Ok((code, serde_json::to_vec(&v)?, "application/json"));
        if req.method() == &Method::Get {
            match path {
                "/api/capabilities" => json(
                    200,
                    if get("require").is_empty() {
                        capture_contract::capabilities::describe(
                            "geminidepthport",
                            env!("CARGO_PKG_VERSION"),
                        )
                    } else {
                        capture_contract::capabilities::negotiate(
                            "geminidepthport",
                            env!("CARGO_PKG_VERSION"),
                            &capture_contract::parse_strict(get("require").as_bytes())?,
                        )?
                    },
                ),
                "/api/build-info" => json(
                    200,
                    capture_contract::capabilities::build_info(
                        "geminidepthport",
                        env!("CARGO_PKG_VERSION"),
                    ),
                ),
                "/api/quality/batch" | "/api/quality/window" => {
                    require(
                        get("request").len() <= 65536,
                        "QUALITY_REQUEST_SIZE",
                        "Request too large",
                    )?;
                    let request = capture_contract::parse_strict(get("request").as_bytes())?;
                    json(
                        200,
                        crate::quality_batch::execute(
                            &m.ctl,
                            if path.ends_with("batch") {
                                "quality-batch"
                            } else {
                                "quality-window"
                            },
                            &request,
                        )?,
                    )
                }
                "/healthz" => json(200, json!({"status":"ok"})),
                "/api/state" => json(200, state(m)?),
                "/api/usbfs" => json(200, crate::usbfs::status(&m.ctl.load()?)),
                "/api/recording-context" => json(
                    200,
                    crate::managed_quality::recording_context(
                        &m.ctl,
                        get("require_strong") == "true",
                    )
                    .map_err(crate::managed_quality::public_error)?,
                ),
                "/api/quality/current" | "/api/quality/events" | "/api/quality/session-summary" => {
                    if path == "/api/quality/session-summary"
                        && (!get("generation").is_empty() || !get("session").is_empty())
                    {
                        return json(
                            200,
                            crate::quality_history::read(
                                &m.ctl,
                                get("camera"),
                                get("generation"),
                                get("session"),
                            )?,
                        );
                    }
                    let cursor = if get("cursor").is_empty() {
                        None
                    } else {
                        Some(serde_json::from_value(capture_contract::parse_strict(
                            get("cursor").as_bytes(),
                        )?)?)
                    };
                    let limit = if get("limit").is_empty() {
                        50
                    } else {
                        get("limit").parse::<usize>()?
                    };
                    json(
                        200,
                        crate::managed_quality::query(
                            &m.ctl,
                            get("camera"),
                            path.rsplit('/').next().unwrap(),
                            cursor,
                            limit,
                        )
                        .map_err(crate::managed_quality::public_error)?,
                    )
                }

                "/api/preview/logs" => {
                    let name = get("role");
                    role(&m.ctl.load()?, name)?;
                    let log = if managed(&m.ctl, name)?.is_some() {
                        m.ctl
                            .run(&["docker", "logs", "--tail", "80", &preview_name(name)])?
                    } else {
                        String::new()
                    };
                    json(200, json!({"log":log}))
                }
                "/api/frame" => {
                    let name = get("role");
                    role(&m.ctl.load()?, name)?;
                    let stream = get("stream");
                    require(
                        ["color", "depth"].contains(&stream),
                        "STREAM_INVALID",
                        "Invalid stream",
                    )?;
                    let file = m
                        .ctl
                        .root
                        .join("work/web-preview")
                        .join(name)
                        .join("out")
                        .join(format!("{stream}.jpg"));
                    let fresh = fs::metadata(&file)
                        .and_then(|v| v.modified())
                        .ok()
                        .and_then(|v| v.elapsed().ok())
                        .is_some_and(|v| v <= Duration::from_secs(3));
                    if !fresh {
                        return json(404, json!({"error":"No fresh frame"}));
                    }
                    Ok((200, fs::read(file)?, "image/jpeg"))
                }
                "/" | "/app.js" | "/style.css" => {
                    let (file, mime) = match path {
                        "/" => ("index.html", "text/html; charset=utf-8"),
                        "/app.js" => ("app.js", "text/javascript; charset=utf-8"),
                        _ => ("style.css", "text/css; charset=utf-8"),
                    };
                    let mut body = fs::read(m.ctl.root.join("web").join(file))?;
                    if file == "index.html" {
                        body = String::from_utf8(body)?
                            .replace("__CSRF__", &m.token)
                            .into_bytes();
                    }
                    Ok((200, body, mime))
                }
                _ => json(404, json!({"error":"Not found"})),
            }
        } else if req.method() == &Method::Post {
            let token = header(&req, "X-CSRF-Token").unwrap_or("");
            if !bool::from(token.as_bytes().ct_eq(m.token.as_bytes())) {
                return json(403, json!({"error":"Invalid origin or CSRF token"}));
            }
            require(
                req.body_length().is_some_and(|n| n > 0 && n <= 32768)
                    && header(&req, "Content-Type") == Some("application/json"),
                "REQUEST_INVALID",
                "Expected bounded JSON request",
            )?;
            let mut body = vec![];
            req.as_reader().take(32769).read_to_end(&mut body)?;
            require(body.len() <= 32768, "REQUEST_INVALID", "Request too large")?;
            let payload: Value = serde_json::from_slice(&body)?;
            require(
                payload.is_object(),
                "REQUEST_INVALID",
                "Expected JSON object",
            )?;
            *m.last_seen.lock().unwrap() = Instant::now();
            if path == "/api/job" {
                return json(202, start_job(m, text(&payload["action"]))?);
            }
            let _busy = m.busy()?;
            let _lock = m.ctl.lock(true)?;
            let name = text(&payload["role"]);
            let result = match path {
                "/api/config" => save_config(&m.ctl, &payload)?,
                "/api/preview/start" => start_preview(&m.ctl, name)?,
                "/api/preview/stop" => {
                    role(&m.ctl.load()?, name)?;
                    stop_preview(&m.ctl, name)?;
                    json!({"message":"预览已停止。"})
                }
                "/api/confirm" => confirm(&m.ctl, name)?,
                _ => anyhow::bail!("REQUEST_INVALID: Unknown action"),
            };
            json(200, result)
        } else {
            json(405, json!({"error":"Method not allowed"}))
        }
    })();
    match result {
        Ok((code, body, mime)) => send(req, code, body, mime),
        Err(e) => json_response(req, 400, json!({"error":e.to_string()})),
    }
}
fn expire_idle_previews(m: &Manager) {
    if m.last_seen.lock().unwrap().elapsed() > Duration::from_secs(120) {
        if let Ok(_busy) = m.busy() {
            if let Ok(_lock) = m.ctl.lock(true) {
                // Recheck after both operation locks: activity may have
                // resumed since the outer idle observation.
                if m.last_seen.lock().unwrap().elapsed() > Duration::from_secs(120) {
                    let _ = stop_owned_previews(&m.ctl);
                }
            }
        }
    }
}

pub fn serve(ctl: Controller, host: &str, port: u16) -> Result<()> {
    require(
        ["127.0.0.1", "0.0.0.0"].contains(&host),
        "SERVER_BIND",
        "Unsupported bind address",
    )?;
    require(
        host != "0.0.0.0" || env::var("ORBBEC_CONTAINERIZED").ok().as_deref() == Some("1"),
        "SERVER_BIND",
        "Wildcard bind reserved for management container",
    )?;
    if env::var("ORBBEC_CONTAINERIZED").ok().as_deref() == Some("1") {
        require(
            env::var_os("ORBBEC_DEPLOY_DIR")
                .and_then(|p| fs::canonicalize(p).ok())
                .is_some_and(|p| p == ctl.root),
            "SERVER_BIND",
            "Deployment directory must match host path",
        )?;
    }
    config::validate(&ctl.load()?, true)?;
    let _service_lock = ctl.service_lock()?;
    let server = Arc::new(Server::http((host, port)).map_err(|e| anyhow::anyhow!("{e}"))?);
    let actual_port = server.server_addr().to_ip().unwrap().port();
    let m = Arc::new(Manager {
        job: Mutex::new(read_json(&ctl.root.join("state/latest-job.json")).unwrap_or(Value::Null)),
        ctl,
        token: random(32),
        busy: AtomicBool::new(false),
        active_job: AtomicBool::new(false),
        last_seen: Mutex::new(Instant::now()),
        #[cfg(test)]
        state_after_job_read: Mutex::new(None),
    });
    STOP.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as usize);
        libc::signal(libc::SIGINT, stop as *const () as usize);
    }
    let mut workers = vec![];
    for _ in 0..8 {
        let s = Arc::clone(&server);
        let m = Arc::clone(&m);
        workers.push(thread::spawn(move || {
            while !STOP.load(Ordering::Relaxed) {
                match s.recv_timeout(Duration::from_millis(250)) {
                    Ok(Some(req)) => handle(req, &m, actual_port),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        }));
    }
    println!("Camera console: http://127.0.0.1:{actual_port}");
    while !STOP.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(250));
        expire_idle_previews(&m);
    }
    cancel_commands();
    for worker in workers {
        let _ = worker.join();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while m.active_job.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    reset_commands();
    if let Ok(_lock) = m.ctl.lock(true) {
        let _ = stop_owned_previews(&m.ctl);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::TestRequest;
    #[test]
    fn authenticated_activity_renews_idle_preview_lease() {
        let root = tempfile::tempdir().unwrap();
        let m = Arc::new(Manager {
            ctl: Controller::new(root.path()).unwrap(),
            token: "test-token".into(),
            busy: AtomicBool::new(false),
            active_job: AtomicBool::new(false),
            last_seen: Mutex::new(Instant::now() - Duration::from_secs(121)),
            job: Mutex::new(Value::Null),
            #[cfg(test)]
            state_after_job_read: Mutex::new(None),
        });
        let request = |token: &str| {
            TestRequest::new()
                .with_method(Method::Post)
                .with_path("/api/unknown")
                .with_body("{}")
                .with_header(Header::from_bytes("Host", "127.0.0.1:8765").unwrap())
                .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
                .with_header(Header::from_bytes("X-CSRF-Token", token).unwrap())
                .into()
        };
        handle(request("wrong-token"), &m, 8765);
        assert!(m.last_seen.lock().unwrap().elapsed() > Duration::from_secs(120));
        handle(request("test-token"), &m, 8765);
        assert!(m.last_seen.lock().unwrap().elapsed() < Duration::from_secs(1));
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod contract_tests;
