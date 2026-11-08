//! Immutable generation bundles and their active pointer.
//!
//! `load` verifies the pointer, manifest and bound configuration files; it does
//! not establish live camera readiness. Creation writes and validates a private
//! bundle before activating it. Callers must hold the control mutation lock;
//! source-byte checks detect inventory drift but do not lock external editors.
use crate::{capture_backend, config, control::Controller, util::*};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Component, Path},
};
pub const KIND: &str = "geminidepthport-generation-v1";
/// Return the verified active bundle, or `None` when no active pointer exists.
/// Malformed or changed bundles fail instead of falling back to another version.
pub fn load(ctl: &Controller) -> Result<Option<Value>> {
    load_inner(ctl).map_err(|e| {
        if e.to_string().starts_with("GENERATION_") {
            e
        } else {
            anyhow::anyhow!("GENERATION_INVALID: Cannot read active generation")
        }
    })
}
fn load_inner(ctl: &Controller) -> Result<Option<Value>> {
    let pointer = ctl.root.join("state/current-generation.json");
    if !pointer.try_exists()? {
        return Ok(None);
    }
    let p = read_json(&pointer)?;
    let id = text(&p["generation"]);
    require(
        matches(r"^[0-9a-f]{32}$", id),
        "GENERATION_INVALID",
        "Invalid generation identifier",
    )?;
    let folder = ctl.root.join("state/generations").join(id);
    require(
        !folder.is_symlink(),
        "GENERATION_INVALID",
        "Generation must be managed directory",
    )?;
    let manifest = folder.join("manifest.json");
    require(
        file_hash(&manifest)? == text(&p["manifest_sha256"]),
        "GENERATION_DRIFT",
        "Manifest changed",
    )?;
    let record = read_json(&manifest)?;
    require(
        record["kind"] == KIND
            && record["generation"] == id
            && record["files"].is_object()
            && record["cameras"].is_array(),
        "GENERATION_INVALID",
        "Invalid generation manifest",
    )?;
    require(
        record["draft"].is_boolean()
            && [1, 2].iter().any(|v| record["schema_version"] == *v)
            && matches(r"^[0-9a-f]{64}$", text(&record["inventory_sha256"]))
            && record["communication_profile"] == config::COMMUNICATION,
        "GENERATION_INVALID",
        "Invalid generation metadata",
    )?;
    let mut required = std::collections::BTreeSet::from(["compose.yaml".to_string()]);
    let mut names = std::collections::BTreeSet::new();
    let cameras = record["cameras"].as_array().unwrap();
    require(
        cameras.len() <= 3,
        "GENERATION_INVALID",
        "Too many generation cameras",
    )?;
    for camera in cameras {
        let name = text(&camera["name"]);
        require(
            camera.as_object().is_some_and(|m| m.len() == 2)
                && matches(r"^[a-z][a-z0-9_]{0,31}$", name)
                && names.insert(name)
                && camera["service"] == format!("camera_{name}"),
            "GENERATION_INVALID",
            "Invalid camera service binding",
        )?;
        for file in ["camera.yaml", "health.json", "binding.json"] {
            required.insert(format!("config/{name}/{file}"));
        }
        if record["observer_backend"] == "ros-observed-v1" {
            required.insert(format!("config/{name}/observer-policy.json"));
        }
        if capture_backend::selected(&record) == "rust" {
            for file in ["capture.json", "runtime.json"] {
                required.insert(format!("config/{name}/{file}"));
            }
        }
    }
    require(
        record.get("capture_backend").is_none_or(|v| v == "rust"),
        "GENERATION_INVALID",
        "Unsupported generation backend",
    )?;
    if capture_backend::selected(&record) == "rust" {
        require(
            (cameras.is_empty() && record["capture_runtime"].is_null())
                || (record["capture_runtime"]["manifest"]["version"] == 1
                    && matches(
                        r"^sha256:[a-f0-9]{64}$",
                        text(&record["capture_runtime"]["image_id"]),
                    )),
            "GENERATION_INVALID",
            "Missing Rust runtime binding",
        )?;
    }
    let actual: std::collections::BTreeSet<_> = record["files"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    require(
        actual == required,
        "GENERATION_INVALID",
        "Incomplete or unexpected deployment files",
    )?;
    let base = folder.canonicalize()?;
    for (relative, expected) in record["files"].as_object().unwrap() {
        let path = Path::new(relative);
        require(
            !path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
            "GENERATION_INVALID",
            "Invalid managed file reference",
        )?;
        let full = folder.join(path).canonicalize()?;
        require(
            full.starts_with(&base),
            "GENERATION_INVALID",
            "Managed file escaped generation",
        )?;
        require(
            file_hash(&full)? == text(expected),
            "GENERATION_DRIFT",
            "Generated content changed",
        )?;
    }
    Ok(Some(record))
}
pub fn communication(d: &Value) -> Value {
    json!({"ROS_DOMAIN_ID":d["ros_domain_id"].to_string(),"ROS_LOCALHOST_ONLY":if capture_backend::selected(d)=="rust"{"0"}else{"1"},"RMW_IMPLEMENTATION":"rmw_fastrtps_cpp","FASTRTPS_DEFAULT_PROFILES_FILE":"/app/fastdds.xml"})
}
pub fn web_service() -> Value {
    let root = "${ORBBEC_DEPLOY_DIR:?Run ./deploy.sh web or set ORBBEC_DEPLOY_DIR}";
    json!({"image":"local/orbbec-console:rust-v1","build":{"context":".","dockerfile":"web.Dockerfile"},"working_dir":root,"user":"${DEPLOY_UID:-0}:${DEPLOY_GID:-0}","group_add":["${DOCKER_GID:-0}"],"command":["geminidepthport","serve","--host","0.0.0.0","--port","8765"],"restart":"unless-stopped","init":true,"stop_grace_period":"30s","labels":{"orbbec.role":"management"},"ports":["127.0.0.1:${WEB_PORT:-8765}:8765"],"environment":{"ORBBEC_CONTAINERIZED":"1","ORBBEC_DEPLOY_DIR":root,"ORBBEC_PUBLIC_PORT":"${WEB_PORT:-8765}","HOME":"/tmp/console-home","DOCKER_CONFIG":"/tmp/docker","XDG_CACHE_HOME":"/tmp/cache"},"volumes":[{"type":"bind","source":root,"target":root},{"type":"bind","source":"/var/run/docker.sock","target":"/var/run/docker.sock"},{"type":"bind","source":"/sys","target":"/sys","read_only":true},{"type":"bind","source":"/dev/bus/usb","target":"/dev/bus/usb","read_only":true},{"type":"bind","source":"/run/udev","target":"/run/udev","read_only":true},{"type":"bind","source":"/etc/udev/rules.d","target":"/etc/udev/rules.d","read_only":true}],"read_only":true,"tmpfs":["/tmp:size=128m"],"cap_drop":["ALL"],"security_opt":["no-new-privileges:true"],"healthcheck":{"test":["CMD","geminidepthport","http-health","--port","8765"],"interval":"10s","timeout":"5s","retries":6,"start_period":"10s"},"logging":{"driver":"json-file","options":{"max-size":"10m","max-file":"3"}}})
}
fn camera_service(ctl: &Controller, d: &Value, env: Value, folder: &Path) -> Result<Value> {
    let patch = read_json(&ctl.root.join("patches/public-orbbec/manifest.json"))?;
    let mut service = json!({"image":d["image"],"build":{"context":ctl.root,"args":{"ORBBEC_COMMIT":d["upstream_commit"],"YUYV_PATCH_SHA256":patch["patch_sha256"]}},"network_mode":"host","ipc":"private","init":true,"user":format!("{}:{}",unsafe{libc::getuid()},unsafe{libc::getgid()}),"profiles":["cameras"],"labels":{"orbbec.role":"camera"},"restart":"no","stop_signal":"SIGINT","stop_grace_period":"30s","read_only":true,"cap_drop":["ALL"],"security_opt":["no-new-privileges:true"],"shm_size":"256m","tmpfs":["/tmp:size=256m"],"environment":env,"volumes":[{"type":"bind","source":folder,"target":"/config","read_only":true},"/dev/bus/usb:/dev/bus/usb:rw","/run/udev:/run/udev:ro"],"device_cgroup_rules":["c 189:* rwm"],"healthcheck":{"test":["CMD","/app/entrypoint.sh","python3","/app/health.py"],"interval":"30s","timeout":"25s","start_period":"45s","retries":3},"logging":{"driver":"json-file","options":{"max-size":"10m","max-file":"3"}}});
    if capture_backend::selected(d) == "rust" {
        service["build"]["dockerfile"] = "capture.Dockerfile".into();
        service["build"]["args"] = json!({"ORBBEC_COMMIT":d["upstream_commit"]});
        service["labels"]["org.geminidepthport.capture-backend"] = "rust-v1".into();
    }
    Ok(service)
}
pub fn camera_files(d: &Value, c: &Value) -> Result<(Value, Value, Value)> {
    let mut p = c["parameters"].clone();
    p["camera_name"] = c["frame_prefix"].clone();
    p["serial_number"] = c["serial_number"].clone();
    p["device_num"] = config::active(d).len().into();
    if d["schema_version"] == 2 {
        let q = config::qos(&p)?;
        p.as_object_mut()
            .unwrap()
            .extend(q.as_object().unwrap().clone());
        for k in [
            "enable_accel",
            "enable_gyro",
            "enable_point_cloud",
            "enable_colored_point_cloud",
        ] {
            p[k] = false.into();
        }
        let catalog = config::catalog();
        for k in ["enable_ir", "enable_left_ir", "enable_right_ir"] {
            if catalog["profiles"][text(&c["profile"])]["parameters"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == k))
            {
                p[k] = false.into();
            }
        }
    }
    let binding = json!({"schema_version":d["schema_version"],"namespace":c["namespace"],"frame_prefix":c["frame_prefix"],"model":c["model"],"launch_file":c["launch_file"],"contract":if d["schema_version"]==2{"camera-interface-v1"}else{"legacy"}});
    let mut h = c["health"].clone();
    h["camera_name"] = c["frame_prefix"].clone();
    let mut streams = vec![];
    for stream in ["color", "depth"] {
        if p[format!("enable_{stream}")] == true {
            let mut spec = json!({"topic":format!("{}/{stream}/image_raw",text(&c["namespace"])),"width":p[format!("{stream}_width")],"height":p[format!("{stream}_height")],"min_fps":p[format!("{stream}_fps")].as_f64().unwrap()*h["min_fps_ratio"].as_f64().unwrap()});
            if d["schema_version"] == 2 {
                spec["encoding"] = (if stream == "color" { "yuyv" } else { "16UC1" }).into();
                spec["frame_id"] =
                    format!("{}_{stream}_optical_frame", text(&c["frame_prefix"])).into();
                spec["required_reliability"] = "reliable".into();
                spec["camera_info_topic"] =
                    format!("{}/{stream}/camera_info", text(&c["namespace"])).into();
            }
            streams.push(spec);
        }
    }
    h["streams"] = json!(streams);
    Ok((p, h, binding))
}
/// Generate and validate a bundle from the saved inventory, then activate it.
/// Caller holds the mutation lock. Generated `.yaml` files use JSON, a YAML 1.2 subset.
pub fn create(ctl: &Controller, d: &Value, draft: bool) -> Result<Value> {
    config::validate(d, draft)?;
    let source = fs::read(ctl.root.join("inventory.json"))?;
    require(
        serde_json::from_slice::<Value>(&source)? == *d,
        "INVENTORY_DRIFT",
        "Reload saved inventory",
    )?;
    let digest = hash(&source);
    let rust_backend = capture_backend::selected(d) == "rust";
    let runtime = if rust_backend && !config::active(d).is_empty() {
        capture_backend::runtime(ctl, d)?
    } else {
        Value::Null
    };
    // A damaged bundle cannot be reused. Replacement still requires every
    // managed camera and preview to be stopped below.
    if let Ok(Some(c)) = load(ctl) {
        if c["inventory_sha256"] == digest
            && c["draft"] == draft
            && (d["schema_version"] != 2 || c["quality_evidence_version"] == 1)
            && (!rust_backend || c.get("capture_runtime").unwrap_or(&Value::Null) == &runtime)
        {
            return Ok(c);
        }
    }
    ctl.assert_stopped()?;
    let parent = ctl.root.join("state/generations");
    private_dir(&parent)?;
    let id = random(16);
    let dest = parent.join(&id);
    let stage = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&parent)?;
    private_dir(&stage.path().join("config"))?;
    let mut cameras = config::active(d);
    cameras.sort_by_key(|c| text(&c["name"]).to_string());
    let mut services = json!({"web":web_service()});
    let mut previous = None::<String>;
    let mut files = json!({});
    let mut summaries = vec![];
    for raw in cameras {
        let c = config::resolved(d, &raw);
        if rust_backend && c["profiles_verified"] == true {
            require(
                c["verification"]["runtime_digest"] == capture_backend::runtime_digest(&runtime),
                "RUST_CONFIRMATION_STALE",
                "Capture runtime changed; confirm actual profile again",
            )?;
        }
        let name = text(&c["name"]);
        let service = format!("camera_{name}");
        let (p, h, b) = camera_files(d, &c)?;
        for (file, value) in [("camera.yaml", p), ("health.json", h), ("binding.json", b)] {
            let relative = format!("config/{name}/{file}");
            let content = serde_json::to_vec_pretty(&value)?;
            write_private(&stage.path().join(&relative), &content)?;
            files[&relative] = hash(content).into();
        }
        if rust_backend {
            for (file, value) in [
                (
                    "capture.json",
                    capture_backend::service_config(d, &c, &id, &runtime),
                ),
                ("runtime.json", runtime["manifest"].clone()),
            ] {
                let relative = format!("config/{name}/{file}");
                let content = serde_json::to_vec_pretty(&value)?;
                write_private(&stage.path().join(&relative), &content)?;
                files[&relative] = hash(content).into();
            }
        }
        if !rust_backend && d["schema_version"] == 2 {
            let observer = crate::official_observer::policy(d, &c, &id)?;
            let relative = format!("config/{name}/observer-policy.json");
            let content = serde_json::to_vec_pretty(&observer)?;
            write_private(&stage.path().join(&relative), &content)?;
            files[&relative] = hash(content).into();
        }
        let mut env = communication(d);
        if !rust_backend && d["schema_version"] == 2 {
            env["ROS_LOCALHOST_ONLY"] = "0".into();
        }
        env.as_object_mut().unwrap().extend(json!({"CAMERA_ROLE":name,"CAMERA_NAME":c["frame_prefix"],"CAMERA_NAMESPACE":c["namespace"],"CAMERA_SCHEMA":d["schema_version"].to_string(),"CAMERA_MODEL":c["model"],"CAMERA_LAUNCH":c["launch_file"],"HOME":"/tmp","ROS_HOME":"/tmp/ros"}).as_object().unwrap().clone());
        let mut spec = camera_service(ctl, d, env, &dest.join("config").join(name))?;
        if rust_backend {
            if c.get("raw_recording").is_some() {
                let recordings = ctl.root.join("state/native-recordings").join(name);
                private_dir(&recordings)?;
                spec["volumes"].as_array_mut().unwrap().push(json!({"type":"bind","source":recordings,"target":"/native-recordings","read_only":false}));
            }
            let gate = ctl.root.join("state/recording-gates").join(name);
            private_dir(&gate)?;
            spec["volumes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"bind","source":gate,"target":"/recording","read_only":false}));
        }

        if d["schema_version"] == 2 {
            private_dir(&stage.path().join("evidence"))?;
            let evidence = stage.path().join("evidence").join(name);
            private_dir(&evidence)?;
            private_dir(&evidence.join(if rust_backend { "quality" } else { "sessions" }))?;
            private_dir(&evidence.join("snapshots"))?;
            spec["volumes"].as_array_mut().unwrap().push(json!({
                "type":"bind","source":dest.join("evidence").join(name),
                "target":if rust_backend{"/tmp/capture"}else{"/tmp/capture-observer"},
                "read_only":false}));
        }
        spec["labels"]["orbbec.generation"] = id.clone().into();
        spec["labels"]["orbbec.camera"] = name.into();
        if let Some(prev) = previous {
            spec["depends_on"] = json!({prev:{"condition":"service_healthy"}});
        }
        services[&service] = spec;
        previous = Some(service.clone());
        summaries.push(json!({"name":name,"service":service}));
    }
    let content = serde_json::to_vec_pretty(&json!({"services":services}))?;
    write_private(&stage.path().join("compose.yaml"), &content)?;
    files["compose.yaml"] = hash(content).into();
    let mut record = json!({"kind":KIND,"generation":id,"draft":draft,"schema_version":d["schema_version"],"inventory_sha256":digest,"communication_profile":config::COMMUNICATION,"cameras":summaries,"files":files});
    if d["schema_version"] == 2 {
        record["quality_evidence_version"] = 1.into();
    }
    if !rust_backend && d["schema_version"] == 2 {
        record["observer_backend"] = "ros-observed-v1".into();
    }
    if rust_backend {
        record["capture_backend"] = "rust".into();
        record["capture_runtime"] = runtime;
    }
    write_private(
        &stage.path().join("manifest.json"),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    ctl.compose_file(
        &stage.path().join("compose.yaml"),
        &["--profile", "cameras", "config", "--quiet"],
    )?;
    require(
        fs::read(ctl.root.join("inventory.json"))? == source,
        "INVENTORY_DRIFT",
        "Inventory changed during generation",
    )?;
    fs::rename(stage.path(), &dest)?;
    // Keep the immutable destination on activation failure: the pointer may
    // already have been replaced before a directory fsync reports an error.
    atomic_json(
        &ctl.root.join("state/current-generation.json"),
        &json!({"generation":id,"manifest_sha256":file_hash(&dest.join("manifest.json"))?}),
    )?;
    Ok(record)
}
