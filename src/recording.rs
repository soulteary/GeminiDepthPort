//! Authenticated, durable recording handshake. Preparing is already protection;
//! failures intentionally retain both the guard and every installed native gate.
use crate::{
    capture_snapshot::{self, Witness},
    control::Controller,
    util::*,
};
use anyhow::Result;
use capture_contract::{
    ConfigurationId, SessionId,
    recording::{GateAck, GatePhase, GateRequest},
    store::SnapshotStore,
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
fn guard(ctl: &Controller) -> PathBuf {
    ctl.root.join("state/recording-guard.json")
}
fn receipt(ctl: &Controller) -> PathBuf {
    ctl.root.join("state/recording-recovery-credential")
}
fn directory(ctl: &Controller, name: &str) -> Result<PathBuf> {
    require(
        capture_contract::token(name) && !name.contains('/'),
        "RECORDING_GATE_NAME",
        "Invalid camera gate name",
    )?;
    let path = ctl.root.join("state/recording-gates").join(name);
    let m = fs::symlink_metadata(&path)?;
    require(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.mode() & 0o777 == 0o700
            && m.uid() == unsafe { libc::geteuid() },
        "RECORDING_GATE_NOT_PRIVATE",
        "Generation must mount a private persistent recording gate",
    )?;
    Ok(path)
}
fn private_read(path: &Path) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    require(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600
            && m.len() <= 65536
            && m.nlink() == 1,
        "RECORDING_CONTROL_INVALID",
        "Private regular control file required",
    )?;
    let mut bytes = vec![];
    file.by_ref().take(65537).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn gate_matches_mount(container: &Value, path: &Path) -> bool {
    container["Mounts"].as_array().is_some_and(|a| {
        a.iter().any(|m| {
            m["Destination"] == "/recording"
                && m["Source"].as_str() == path.to_str()
                && m["RW"] == true
        })
    })
}
fn wait_ack(ctl: &Controller, witness: &Witness, request: &GateRequest) -> Result<()> {
    let path = directory(ctl, &witness.camera_name)?.join("ack.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(bytes) = private_read(&path) {
            if let Ok(value) = capture_contract::parse_strict(&bytes) {
                if let Ok(ack) = serde_json::from_value::<GateAck>(value) {
                    if request.verify_ack(&ack, witness.worker_pid).is_ok() {
                        return Ok(());
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("RECORDING_GATE_ACK_TIMEOUT: Preparing protection retained")
}
fn write_requests(ctl: &Controller, witnesses: &[Witness], requests: &[GateRequest]) -> Result<()> {
    for (w, r) in witnesses.iter().zip(requests) {
        atomic_json(
            &directory(ctl, &w.camera_name)?.join("control.json"),
            &serde_json::to_value(r)?,
        )?;
    }
    for (w, r) in witnesses.iter().zip(requests) {
        wait_ack(ctl, w, r)?;
    }
    Ok(())
}
fn recheck(ctl: &Controller, d: &Value, generation: &Value, witnesses: &[Witness]) -> Result<()> {
    require(
        ctl.load()? == *d && ctl.check_render(d)?["generation"] == generation["generation"],
        "RECORDING_CONFIGURATION_CHANGED",
        "Capture configuration changed while preparing",
    )?;
    let store = SnapshotStore::open(&ctl.root.join("state/capture-snapshots"))?;
    for witness in witnesses {
        let camera = d["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == witness.camera_name)
            .unwrap();
        let after = capture_snapshot::live_witness(ctl, d, generation, camera)?;
        require(
            capture_snapshot::same_session(witness, &after),
            "RECORDING_SESSION_CHANGED",
            "Native session changed while preparing",
        )?;
        let snapshot = witness.snapshot.as_ref().unwrap();
        store
            .read(&snapshot.snapshot_id, &snapshot.content.binding)?
            .require_runtime()?;
    }
    Ok(())
}
pub fn protect(ctl: &Controller, d: &Value, generation: &Value) -> Result<Value> {
    ctl.unprotected()?;
    let witnesses = capture_snapshot::acquire(ctl, d, generation)?;
    let instances = ctl.existing(generation)?;
    for witness in &witnesses {
        let container = instances
            .iter()
            .find(|i| i["Id"].as_str() == Some(witness.container_id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("RECORDING_INSTANCE_CHANGED"))?;
        require(
            gate_matches_mount(container, &directory(ctl, &witness.camera_name)?),
            "RECORDING_PERSISTENT_GATE_REQUIRED",
            "Regenerate and start a service with a persistent recording control mount",
        )?;
    }
    let token = random(32);
    let token_sha256 = ConfigurationId::parse(hash(&token))?;
    let mut requests = vec![];
    for witness in &witnesses {
        let snapshot = witness.snapshot.as_ref().unwrap();
        requests.push(GateRequest {
            version: 1,
            phase: GatePhase::Preparing,
            request_id: SessionId::parse(random(16))?,
            binding: snapshot.content.binding.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            runtime_config_sha256: witness.runtime_config_sha256.clone(),
            token_sha256: token_sha256.clone(),
        });
    }
    let items = |requests: &[GateRequest]| {
        witnesses
            .iter()
            .zip(requests)
            .map(
                |(w, r)| json!({"camera_name":w.camera_name,"worker_pid":w.worker_pid,"request":r}),
            )
            .collect::<Vec<_>>()
    };
    let mut record = json!({"version":2,"state":"preparing","generation":generation["generation"],"token_sha256":token_sha256,"requests":items(&requests)});
    // A private recovery credential survives even when ACK/health fails before
    // CLI can return it. Never put the credential into the guard or raw logs.
    atomic(&receipt(ctl), token.as_bytes())?;
    atomic_json(&guard(ctl), &record)?;
    let operation = (|| -> Result<Value> {
        write_requests(ctl, &witnesses, &requests)?;
        recheck(ctl, d, generation, &witnesses)?;
        for witness in &witnesses {
            ctl.probe(&witness.service)?;
        }
        recheck(ctl, d, generation, &witnesses)?;
        for request in &mut requests {
            request.phase = GatePhase::Active;
        }
        write_requests(ctl, &witnesses, &requests)?;
        recheck(ctl, d, generation, &witnesses)?;
        record["state"] = "active".into();
        record["requests"] = json!(items(&requests));
        atomic_json(&guard(ctl), &record)?;
        Ok(
            json!({"token":token,"state":"active","snapshot_count":witnesses.len(),"runtime_session_bound":true}),
        )
    })();
    operation.map_err(|_|anyhow::anyhow!("RECORDING_PREPARE_FAILED: Protection retained; inspect recording-status and use private recovery credential with release-protection"))
}
pub fn release(ctl: &Controller, token: &str, mut record: Value) -> Result<()> {
    require(
        record["token_sha256"]
            .as_str()
            .is_some_and(|s| bool::from(s.as_bytes().ct_eq(hash(token).as_bytes()))),
        "GUARD_CREDENTIAL",
        "Valid recording credential required",
    )?;
    require(
        record["version"] == 2,
        "GUARD_VERSION",
        "Unsupported recording guard",
    )?;
    let requests = record["requests"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("GUARD_INVALID"))?
        .clone();
    // Validate the complete release scope before deleting the first gate.
    let mut paths = vec![];
    for item in &requests {
        let name = item["camera_name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("GUARD_INVALID"))?;
        let request: GateRequest = serde_json::from_value(item["request"].clone())?;
        require(
            request.token_sha256.as_str() == text(&record["token_sha256"]),
            "GUARD_INVALID",
            "Gate credential differs",
        )?;
        paths.push(directory(ctl, name)?);
    }
    record["state"] = "releasing".into();
    atomic_json(&guard(ctl), &record)?;
    for path in paths {
        // Credentialed release works with offline services and malformed gates.
        // Only two deterministic files in the private owned mount are removed.
        for name in ["control.json", "ack.json"] {
            match fs::remove_file(path.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        File::open(path)?.sync_all()?;
    }
    fs::remove_file(guard(ctl))?;
    match fs::remove_file(receipt(ctl)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    File::open(ctl.root.join("state"))?.sync_all()?;
    Ok(())
}
/// Read-only proof of an active gate, not merely a configured mount or a saved
/// manager guard. Every current request and native ACK must still agree.
pub(crate) struct ActiveBinding<'a> {
    pub camera_name: &'a str,
    pub binding: &'a capture_contract::snapshot::SnapshotBinding,
    pub snapshot_id: &'a capture_contract::SnapshotId,
    pub runtime_config_sha256: &'a ConfigurationId,
    pub worker_pid: u32,
}
pub(crate) fn active_bindings_match(
    ctl: &Controller,
    generation: &str,
    bindings: &[ActiveBinding<'_>],
) -> bool {
    (|| -> Result<bool> {
        let record = capture_contract::parse_strict(&private_read(&guard(ctl))?)?;
        if record["version"] != 2
            || record["state"] != "active"
            || record["generation"] != generation
            || bindings.is_empty()
        {
            return Ok(false);
        }
        let credential = ConfigurationId::parse(text(&record["token_sha256"]))?;
        let items = record["requests"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GUARD_INVALID"))?;
        if items.len() != bindings.len() {
            return Ok(false);
        }
        let mut names = std::collections::BTreeSet::new();
        for item in items {
            if !names.insert(text(&item["camera_name"])) {
                return Ok(false);
            }
        }
        for expected in bindings {
            let item = items
                .iter()
                .find(|i| i["camera_name"] == expected.camera_name)
                .ok_or_else(|| anyhow::anyhow!("GUARD_INVALID"))?;
            let request = GateRequest::parse(&serde_json::to_vec(&item["request"])?)?;
            if request.phase != GatePhase::Active
                || request.binding != *expected.binding
                || request.snapshot_id != *expected.snapshot_id
                || request.runtime_config_sha256 != *expected.runtime_config_sha256
                || request.token_sha256 != credential
                || item["worker_pid"].as_u64() != Some(u64::from(expected.worker_pid))
            {
                return Ok(false);
            }
            let folder = directory(ctl, expected.camera_name)?;
            let installed = GateRequest::parse(&private_read(&folder.join("control.json"))?)?;
            if installed != request {
                return Ok(false);
            }
            let ack: GateAck = serde_json::from_value(capture_contract::parse_strict(
                &private_read(&folder.join("ack.json"))?,
            )?)?;
            request.verify_ack(&ack, expected.worker_pid)?;
        }
        Ok(true)
    })()
    .unwrap_or(false)
}
pub fn status(ctl: &Controller) -> Value {
    match fs::symlink_metadata(guard(ctl)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            json!({"version":1,"blocked":false,"state":"unprotected"})
        }
        _ => match private_read(&guard(ctl))
            .ok()
            .and_then(|b| capture_contract::parse_strict(&b).ok())
        {
            Some(value) => {
                json!({"version":1,"blocked":true,"state":value.get("state").cloned().unwrap_or(json!("legacy-active")),"guard_version":value["version"],"generation":value["generation"],"camera_count":value["requests"].as_array().map(Vec::len),"recovery_credential_available":receipt(ctl).is_file()})
            }
            None => {
                json!({"version":1,"blocked":true,"state":"invalid","recovery_credential_available":receipt(ctl).is_file()})
            }
        },
    }
}
#[cfg(test)]
mod active_readback_tests {
    use super::*;
    use capture_contract::{
        CalibrationId, GenerationId, InstanceId, SnapshotId, snapshot::SnapshotBinding,
    };
    fn request() -> GateRequest {
        GateRequest {
            version: 1,
            phase: GatePhase::Active,
            request_id: SessionId::parse("a".repeat(32)).unwrap(),
            binding: SnapshotBinding {
                instance_id: InstanceId::parse("b".repeat(64)).unwrap(),
                session_id: SessionId::parse("c".repeat(32)).unwrap(),
                generation_id: GenerationId::parse("d".repeat(32)).unwrap(),
                configuration_id: ConfigurationId::parse("e".repeat(64)).unwrap(),
                calibration_id: CalibrationId::parse("f".repeat(64)).unwrap(),
            },
            snapshot_id: SnapshotId::parse("0".repeat(64)).unwrap(),
            runtime_config_sha256: ConfigurationId::parse("1".repeat(64)).unwrap(),
            token_sha256: ConfigurationId::parse("2".repeat(64)).unwrap(),
        }
    }
    #[test]
    fn saved_active_guard_is_not_enough_without_actual_matching_control_and_ack() {
        let root = tempfile::tempdir().unwrap();
        let ctl = Controller::new(root.path()).unwrap();
        crate::util::private_dir(&root.path().join("state/recording-gates/camera_a")).unwrap();
        let folder = directory(&ctl, "camera_a").unwrap();
        let request = request();
        let record = json!({"version":2,"state":"active","generation":request.binding.generation_id,"token_sha256":request.token_sha256,"requests":[{"camera_name":"camera_a","worker_pid":42,"request":request}]});
        atomic_json(&guard(&ctl), &record).unwrap();
        let binding = ActiveBinding {
            camera_name: "camera_a",
            binding: &request.binding,
            snapshot_id: &request.snapshot_id,
            runtime_config_sha256: &request.runtime_config_sha256,
            worker_pid: 42,
        };
        let expected = [binding];
        let generation = request.binding.generation_id.as_str();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        atomic_json(
            &folder.join("control.json"),
            &serde_json::to_value(&request).unwrap(),
        )
        .unwrap();
        let ack = GateAck {
            version: 1,
            request_id: request.request_id.clone(),
            request_sha256: request.digest().unwrap(),
            worker_pid: 42,
            supervisor_pid: 1,
            session_id: request.binding.session_id.clone(),
        };
        atomic_json(
            &folder.join("ack.json"),
            &serde_json::to_value(&ack).unwrap(),
        )
        .unwrap();
        assert!(active_bindings_match(&ctl, generation, &expected));
        let mut wrong = ack.clone();
        wrong.request_sha256 = ConfigurationId::parse("3".repeat(64)).unwrap();
        atomic_json(
            &folder.join("ack.json"),
            &serde_json::to_value(wrong).unwrap(),
        )
        .unwrap();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        atomic_json(
            &folder.join("ack.json"),
            &serde_json::to_value(&ack).unwrap(),
        )
        .unwrap();
        let mut wrong_record = record.clone();
        wrong_record["requests"][0]["request"]["binding"]["calibration_id"] = json!("3".repeat(64));
        atomic_json(&guard(&ctl), &wrong_record).unwrap();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        atomic_json(&guard(&ctl), &record).unwrap();
        for mode in 0..3 {
            let mut changed = record.clone();
            match mode {
                0 => changed["requests"] = json!([]),
                1 => changed["requests"][0]["camera_name"] = json!("camera_b"),
                _ => changed["state"] = json!("preparing"),
            }
            atomic_json(&guard(&ctl), &changed).unwrap();
            assert!(!active_bindings_match(&ctl, generation, &expected));
        }
        let mut duplicated = record.clone();
        duplicated["requests"]
            .as_array_mut()
            .unwrap()
            .push(record["requests"][0].clone());
        atomic_json(&guard(&ctl), &duplicated).unwrap();
        let two = [
            ActiveBinding {
                camera_name: "camera_a",
                binding: &request.binding,
                snapshot_id: &request.snapshot_id,
                runtime_config_sha256: &request.runtime_config_sha256,
                worker_pid: 42,
            },
            ActiveBinding {
                camera_name: "camera_b",
                binding: &request.binding,
                snapshot_id: &request.snapshot_id,
                runtime_config_sha256: &request.runtime_config_sha256,
                worker_pid: 42,
            },
        ];
        assert!(!active_bindings_match(&ctl, generation, &two));
        atomic_json(&guard(&ctl), &record).unwrap();
        let mut preparing = request.clone();
        preparing.phase = GatePhase::Preparing;
        atomic_json(
            &folder.join("control.json"),
            &serde_json::to_value(preparing).unwrap(),
        )
        .unwrap();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        atomic_json(
            &folder.join("control.json"),
            &serde_json::to_value(&request).unwrap(),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(folder.join("ack.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        fs::set_permissions(folder.join("ack.json"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(folder.join("control.json")).unwrap();
        assert!(!active_bindings_match(&ctl, generation, &expected));
        assert_eq!(status(&ctl)["blocked"], true);
        assert!(guard(&ctl).exists());
    }
}
