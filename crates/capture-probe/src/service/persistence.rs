//! One in-flight batch and one replaceable pending snapshot; critical writes
//! wait for their own durable completion. No SDK handle crosses this boundary.
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

pub(super) type Batch = Vec<(PathBuf, Value)>;
#[derive(Default)]
struct State {
    pending: Option<(u64, Batch)>,
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
        json!({"wall":self.wall.report(),"errors":self.errors,
            "submitted_sequence":self.submitted,"completed_sequence":self.completed,
            "pending_snapshots":self.pending,"pending_capacity":1,
            "coalesced_snapshots":self.coalesced,
            "scope":"completed background batches; quality files precede service status; critical updates wait for fsync"})
    }
}
type Shared = Arc<(Mutex<State>, Condvar)>;
pub(super) struct Persistence {
    shared: Shared,
    worker: Option<JoinHandle<()>>,
}
impl Persistence {
    pub(super) fn new() -> Result<Self> {
        Self::with_writer(|batch| {
            for (path, value) in batch {
                super::atomic_status(&path, &value)?;
            }
            Ok(())
        })
    }
    fn with_writer(write: impl Fn(Batch) -> Result<()> + Send + 'static) -> Result<Self> {
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
                    let Some((sequence, batch)) = state.pending.take() else {
                        break;
                    };
                    drop(state);
                    let started = Instant::now();
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(batch)))
                            .unwrap_or_else(|_| Err(anyhow!("status persistence thread panicked")));
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.wall.observe(started.elapsed());
                    state.completed = sequence;
                    if let Err(error) = result {
                        state.error = Some(format!("{error:#}"));
                        state.pending = None;
                        state.closing = true;
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
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.error.is_none() && !state.closing,
            "status persistence unavailable: {}",
            state.error.as_deref().unwrap_or_default()
        );
        state.submitted += 1;
        let sequence = state.submitted;
        if state.pending.replace((sequence, batch)).is_some() {
            state.coalesced += 1;
        }
        changed.notify_all();
        while critical && state.completed < sequence && state.error.is_none() {
            state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        ensure!(
            state.error.is_none(),
            "status persistence failed: {}",
            state.error.as_deref().unwrap_or_default()
        );
        Ok(())
    }
    fn snapshot(shared: &Shared) -> Report {
        let state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
        Report {
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
            assert_eq!(writer.report()["errors"], 1);
            assert_eq!(writer.report()["completed_sequence"], 1);
        }
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
