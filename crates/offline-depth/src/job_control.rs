//! Cooperative per-frame cancellation and bounded, private JSONL progress.
use crate::{OfflineError, Result, RunReport};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
static SIGNALLED: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupted(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::Relaxed);
}
#[derive(Default)]
struct State {
    progress: Option<File>,
    stage: Option<PathBuf>,
    pair: Option<u64>,
    counters: Value,
    last_count: u64,
}
#[derive(Clone, Default)]
pub struct JobControl {
    cancelled: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    watch_signals: bool,
}
thread_local! {static ACTIVE:RefCell<Option<JobControl>>=const { RefCell::new(None) };}
impl JobControl {
    pub fn new(progress: Option<&Path>) -> Result<Self> {
        let mut result = Self::default();
        if let Some(path) = progress {
            if !path.is_absolute() {
                return Err(OfflineError::Invalid("ABSOLUTE_PROGRESS_REQUIRED"));
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            result.state = Arc::new(Mutex::new(State {
                progress: Some(options.open(path)?),
                ..State::default()
            }));
        }
        Ok(result)
    }
    pub fn install_cli_signals(&mut self) {
        SIGNALLED.store(false, Ordering::Relaxed);
        self.watch_signals = true;
        unsafe {
            libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t);
            libc::signal(
                libc::SIGTERM,
                interrupted as *const () as libc::sighandler_t,
            );
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed)
    }
    pub fn diagnostics(&self) -> Value {
        self.state.lock().unwrap().counters.clone()
    }
    pub fn execute<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.started.swap(true, Ordering::Relaxed) {
            return Err(OfflineError::Invalid("JOB_CONTROL_ALREADY_USED"));
        }
        struct Reset(Option<JobControl>);
        impl Drop for Reset {
            fn drop(&mut self) {
                ACTIVE.with(|a| *a.borrow_mut() = self.0.take())
            }
        }
        let previous = ACTIVE.with(|a| a.replace(Some(self.clone())));
        let _reset = Reset(previous);
        let start = self.emit("running", None);
        let result = start.and_then(|_| checkpoint()).and_then(|_| f());
        match &result {
            Ok(_) => self
                .emit("complete", None)
                .map_err(|_| OfflineError::Invalid("PROGRESS_FINALIZATION_UNCERTAIN"))?,
            Err(e) => {
                let _ = self.emit(
                    if e.code() == "JOB_CANCELLED" {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    Some(e),
                );
            }
        }
        result
    }
    fn emit(&self, status: &str, error: Option<&OfflineError>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let event = json!({"version":1,"state":status,"processed_pairs":state.last_count,
            "last_observed_pair_sequence":state.pair,"code":error.map(|e|e.code()),
            "detail_code":error.map(|e|e.detail_code())});
        if let Some(file) = state.progress.as_mut() {
            serde_json::to_writer(&mut *file, &event)?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        if error.is_some() {
            if let Some(stage) = &state.stage {
                if stage.is_dir() {
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    if let Ok(mut file) = options.open(stage.join("FAILED.json")) {
                        let _ = serde_json::to_writer(&mut file, &event);
                        let _ = file.sync_all();
                    }
                }
            }
        }
        Ok(())
    }
}
pub fn checkpoint() -> Result<()> {
    ACTIVE.with(|a| {
        if a.borrow().as_ref().is_some_and(|c| {
            c.cancelled.load(Ordering::Relaxed)
                || (c.watch_signals && SIGNALLED.load(Ordering::Relaxed))
        }) {
            Err(OfflineError::Invalid("JOB_CANCELLED"))
        } else {
            Ok(())
        }
    })
}
pub(crate) fn track(stage: &Path) {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow().as_ref() {
            c.state.lock().unwrap().stage = Some(stage.to_path_buf())
        }
    })
}
pub(crate) fn observe(pair: u64) {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow().as_ref() {
            c.state.lock().unwrap().pair = Some(pair)
        }
    })
}
pub(crate) fn advance(report: &RunReport, stats: Option<&Value>) -> Result<()> {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow().as_ref() {
            let mut state = c.state.lock().unwrap();
            state.last_count = report.validated_pairs;
            if let Some(stats) = stats {
                if !state.counters.is_object() {
                    state.counters = json!({});
                }
                for (key, value) in stats.as_object().unwrap() {
                    let n = state.counters[key]
                        .as_u64()
                        .unwrap_or(0)
                        .checked_add(value.as_u64().unwrap_or(0))
                        .ok_or(OfflineError::Invalid("DIAGNOSTIC_COUNTER_OVERFLOW"))?;
                    state.counters[key] = json!(n);
                }
            }
            drop(state);
            if report.validated_pairs % 100 == 0 {
                c.emit("running", None)?;
            }
        }
        Ok::<(), OfflineError>(())
    })?;
    checkpoint()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_boundary_final_progress_write_failure_does_not_report_completion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("progress.jsonl");
        let control = JobControl::new(Some(&path)).unwrap();
        let result = control.execute(|| {
            // A read-only descriptor gives a deterministic real write error after
            // the initial progress record, without filling a filesystem.
            control.state.lock().unwrap().progress = Some(File::open(&path).unwrap());
            Ok(())
        });
        assert_eq!(
            result.unwrap_err().code(),
            "PROGRESS_FINALIZATION_UNCERTAIN"
        );
        let progress = std::fs::read_to_string(&path).unwrap();
        let events = progress
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["state"], "running");
        assert!(ACTIVE.with(|active| active.borrow().is_none()));
    }
}
