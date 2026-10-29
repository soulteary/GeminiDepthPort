//! Host lifecycle operations over one canonical deployment root.
//!
//! CLI/Web callers serialize mutations with `Controller::lock(true)`, which also
//! rejects recording protection. Helpers do not acquire that lock themselves.
//! Operations may invoke Docker/Compose and write private state; failed group
//! startup can leave owned instances running for diagnosis. Container ownership
//! and stop behavior are defined in `docs/guides/lifecycle.md`.
use crate::{capture_backend, config, generation, util::*};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
pub const PROJECT: &str = "orbbec-multicam";
#[derive(Clone)]
pub struct Controller {
    pub root: PathBuf,
    #[cfg(test)]
    pub(crate) command_runner: Option<TestCommandRunner>,
    #[cfg(test)]
    pub(crate) host_root: Option<PathBuf>,
}
#[cfg(test)]
pub(crate) type TestCommandRunner =
    std::sync::Arc<dyn Fn(&[String]) -> Result<String> + Send + Sync>;
pub struct ControlLock(File);
impl Drop for ControlLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Controller {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
            #[cfg(test)]
            command_runner: None,
            #[cfg(test)]
            host_root: None,
        })
    }
    /// Acquire the nonblocking deployment lock; retain the guard for the operation.
    /// With `mutation`, reject an existing recording guard before returning.
    pub fn lock(&self, mutation: bool) -> Result<ControlLock> {
        private_dir(&self.root.join("state"))?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join("state/control.lock"))?;
        require(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "CONTROL_BUSY",
            "Another lifecycle operation is in progress",
        )?;
        let lock = ControlLock(f);
        if mutation {
            self.unprotected()?;
        }
        Ok(lock)
    }
    pub fn service_lock(&self) -> Result<ControlLock> {
        private_dir(&self.root.join("state"))?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join("state/web.lock"))?;
        require(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "WEB_BUSY",
            "A management server already owns this deployment",
        )?;
        Ok(ControlLock(f))
    }
    pub fn unprotected(&self) -> Result<()> {
        require(
            fs::symlink_metadata(self.root.join("state/recording-guard.json"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "RECORDING_PROTECTED",
            "Recording protection blocks this operation",
        )
    }
    pub fn load(&self) -> Result<Value> {
        read_json(&self.root.join("inventory.json"))
    }
    pub fn load_revision(&self) -> Result<(Value, String)> {
        let source = fs::read(self.root.join("inventory.json"))?;
        Ok((serde_json::from_slice(&source)?, hash(source)))
    }
    pub fn revision(&self) -> Result<String> {
        file_hash(&self.root.join("inventory.json"))
    }
    pub fn run(&self, a: &[&str]) -> Result<String> {
        #[cfg(test)]
        if let Some(run) = &self.command_runner {
            return run(&args(a));
        }
        command(
            &self.root,
            &args(a),
            if a.starts_with(&["docker", "build"]) {
                3600
            } else {
                60
            },
            None,
        )
    }
    pub fn run_vec(&self, a: &[String], timeout: u64) -> Result<String> {
        #[cfg(test)]
        if let Some(run) = &self.command_runner {
            return run(a);
        }
        command(&self.root, a, timeout, None)
    }
    pub fn compose(&self, a: &[&str]) -> Result<String> {
        let current = generation::load(self)?;
        let file = current
            .as_ref()
            .map(|s| {
                self.root
                    .join("state/generations")
                    .join(text(&s["generation"]))
                    .join("compose.yaml")
            })
            .unwrap_or_else(|| self.root.join("compose.yaml"));
        self.compose_file(&file, a)
    }
    pub fn compose_file(&self, file: &Path, a: &[&str]) -> Result<String> {
        let mut cmd = args(&[
            "docker",
            "compose",
            "--project-directory",
            self.root.to_str().unwrap(),
            "-p",
            PROJECT,
            "-f",
            file.to_str().unwrap(),
        ]);
        cmd.extend(args(a));
        self.run_vec(&cmd, 60)
    }
    pub fn save(&self, data: &Value, confirmed: bool) -> Result<()> {
        self.save_at_revision(data, confirmed, &self.revision()?)
    }
    /// Save only while the source revision still matches, invalidating stale checks.
    /// Caller holds the mutation lock; external editors are not serialized by it.
    pub fn save_at_revision(&self, data: &Value, confirmed: bool, expected: &str) -> Result<()> {
        let source = fs::read(self.root.join("inventory.json"))?;
        require(
            hash(&source) == expected,
            "INVENTORY_DRIFT",
            "Inventory changed during operation",
        )?;
        let previous: Value = serde_json::from_slice(&source)?;
        for value in [&previous, data] {
            require(
                value.is_object()
                    && value["cameras"].as_array().is_some_and(|a| {
                        a.iter().all(|c| {
                            c.is_object() && c["name"].is_string() && c["parameters"].is_object()
                        })
                    }),
                "CONFIG_SHAPE",
                "Malformed inventory structure",
            )?;
        }
        let candidate = config::invalidate(&previous, data);
        for c in candidate["cameras"].as_array().unwrap() {
            let before = previous["cameras"]
                .as_array()
                .and_then(|a| a.iter().find(|b| b["name"] == c["name"]));
            let newly = c["profiles_verified"] == true
                && before.is_none_or(|b| {
                    b["profiles_verified"] != true || b["verification"] != c["verification"]
                });
            require(
                !newly || confirmed,
                "PROFILE_PROBE_REQUIRED",
                "Confirmation requires runtime probe",
            )?;
        }
        config::validate(&candidate, true)?;
        let path = self.root.join("inventory.json");
        let mut bytes = serde_json::to_vec_pretty(&candidate)?;
        bytes.push(b'\n');
        atomic_checked(&path, &bytes, || {
            require(
                fs::read(&path).ok().as_deref() == Some(source.as_slice()),
                "INVENTORY_DRIFT",
                "Inventory changed before save",
            )
        })
    }
    fn host_path(&self, path: &str) -> PathBuf {
        #[cfg(test)]
        if let Some(root) = &self.host_root {
            return root.join(path.trim_start_matches('/'));
        }
        PathBuf::from(path)
    }
    pub fn scan(&self) -> Result<Value> {
        let mut result = vec![];
        if let Ok(entries) = fs::read_dir(self.host_path("/sys/bus/usb/devices")) {
            let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for p in paths {
                let read = |name: &str| {
                    fs::read_to_string(p.join(name))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                if read("idVendor").eq_ignore_ascii_case("2bc5") {
                    let product = read("product");
                    result.push(json!({"port":p.file_name().unwrap().to_string_lossy(),"serial_number":read("serial"),"product":product,"speed_mbps":read("speed"),"bus":read("busnum"),"device":read("devnum"),"model":identify(&product)}));
                }
            }
        }
        Ok(json!(result))
    }
    pub fn instances(&self, kind: &str) -> Result<Vec<Value>> {
        let preview = kind == "preview";
        let mut cmd = args(&["docker", "ps", "-aq", "--no-trunc"]);
        let filters = if preview {
            vec![format!("label=orbbec.web.preview={PROJECT}")]
        } else {
            vec![
                format!("label=com.docker.compose.project={PROJECT}"),
                format!("label=orbbec.role={kind}"),
                "label=com.docker.compose.oneoff=False".into(),
            ]
        };
        for f in filters {
            cmd.extend(["--filter".into(), f]);
        }
        let raw = self.run_vec(&cmd, 60)?;
        let ids: Vec<_> = raw.split_whitespace().collect();
        require(
            ids.iter().all(|id| matches(r"^[a-f0-9]{12,64}$", id)),
            "INSTANCE_DISCOVERY",
            "Invalid container reference",
        )?;
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut cmd = args(&["docker", "inspect"]);
        cmd.extend(ids.iter().map(|s| s.to_string()));
        let values: Vec<Value> = serde_json::from_str(&self.run_vec(&cmd, 60)?)?;
        require(
            values.len() == ids.len(),
            "INSTANCE_DISCOVERY",
            "Incomplete discovery",
        )?;
        let requested: std::collections::BTreeSet<_> = ids.iter().copied().collect();
        let returned: std::collections::BTreeSet<_> =
            values.iter().map(|v| text(&v["Id"])).collect();
        require(
            returned == requested && returned.len() == values.len(),
            "INSTANCE_DISCOVERY",
            "Duplicate or missing inspection result",
        )?;
        for v in &values {
            let l = &v["Config"]["Labels"];
            require(
                l.is_object() && v["State"]["Running"].is_boolean(),
                "INSTANCE_DISCOVERY",
                "Malformed container inspection",
            )?;
            let owned = if preview {
                l["orbbec.web.preview"] == PROJECT
            } else {
                l["com.docker.compose.project"] == PROJECT
                    && l["orbbec.role"] == kind
                    && text(&l["com.docker.compose.oneoff"]).eq_ignore_ascii_case("false")
            };
            require(
                owned && ids.contains(&text(&v["Id"])),
                "INSTANCE_OWNERSHIP",
                "Unmanaged container refused",
            )?;
        }
        Ok(values)
    }
    pub fn assert_no_previews(&self) -> Result<()> {
        require(
            self.instances("preview")?.is_empty(),
            "PREVIEWS_RUNNING",
            "Stop previews before maintenance",
        )
    }
    pub fn assert_stopped(&self) -> Result<()> {
        require(
            !self
                .instances("camera")?
                .iter()
                .any(|v| v["State"]["Running"] == true),
            "CAMERAS_RUNNING",
            "Stop cameras before maintenance",
        )?;
        self.assert_no_previews()
    }
    pub fn check_render(&self, d: &Value) -> Result<Value> {
        config::validate(d, false)?;
        let s = generation::load(self)?;
        require(
            s.as_ref().is_some_and(|s| s["draft"] == false),
            "GENERATION_DRAFT",
            "Generate verified deployment first",
        )?;
        let s = s.unwrap();
        require(
            s["inventory_sha256"] == self.revision()? && self.load()? == *d,
            "INVENTORY_DRIFT",
            "Saved inventory differs from generation",
        )?;
        require(
            s["cameras"].as_array().is_some_and(|a| !a.is_empty()),
            "NO_ACTIVE_CAMERAS",
            "No required streams",
        )?;
        let mut cameras = config::active(d);
        cameras.sort_by_key(|c| text(&c["name"]).to_string());
        let expected: Vec<_> = cameras
            .iter()
            .map(|c| json!({"name":c["name"],"service":format!("camera_{}",text(&c["name"]))}))
            .collect();
        require(
            capture_backend::selected(&s) == capture_backend::selected(d),
            "GENERATION_BINDING",
            "Capture backend differs from generation",
        )?;
        require(
            s["cameras"] == json!(expected)
                && s["schema_version"] == d["schema_version"]
                && s["communication_profile"] == config::COMMUNICATION,
            "GENERATION_BINDING",
            "Generation differs from enabled camera contract",
        )?;
        Ok(s)
    }
    pub fn image_contract(&self, d: &Value) -> Result<()> {
        let image: Value =
            serde_json::from_str(&self.run(&["docker", "image", "inspect", text(&d["image"])])?)?;
        let labels = &image[0]["Config"]["Labels"];
        require(
            labels["org.opencontainers.image.revision"] == d["upstream_commit"],
            "IMAGE_REVISION",
            "Driver revision differs",
        )?;
        if capture_backend::selected(d) == "rust" {
            require(
                labels["org.geminidepthport.capture-backend"] == "rust-v1"
                    && labels["org.geminidepthport.explicit-bindings"] == "1"
                    && labels["org.geminidepthport.communication-profile"] == config::COMMUNICATION,
                "IMAGE_CONTRACT_UNSUPPORTED",
                "Rust image does not declare required interface",
            )?;
            capture_backend::runtime(self, d)?;
            return Ok(());
        }
        if d["schema_version"] == 2 {
            let patch: Value = read_json(&self.root.join("patches/public-orbbec/manifest.json"))?;
            require(
                labels["org.geminidepthport.explicit-bindings"] == "1"
                    && labels["org.geminidepthport.native-yuyv"] == "1"
                    && labels["org.geminidepthport.native-yuyv-patch"] == patch["patch_sha256"]
                    && labels["org.geminidepthport.communication-profile"] == config::COMMUNICATION,
                "IMAGE_CONTRACT_UNSUPPORTED",
                "Driver does not declare required interface",
            )?;
        }
        Ok(())
    }
    pub fn preflight(&self, d: &Value) -> Result<()> {
        crate::usbfs::preflight(d)?;
        self.assert_no_previews()?;
        self.check_render(d)?;
        require(
            cfg!(target_os = "linux"),
            "PLATFORM_UNSUPPORTED",
            "Deployment requires native Linux",
        )?;
        require(
            self.host_path("/dev/bus/usb").is_dir()
                && self.host_path("/run/udev").is_dir()
                && (self
                    .host_path("/etc/udev/rules.d/99-obsensor-libusb.rules")
                    .is_file()
                    || self
                        .host_path("/lib/udev/rules.d/99-obsensor-libusb.rules")
                        .is_file()),
            "DEVICE_ACCESS",
            "Device access or host rules unavailable",
        )?;
        let found = self.scan()?;
        for c in config::active(d) {
            require(
                found
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| v["serial_number"] == c["serial_number"])
                    .count()
                    == 1,
                "DEVICE_UNAVAILABLE",
                "Required binding unavailable",
            )?;
        }
        self.run(&["docker", "info"])?;
        self.compose(&["config", "--quiet"])?;
        self.image_contract(d)?;
        if self.check_render(d)?["observer_backend"] == "ros-observed-v1" {
            let inspected: Value = serde_json::from_str(&self.run(&[
                "docker",
                "image",
                "inspect",
                text(&d["image"]),
            ])?)?;
            require(
                inspected[0]["Config"]["Labels"]["org.geminidepthport.official-observer"] == "v1",
                "OFFICIAL_OBSERVER_IMAGE_REQUIRED",
                "Rebuild the official image with observer lifecycle support",
            )?;
        }
        if capture_backend::selected(d) == "rust" {
            let current = self.check_render(d)?;
            require(
                current["capture_runtime"] == capture_backend::runtime(self, d)?,
                "RUST_RUNTIME_DRIFT",
                "Capture image or runtime changed; regenerate after stopping",
            )?;
        }
        Ok(())
    }
    pub fn record(&self, v: &Value) -> Result<Value> {
        atomic_json(&self.root.join("state/lifecycle.json"), v)?;
        Ok(v.clone())
    }
    pub fn owns_preview(&self, item: &Value) -> bool {
        let base = self.root.join("work/web-preview");
        item["Mounts"].as_array().is_some_and(|mounts| {
            mounts.iter().any(|m| {
                if m["Destination"] != "/config" {
                    return false;
                }
                let source = std::path::Path::new(text(&m["Source"]));
                source.strip_prefix(&base).ok().is_some_and(|relative| {
                    let components: Vec<_> = relative.components().collect();
                    components.len() == 2
                        && components
                            .iter()
                            .all(|c| matches!(c, std::path::Component::Normal(_)))
                        && relative.file_name().is_some_and(|v| v == "config")
                })
            })
        })
    }
    pub fn stop(&self, kind: &str, remove: bool) -> Result<Value> {
        let values = self.instances(kind)?;
        if kind == "preview" {
            require(
                values.iter().all(|item| self.owns_preview(item)),
                "INSTANCE_OWNERSHIP",
                "Preview belongs to another deployment root",
            )?;
        }
        let mut failures = vec![];
        for item in values {
            let id = text(&item["Id"]);
            let result = (|| {
                if item["State"]["Running"] == true {
                    self.run(&["docker", "stop", "--time", "30", id])?;
                }
                if remove {
                    self.run(&["docker", "rm", id])?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            if result.is_err() {
                failures.push("STOP_FAILED");
            }
        }
        if self
            .instances(kind)?
            .iter()
            .any(|v| v["State"]["Running"] == true)
        {
            failures.push("STOP_INCOMPLETE");
        }
        let result = json!({"ready":false,"status":if failures.is_empty(){"stopped"}else{"failed"},"errors":failures});
        if kind == "camera" {
            self.record(&result)?;
        }
        require(
            failures.is_empty(),
            "STOP_FAILED",
            "Inspect controlled lifecycle status",
        )?;
        Ok(result)
    }
    pub fn existing(&self, s: &Value) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut names = std::collections::BTreeSet::new();
        for item in self.instances("camera")? {
            if item["State"]["Running"] != true {
                continue;
            }
            let l = &item["Config"]["Labels"];
            require(
                l["orbbec.generation"] == s["generation"]
                    && s["cameras"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c["name"] == l["orbbec.camera"]),
                "RUNNING_CONFIGURATION_CHANGED",
                "Stop previous generation before switching",
            )?;
            require(
                names.insert(text(&l["orbbec.camera"]).to_string()),
                "DUPLICATE_INSTANCE",
                "Duplicate driver refused",
            )?;
            out.push(item);
        }
        Ok(out)
    }
    pub fn healthy(&self, service: &str, timeout: u64) -> Result<()> {
        let id = self
            .compose(&["ps", "-a", "-q", service])?
            .trim()
            .to_string();
        require(
            matches(r"^[a-f0-9]{12,64}$", &id),
            "CAMERA_MISSING",
            "Camera instance missing",
        )?;
        let end = Instant::now() + Duration::from_secs(timeout);
        while Instant::now() < end {
            let state: Value = serde_json::from_str(&self.run(&["docker", "inspect", &id])?)?;
            let state = &state[0]["State"];
            if state["Running"] == true && state["Health"]["Status"] == "healthy" {
                return Ok(());
            }
            if ["dead", "exited"].contains(&text(&state["Status"])) {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        bail!("CAMERA_HEALTH_FAILED: Actual frame health check did not pass")
    }
    pub fn probe(&self, service: &str) -> Result<()> {
        self.compose(&[
            "exec",
            "-T",
            service,
            "/app/entrypoint.sh",
            "python3",
            "/app/health.py",
        ])?;
        let d = self.load()?;
        if capture_backend::selected(&d) != "rust"
            && self.check_render(&d)?["observer_backend"] == "ros-observed-v1"
        {
            let name = service
                .strip_prefix("camera_")
                .ok_or_else(|| anyhow::anyhow!("OBSERVER_SERVICE_BINDING"))?;
            let observed = crate::managed_quality::query(self, name, "current", None, 1)?;
            require(
                observed["worker_ready"] == true
                    && observed["session_kind"] == "observer"
                    && observed["snapshot_id"].as_str().is_some()
                    && observed["status_age_ms"]
                        .as_f64()
                        .is_some_and(|age| age <= 2000.),
                "OFFICIAL_OBSERVER_NOT_READY",
                "A ready observed session with a fresh heartbeat is required",
            )?;
        }
        Ok(())
    }
    pub fn ensure_started(&self, d: &Value) -> Result<Value> {
        let prepared = (|| {
            if self.check_render(d).is_err() {
                self.assert_stopped()?;
                generation::create(self, d, false)?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(e) = prepared {
            let code = e
                .to_string()
                .split(':')
                .next()
                .unwrap_or("START_FAILED")
                .to_string();
            self.record(&json!({"ready":false,"status":"failed","error":code}))?;
            return Err(e);
        }
        self.start(d)
    }
    pub fn start(&self, d: &Value) -> Result<Value> {
        let s = self.check_render(d)?;
        let cameras = s["cameras"].as_array().unwrap();
        let states: Vec<_> = cameras
            .iter()
            .enumerate()
            .map(|(i, _)| json!({"index":i,"status":"not_started"}))
            .collect();
        let mut result = json!({"ready":false,"status":"starting","generation":s["generation"],"cameras":states});
        self.record(&result)?;
        let mut index = None;
        let operation = (|| {
            self.preflight(d)?;
            let existing = self.existing(&s)?;
            for (i, c) in cameras.iter().enumerate() {
                index = Some(i);
                if !existing
                    .iter()
                    .any(|v| v["Config"]["Labels"]["orbbec.camera"] == c["name"])
                {
                    self.compose(&[
                        "run",
                        "--rm",
                        "--no-deps",
                        text(&c["service"]),
                        "python3",
                        "/app/validate_runtime.py",
                    ])?;
                }
            }
            for (i, c) in cameras.iter().enumerate() {
                index = Some(i);
                let service = text(&c["service"]);
                if let Some(item) = existing
                    .iter()
                    .find(|v| v["Config"]["Labels"]["orbbec.camera"] == c["name"])
                {
                    require(
                        item["State"]["Health"]["Status"] == "healthy",
                        "CAMERA_HEALTH_FAILED",
                        "Explicit recovery required",
                    )?;
                } else {
                    self.compose(&["up", "-d", "--no-deps", "--no-build", service])?;
                }
                self.healthy(
                    service,
                    d["startup_timeout_seconds"].as_f64().unwrap().ceil() as u64,
                )?;
                self.probe(service)?;
                result["cameras"][i]["status"] = "healthy".into();
                self.record(&result)?;
            }
            for (i, c) in cameras.iter().enumerate() {
                index = Some(i);
                self.probe(text(&c["service"]))?;
            }
            require(
                self.existing(&s)?.len() == cameras.len(),
                "GROUP_INCOMPLETE",
                "Required camera group incomplete",
            )?;
            result["ready"] = true.into();
            result["status"] = "ready".into();
            result["reused"] = (existing.len() == cameras.len()).into();
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(e) = operation {
            let code = e
                .to_string()
                .split(':')
                .next()
                .unwrap_or("START_FAILED")
                .to_string();
            if let Some(i) = index {
                result["cameras"][i]["status"] = "failed".into();
                result["cameras"][i]["error"] = code.clone().into();
            }
            result["status"] = "failed".into();
            result["error"] = code.into();
            self.record(&result)?;
            return Err(e);
        }
        self.record(&result)
    }
    pub fn protect(&self) -> Result<Value> {
        let d = self.load()?;
        let s = self.check_render(&d)?;
        if capture_backend::selected(&d) == "rust" {
            return crate::recording::protect(self, &d, &s);
        }
        require(
            self.existing(&s)?.len() == s["cameras"].as_array().unwrap().len(),
            "GROUP_INCOMPLETE",
            "Required cameras not running",
        )?;
        for c in s["cameras"].as_array().unwrap() {
            self.healthy(
                text(&c["service"]),
                d["startup_timeout_seconds"].as_f64().unwrap().ceil() as u64,
            )?;
            self.probe(text(&c["service"]))?;
        }
        let token = random(32);
        atomic_json(
            &self.root.join("state/recording-guard.json"),
            &json!({"version":1,"generation":s["generation"],"token_sha256":hash(&token)}),
        )?;
        Ok(
            json!({"token":token,"guard_version":1,"native_restart_gate":false,"session_bound":false}),
        )
    }
    pub fn release(&self, token: &str) -> Result<()> {
        let path = self.root.join("state/recording-guard.json");
        if let Ok(record) = read_json(&path) {
            if record["version"] == 2 {
                return crate::recording::release(self, token, record);
            }
        }
        let valid = read_json(&path)
            .ok()
            .and_then(|v| {
                v["token_sha256"]
                    .as_str()
                    .map(|s| s.as_bytes().ct_eq(hash(token).as_bytes()).into())
            })
            .unwrap_or(false);
        require(
            valid,
            "GUARD_CREDENTIAL",
            "Valid recording credential required",
        )?;
        fs::remove_file(path)?;
        Ok(())
    }
    pub fn build(&self, d: &Value) -> Result<Value> {
        config::validate(d, true)?;
        let mut command = args(&[
            "docker",
            "build",
            "--build-arg",
            &format!("ORBBEC_COMMIT={}", text(&d["upstream_commit"])),
        ]);
        if capture_backend::selected(d) == "rust" {
            command.extend(args(&["-f", "capture.Dockerfile"]));
        } else {
            let patch = read_json(&self.root.join("patches/public-orbbec/manifest.json"))?;
            command.extend(args(&[
                "--build-arg",
                &format!("YUYV_PATCH_SHA256={}", text(&patch["patch_sha256"])),
            ]));
        }
        command.extend(args(&["-t", text(&d["image"]), "."]));
        let output = self.run_vec(&command, 3600)?;
        Ok(json!({"log":output}))
    }
    pub fn migrate(&self, b: &Value, apply: bool, expected: &str) -> Result<Value> {
        let source = fs::read(self.root.join("inventory.json"))?;
        let candidate = config::migrate(&serde_json::from_slice(&source)?, b)?;
        let mut bytes = source.clone();
        bytes.push(0);
        bytes.extend(canonical(&candidate).bytes());
        let plan = hash(bytes);
        let mut summary = json!({"schema_version":2,"camera_count":candidate["cameras"].as_array().unwrap().len(),"enabled_count":config::active(&candidate).len(),"plan_digest":plan,"confirmation_required":true,"native_yuyv_required":true,"runtime_supported":true,"generation_supported":true,"applied":false});
        if apply {
            require(
                expected == plan,
                "MIGRATION_STALE",
                "Preview exact migration before applying",
            )?;
            self.unprotected()?;
            self.assert_stopped()?;
            require(
                fs::read(self.root.join("inventory.json"))? == source,
                "MIGRATION_STALE",
                "Inventory changed",
            )?;
            let folder = self.root.join("state/migrations");
            private_dir(&folder)?;
            let mut backup = tempfile::NamedTempFile::new_in(&folder)?;
            backup.write_all(&source)?;
            backup.as_file().sync_all()?;
            backup.keep()?;
            atomic_json(&self.root.join("inventory.json"), &candidate)?;
            summary["applied"] = true.into();
            summary["backup_created"] = true.into();
        }
        Ok(summary)
    }
}
pub fn identify(product: &str) -> Option<&'static str> {
    let norm = |s: &str| {
        s.to_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    };
    let p = norm(product);
    ["Gemini 335L", "Gemini 305", "Gemini 2"]
        .into_iter()
        .find(|name| p == norm(name) || p == format!("orbbec{}", norm(name)))
}
