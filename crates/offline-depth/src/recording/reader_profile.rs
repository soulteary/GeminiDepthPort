//! Opt-in reader CPU diagnosis. Disabled mode does not read clocks or lock stats.
use super::*;
use std::collections::BTreeMap;
#[derive(Default)]
pub(super) struct Profile {
    enabled: bool,
    stats: Mutex<BTreeMap<&'static str, Sample>>,
}
#[derive(Default)]
struct Sample {
    calls: u64,
    errors: u64,
    wall_ns: u128,
    cpu_ns: u128,
    cpu_samples: u64,
}
pub(super) struct Started(Instant, Option<u64>);
impl Profile {
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn enable(&mut self) {
        self.enabled = true;
    }
    pub fn start(&self) -> Option<Started> {
        self.enabled
            .then(|| Started(Instant::now(), super::telemetry::cpu()))
    }
    pub fn finish(&self, stage: &'static str, started: Option<Started>, ok: bool) {
        if let Some(Started(wall, cpu)) = started {
            let elapsed = wall.elapsed().as_nanos();
            let elapsed_cpu = cpu
                .zip(super::telemetry::cpu())
                .and_then(|(a, b)| b.checked_sub(a));
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            let sample = stats.entry(stage).or_default();
            sample.calls += 1;
            sample.errors += u64::from(!ok);
            sample.wall_ns += elapsed;
            if let Some(ns) = elapsed_cpu {
                sample.cpu_ns += u128::from(ns);
                sample.cpu_samples += 1;
            }
        }
    }
    pub fn measure<T, E>(
        &self,
        stage: &'static str,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let start = self.start();
        let result = run();
        self.finish(stage, start, result.is_ok());
        result
    }
    pub fn report(&self) -> serde_json::Value {
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        let stages = stats.iter().map(|(k,s)| (*k, serde_json::json!({"calls":s.calls,"errors":s.errors,"wall_ns":s.wall_ns.to_string(),"thread_cpu_ns":s.cpu_ns.to_string(),"cpu_samples":s.cpu_samples}))).collect::<BTreeMap<_,_>>();
        serde_json::json!({"enabled":self.enabled,"stages":stages,"scope":"reader calls only, excludes open/drop; read_call includes all sub-stages and their instrumentation; image_read_hash includes file access and SHA256; timings are diagnostic and must not be summed with read_call"})
    }
}
