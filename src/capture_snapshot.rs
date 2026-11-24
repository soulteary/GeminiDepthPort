//! Consume the worker's actual SDK snapshot, preserving its evidence ID. The
//! manager never reconstructs a different ROS snapshot for a Rust session.
use crate::{config, control::Controller, util::*};
use anyhow::Result;
use capture_contract::{snapshot::*, store::SnapshotStore, *};
use serde_json::{Value, json};
use std::path::PathBuf;
#[derive(Clone)]
pub struct Witness {
    pub camera_name: String,
    pub service: String,
    pub instance_id: InstanceId,
    pub container_id: String,
    pub instance_source: String,
    pub session_id: SessionId,
    pub worker_pid: u32,
    pub runtime_config_sha256: ConfigurationId,
    pub snapshot: Option<SnapshotEnvelope>,
    sdk_snapshot_id: SnapshotId,
    sdk_snapshot_path: String,
}
impl Witness {
    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.sdk_snapshot_id
    }
}
fn source_identity_matches(status: &Value, official: bool) -> bool {
    status["kind"]
        == if official {
            "official-observer-status"
        } else {
            "capture-service-status"
        }
        && (!official
            || (status["session_kind"] == "observer" && status["native_session_verified"] == false))
}
fn sdk_reference(status: &Value) -> Result<(SnapshotId, String)> {
    let id = SnapshotId::parse(
        status["snapshot_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_SDK_EVIDENCE_MISSING"))?,
    )?;
    let expected = format!("/tmp/capture/snapshots/{id}.json");
    require(
        status["snapshot_path"].as_str() == Some(expected.as_str()),
        "SNAPSHOT_SDK_PATH_INVALID",
        "SDK snapshot must use the declared private content address",
    )?;
    Ok((id, expected))
}
pub fn live_witness(
    ctl: &Controller,
    d: &Value,
    generation: &Value,
    camera: &Value,
) -> Result<Witness> {
    witness(ctl, d, generation, camera, true)
}
/// Read terminal/degraded quality evidence without pretending the worker is live.
pub fn status_witness(
    ctl: &Controller,
    d: &Value,
    generation: &Value,
    camera: &Value,
) -> Result<Witness> {
    witness(ctl, d, generation, camera, false)
}
fn witness(
    ctl: &Controller,
    d: &Value,
    generation: &Value,
    camera: &Value,
    require_ready: bool,
) -> Result<Witness> {
    let name = text(&camera["name"]);
    let container = ctl
        .existing(generation)?
        .into_iter()
        .find(|v| v["Config"]["Labels"]["orbbec.camera"] == name)
        .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_INSTANCE_MISSING"))?;
    let container_id = InstanceId::parse(text(&container["Id"]))?;
    let official = crate::capture_backend::selected(d) != "rust";
    require(
        !official || generation["observer_backend"] == "ros-observed-v1",
        "OFFICIAL_OBSERVER_UNAVAILABLE",
        "Regenerate and start an observer-enabled official image",
    )?;
    let status_path = if official {
        "/tmp/capture-observer/status.json"
    } else {
        "/tmp/capture/status.json"
    };
    let code = "import json,os,sys;from pathlib import Path;v=json.loads(Path(sys.argv[1]).read_text());[(os.kill(v[k],0)) for k in ['pid','driver_pid'] if sys.argv[2]=='1' and k in v];print(json.dumps(v))";
    let raw = ctl.run(&[
        "docker",
        "exec",
        container_id.as_str(),
        "python3",
        "-c",
        code,
        status_path,
        if require_ready { "1" } else { "0" },
    ])?;
    let status = capture_contract::parse_strict(raw.as_bytes())?;
    let instance_id = InstanceId::parse(
        status["instance_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_NATIVE_INSTANCE_REQUIRED"))?,
    )?;
    let (sdk_snapshot_id, sdk_snapshot_path) = if official {
        let id = SnapshotId::parse(text(&status["snapshot_id"]))?;
        let path = format!("/tmp/capture-observer/snapshots/{id}.json");
        require(
            status["snapshot_path"].as_str() == Some(path.as_str()),
            "SNAPSHOT_OBSERVER_PATH_INVALID",
            "Observer snapshot path differs",
        )?;
        (id, path)
    } else {
        sdk_reference(&status)?
    };
    let runtime_config_sha256 =
        ConfigurationId::parse(file_hash(&generation_config(ctl, generation, name))?)?;
    let age = now() * 1000. - status["updated_unix_ms"].as_f64().unwrap_or(0.);
    let ready = status["ready"] == true
        && status["state"] == "ready"
        && age
            <= if official {
                10_000.
            } else {
                d["rust_capture"]["service"]["heartbeat_timeout_ms"]
                    .as_f64()
                    .unwrap_or(0.)
            }
        && if official {
            status["session_kind"] == "observer"
                && status["native_session_verified"] == false
                && ["color", "depth"].iter().all(|r| {
                    status["received_counts"][*r]
                        .as_u64()
                        .is_some_and(|v| v >= 3)
                })
        } else {
            status["last_pair_age_ms"].as_u64().is_some_and(|v| {
                v <= d["rust_capture"]["service"]["idle_timeout_ms"]
                    .as_u64()
                    .unwrap_or(0)
            })
        };
    require(
        source_identity_matches(&status, official)
            && status["version"] == 1
            && status["generation"] == generation["generation"]
            && status["config_sha256"].as_str() == Some(runtime_config_sha256.as_str())
            && age >= 0.
            && (!require_ready || ready),
        "SNAPSHOT_SESSION_NOT_READY",
        "Capture identity or readiness does not match",
    )?;
    let worker_pid = status["pid"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_WORKER_PID"))?;
    Ok(Witness {
        camera_name: name.into(),
        service: format!("camera_{name}"),
        instance_id,
        container_id: container_id.to_string(),
        instance_source: if official {
            "observer-supervisor-status"
        } else {
            "supervisor-status"
        }
        .into(),
        session_id: SessionId::parse(text(&status["session_id"]))?,
        worker_pid,
        runtime_config_sha256,
        snapshot: None,
        sdk_snapshot_id,
        sdk_snapshot_path,
    })
}
fn generation_config(ctl: &Controller, generation: &Value, name: &str) -> PathBuf {
    ctl.root
        .join("state/generations")
        .join(text(&generation["generation"]))
        .join("config")
        .join(name)
        .join(if generation["capture_backend"] == "rust" {
            "capture.json"
        } else {
            "observer-policy.json"
        })
}
pub fn same_session(a: &Witness, b: &Witness) -> bool {
    a.camera_name == b.camera_name
        && a.instance_id == b.instance_id
        && a.container_id == b.container_id
        && a.session_id == b.session_id
        && a.worker_pid == b.worker_pid
        && a.runtime_config_sha256 == b.runtime_config_sha256
        && a.sdk_snapshot_id == b.sdk_snapshot_id
        && a.sdk_snapshot_path == b.sdk_snapshot_path
}
// Pin both directory inodes and open only the typed hash filename. The code is
// executed without a shell, as the container's configured capture user.
const PRIVATE_SNAPSHOT_READER: &str = r#"import os,stat,sys
name=sys.argv[1]
if len(name)!=64 or any(c not in '0123456789abcdef' for c in name):raise SystemExit(2)
flags=os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW
parent=os.open('/tmp/capture',flags|os.O_DIRECTORY)
def private_dir(fd):
 m=os.fstat(fd)
 if not stat.S_ISDIR(m.st_mode) or stat.S_IMODE(m.st_mode)!=0o700 or m.st_uid!=os.geteuid():raise SystemExit(2)
private_dir(parent)
directory=os.open('snapshots',flags|os.O_DIRECTORY,dir_fd=parent)
private_dir(directory)
fd=os.open(name+'.json',flags|os.O_NONBLOCK,dir_fd=directory)
m=os.fstat(fd)
if not stat.S_ISREG(m.st_mode) or stat.S_IMODE(m.st_mode)!=0o600 or m.st_uid!=os.geteuid() or m.st_nlink!=1 or m.st_size>4194304:raise SystemExit(2)
with os.fdopen(fd,'rb') as f:raw=f.read(4194305)
if len(raw)>4194304:raise SystemExit(2)
sys.stdout.buffer.write(raw)
"#;
fn validate_sdk_snapshot(
    bytes: &[u8],
    witness: &Witness,
    generation: &Value,
    camera: &Value,
) -> Result<SnapshotEnvelope> {
    let parsed = capture_contract::parse_strict(bytes)?;
    let envelope: SnapshotEnvelope = serde_json::from_value(parsed)?;
    envelope.verify(&envelope.content.binding)?;
    envelope.require_runtime()?;
    let binding = &envelope.content.binding;
    require(
        envelope.snapshot_id == witness.sdk_snapshot_id
            && binding.instance_id == witness.instance_id
            && binding.session_id == witness.session_id
            && binding.generation_id.as_str() == text(&generation["generation"])
            && envelope.content.configuration["runtime_config_sha256"].as_str()
                == Some(witness.runtime_config_sha256.as_str()),
        "SNAPSHOT_SDK_BINDING_MISMATCH",
        "SDK snapshot differs from the current native session or config",
    )?;
    require(
        envelope.content.calibration.source
            == if witness.instance_source == "observer-supervisor-status" {
                CalibrationSource::RosCameraInfoTf
            } else {
                CalibrationSource::SdkActiveProfiles
            },
        "SNAPSHOT_SDK_SOURCE_REQUIRED",
        "Rust capture requires actual active-profile SDK calibration",
    )?;
    for role in ["color", "depth"] {
        let model = envelope
            .content
            .calibration
            .models
            .get(role)
            .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_SDK_MODEL_MISSING"))?;
        require(
            model.frame_id == format!("{}_{role}_optical_frame", text(&camera["frame_prefix"]))
                && u64::from(model.width)
                    == camera["parameters"][format!("{role}_width")]
                        .as_u64()
                        .unwrap_or(0)
                && u64::from(model.height)
                    == camera["parameters"][format!("{role}_height")]
                        .as_u64()
                        .unwrap_or(0),
            "SNAPSHOT_SDK_PROFILE_MISMATCH",
            "Actual SDK camera model differs from the declared output",
        )?;
    }
    require(
        envelope.content.calibration.transforms.iter().any(|t| {
            t.source_frame == format!("{}_depth_optical_frame", text(&camera["frame_prefix"]))
                && t.target_frame
                    == format!("{}_color_optical_frame", text(&camera["frame_prefix"]))
        }),
        "SNAPSHOT_SDK_D2C_MISSING",
        "Active SDK depth-to-color transform required",
    )?;
    require(
        matches!(
            envelope.content.depth,
            DepthRepresentation::Millimetres { .. }
        ),
        "SNAPSHOT_SDK_DEPTH_UNIT_MISMATCH",
        "Published Rust depth must declare integer millimetres",
    )?;
    Ok(envelope)
}
pub fn acquire(ctl: &Controller, d: &Value, generation: &Value) -> Result<Vec<Witness>> {
    let cameras = config::active(d);
    let mut witnesses: Vec<_> = cameras
        .iter()
        .map(|c| live_witness(ctl, d, generation, c))
        .collect::<Result<_>>()?;
    private_dir(&ctl.root.join("state"))?;
    let store = SnapshotStore::open(&ctl.root.join("state/capture-snapshots"))?;
    for (camera, witness) in cameras.iter().zip(witnesses.iter_mut()) {
        let raw = ctl
            .run(&[
                "docker",
                "exec",
                &witness.container_id,
                "python3",
                "-c",
                &if witness.instance_source == "observer-supervisor-status" {
                    PRIVATE_SNAPSHOT_READER.replace("/tmp/capture", "/tmp/capture-observer")
                } else {
                    PRIVATE_SNAPSHOT_READER.into()
                },
                witness.sdk_snapshot_id.as_str(),
            ])
            .map_err(|_| anyhow::anyhow!("SNAPSHOT_SDK_PRIVATE_READ_FAILED"))?;
        let snapshot = validate_sdk_snapshot(raw.as_bytes(), witness, generation, camera)?;
        require(
            same_session(witness, &live_witness(ctl, d, generation, camera)?),
            "SNAPSHOT_SESSION_CHANGED",
            "Session changed during SDK snapshot read",
        )?;
        store.publish(&snapshot)?;
        witness.snapshot = Some(snapshot);
    }
    for (camera, witness) in cameras.iter().zip(&witnesses) {
        require(
            same_session(witness, &live_witness(ctl, d, generation, camera)?),
            "SNAPSHOT_SESSION_CHANGED",
            "A capture session changed before collection completed",
        )?;
    }
    require(
        ctl.load()? == *d,
        "SNAPSHOT_CONFIGURATION_CHANGED",
        "Inventory changed during snapshot collection",
    )?;
    Ok(witnesses)
}
pub fn machine_summary(witnesses: &[Witness]) -> Value {
    json!({"version":1,"kind":"capture-snapshot-collection","physical_accuracy_verified":false,"snapshots":witnesses.iter().filter_map(|w|w.snapshot.as_ref()).map(|s|json!({"snapshot_id":s.snapshot_id,"binding":s.content.binding})).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
    };
    fn fixture() -> (SnapshotEnvelope, Witness, Value, Value) {
        let session = SessionId::parse("a".repeat(32)).unwrap();
        let instance = InstanceId::parse("b".repeat(64)).unwrap();
        let config_sha = ConfigurationId::parse("c".repeat(64)).unwrap();
        let generation = GenerationId::parse("d".repeat(32)).unwrap();
        let models = BTreeMap::from_iter(["color", "depth"].map(|role| {
            (
                role.into(),
                CameraModel {
                    frame_id: format!("camera_{role}_optical_frame"),
                    width: 640,
                    height: 480,
                    k: [500., 0., 320., 0., 500., 240., 0., 0., 1.],
                    distortion_model: DistortionModel::None,
                    distortion_coefficients: vec![],
                },
            )
        }));
        let calibration = CalibrationRecord {
            version: 1,
            source: CalibrationSource::SdkActiveProfiles,
            actual_runtime_observed: true,
            source_session_id: Some(session.clone()),
            source_instance_id: Some(instance.clone()),
            observed_unix_ns: Some(1),
            models,
            transforms: vec![RigidTransform {
                source_frame: "camera_depth_optical_frame".into(),
                target_frame: "camera_color_optical_frame".into(),
                rotation_row_major: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                translation_m: [0.02, 0., 0.],
            }],
            physical_accuracy_verified: false,
            legacy_source_sha256: None,
        };
        let configuration = json!({"runtime_config_sha256":config_sha});
        let binding = SnapshotBinding {
            instance_id: instance.clone(),
            session_id: session.clone(),
            generation_id: generation.clone(),
            configuration_id: ConfigurationId::parse(
                content_digest(
                    ContentKind::Configuration,
                    &serde_json::to_vec(&configuration).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
            calibration_id: calibration.id().unwrap(),
        };
        let snapshot = CaptureSnapshot {
            version: 1,
            binding,
            configuration,
            calibration,
            depth: DepthRepresentation::Millimetres {
                quantization:
                    QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
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
        .unwrap();
        let witness = Witness {
            camera_name: "camera_a".into(),
            service: "camera_camera_a".into(),
            instance_id: instance,
            container_id: "e".repeat(64),
            instance_source: "supervisor-status".into(),
            session_id: session,
            worker_pid: 42,
            runtime_config_sha256: config_sha,
            snapshot: None,
            sdk_snapshot_id: snapshot.snapshot_id.clone(),
            sdk_snapshot_path: format!("/tmp/capture/snapshots/{}.json", snapshot.snapshot_id),
        };
        (
            snapshot,
            witness,
            json!({"generation":generation}),
            json!({"frame_prefix":"camera","parameters":{"color_width":640,"color_height":480,"depth_width":640,"depth_height":480}}),
        )
    }
    #[test]
    fn official_terminal_source_cannot_be_misrepresented_as_sdk_session() {
        for ready in [false, true] {
            let mut status = json!({"kind":"official-observer-status","session_kind":"observer","native_session_verified":false,"ready":ready});
            assert!(source_identity_matches(&status, true));
            status["session_kind"] = "sdk-worker".into();
            assert!(!source_identity_matches(&status, true));
            status["session_kind"] = "observer".into();
            status["native_session_verified"] = true.into();
            assert!(!source_identity_matches(&status, true));
        }
    }
    #[test]
    fn preserves_the_native_snapshot_id_and_rejects_all_identity_drift() {
        let (snapshot, witness, generation, camera) = fixture();
        let bytes = snapshot.to_bytes().unwrap();
        assert_eq!(
            validate_sdk_snapshot(&bytes, &witness, &generation, &camera)
                .unwrap()
                .snapshot_id,
            snapshot.snapshot_id
        );
        let mut corrupt: Value = serde_json::from_slice(&bytes).unwrap();
        corrupt["content"]["configuration"]["runtime_config_sha256"] = json!("0".repeat(64));
        assert!(
            validate_sdk_snapshot(
                &serde_json::to_vec(&corrupt).unwrap(),
                &witness,
                &generation,
                &camera
            )
            .is_err()
        );
        let mut bad = witness.clone();
        bad.session_id = SessionId::parse("f".repeat(32)).unwrap();
        assert!(validate_sdk_snapshot(&bytes, &bad, &generation, &camera).is_err());
        bad = witness.clone();
        bad.instance_id = InstanceId::parse("f".repeat(64)).unwrap();
        assert!(validate_sdk_snapshot(&bytes, &bad, &generation, &camera).is_err());
        bad = witness.clone();
        bad.runtime_config_sha256 = ConfigurationId::parse("f".repeat(64)).unwrap();
        assert!(validate_sdk_snapshot(&bytes, &bad, &generation, &camera).is_err());
        let mut bad_generation = generation.clone();
        bad_generation["generation"] = json!("f".repeat(32));
        assert!(validate_sdk_snapshot(&bytes, &witness, &bad_generation, &camera).is_err());
        let mut bad = witness.clone();
        bad.container_id = "f".repeat(64);
        assert!(!same_session(&witness, &bad));
        bad = witness.clone();
        bad.sdk_snapshot_id = SnapshotId::parse("f".repeat(64)).unwrap();
        assert!(!same_session(&witness, &bad));
    }
    #[test]
    fn rejects_resealed_ros_provenance_geometry_or_unit_downgrade() {
        let (snapshot, mut witness, generation, camera) = fixture();
        for failure in 0..4 {
            let mut content = snapshot.content.clone();
            match failure {
                0 => {
                    content.calibration.source = CalibrationSource::RosCameraInfoTf;
                    content.binding.calibration_id = content.calibration.id().unwrap();
                }
                1 => {
                    content.calibration.models.get_mut("color").unwrap().width = 320;
                    content.binding.calibration_id = content.calibration.id().unwrap();
                }
                2 => content.depth = DepthRepresentation::SdkCounts { scale_mm: 1. },
                _ => {
                    content.calibration.transforms.clear();
                    content.binding.calibration_id = content.calibration.id().unwrap();
                }
            }
            let altered = content.seal().unwrap();
            witness.sdk_snapshot_id = altered.snapshot_id.clone();
            assert!(
                validate_sdk_snapshot(&altered.to_bytes().unwrap(), &witness, &generation, &camera)
                    .is_err()
            );
        }
    }
    #[test]
    fn validly_sealed_snapshot_must_include_each_requested_stream_model() {
        let (snapshot, mut witness, generation, camera) = fixture();
        let mut content = snapshot.content;
        content.calibration.models.remove("color");
        content.calibration.transforms.clear();
        content.binding.calibration_id = content.calibration.id().unwrap();
        let incomplete = content.seal().unwrap();
        witness.sdk_snapshot_id = incomplete.snapshot_id.clone();
        let error = validate_sdk_snapshot(
            &incomplete.to_bytes().unwrap(),
            &witness,
            &generation,
            &camera,
        )
        .expect_err("missing color model must fail");
        assert!(error.to_string().starts_with("SNAPSHOT_SDK_MODEL_MISSING"));
    }

    #[test]
    fn strict_reference_cannot_escape_or_be_missing() {
        let (snapshot, _, _, _) = fixture();
        let valid = json!({"snapshot_id":snapshot.snapshot_id,"snapshot_path":format!("/tmp/capture/snapshots/{}.json",snapshot.snapshot_id)});
        assert!(sdk_reference(&valid).is_ok());
        for path in [
            "/tmp/capture/snapshots/../private.json",
            "/other/snapshot.json",
            "/tmp/capture/snapshots/not-the-id.json",
        ] {
            let mut bad = valid.clone();
            bad["snapshot_path"] = path.into();
            assert!(sdk_reference(&bad).is_err());
        }
        assert!(sdk_reference(&json!({})).is_err());
        for invalid in [json!("not-a-content-digest"), json!("g".repeat(64))] {
            let mut bad = valid.clone();
            bad["snapshot_id"] = invalid;
            assert!(sdk_reference(&bad).is_err());
        }
    }
    #[test]
    fn private_reader_rejects_exposed_files_symlinks_and_bad_directories() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let snapshots = directory.path().join("snapshots");
        fs::create_dir(&snapshots).unwrap();
        fs::set_permissions(&snapshots, fs::Permissions::from_mode(0o700)).unwrap();
        let id = "a".repeat(64);
        let path = snapshots.join(format!("{id}.json"));
        fs::write(&path, b"synthetic-private-bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        // Same production reader, substituting only the isolated test namespace.
        let code =
            PRIVATE_SNAPSHOT_READER.replace("/tmp/capture", directory.path().to_str().unwrap());
        let read = || {
            Command::new("python3")
                .args(["-c", &code, &id])
                .output()
                .unwrap()
        };
        let ok = read();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(ok.stdout, b"synthetic-private-bytes");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!read().status.success());
        fs::remove_file(&path).unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside-private").unwrap();
        symlink(&outside, &path).unwrap();
        assert!(!read().status.success());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"valid").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let linked = directory.path().join("second-link");
        fs::hard_link(&path, &linked).unwrap();
        assert!(!read().status.success());
        fs::remove_file(linked).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(4 * 1024 * 1024 + 1)
            .unwrap();
        assert!(!read().status.success());
        fs::write(&path, b"valid").unwrap();
        fs::set_permissions(&snapshots, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!read().status.success());
    }
}
