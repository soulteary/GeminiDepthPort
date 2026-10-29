//! Explicit offline history: no Docker/PID/ACK/readiness inference.
use crate::{control::Controller, util::*};
use anyhow::{Context, Result};
use capture_contract::{snapshot::SnapshotEnvelope, *};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};
struct Directory(File);
impl Directory {
    fn root(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        require(
            file.metadata()?.uid() == unsafe { libc::geteuid() },
            "QUALITY_HISTORY_OWNER",
            "Management root owner differs",
        )?;
        Ok(Self(file))
    }
    fn open(&self, name: &str, dir: bool) -> Result<File> {
        require(
            !name.is_empty() && !name.contains('/') && name != "." && name != "..",
            "QUALITY_HISTORY_PATH",
            "Single component required",
        )?;
        let name = CString::new(name)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if dir { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        require(
            m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == if dir { 0o700 } else { 0o600 }
                && if dir {
                    m.is_dir()
                } else {
                    m.is_file() && m.nlink() == 1 && m.len() <= 4 * 1024 * 1024
                },
            "QUALITY_HISTORY_PERMISSIONS",
            "Private owner-only evidence required",
        )?;
        Ok(file)
    }
    fn child(&self, name: &str) -> Result<Self> {
        Ok(Self(self.open(name, true)?))
    }
    fn bytes(&self, name: &str) -> Result<Vec<u8>> {
        let mut bytes = vec![];
        self.open(name, false)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        require(
            bytes.len() <= 4 * 1024 * 1024,
            "QUALITY_HISTORY_SIZE",
            "Evidence too large",
        )?;
        Ok(bytes)
    }
    fn json(&self, name: &str) -> Result<Value> {
        Ok(parse_strict(&self.bytes(name)?)?)
    }
    fn optional(&self, name: &str) -> Result<Option<Value>> {
        match self.json(name) {
            Ok(v) => Ok(Some(v)),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}
fn interruption(
    value: &Value,
    session: &str,
    generation: &str,
    config_sha: &str,
) -> Result<InstanceId> {
    let instance = InstanceId::parse(text(&value["instance_id"]))?;
    require(
        value["version"] == 1
            && value["session_id"] == session
            && value["generation_id"] == generation
            && value["config_sha256"] == config_sha
            && value["at"]["session_id"] == session
            && value["at"]["offset_ns"].as_u64().is_some()
            && value["observed_unix_ns"].as_u64().is_some_and(|v| v > 0)
            && value["core_reported"].is_boolean()
            && value["quality_finalized"].is_boolean()
            && matches(r"^[a-z-]{1,64}$", text(&value["reason"])),
        "QUALITY_HISTORY_INTERRUPTION",
        "Controller interruption identity differs",
    )?;
    Ok(instance)
}
fn read_inner(ctl: &Controller, camera: &str, generation: &str, session: &str) -> Result<Value> {
    require(
        matches(r"^[a-z][a-z0-9_]{0,31}$", camera) && matches(r"^[a-f0-9]{32}$", generation),
        "QUALITY_HISTORY_ARGUMENT",
        "Explicit camera/generation required",
    )?;
    let sid = SessionId::parse(session)?;
    let folder = Directory::root(&ctl.root)?
        .child("state")?
        .child("generations")?
        .child(generation)?;
    let manifest = folder.json("manifest.json")?;
    require(
        manifest["kind"] == crate::generation::KIND
            && manifest["generation"] == generation
            && manifest["quality_evidence_version"] == 1
            && manifest["cameras"]
                .as_array()
                .is_some_and(|a| a.iter().any(|c| c["name"] == camera)),
        "QUALITY_HISTORY_GENERATION",
        "Generation does not declare this camera/evidence",
    )?;
    let native = manifest["capture_backend"] == "rust";
    require(
        native || manifest["observer_backend"] == "ros-observed-v1",
        "QUALITY_HISTORY_BACKEND",
        "Observed/native evidence required",
    )?;
    let config_name = if native {
        "capture.json"
    } else {
        "observer-policy.json"
    };
    let bytes = folder.child("config")?.child(camera)?.bytes(config_name)?;
    let expected_hash = hash(&bytes);
    require(
        manifest["files"][format!("config/{camera}/{config_name}")] == expected_hash,
        "QUALITY_HISTORY_CONFIG",
        "Generation configuration drift",
    )?;
    let base = folder.child("evidence")?.child(camera)?;
    let session_dir = base
        .child(if native { "quality" } else { "sessions" })?
        .child(session)?;
    let mut result = json!({"version":1,"kind":"quality-session-summary","live":false,"worker_ready":false,
        "native_gate_ack_verified":false,"session_bound":false,"physical_accuracy_verified":false,"camera_name":camera,"generation_id":generation,
        "session_id":session,"session_kind":if native{"sdk-worker"}else{"observer"},"backend":if native{"rust"}else{"official"},
        "complete":false,"available":false,"incomplete":true,"reason":"CURRENT_UNAVAILABLE","state":"incomplete","snapshot_verified":false,"summary":null});
    let Some(current) = session_dir.optional("current.json")? else {
        if !native {
            if let Some(fact) = session_dir.optional("interruption.json")? {
                let instance = interruption(&fact, session, generation, &expected_hash)?;
                result["instance_id"] = json!(instance);
                result["reason"] = "OBSERVER_INTERRUPTION_WITHOUT_QUALITY".into();
                result["facts"] = json!({"quality":null,"events":null,"lifecycle_interruption":fact,"binding_verified":false});
            }
        }
        return Ok(result);
    };
    require(
        current["kind"]
            == if native {
                "capture-service-quality"
            } else {
                "official-observer-quality"
            }
            && current["version"] == 1
            && current["session_id"] == session
            && (current["quality"].is_null() || current["quality"]["session_id"] == session),
        "QUALITY_HISTORY_SESSION",
        "Current belongs to another session",
    )?;
    let instance = InstanceId::parse(text(&current["instance_id"]))?;
    if !current["lifecycle_interruption"].is_null() {
        require(
            interruption(
                &current["lifecycle_interruption"],
                session,
                generation,
                &expected_hash,
            )? == instance,
            "QUALITY_HISTORY_INTERRUPTION",
            "Controller instance differs",
        )?;
    }
    result["instance_id"] = json!(instance);
    result["reason"] = "SNAPSHOT_UNAVAILABLE".into();
    result["facts"] = json!({"quality":current["quality"],"events":null,"lifecycle_interruption":current["lifecycle_interruption"],"binding_verified":false});
    if let Some(events) = session_dir.optional("events.json")? {
        let events = crate::managed_quality::page_events(events, None, 1024)?;
        require(
            events["origin_session_id"] == current["quality"]["origin_session_id"]
                && events["latest_sequence"].as_u64().is_some()
                && events["latest_sequence"] == current["quality"]["latest_event_sequence"],
            "QUALITY_HISTORY_EVENTS",
            "Feed origin or current/event revision differs",
        )?;
        result["facts"]["events"] = events;
    }

    let Some(snapshot_text) = current["snapshot_id"].as_str() else {
        return Ok(result);
    };
    let snapshot_id = SnapshotId::parse(snapshot_text)?;
    let snapshot_bytes = base
        .child("snapshots")?
        .bytes(&format!("{snapshot_id}.json"))?;
    let value = parse_strict(&snapshot_bytes)?;
    let binding = serde_json::from_value(value["content"]["binding"].clone())?;
    let envelope = SnapshotEnvelope::parse(&snapshot_bytes, &binding)?;
    envelope.require_runtime()?;
    require(
        envelope.snapshot_id == snapshot_id
            && envelope.content.binding.session_id == sid
            && envelope.content.binding.instance_id == instance
            && envelope.content.binding.generation_id.as_str() == generation
            && envelope.content.configuration["runtime_config_sha256"] == expected_hash
            && envelope.content.calibration.source
                == if native {
                    snapshot::CalibrationSource::SdkActiveProfiles
                } else {
                    snapshot::CalibrationSource::RosCameraInfoTf
                },
        "QUALITY_HISTORY_SNAPSHOT",
        "Snapshot differs from generation/session/source",
    )?;
    result["snapshot_id"] = json!(snapshot_id);
    result["snapshot_verified"] = true.into();
    result["reason"] = "FINALIZATION_INCOMPLETE".into();
    result["facts"]["binding_verified"] = true.into();
    let Some(summary) = session_dir.optional("summary.json")? else {
        return Ok(result);
    };
    require(
        summary["version"] == 1
            && summary["instance_id"] == current["instance_id"]
            && summary["summary"]["version"] == 1
            && summary["summary"]["session_id"] == session
            && summary["summary"]["finalized"] == true,
        "QUALITY_HISTORY_SUMMARY",
        "Summary is not finalized for this session",
    )?;
    let decoded: quality_core::SessionSummary = serde_json::from_value(summary["summary"].clone())?;
    require(
        decoded.version == 1 && decoded.session_id == sid && decoded.finalized,
        "QUALITY_HISTORY_SUMMARY",
        "Summary contract invalid",
    )?;
    // A summary alone can survive a partial/crashed final commit. Both producer
    // records must agree before claiming the session evidence is complete.
    if current["quality"]["current_session_finalized"] == true
        && current["quality"]["finalized_sessions"]
            .as_array()
            .is_some_and(|a| a.contains(&summary["summary"]))
    {
        result["complete"] = true.into();
        result["available"] = true.into();
        result["incomplete"] = false.into();
        result["reason"] = Value::Null;
        result["state"] = "finalized".into();
        result["summary"] = summary;
    }
    Ok(result)
}
pub fn read(ctl: &Controller, camera: &str, generation: &str, session: &str) -> Result<Value> {
    read_inner(ctl, camera, generation, session)
        .context("QUALITY_HISTORY_UNAVAILABLE")
        .map_err(crate::managed_quality::public_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_contract::snapshot::*;
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    fn save(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn fixture(native: bool) -> (tempfile::TempDir, Controller, String) {
        let temp = tempfile::tempdir().unwrap();
        let ctl = Controller::new(temp.path()).unwrap();
        let gid = "c".repeat(32);
        let folder = ctl.root.join("state/generations").join(&gid);
        for part in [
            "state".to_string(),
            "state/generations".to_string(),
            format!("state/generations/{gid}"),
            format!("state/generations/{gid}/config"),
            format!("state/generations/{gid}/config/cam_a"),
            format!("state/generations/{gid}/evidence"),
            format!("state/generations/{gid}/evidence/cam_a"),
            format!("state/generations/{gid}/evidence/cam_a/snapshots"),
            format!(
                "state/generations/{gid}/evidence/cam_a/{}",
                if native { "quality" } else { "sessions" }
            ),
        ] {
            private_dir(&ctl.root.join(part)).unwrap();
        }
        let name = if native {
            "capture.json"
        } else {
            "observer-policy.json"
        };
        save(
            &folder.join("config/cam_a").join(name),
            &json!({"generation":gid,"private":"synthetic"}),
        );
        save(
            &folder.join("manifest.json"),
            &json!({"kind":crate::generation::KIND,"generation":gid,"quality_evidence_version":1,"capture_backend":if native{"rust"}else{"official"},"observer_backend":"ros-observed-v1","cameras":[{"name":"cam_a"}],"files":{format!("config/cam_a/{name}"):file_hash(&folder.join("config/cam_a").join(name)).unwrap()}}),
        );
        (temp, ctl, gid)
    }
    fn session(
        ctl: &Controller,
        gid: &str,
        sid: &str,
        native: bool,
        finalized: bool,
    ) -> std::path::PathBuf {
        let base = ctl
            .root
            .join("state/generations")
            .join(gid)
            .join("evidence/cam_a");
        let directory = base
            .join(if native { "quality" } else { "sessions" })
            .join(sid);
        private_dir(&directory).unwrap();
        let session = SessionId::parse(sid).unwrap();
        let instance = InstanceId::parse("b".repeat(64)).unwrap();
        let model = CameraModel {
            frame_id: "depth".into(),
            width: 2,
            height: 2,
            k: [1., 0., 1., 0., 1., 1., 0., 0., 1.],
            distortion_model: DistortionModel::None,
            distortion_coefficients: vec![],
        };
        let calibration = CalibrationRecord {
            version: 1,
            source: if native {
                CalibrationSource::SdkActiveProfiles
            } else {
                CalibrationSource::RosCameraInfoTf
            },
            actual_runtime_observed: true,
            source_session_id: Some(session.clone()),
            source_instance_id: Some(instance.clone()),
            observed_unix_ns: Some(1),
            models: BTreeMap::from([("depth".into(), model)]),
            transforms: vec![],
            physical_accuracy_verified: false,
            legacy_source_sha256: None,
        };
        let config = ctl
            .root
            .join("state/generations")
            .join(gid)
            .join("config/cam_a")
            .join(if native {
                "capture.json"
            } else {
                "observer-policy.json"
            });
        let configuration = json!({"runtime_config_sha256":file_hash(&config).unwrap()});
        let snapshot = CaptureSnapshot {
            version: 1,
            binding: SnapshotBinding {
                instance_id: instance.clone(),
                session_id: session.clone(),
                generation_id: GenerationId::parse(gid).unwrap(),
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
            depth: DepthRepresentation::SdkCounts { scale_mm: 1. },
            quality_policy: QualityPolicy {
                version: 1,
                max_source_age_ns: 1,
                max_future_ns: 1,
                max_pair_delta_us: 1,
                min_valid_fraction: 0.,
                max_zero_fraction: 1.,
                action: QualityAction::ReportOnly,
            },
        }
        .seal()
        .unwrap();
        let id = snapshot.snapshot_id.clone();
        save(
            &base.join("snapshots").join(format!("{id}.json")),
            &serde_json::to_value(snapshot).unwrap(),
        );
        let summary = json!({"version":1,"session_id":sid,"finalized":true,"last_mono_offset_ns":10,"frame_counts":{"depth":2},"open_episode_sequences":[]});
        save(
            &directory.join("current.json"),
            &json!({"version":1,"kind":if native{"capture-service-quality"}else{"official-observer-quality"},"session_id":sid,"instance_id":instance,"snapshot_id":id,"quality":{"session_id":sid,"current_session_finalized":finalized,"finalized_sessions":if finalized{json!([summary])}else{json!([])}}}),
        );
        if finalized {
            save(
                &directory.join("summary.json"),
                &json!({"version":1,"instance_id":instance,"summary":summary}),
            );
        }
        directory
    }
    #[test]
    fn normal_stop_history_survives_without_container_or_inventory_and_sessions_do_not_mix() {
        for native in [true, false] {
            let (_temp, ctl, gid) = fixture(native);
            let first = "a".repeat(32);
            let next = "d".repeat(32);
            session(&ctl, &gid, &first, native, true);
            session(&ctl, &gid, &next, native, false);
            let history = read_inner(&ctl, "cam_a", &gid, &first).unwrap();
            assert_eq!(history["state"], "finalized");
            assert_eq!(history["live"], false);
            assert_eq!(history["worker_ready"], false);
            assert_eq!(history["session_bound"], false);
            assert_eq!(history["native_gate_ack_verified"], false);
            let crashed = read(&ctl, "cam_a", &gid, &next).unwrap();
            assert_eq!(crashed["state"], "incomplete");
            assert!(crashed["summary"].is_null());
            assert_ne!(history["snapshot_id"], crashed["snapshot_id"]);
        }
    }
    #[test]
    fn crash_partial_final_commit_does_not_fabricate_finalization() {
        let (_temp, ctl, gid) = fixture(true);
        let sid = "a".repeat(32);
        let dir = session(&ctl, &gid, &sid, true, true);
        let mut current = read_json(&dir.join("current.json")).unwrap();
        current["quality"]["current_session_finalized"] = false.into();
        save(&dir.join("current.json"), &current);
        let history = read_inner(&ctl, "cam_a", &gid, &sid).unwrap();
        assert_eq!(history["complete"], false);
        assert!(history["summary"].is_null());
    }
    #[test]
    fn reject_escape_symlink_loose_permissions_and_snapshot_tampering() {
        let (_temp, ctl, gid) = fixture(true);
        let sid = "a".repeat(32);
        let dir = session(&ctl, &gid, &sid, true, true);
        assert!(read(&ctl, "../cam_a", &gid, &sid).is_err());
        assert!(read(&ctl, "cam_a", "../bad", &sid).is_err());
        assert!(read(&ctl, "cam_a", &gid, "../bad").is_err());
        let path = dir.join("summary.json");
        let original = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        symlink("current.json", &path).unwrap();
        assert!(read(&ctl, "cam_a", &gid, &sid).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&ctl, "cam_a", &gid, &sid).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let base = ctl
            .root
            .join("state/generations")
            .join(&gid)
            .join("evidence/cam_a/snapshots");
        let snap = fs::read_dir(base).unwrap().next().unwrap().unwrap().path();
        let mut value = read_json(&snap).unwrap();
        value["content"]["configuration"]["runtime_config_sha256"] = "e".repeat(64).into();
        save(&snap, &value);
        assert!(read(&ctl, "cam_a", &gid, &sid).is_err());
    }
    #[test]
    fn snapshot_none_keeps_explicitly_unbound_recorder_facts() {
        let (_temp, ctl, gid) = fixture(true);
        let sid = "a".repeat(32);
        let dir = session(&ctl, &gid, &sid, true, false);
        let mut current = read_json(&dir.join("current.json")).unwrap();
        current["snapshot_id"] = Value::Null;
        current["quality"]["origin_session_id"] = sid.clone().into();
        current["quality"]["latest_event_sequence"] = 1.into();
        save(&dir.join("current.json"), &current);
        save(
            &dir.join("events.json"),
            &json!({"version":1,"origin_session_id":sid,"latest_sequence":1,"gap":null,"events":[{"version":1,"sequence":1,"scope":{"kind":"recorder"},"fact":"native-start-failed"}],"next_cursor":{"origin_session_id":sid,"after_sequence":1},"has_more":false}),
        );
        let history = read_inner(&ctl, "cam_a", &gid, &sid).unwrap();
        assert_eq!(history["available"], false);
        assert_eq!(history["snapshot_verified"], false);
        assert_eq!(history["facts"]["binding_verified"], false);
        assert_eq!(
            history["facts"]["events"]["events"][0]["scope"]["kind"],
            "recorder"
        );
        assert_eq!(history["reason"], "SNAPSHOT_UNAVAILABLE");
    }
    #[test]
    fn same_feed_stale_events_cannot_masquerade_as_complete_final_history() {
        for native in [true, false] {
            let (_temp, ctl, gid) = fixture(native);
            let sid = "a".repeat(32);
            let dir = session(&ctl, &gid, &sid, native, true);
            let mut current = read_json(&dir.join("current.json")).unwrap();
            current["quality"]["origin_session_id"] = sid.clone().into();
            current["quality"]["latest_event_sequence"] = 2.into();
            save(&dir.join("current.json"), &current);
            let page = |latest: u64| {
                json!({"version":1,"origin_session_id":sid,"latest_sequence":latest,
                "gap":null,"events":(1..=latest).map(|sequence|json!({"version":1,"sequence":sequence,"scope":{"kind":"recorder"},"fact":"synthetic"})).collect::<Vec<_>>(),
                "next_cursor":{"origin_session_id":sid,"after_sequence":latest},"has_more":false})
            };
            // Individually valid bounded page, same feed and correct tail, but
            // it predates the finalized current record and omits its final event.
            save(&dir.join("events.json"), &page(1));
            assert!(read_inner(&ctl, "cam_a", &gid, &sid).is_err());
            save(&dir.join("events.json"), &page(2));
            let history = read_inner(&ctl, "cam_a", &gid, &sid).unwrap();
            assert_eq!(history["complete"], true);
            assert_eq!(history["facts"]["events"]["latest_sequence"], 2);
            assert_eq!(history["live"], false);
            assert_eq!(history["worker_ready"], false);
            assert_eq!(history["native_gate_ack_verified"], false);
        }
    }
    #[test]
    fn standalone_observer_interruption_never_fabricates_quality_or_summary() {
        let (_temp, ctl, gid) = fixture(false);
        let sid = "a".repeat(32);
        let dir = session(&ctl, &gid, &sid, false, false);
        let mut current = read_json(&dir.join("current.json")).unwrap();
        fs::remove_file(dir.join("current.json")).unwrap();
        let config = ctl
            .root
            .join("state/generations")
            .join(&gid)
            .join("config/cam_a/observer-policy.json");
        let mut fact = json!({"version":1,"instance_id":"b".repeat(64),"session_id":sid,"snapshot_id":null,
            "generation_id":gid,"config_sha256":file_hash(&config).unwrap(),"reason":"core-unavailable",
            "at":{"session_id":sid,"offset_ns":1},"observed_unix_ns":1,"core_reported":false,"quality_finalized":false});
        save(&dir.join("interruption.json"), &fact);
        let history = read_inner(&ctl, "cam_a", &gid, &sid).unwrap();
        assert_eq!(history["complete"], false);
        assert!(history["facts"]["quality"].is_null());
        assert_eq!(
            history["facts"]["lifecycle_interruption"]["reason"],
            "core-unavailable"
        );
        current["quality"] = Value::Null;
        current["snapshot_id"] = Value::Null;
        current["lifecycle_interruption"] = fact.clone();
        save(&dir.join("current.json"), &current);
        assert!(read_inner(&ctl, "cam_a", &gid, &sid).unwrap()["facts"]["quality"].is_null());
        fs::remove_file(dir.join("current.json")).unwrap();
        fact["config_sha256"] = "f".repeat(64).into();
        save(&dir.join("interruption.json"), &fact);
        assert!(read_inner(&ctl, "cam_a", &gid, &sid).is_err());
    }
    #[test]
    fn historical_session_config_and_summary_mismatches_remain_incomplete() {
        for mode in 0..6 {
            let (_temp, ctl, gid) = fixture(true);
            let sid = "a".repeat(32);
            let dir = session(&ctl, &gid, &sid, true, true);
            let folder = ctl.root.join("state/generations").join(&gid);
            let (path, mut value) = if mode < 3 {
                let p = folder.join("manifest.json");
                let v = read_json(&p).unwrap();
                (p, v)
            } else if mode < 5 {
                let p = dir.join("current.json");
                let v = read_json(&p).unwrap();
                (p, v)
            } else {
                let p = dir.join("summary.json");
                let v = read_json(&p).unwrap();
                (p, v)
            };
            match mode {
                0 => value["cameras"] = json!([]),
                1 => {
                    value["capture_backend"] = json!("official");
                    value["observer_backend"] = Value::Null;
                }
                2 => value["files"]["config/cam_a/capture.json"] = json!("f".repeat(64)),
                3 => value["session_id"] = json!("b".repeat(32)),
                4 => value["snapshot_id"] = json!("b".repeat(64)),
                _ => value["summary"]["finalized"] = json!(false),
            }
            save(&path, &value);
            assert!(read_inner(&ctl, "cam_a", &gid, &sid).is_err(), "{mode}");
        }
    }
    #[test]
    fn history_directory_rejects_component_traversal_and_oversized_private_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Directory::root(tmp.path()).unwrap();
        assert!(dir.open("../escape", false).is_err());
        let path = tmp.path().join("large.json");
        let file = File::create(&path).unwrap();
        file.set_len(4 * 1024 * 1024 + 1).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(dir.bytes("large.json").is_err());
        if unsafe { libc::geteuid() } != 0 {
            assert!(Directory::root(Path::new("/")).is_err());
        } else {
            let name = CString::new(tmp.path().as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::chown(name.as_ptr(), 65534, u32::MAX) }, 0);
            assert!(Directory::root(tmp.path()).is_err());
        }
    }
}
