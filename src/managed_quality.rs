//! Token-free bounded management queries over private capture evidence.
use crate::{capture_snapshot, control::Controller, util::*};
use anyhow::Result;
use quality_core::EventCursor;
use serde_json::{Value, json};
const READER: &str = r#"import os,stat,sys,json,time
backend=sys.argv[1]
if backend not in ('rust','official'):raise SystemExit(2)
def unique(items):
 out={}
 for k,v in items:
  if k in out:raise ValueError('duplicate')
  out[k]=v
 return out
def decode(raw):return json.loads(raw,object_pairs_hook=unique,parse_constant=lambda v:(_ for _ in ()).throw(ValueError('nonfinite')))
flags=os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW
base=os.open('/tmp/capture' if backend=='rust' else '/tmp/capture-observer',flags|os.O_DIRECTORY)
def directory(fd,private=False):
 m=os.fstat(fd)
 if not stat.S_ISDIR(m.st_mode) or m.st_uid!=os.geteuid() or stat.S_IMODE(m.st_mode)&0o022 or (private and stat.S_IMODE(m.st_mode)!=0o700):raise SystemExit(2)
directory(base,True)
def read(fd,name,optional=False):
 try:f=os.open(name,flags|os.O_NONBLOCK,dir_fd=fd)
 except FileNotFoundError:
  if optional:return None
  raise
 with os.fdopen(f,'rb') as handle:
  m=os.fstat(handle.fileno())
  if not stat.S_ISREG(m.st_mode) or stat.S_IMODE(m.st_mode)!=0o600 or m.st_uid!=os.geteuid() or m.st_nlink!=1 or m.st_size>4194304:raise SystemExit(2)
  raw=handle.read(4194305)
  if len(raw)>4194304:raise SystemExit(2)
  return decode(raw)
status=read(base,'status.json');session=status['session_id']
if len(session)!=32 or any(c not in '0123456789abcdef' for c in session):raise SystemExit(2)
intermediate=os.open('quality' if backend=='rust' else 'sessions',flags|os.O_DIRECTORY,dir_fd=base);directory(intermediate)
folder=os.open(session,flags|os.O_DIRECTORY,dir_fd=intermediate);directory(folder,True)
def alive(name):
 value=status.get(name)
 if type(value)!=int or value<=0:return False
 try:os.kill(value,0);return True
 except OSError:return False
processes_alive=alive('pid') and (backend=='rust' or alive('driver_pid'))
print(json.dumps({'processes_alive':processes_alive,'status':status,'current':read(folder,'current.json'),'events':read(folder,'events.json'),'summary':read(folder,'summary.json',True)},allow_nan=False))
"#;
/// Page only already-produced events. Never reclassify or manufacture quality.
pub fn page_events(mut page: Value, cursor: Option<EventCursor>, limit: usize) -> Result<Value> {
    require(
        (1..=1024).contains(&limit),
        "QUALITY_PAGE_LIMIT",
        "Quality limit must be 1..1024",
    )?;
    require(
        page["version"] == 1 && page["has_more"] == false,
        "QUALITY_EVENT_SCHEMA",
        "Complete bounded producer page required",
    )?;
    let origin = capture_contract::SessionId::parse(text(&page["origin_session_id"]))?;
    let latest = page["latest_sequence"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("QUALITY_EVENT_SCHEMA"))?;
    let after = if let Some(c) = cursor {
        require(
            c.origin_session_id == origin,
            "QUALITY_CURSOR_SESSION_MISMATCH",
            "Cursor belongs to another event feed",
        )?;
        require(
            c.after_sequence <= latest,
            "QUALITY_CURSOR_AHEAD",
            "Cursor ahead of producer",
        )?;
        c.after_sequence
    } else {
        0
    };
    let events = page["events"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("QUALITY_EVENT_SCHEMA"))?;
    require(
        events.len() <= 100000
            && events.iter().all(|e| {
                e["version"] == 1 && e["sequence"].as_u64().is_some_and(|s| s > 0 && s <= latest)
            })
            && events
                .windows(2)
                .all(|w| w[0]["sequence"].as_u64() < w[1]["sequence"].as_u64()),
        "QUALITY_EVENT_SCHEMA",
        "Producer event sequence invalid",
    )?;
    require(
        (latest == 0 && events.is_empty())
            || (latest > 0
                && !events.is_empty()
                && events.last().and_then(|e| e["sequence"].as_u64()) == Some(latest)
                && events.windows(2).all(|w| {
                    w[0]["sequence"].as_u64().and_then(|n| n.checked_add(1))
                        == w[1]["sequence"].as_u64()
                })),
        "QUALITY_EVENT_SCHEMA",
        "Producer page omits retained event sequence or tail",
    )?;
    require(
        page["next_cursor"]["origin_session_id"] == page["origin_session_id"]
            && page["next_cursor"]["after_sequence"].as_u64() == Some(latest),
        "QUALITY_EVENT_SCHEMA",
        "Producer cursor differs from complete page",
    )?;
    let first = events
        .first()
        .and_then(|e| e["sequence"].as_u64())
        .unwrap_or(latest.saturating_add(1));
    let retained = events
        .iter()
        .filter(|e| e["sequence"].as_u64().unwrap() > after)
        .cloned()
        .collect::<Vec<_>>();
    let has_more = retained.len() > limit;
    let selected = retained.into_iter().take(limit).collect::<Vec<_>>();
    let next = selected
        .last()
        .and_then(|e| e["sequence"].as_u64())
        .unwrap_or(after);
    page["gap"] = if after.saturating_add(1) < first {
        json!({"code":"EVENT_GAP","requested_after":after,"first_retained_sequence":first,"last_lost_sequence":first-1})
    } else {
        Value::Null
    };
    page["events"] = json!(selected);
    page["has_more"] = json!(has_more);
    page["next_cursor"] = json!({"origin_session_id":origin,"after_sequence":next});
    Ok(page)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QualityIdentity {
    container_id: String,
    instance_id: capture_contract::InstanceId,
    session_id: capture_contract::SessionId,
    worker_pid: u32,
    runtime_config_sha256: capture_contract::ConfigurationId,
    snapshot_id: Option<capture_contract::SnapshotId>,
}
fn quality_identity(
    status: &Value,
    container_id: &str,
    generation: &Value,
    runtime_hash: &str,
    backend: &str,
) -> Result<QualityIdentity> {
    require(
        status["version"] == 1
            && status["kind"]
                == if backend == "rust" {
                    "capture-service-status"
                } else {
                    "official-observer-status"
                }
            && status["generation"] == generation["generation"]
            && status["config_sha256"].as_str() == Some(runtime_hash)
            && (backend == "rust"
                || (status["session_kind"] == "observer"
                    && status["native_session_verified"] == false)),
        "QUALITY_BINDING_MISMATCH",
        "Quality status source/config identity differs",
    )?;
    require(
        status["updated_unix_ms"]
            .as_f64()
            .is_some_and(|ms| ms > 0. && ms <= now() * 1000.),
        "QUALITY_STATUS_CLOCK_INVALID",
        "Status Unix time invalid",
    )?;
    let snapshot_id = if status["snapshot_id"].is_null() {
        require(
            status["snapshot_path"].is_null(),
            "QUALITY_BINDING_MISMATCH",
            "Unobserved snapshot has no path",
        )?;
        None
    } else {
        let id = capture_contract::SnapshotId::parse(text(&status["snapshot_id"]))?;
        let path = format!(
            "{}/snapshots/{id}.json",
            if backend == "rust" {
                "/tmp/capture"
            } else {
                "/tmp/capture-observer"
            }
        );
        require(
            status["snapshot_path"].as_str() == Some(&path),
            "QUALITY_BINDING_MISMATCH",
            "Snapshot reference path differs",
        )?;
        Some(id)
    };
    Ok(QualityIdentity {
        container_id: capture_contract::InstanceId::parse(container_id)?.to_string(),
        instance_id: capture_contract::InstanceId::parse(text(&status["instance_id"]))?,
        session_id: capture_contract::SessionId::parse(text(&status["session_id"]))?,
        worker_pid: status["pid"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| anyhow::anyhow!("QUALITY_BINDING_MISMATCH"))?,
        runtime_config_sha256: capture_contract::ConfigurationId::parse(runtime_hash)?,
        snapshot_id,
    })
}
fn quality_bundle(
    ctl: &Controller,
    d: &Value,
    generation: &Value,
    camera: &Value,
) -> Result<(QualityIdentity, Value)> {
    let backend = crate::capture_backend::selected(d);
    require(
        backend == "rust" || generation["observer_backend"] == "ros-observed-v1",
        "OFFICIAL_OBSERVER_UNAVAILABLE",
        "Regenerate observer-enabled deployment",
    )?;
    let container = ctl
        .existing(generation)?
        .into_iter()
        .find(|c| c["Config"]["Labels"]["orbbec.camera"] == camera["name"])
        .ok_or_else(|| anyhow::anyhow!("QUALITY_CAMERA_UNAVAILABLE"))?;
    let container_id = capture_contract::InstanceId::parse(text(&container["Id"]))?;
    let config = ctl
        .root
        .join("state/generations")
        .join(text(&generation["generation"]))
        .join("config")
        .join(text(&camera["name"]))
        .join(if backend == "rust" {
            "capture.json"
        } else {
            "observer-policy.json"
        });
    let runtime_hash = file_hash(&config)?;
    let raw = ctl
        .run(&[
            "docker",
            "exec",
            container_id.as_str(),
            "python3",
            "-c",
            READER,
            backend,
        ])
        .map_err(|_| anyhow::anyhow!("QUALITY_PRIVATE_EVIDENCE_UNAVAILABLE"))?;
    let bundle = capture_contract::parse_strict(raw.as_bytes())?;
    let identity = quality_identity(
        &bundle["status"],
        container_id.as_str(),
        generation,
        &runtime_hash,
        backend,
    )?;
    Ok((identity, bundle))
}
fn validate_bundle(value: &Value, before: &QualityIdentity) -> Result<Value> {
    let current = &value["current"];
    let quality = current["quality"].clone();
    require(
        current["instance_id"].as_str() == Some(before.instance_id.as_str())
            && current["session_id"].as_str() == Some(before.session_id.as_str())
            && current["snapshot_id"] == value["status"]["snapshot_id"]
            && current["snapshot_id"].as_str() == before.snapshot_id.as_ref().map(|s| s.as_str()),
        "QUALITY_BINDING_MISMATCH",
        "Quality evidence belongs to another capture context",
    )?;
    require(
        quality["version"] == 1
            && quality["active_episodes"].is_array()
            && quality["finalized_sessions"].is_array()
            && quality["session_id"].as_str() == Some(before.session_id.as_str()),
        "QUALITY_SNAPSHOT_SCHEMA",
        "Versioned bound producer snapshot required",
    )?;
    capture_contract::SessionId::parse(text(&quality["origin_session_id"]))?;
    require(
        quality["latest_event_sequence"].as_u64().is_some()
            && quality["current_session_finalized"].is_boolean()
            && quality["at"]["session_id"].as_str() == Some(before.session_id.as_str())
            && quality["at"]["offset_ns"].as_u64().is_some(),
        "QUALITY_SNAPSHOT_SCHEMA",
        "Quality clock/cursor metadata missing",
    )?;
    require(
        value["events"]["origin_session_id"] == quality["origin_session_id"]
            && value["events"]["latest_sequence"] == quality["latest_event_sequence"],
        "QUALITY_EVIDENCE_CHANGED",
        "Current/event revisions differ; retry query",
    )?;
    require(
        quality["current_session_finalized"] != true || value["status"]["ready"] != true,
        "QUALITY_EVIDENCE_CHANGED",
        "Finalized worker cannot be reported ready",
    )?;
    Ok(quality)
}
fn session_summary_result(
    value: &Value,
    quality: &Value,
    before: &QualityIdentity,
) -> Result<Value> {
    let summary = &value["summary"];
    if !summary.is_null() {
        require(
            summary["version"] == 1
                && summary["summary"]["version"] == 1
                && summary["summary"]["finalized"] == true
                && quality["current_session_finalized"] == true
                && summary["instance_id"].as_str() == Some(before.instance_id.as_str())
                && summary["summary"]["session_id"].as_str() == Some(before.session_id.as_str())
                && quality["finalized_sessions"]
                    .as_array()
                    .is_some_and(|sessions| sessions.iter().any(|s| *s == summary["summary"])),
            "QUALITY_SUMMARY_BINDING",
            "Summary is not this finalized producer session",
        )?;
    }
    Ok(
        json!({"available":!summary.is_null(),"summary":summary,"finalized_sessions":quality["finalized_sessions"]}),
    )
}
fn query_inner(
    ctl: &Controller,
    name: &str,
    kind: &str,
    cursor: Option<EventCursor>,
    limit: usize,
) -> Result<Value> {
    let d = ctl.load()?;
    let generation = ctl.check_render(&d)?;
    let camera = d["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name && c["enabled"] == true)
        .ok_or_else(|| anyhow::anyhow!("QUALITY_CAMERA_UNAVAILABLE"))?;
    let backend = crate::capture_backend::selected(&d);
    let (before, value) = quality_bundle(ctl, &d, &generation, camera)?;
    let current = &value["current"];
    let quality = validate_bundle(&value, &before)?;
    let (after, _) = quality_bundle(ctl, &d, &generation, camera)?;
    require(
        before == after && ctl.load()? == d,
        "QUALITY_SESSION_CHANGED",
        "Capture context changed while querying quality",
    )?;
    let result = match kind {
        "current" => json!({"quality":quality}),
        "events" => {
            let page = value["events"].clone();
            require(
                page["origin_session_id"] == quality["origin_session_id"],
                "QUALITY_EVENT_BINDING",
                "Event feed origin differs",
            )?;
            json!({"page":page_events(page,cursor,limit)?})
        }
        "session-summary" => session_summary_result(&value, &quality, &before)?,
        _ => anyhow::bail!("QUALITY_QUERY_KIND"),
    };
    let age_ms =
        (now() * 1000. - value["status"]["updated_unix_ms"].as_f64().unwrap_or(0.)).max(0.);
    let fresh = age_ms
        <= if backend == "rust" {
            d["rust_capture"]["service"]["heartbeat_timeout_ms"]
                .as_f64()
                .unwrap_or(0.)
        } else {
            10_000.
        };
    let pair_fresh = backend != "rust"
        || value["status"]["last_pair_age_ms"]
            .as_f64()
            .is_some_and(|age| {
                age >= 0.
                    && age + age_ms
                        <= d["rust_capture"]["service"]["idle_timeout_ms"]
                            .as_f64()
                            .unwrap_or(0.)
            });
    let worker_ready = value["status"]["ready"] == true
        && value["status"]["state"] == "ready"
        && value["processes_alive"] == true
        && fresh
        && pair_fresh;
    Ok(
        json!({"version":1,"kind":format!("quality-{kind}"),"backend":backend,"camera_name":name,"instance_id":before.instance_id,"session_id":before.session_id,"session_kind":if backend=="rust"{"sdk-worker"}else{"observer"},"snapshot_id":current["snapshot_id"],"lifecycle_interruption":current["lifecycle_interruption"],"generation_id":generation["generation"],"worker_ready":worker_ready,"worker_reported_ready":value["status"]["ready"],"status_age_ms":((now()*1000.)-value["status"]["updated_unix_ms"].as_f64().unwrap_or(0.)).max(0.),"result":result}),
    )
}
pub fn recording_context(ctl: &Controller, strong: bool) -> Result<Value> {
    let d = ctl.load()?;
    let backend = crate::capture_backend::selected(&d);
    let native = backend == "rust";
    require(
        !strong || native,
        "RECORDING_STRONG_GUARANTEE_UNSUPPORTED",
        "Official observed sessions cannot gate native SDK automatic reopen",
    )?;
    let generation = ctl.check_render(&d)?;
    let witnesses = capture_snapshot::acquire(ctl, &d, &generation)?;
    let instances = ctl.existing(&generation)?;
    let native_gate_available = native
        && witnesses.iter().all(|w| {
            let config = ctl
                .root
                .join("state/generations")
                .join(text(&generation["generation"]))
                .join("config")
                .join(&w.camera_name)
                .join("capture.json");
            let configured = read_json(&config)
                .is_ok_and(|c| c["service"]["recording_control_path"] == "/recording/control.json");
            configured
                && instances
                    .iter()
                    .find(|i| i["Id"] == w.container_id)
                    .is_some_and(|i| {
                        i["Mounts"].as_array().is_some_and(|m| {
                            m.iter().any(|v| {
                                v["Destination"] == "/recording"
                                    && v["RW"] == true
                                    && v["Source"].as_str()
                                        == ctl
                                            .root
                                            .join("state/recording-gates")
                                            .join(&w.camera_name)
                                            .to_str()
                            })
                        })
                    })
        });
    require(
        !strong || native_gate_available,
        "RECORDING_NATIVE_GATE_UNAVAILABLE",
        "Current service lacks the persistent native gate capability",
    )?;

    let cameras=witnesses.iter().map(|w|json!({"camera_name":w.camera_name,"container_id":w.container_id,"instance_id":w.instance_id,"session_id":w.session_id,"session_kind":if native{"sdk-worker"}else{"observer"},"snapshot":w.snapshot,"quality":{"current":format!("/api/quality/current?camera={}",w.camera_name),"events":format!("/api/quality/events?camera={}",w.camera_name),"session-summary":format!("/api/quality/session-summary?camera={}",w.camera_name)}})).collect::<Vec<_>>();
    let guard = crate::recording::status(ctl);
    let bindings = witnesses
        .iter()
        .filter_map(|w| {
            w.snapshot
                .as_ref()
                .map(|s| crate::recording::ActiveBinding {
                    camera_name: &w.camera_name,
                    binding: &s.content.binding,
                    snapshot_id: &s.snapshot_id,
                    runtime_config_sha256: &w.runtime_config_sha256,
                    worker_pid: w.worker_pid,
                })
        })
        .collect::<Vec<_>>();
    let bound = native_gate_available
        && crate::recording::active_bindings_match(ctl, text(&generation["generation"]), &bindings);
    for witness in &witnesses {
        let camera = d["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == witness.camera_name)
            .ok_or_else(|| anyhow::anyhow!("QUALITY_CAMERA_UNAVAILABLE"))?;
        let after = capture_snapshot::live_witness(ctl, &d, &generation, camera)?;
        require(
            capture_snapshot::same_session(witness, &after),
            "RECORDING_SESSION_CHANGED",
            "Capture context changed while querying protection",
        )?;
    }
    require(
        ctl.load()? == d,
        "RECORDING_CONFIGURATION_CHANGED",
        "Configuration changed during context query",
    )?;
    Ok(
        json!({"version":1,"kind":"recording-context","backend":backend,"generation_id":generation["generation"],"cameras":cameras,"protection":{"guard_version":if native{2}else{1},"state":guard["state"],"native_restart_gate":native_gate_available,"native_gate_ack_verified":bound,"capability_evidence":"configuration-and-persistent-mount; active-session-requires-current-ACK","session_bound":bound,"strong_guarantee_available":native_gate_available},"physical_accuracy_verified":false}),
    )
}

pub fn public_error(error: anyhow::Error) -> anyhow::Error {
    let text = error.to_string();
    let code = text.split(':').next().unwrap_or("");
    if !code.is_empty()
        && code.len() <= 64
        && code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
    {
        anyhow::anyhow!("{code}")
    } else {
        anyhow::anyhow!("QUALITY_QUERY_UNAVAILABLE")
    }
}

pub fn query(
    ctl: &Controller,
    name: &str,
    kind: &str,
    cursor: Option<EventCursor>,
    limit: usize,
) -> Result<Value> {
    query_inner(ctl, name, kind, cursor, limit).map_err(public_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn feed() -> Value {
        json!({"version":1,"origin_session_id":"a".repeat(32),"latest_sequence":102,"gap":null,"events":(100..=102).map(|s|json!({"version":1,"sequence":s,"scope":{"kind":if s==100{"recorder"}else{"future-scope"}},"fact":"unchanged"})).collect::<Vec<_>>(),"next_cursor":{"origin_session_id":"a".repeat(32),"after_sequence":102},"has_more":false})
    }
    #[test]
    fn page_schema_errors_identify_invalid_latest_and_event_array() {
        for (field, value) in [("latest_sequence", json!("102")), ("events", json!({}))] {
            let mut page = feed();
            page[field] = value;
            assert_eq!(
                page_events(page, None, 10).unwrap_err().to_string(),
                "QUALITY_EVENT_SCHEMA"
            );
        }
    }

    #[test]
    fn missing_camera_instance_returns_a_bounded_availability_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctl = Controller::new(directory.path()).unwrap();
        ctl.command_runner = Some(std::sync::Arc::new(|args, _| {
            assert_eq!(&args[..2], ["docker", "ps"]);
            Ok(String::new())
        }));
        let err = quality_bundle(
            &ctl,
            &json!({"capture_backend":"rust"}),
            &json!({"generation":"d".repeat(32),"cameras":[{"name":"front"}]}),
            &json!({"name":"front"}),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "QUALITY_CAMERA_UNAVAILABLE");
    }
    #[test]
    fn pages_preserve_unknown_scope_facts_and_explicit_gap() {
        let page = page_events(feed(), None, 2).unwrap();
        assert_eq!(page["events"][0]["scope"]["kind"], "recorder");
        assert_eq!(page["events"][1]["scope"]["kind"], "future-scope");
        assert_eq!(page["gap"]["code"], "EVENT_GAP");
        assert_eq!(page["gap"]["last_lost_sequence"], 99);
        assert_eq!(page["has_more"], true);
        let cursor: EventCursor = serde_json::from_value(page["next_cursor"].clone()).unwrap();
        let next = page_events(feed(), Some(cursor), 2).unwrap();
        assert_eq!(next["events"].as_array().unwrap().len(), 1);
        assert_eq!(next["has_more"], false);
        assert_eq!(next["next_cursor"]["after_sequence"], 102);
    }
    #[test]
    fn reject_wrong_feed_ahead_cursor_and_unbounded_page() {
        let id = capture_contract::SessionId::parse("b".repeat(32)).unwrap();
        assert!(
            page_events(
                feed(),
                Some(EventCursor {
                    origin_session_id: id,
                    after_sequence: 0
                }),
                10
            )
            .is_err()
        );
        assert!(page_events(feed(), None, 1025).is_err());
        let id = capture_contract::SessionId::parse("a".repeat(32)).unwrap();
        assert!(
            page_events(
                feed(),
                Some(EventCursor {
                    origin_session_id: id,
                    after_sequence: 103
                }),
                10
            )
            .is_err()
        );
    }
    #[test]
    fn public_errors_never_include_private_details() {
        assert_eq!(
            public_error(anyhow::anyhow!("SNAPSHOT_SESSION_CHANGED: /private/secret")).to_string(),
            "SNAPSHOT_SESSION_CHANGED"
        );
        assert_eq!(
            public_error(anyhow::anyhow!("bad file /private/secret SERIAL")).to_string(),
            "QUALITY_QUERY_UNAVAILABLE"
        );
    }
    fn terminal_bundle() -> (Value, Value) {
        let generation = json!({"generation":"d".repeat(32)});
        let status = json!({"version":1,"kind":"capture-service-status","instance_id":"b".repeat(64),"session_id":"a".repeat(32),"snapshot_id":null,"snapshot_path":null,"generation":generation["generation"],"config_sha256":"e".repeat(64),"pid":std::process::id(),"ready":false,"state":"degraded","updated_unix_ms":now()*1000.});
        let summary = json!({"version":1,"session_id":"a".repeat(32),"finalized":true,"last_mono_offset_ns":10,"frame_counts":{"color":0,"depth":0},"open_episode_sequences":[1]});
        let quality = json!({"version":1,"session_id":"a".repeat(32),"origin_session_id":"a".repeat(32),"at":{"session_id":"a".repeat(32),"offset_ns":10},"active_episodes":[],"finalized_sessions":[summary],"current_session_finalized":true,"latest_event_sequence":1});
        let bundle = json!({"status":status,"current":{"version":1,"instance_id":"b".repeat(64),"session_id":"a".repeat(32),"snapshot_id":null,"quality":quality},"events":{"version":1,"origin_session_id":"a".repeat(32),"latest_sequence":1,"gap":null,"events":[{"version":1,"sequence":1,"scope":{"kind":"recorder"},"fact":"SDK failed before snapshot"}],"next_cursor":{"origin_session_id":"a".repeat(32),"after_sequence":1},"has_more":false},"summary":{"version":1,"instance_id":"b".repeat(64),"summary":summary}});
        (generation, bundle)
    }
    #[test]
    fn terminal_recorder_events_without_snapshot_remain_bound_and_readable() {
        let (generation, bundle) = terminal_bundle();
        let before = quality_identity(
            &bundle["status"],
            &"f".repeat(64),
            &generation,
            &"e".repeat(64),
            "rust",
        )
        .unwrap();
        assert!(before.snapshot_id.is_none());
        let quality = validate_bundle(&bundle, &before).unwrap();
        let page = page_events(bundle["events"].clone(), None, 10).unwrap();
        assert_eq!(page["events"][0]["scope"]["kind"], "recorder");
        assert_eq!(
            session_summary_result(&bundle, &quality, &before).unwrap()["available"],
            true
        );
        let mut wrong = bundle.clone();
        wrong["current"]["snapshot_id"] = json!("0".repeat(64));
        assert!(validate_bundle(&wrong, &before).is_err());
        let mut wrong = bundle.clone();
        wrong["events"]["latest_sequence"] = json!(0);
        assert!(validate_bundle(&wrong, &before).is_err());
        let mut wrong = bundle.clone();
        wrong["summary"]["summary"]["finalized"] = json!(false);
        assert!(session_summary_result(&wrong, &quality, &before).is_err());
        let mut wrong = bundle.clone();
        wrong["summary"]["summary"]["frame_counts"]["depth"] = json!(1);
        assert!(session_summary_result(&wrong, &quality, &before).is_err());
    }
    #[test]
    fn observer_identity_cannot_be_promoted_to_native_when_not_ready() {
        let (generation, mut bundle) = terminal_bundle();
        bundle["status"]["kind"] = json!("official-observer-status");
        bundle["status"]["session_kind"] = json!("observer");
        bundle["status"]["native_session_verified"] = json!(false);
        assert!(
            quality_identity(
                &bundle["status"],
                &"f".repeat(64),
                &generation,
                &"e".repeat(64),
                "official"
            )
            .is_ok()
        );
        bundle["status"]["native_session_verified"] = json!(true);
        assert!(
            quality_identity(
                &bundle["status"],
                &"f".repeat(64),
                &generation,
                &"e".repeat(64),
                "official"
            )
            .is_err()
        );
    }
    #[test]
    fn incomplete_retained_event_page_cannot_hide_internal_or_trailing_gaps() {
        let mut missing = feed();
        missing["events"].as_array_mut().unwrap().remove(1);
        assert!(page_events(missing, None, 10).is_err());
        let mut missing = feed();
        missing["events"].as_array_mut().unwrap().pop();
        assert!(page_events(missing, None, 10).is_err());
        let mut missing = feed();
        missing["next_cursor"]["after_sequence"] = json!(101);
        assert!(page_events(missing, None, 10).is_err());
    }
    #[test]
    fn real_private_reader_accepts_snapshotless_terminal_files_and_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("capture");
        private_dir(&base).unwrap();
        let folder = base.join("quality").join("a".repeat(32));
        private_dir(&folder).unwrap();
        let (_, bundle) = terminal_bundle();
        atomic_json(&base.join("status.json"), &bundle["status"]).unwrap();
        for name in ["current", "events", "summary"] {
            atomic_json(&folder.join(format!("{name}.json")), &bundle[name]).unwrap();
        }
        let reader = READER.replace(
            "'/tmp/capture'",
            &serde_json::to_string(base.to_str().unwrap()).unwrap(),
        );
        let run = || {
            std::process::Command::new("python3")
                .args(["-c", &reader, "rust"])
                .output()
                .unwrap()
        };
        let result = run();
        assert!(
            result.status.success(),
            "private reader must accept terminal evidence"
        );
        let actual: capture_contract::Result<Value> =
            capture_contract::parse_strict(&result.stdout);
        assert!(actual.unwrap()["current"]["snapshot_id"].is_null());
        std::fs::remove_file(folder.join("events.json")).unwrap();
        symlink(base.join("status.json"), folder.join("events.json")).unwrap();
        assert!(!run().status.success());
    }
}
