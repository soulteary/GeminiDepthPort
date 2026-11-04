//! One in-flight batch and one replaceable pending snapshot; critical writes
//! wait for their own durable completion. No SDK handle crosses this boundary.
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) type Batch = Vec<(PathBuf, Value)>;
#[derive(PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    uid: u32,
    mode: u32,
    links: u64,
}
impl FileIdentity {
    fn new(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            uid: metadata.uid(),
            mode: metadata.mode(),
            links: metadata.nlink(),
        }
    }
}
#[derive(PartialEq, Eq)]
struct EventKey {
    path: PathBuf,
    session: String,
    instance: String,
    digest: Vec<u8>,
}
impl EventKey {
    fn from_batch(batch: &Batch) -> Result<Option<Self>> {
        let Some((_, status)) = batch.last() else {
            return Ok(None);
        };
        let fields = (
            status["quality"]["events_path"].as_str(),
            status["session_id"].as_str(),
            status["instance_id"].as_str(),
        );
        let (Some(path), Some(session), Some(instance)) = fields else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        let Some((_, events)) = batch.iter().find(|(p, _)| *p == path) else {
            return Ok(None);
        };
        Ok(Some(Self {
            path,
            session: session.into(),
            instance: instance.into(),
            digest: ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(events)?)
                .as_ref()
                .to_vec(),
        }))
    }
}
struct EventCache {
    key: EventKey,
    file: FileIdentity,
    directory: (u64, u64),
}
impl EventCache {
    fn identity(path: &Path) -> Result<Option<(FileIdentity, (u64, u64))>> {
        let parent = path.parent().context("events parent required")?;
        let directory = fs::symlink_metadata(parent)?;
        ensure!(
            directory.is_dir() && directory.uid() == unsafe { libc::geteuid() },
            "events directory must be owned and not a symlink"
        );
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok((metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1)
            .then(|| {
                (
                    FileIdentity::new(&metadata),
                    (directory.dev(), directory.ino()),
                )
            }))
    }
    fn matches(&self, key: &EventKey) -> Result<bool> {
        Ok(self.key == *key
            && Self::identity(&key.path)?
                .is_some_and(|(file, directory)| file == self.file && directory == self.directory))
    }
}
#[derive(Default)]
struct DurableWriter {
    // One currently owned events file, not a map that grows across sessions.
    events: Option<EventCache>,
}
impl DurableWriter {
    fn write(&mut self, batch: Batch) -> Result<()> {
        let event = EventKey::from_batch(&batch)?;
        let mut written_event = None;
        let mut directory: Option<PathBuf> = None;
        for (path, value) in batch {
            let parent = path.parent().context("status parent required")?;
            if directory.as_deref() != Some(parent) {
                if let Some(previous) = directory.take() {
                    fs::File::open(previous)?.sync_all()?;
                }
                directory = Some(parent.into());
            }
            let is_event = event.as_ref().is_some_and(|event| event.path == path);
            let unchanged = match (&event, &self.events) {
                (Some(key), Some(cache)) if is_event => cache.matches(key)?,
                _ => false,
            };
            if unchanged {
                continue;
            }
            let metadata = super::atomic_status_file(&path, &value)?;
            if is_event {
                written_event = Some(FileIdentity::new(&metadata));
            }
        }
        if let Some(directory) = directory {
            fs::File::open(directory)?.sync_all()?;
        }
        if let Some(file) = written_event {
            let key = event.unwrap();
            let (actual, directory) = EventCache::identity(&key.path)?
                .context("events file changed after publication")?;
            ensure!(actual == file, "events file changed during persistence");
            // No submitted/coalesced batch may populate this cache. Both the
            // quality directory and the dependent status have completed fsync.
            self.events = Some(EventCache {
                key,
                file,
                directory,
            });
        }
        Ok(())
    }
}
#[derive(Default)]
struct State {
    active: Option<(u64, Instant)>,
    critical_events: VecDeque<Value>,
    critical_wait: capture_contract::timing::StageStats,
    pump_errors: u64,
    queue_wait: capture_contract::timing::StageStats,
    pending: Option<(u64, Batch, Instant)>,
    protected: Option<u64>,
    submitted: u64,
    completed: u64,
    coalesced: u64,
    error: Option<String>,
    closing: bool,
    wall: capture_contract::timing::StageStats,
    #[cfg(test)]
    render_gate: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}
struct Report {
    protected: Option<u64>,
    active: Option<(u64, Instant)>,
    critical_events: VecDeque<Value>,
    critical_wait: capture_contract::timing::StageStats,
    pump_errors: u64,
    queue_wait: capture_contract::timing::StageStats,
    wall: capture_contract::timing::StageStats,
    errors: u64,
    submitted: u64,
    completed: u64,
    pending: usize,
    coalesced: u64,
    #[cfg(test)]
    render_gate: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}
impl Report {
    fn value(self) -> Value {
        #[cfg(test)]
        if let Some(gate) = &self.render_gate {
            gate.0.wait();
            gate.1.wait();
        }
        json!({"active_write":self.active.map(|(sequence,started)|json!({"sequence":sequence,"elapsed_ns":started.elapsed().as_nanos()})),"critical_events":self.critical_events,"critical_event_capacity":16,"critical_wait":self.critical_wait.report(),"pump_errors":self.pump_errors,"queue_wait":self.queue_wait.report(),"wall":self.wall.report(),"errors":self.errors,
            "submitted_sequence":self.submitted,"completed_sequence":self.completed,
            "pending_snapshots":self.pending,"pending_capacity":1,"protected_sequence":self.protected,
            "coalesced_snapshots":self.coalesced,
            "scope":"completed background batches; same-directory quality replacements share a directory fsync before service status; one identity-checked durable events cache; critical updates wait for all fsync"})
    }
}
type Shared = Arc<(Mutex<State>, Condvar)>;
// An opaque ticket separates immutable submission from its durability fence.
// At most one critical ticket may be outstanding. SDK handles remain on the
// caller thread; only the owned JSON batch enters the persistence worker.
#[derive(Debug)]
pub(super) struct PumpFailure(pub(super) anyhow::Error);
impl std::fmt::Display for PumpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for PumpFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}
pub(super) struct Submission {
    sequence: u64,
    critical: bool,
    started: Instant,
    entered_unix_ms: u64,
    state_label: Option<Option<Value>>,
}
pub(super) struct Persistence {
    shared: Shared,
    worker: Option<JoinHandle<()>>,
}
impl Persistence {
    pub(super) fn new() -> Result<Self> {
        let mut writer = DurableWriter::default();
        Self::with_writer(move |batch| writer.write(batch))
    }
    fn with_writer(mut write: impl FnMut(Batch) -> Result<()> + Send + 'static) -> Result<Self> {
        let shared: Shared = Default::default();
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("status-persistence".into())
            .spawn(move || {
                let (lock, changed) = &*worker_shared;
                loop {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.pending.is_none() && !state.closing {
                        state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    let Some((sequence, batch, enqueued)) = state.pending.take() else {
                        break;
                    };
                    let started = Instant::now();
                    state.queue_wait.observe(enqueued.elapsed());
                    state.active = Some((sequence, started));
                    drop(state);
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(batch)))
                            .unwrap_or_else(|_| Err(anyhow!("status persistence thread panicked")));
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.wall.observe(started.elapsed());
                    state.active = None;
                    state.completed = sequence;
                    if state
                        .protected
                        .is_some_and(|protected| sequence >= protected)
                    {
                        state.protected = None;
                    }
                    if let Err(error) = result {
                        state.error = Some(format!("{error:#}"));
                        state.pending = None;
                        state.closing = true;
                        state.protected = None;
                    }
                    changed.notify_all();
                    if state.error.is_some() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }
    pub(super) fn check(&self) -> Result<()> {
        let state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.error.is_none(),
            "status persistence failed: {}",
            state.error.as_deref().unwrap_or_default()
        );
        Ok(())
    }
    pub(super) fn submit(&mut self, batch: Batch, critical: bool) -> Result<()> {
        let submission = self.enqueue(batch, critical)?;
        self.wait(submission)
    }
    pub(super) fn enqueue(&mut self, batch: Batch, critical: bool) -> Result<Submission> {
        let started = Instant::now();
        let entered_unix_ms = super::unix_ms();
        let state_label = critical.then(|| batch.last().map(|(_, value)| json!({"state":value["state"],"raw_state":value["raw_recording"]["state"]})));
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.error.is_none() && !state.closing,
            "status persistence unavailable: {}",
            state.error.as_deref().unwrap_or_default()
        );
        // A pump must never replace a critical batch with a later heartbeat.
        // Reject before mutation instead of silently coalescing that evidence.
        ensure!(state.protected.is_none(), "critical status fence pending");
        state.submitted += 1;
        let sequence = state.submitted;
        if state
            .pending
            .replace((sequence, batch, Instant::now()))
            .is_some()
        {
            state.coalesced += 1;
        }
        if critical {
            state.protected = Some(sequence);
        }
        changed.notify_all();
        Ok(Submission {
            sequence,
            critical,
            started,
            entered_unix_ms,
            state_label,
        })
    }
    pub(super) fn wait(&mut self, submission: Submission) -> Result<()> {
        self.wait_inner(submission, None)
    }
    pub(super) fn wait_with_pump(
        &mut self,
        submission: Submission,
        mut pump: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.wait_inner(submission, Some(&mut pump))
    }
    fn wait_inner(
        &mut self,
        submission: Submission,
        mut pump: Option<&mut dyn FnMut() -> Result<()>>,
    ) -> Result<()> {
        let Submission {
            sequence,
            critical,
            started,
            entered_unix_ms,
            state_label,
        } = submission;
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pump_error = None;
        let mut pump_calls = 0u64;
        while critical && state.completed < sequence && state.error.is_none() {
            if let Some(pump) = pump.as_mut().filter(|_| pump_error.is_none()) {
                // Never call SDK/ROS code while holding the writer's mutex.
                drop(state);
                pump_calls += 1;
                pump_error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut **pump))
                    .unwrap_or_else(|_| Err(anyhow!("status wait IMU pump panicked")))
                    .err();
                state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if state.completed >= sequence || state.error.is_some() {
                    break;
                }
                // A genuine pump error is retained while the immutable critical
                // batch finishes. Do not mask it by clearing samples/retrying.
                if pump_error.is_some() {
                    continue;
                }
                state = changed
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            } else {
                state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }
        if critical {
            state.pump_errors += u64::from(pump_error.is_some());
            state.critical_wait.observe(started.elapsed());
            if state.critical_events.len() == 16 {
                state.critical_events.pop_front();
            }
            let completed = state.completed;
            let ok = state.error.is_none();
            state.critical_events.push_back(json!({"sequence":sequence,"completed_sequence":completed,"target":state_label,"entered_unix_ms":entered_unix_ms,"returned_unix_ms":super::unix_ms(),"wall_ns":started.elapsed().as_nanos(),"ok":ok,"pump_calls":pump_calls,"pump_failed":pump_error.is_some()}));
        }
        ensure!(
            state.error.is_none(),
            "status persistence failed: {}",
            state.error.as_deref().unwrap_or_default()
        );
        if let Some(error) = pump_error {
            return Err(anyhow!(PumpFailure(error)));
        }
        Ok(())
    }
    fn snapshot(shared: &Shared) -> Report {
        let state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
        Report {
            protected: state.protected,
            active: state.active,
            critical_events: state.critical_events.clone(),
            critical_wait: state.critical_wait.clone(),
            pump_errors: state.pump_errors,
            queue_wait: state.queue_wait.clone(),
            wall: state.wall.clone(),
            errors: u64::from(state.error.is_some()),
            submitted: state.submitted,
            completed: state.completed,
            pending: usize::from(state.pending.is_some()),
            coalesced: state.coalesced,
            #[cfg(test)]
            render_gate: state.render_gate.clone(),
        }
    }
    pub(super) fn report(&self) -> Value {
        Self::snapshot(&self.shared).value()
    }
}
impl Drop for Persistence {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closing = true;
        changed.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    fn batch(n: u64) -> Batch {
        vec![(PathBuf::from("status"), json!(n))]
    }
    fn quality_batch(root: &Path, heartbeat: u64, event: u64, session: &str) -> Batch {
        let quality = root.join("quality/session");
        let events = quality.join("events.json");
        vec![
            (quality.join("query.json"), json!({"heartbeat":heartbeat})),
            (quality.join("current.json"), json!({"heartbeat":heartbeat})),
            (events.clone(), json!({"sequence":event})),
            (
                root.join("status.json"),
                json!({"kind":"capture-service-status","heartbeat_sequence":heartbeat,"session_id":session,"instance_id":"instance","quality":{"events_path":events}}),
            ),
        ]
    }
    fn directory() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "gdp-quality-durable-{}",
            super::super::random_session().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        root
    }
    #[test]
    fn stable_events_are_durable_cached_while_heartbeats_and_domains_advance() {
        let root = directory();
        let events = root.join("quality/session/events.json");
        let mut writer = DurableWriter::default();
        writer.write(Vec::new()).unwrap();
        assert!(EventKey::from_batch(&batch(0)).unwrap().is_none());
        let mut missing = quality_batch(&root, 1, 1, "a");
        missing.remove(2);
        assert!(EventKey::from_batch(&missing).unwrap().is_none());
        writer.write(quality_batch(&root, 1, 1, "a")).unwrap();
        let initial = fs::metadata(&events).unwrap().ino();
        writer.write(quality_batch(&root, 2, 1, "a")).unwrap();
        assert_eq!(fs::metadata(&events).unwrap().ino(), initial);
        let saved: Value =
            serde_json::from_slice(&fs::read(root.join("status.json")).unwrap()).unwrap();
        assert_eq!(saved["heartbeat_sequence"], 2);
        writer.write(quality_batch(&root, 3, 2, "a")).unwrap();
        let changed = fs::metadata(&events).unwrap().ino();
        assert_ne!(changed, initial);
        writer.write(quality_batch(&root, 4, 2, "b")).unwrap();
        assert_ne!(fs::metadata(&events).unwrap().ino(), changed);
        let mut instance = quality_batch(&root, 5, 2, "b");
        instance.last_mut().unwrap().1["instance_id"] = json!("another-instance");
        let previous = fs::metadata(&events).unwrap().ino();
        writer.write(instance).unwrap();
        assert_ne!(fs::metadata(&events).unwrap().ino(), previous);
        assert_eq!(
            writer.events.as_ref().unwrap().key.instance,
            "another-instance"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn replaced_removed_modified_or_untrusted_cached_files_are_rewritten() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = directory();
        let events = root.join("quality/session/events.json");
        let mut writer = DurableWriter::default();
        writer.write(quality_batch(&root, 1, 1, "a")).unwrap();
        for fault in [
            "modify",
            "replace",
            "remove",
            "mode",
            "link",
            "symlink",
            "directory",
        ] {
            let previous = fs::metadata(&events).unwrap().ino();
            match fault {
                "modify" => fs::write(&events, b"corrupt").unwrap(),
                "replace" => {
                    fs::write(root.join("replacement"), b"corrupt").unwrap();
                    fs::rename(root.join("replacement"), &events).unwrap();
                }
                "remove" => fs::remove_file(&events).unwrap(),
                "mode" => fs::set_permissions(&events, fs::Permissions::from_mode(0o644)).unwrap(),
                "link" => fs::hard_link(&events, root.join("linked")).unwrap(),
                "symlink" => {
                    fs::remove_file(&events).unwrap();
                    symlink(root.join("outside"), &events).unwrap();
                }
                _ => {
                    fs::remove_file(&events).unwrap();
                    fs::create_dir(&events).unwrap();
                    assert!(writer.write(quality_batch(&root, 2, 1, "a")).is_err());
                    assert_eq!(writer.events.as_ref().unwrap().file.ino, previous);
                    fs::remove_dir(&events).unwrap();
                }
            }
            writer.write(quality_batch(&root, 3, 1, "a")).unwrap();
            let value: Value = serde_json::from_slice(&fs::read(&events).unwrap()).unwrap();
            assert_eq!(value["sequence"], 1, "{fault}");
            assert!(fs::symlink_metadata(&events).unwrap().is_file());
            assert_eq!(fs::metadata(&events).unwrap().mode() & 0o777, 0o600);
        }
        let parent = root.join("quality/session");
        assert!(EventCache::identity(&parent.join("x".repeat(4096))).is_err());
        assert!(super::super::prepare_status_parent(&parent.join("x".repeat(4096))).is_err());
        fs::rename(&parent, root.join("old-parent")).unwrap();
        std::os::unix::fs::symlink(root.join("old-parent"), &parent).unwrap();
        assert!(writer.write(quality_batch(&root, 4, 1, "a")).is_err());
        assert!(EventCache::identity(Path::new("/")).is_err());
        assert!(super::super::atomic_status_file(Path::new("/"), &json!(1)).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn coalesced_events_do_not_populate_the_durable_cache() {
        let root = directory();
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let mut disk = DurableWriter::default();
        let mut persistence = Persistence::with_writer(move |batch| {
            if batch.last().unwrap().1["heartbeat_sequence"] == 1 {
                entered_tx.send(()).unwrap();
                release.recv().unwrap();
            }
            disk.write(batch)
        })
        .unwrap();
        persistence
            .submit(quality_batch(&root, 1, 1, "a"), false)
            .unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        persistence
            .submit(quality_batch(&root, 2, 2, "a"), false)
            .unwrap();
        let ticket = persistence
            .enqueue(quality_batch(&root, 3, 1, "a"), true)
            .unwrap();
        release_tx.send(()).unwrap();
        persistence.wait(ticket).unwrap();
        assert_eq!(persistence.report()["coalesced_snapshots"], 1);
        let saved: Value =
            serde_json::from_slice(&fs::read(root.join("quality/session/events.json")).unwrap())
                .unwrap();
        assert_eq!(saved["sequence"], 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn split_critical_fence_pumps_without_the_lock_or_early_ack() {
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let mut writer = Persistence::with_writer(move |_| {
            entered_tx.send(()).unwrap();
            release.recv().unwrap();
            Ok(())
        })
        .unwrap();
        let ticket = writer.enqueue(batch(1), true).unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(writer.report()["protected_sequence"], 1);
        assert!(writer.enqueue(batch(2), false).is_err());
        assert_eq!(writer.report()["submitted_sequence"], 1);
        let shared = writer.shared.clone();
        let (pumped_tx, pumped) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let child = thread::spawn(move || {
            let mut calls = 0;
            let result = writer.wait_with_pump(ticket, || {
                assert!(
                    shared.0.try_lock().is_ok(),
                    "pump must not hold the persistence lock"
                );
                calls += 1;
                if calls == 2 {
                    pumped_tx.send(()).unwrap();
                }
                Ok(())
            });
            done_tx.send((result, writer.report())).unwrap();
        });
        pumped.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            done.try_recv().is_err(),
            "critical fence acknowledged before fsync release"
        );
        release_tx.send(()).unwrap();
        let (result, report) = done.recv_timeout(Duration::from_secs(2)).unwrap();
        result.unwrap();
        child.join().unwrap();
        assert_eq!(report["completed_sequence"], 1);
        assert!(report["protected_sequence"].is_null());
        assert!(report["critical_events"][0]["pump_calls"].as_u64().unwrap() >= 2);
        assert_eq!(report["critical_events"][0]["pump_failed"], false);
    }
    #[test]
    fn queued_critical_snapshot_cannot_be_replaced_while_an_older_batch_is_inflight() {
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let mut writer = Persistence::with_writer(move |batch| {
            let n = batch[0].1.as_u64().unwrap();
            entered_tx.send(n).unwrap();
            if n == 1 {
                release.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
        writer.submit(batch(1), false).unwrap();
        assert_eq!(entered.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        let ticket = writer.enqueue(batch(2), true).unwrap();
        assert!(writer.enqueue(batch(3), false).is_err());
        assert!(writer.enqueue(batch(4), true).is_err());
        assert_eq!(writer.report()["coalesced_snapshots"], 0);
        release_tx.send(()).unwrap();
        writer.wait(ticket).unwrap();
        assert_eq!(entered.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        assert_eq!(writer.report()["completed_sequence"], 2);
    }
    #[test]
    fn pump_error_or_panic_is_not_swallowed_and_still_waits_for_its_critical_batch() {
        for panic in [false, true] {
            let (entered_tx, entered) = mpsc::channel();
            let (release_tx, release) = mpsc::channel();
            let mut writer = Persistence::with_writer(move |_| {
                entered_tx.send(()).unwrap();
                release.recv().unwrap();
                Ok(())
            })
            .unwrap();
            let ticket = writer.enqueue(batch(1), true).unwrap();
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            let (pumped_tx, pumped) = mpsc::channel();
            let (done_tx, done) = mpsc::channel();
            let child = thread::spawn(move || {
                let result = writer.wait_with_pump(ticket, || {
                    pumped_tx.send(()).unwrap();
                    assert!(!panic, "injected IMU pump panic");
                    Err(anyhow!("injected invalid IMU"))
                });
                done_tx.send((result, writer.report())).unwrap();
            });
            pumped.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(done.recv_timeout(Duration::from_millis(20)).is_err());
            release_tx.send(()).unwrap();
            let (result, report) = done.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(result.unwrap_err().to_string().contains(if panic {
                "pump panicked"
            } else {
                "invalid IMU"
            }));
            child.join().unwrap();
            assert_eq!(report["critical_events"][0]["ok"], true);
            assert_eq!(report["critical_events"][0]["pump_failed"], true);
            assert_eq!(report["critical_events"][0]["pump_calls"], 1);
        }
    }
    #[test]
    fn persistence_failures_wake_pumping_fences_and_completed_tickets_need_no_pump() {
        for panic in [false, true] {
            let (entered_tx, entered) = mpsc::channel();
            let (release_tx, release) = mpsc::channel();
            let mut writer = Persistence::with_writer(move |_| {
                entered_tx.send(()).unwrap();
                release.recv().unwrap();
                assert!(!panic, "injected persistence panic");
                Err(anyhow!("injected fsync error"))
            })
            .unwrap();
            let ticket = writer.enqueue(batch(1), true).unwrap();
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            let result = writer.wait_with_pump(ticket, || {
                release_tx.send(()).ok();
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(writer.report()["critical_events"][0]["ok"], false);
            assert!(
                writer.report()["critical_events"][0]["pump_calls"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
        let mut writer = Persistence::with_writer(|_| Ok(())).unwrap();
        let ticket = writer.enqueue(batch(1), false).unwrap();
        writer
            .wait_with_pump(ticket, || panic!("ordinary snapshots must not pump"))
            .unwrap();
        let ticket = writer.enqueue(batch(2), true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.report()["completed_sequence"] != 2 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        writer
            .wait_with_pump(ticket, || panic!("completed tickets must not pump"))
            .unwrap();
    }
    #[test]
    fn critical_write_completes_while_report_formatting_is_paused() {
        let (tx, rx) = mpsc::channel();
        let mut writer = Persistence::with_writer(move |batch| {
            tx.send(batch[0].1.clone()).unwrap();
            Ok(())
        })
        .unwrap();
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        writer.shared.0.lock().unwrap().render_gate = Some(gate.clone());
        let shared = writer.shared.clone();
        let reporting = std::thread::spawn(move || Persistence::snapshot(&shared).value());
        gate.0.wait();
        let unlocked = writer.shared.0.try_lock().is_ok();
        if unlocked {
            writer.submit(batch(1), true).unwrap();
            assert_eq!(rx.recv().unwrap(), json!(1));
        }
        gate.1.wait();
        let old = reporting.join().unwrap();
        writer.shared.0.lock().unwrap().render_gate = None;
        assert!(
            unlocked,
            "report formatting must not hold persistence state"
        );
        assert_eq!(old["submitted_sequence"], 0);
        assert_eq!(old["completed_sequence"], 0);
        let fresh = writer.report();
        assert_eq!(fresh["submitted_sequence"], 1);
        assert_eq!(fresh["completed_sequence"], 1);
        assert_eq!(fresh["wall"]["samples"], 1);
    }
    #[test]
    fn stalled_io_does_not_block_updates_and_critical_state_fences_the_latest_batch() {
        let (entered_tx, entered) = mpsc::channel();
        let (resume_tx, resume) = mpsc::channel();
        let mut writer = Persistence::with_writer(move |batch| {
            let n = batch[0].1.as_u64().unwrap();
            entered_tx.send(n).unwrap();
            if n == 1 {
                resume.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
        writer.submit(batch(1), false).unwrap();
        assert_eq!(entered.recv().unwrap(), 1);
        let active = writer.report();
        assert_eq!(active["active_write"]["sequence"], 1);
        assert!(active["active_write"]["elapsed_ns"].is_number());
        for n in 2..=100 {
            writer.submit(batch(n), false).unwrap();
        }
        assert_eq!(writer.report()["pending_snapshots"], 1);
        assert_eq!(writer.report()["coalesced_snapshots"], 98);
        let (done_tx, done) = mpsc::channel();
        let handle = thread::spawn(move || {
            writer.submit(batch(101), true).unwrap();
            done_tx.send(writer.report()).unwrap();
        });
        assert!(done.recv_timeout(Duration::from_millis(30)).is_err());
        resume_tx.send(()).unwrap();
        // Scheduling may let snapshot 100 enter before critical 101 is queued.
        let mut observed = Vec::new();
        let report = done.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.join().unwrap();
        while let Ok(n) = entered.try_recv() {
            observed.push(n);
        }
        assert_eq!(observed.last(), Some(&101));
        assert!(observed.iter().all(|n| *n == 100 || *n == 101));
        assert_eq!(report["completed_sequence"], 101);
        assert_eq!(report["pending_snapshots"], 0);
    }
    #[test]
    fn io_errors_and_writer_panics_are_sticky_and_wake_critical_waiters() {
        for panic in [false, true] {
            let mut writer = Persistence::with_writer(move |_| {
                assert!(!panic, "injected writer panic");
                Err(anyhow!("injected fsync failure"))
            })
            .unwrap();
            assert!(writer.submit(batch(1), true).is_err());
            assert!(writer.check().is_err());
            assert!(writer.submit(batch(2), false).is_err());
            assert_eq!(writer.report()["critical_events"][0]["ok"], false);
            assert_eq!(writer.report()["errors"], 1);
            assert_eq!(writer.report()["completed_sequence"], 1);
        }
    }
    #[test]
    fn critical_event_history_is_bounded_and_correlates_durable_sequences() {
        let mut writer = Persistence::with_writer(|_| Ok(())).unwrap();
        for sequence in 1..=25 {
            writer
                .submit(
                    vec![(
                        PathBuf::from("status"),
                        json!({"state":"ready","raw_recording":{"state":"draining"}}),
                    )],
                    true,
                )
                .unwrap();
            assert_eq!(writer.report()["completed_sequence"], sequence);
        }
        let report = writer.report();
        let events = report["critical_events"].as_array().unwrap();
        assert_eq!(events.len(), 16);
        assert_eq!(events[0]["sequence"], 10);
        assert_eq!(events[15]["completed_sequence"], 25);
        assert_eq!(events[15]["target"]["raw_state"], "draining");
        assert!(events[15]["entered_unix_ms"].is_number());
        assert!(events[15]["returned_unix_ms"].is_number());
        assert_eq!(report["critical_wait"]["samples"], 25);
        assert_eq!(report["queue_wait"]["samples"], 25);
        assert!(report["active_write"].is_null());
    }
    #[test]
    fn dropping_writer_flushes_the_last_pending_snapshot() {
        let (tx, rx) = mpsc::channel();
        let mut writer = Persistence::with_writer(move |batch| {
            tx.send(batch[0].1.clone()).unwrap();
            Ok(())
        })
        .unwrap();
        writer.submit(batch(1), false).unwrap();
        writer.submit(batch(2), false).unwrap();
        drop(writer);
        assert_eq!(rx.into_iter().last(), Some(json!(2)));
    }
}
