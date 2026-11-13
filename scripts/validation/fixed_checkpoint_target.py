#!/usr/bin/env python3
"""Isolated fixed 8/6 MiB dispatch experiment; production files remain untouched."""
import argparse
import json
from pathlib import Path
import first_checkpoint_phase as phase


def patch_writer(source):
    # Reuse identical bounded event instrumentation, remove the first-batch policy.
    result=phase.patch_writer(source)
    result=phase.replace_once(result,'    experiment_first_pairs: u64,','    experiment_target_mib: u64,')
    result=phase.replace_once(result,'''        state.experiment_first_pairs = std::env::var("GDP_EXPERIMENT_FIRST_PAIRS")
            .unwrap_or_else(|_| "7".into()).parse().expect("first pairs integer");
        assert!([3,5,7].contains(&state.experiment_first_pairs));''','''        state.experiment_target_mib = std::env::var("GDP_EXPERIMENT_TARGET_MIB")
            .expect("explicit experimental target required").parse().expect("MiB integer");
        assert!([6,8].contains(&state.experiment_target_mib));
        EXPERIMENT_TARGET.store(state.experiment_target_mib, std::sync::atomic::Ordering::Relaxed);
        EXPERIMENT_EVENTS.lock().unwrap().reserve_exact(131072);''')
    result=phase.replace_once(result,'''            let first = self.chunked.as_ref().unwrap();
            (if first.chunks <= 1 { first.experiment_first_pairs } else { CHUNK_PAIRS / 2 }, CHUNK_BYTES / 2)''',
        '            (CHUNK_PAIRS / 2, self.chunked.as_ref().unwrap().experiment_target_mib * 1024 * 1024)')
    result=phase.replace_once(result,'            let verified = self.diagnostics.1.measure_credit_wait(',
        '            experiment_record("credit_start", pending.info, Some(credit), Some(reason));\n            let verified = self.diagnostics.1.measure_credit_wait(')
    result=phase.replace_once(result,'            self.accept_checkpoint(verified, pending.checkpoint_bytes);',
        '            experiment_record("credit_end", pending.info, Some(credit), Some(reason));\n            self.accept_checkpoint(verified, pending.checkpoint_bytes);')
    result=phase.replace_once(result,"struct ExperimentEvent { phase: &'static str, checkpoint: u64, pairs: u64, monotonic_ns: u64 }",'''struct ExperimentEvent { phase: &'static str, checkpoint: u64, pairs: u64, bytes: u64, monotonic_ns: u64,
    credit: Option<super::telemetry::CreditAtWait>, reason: Option<WaitReason> }
static EXPERIMENT_TARGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);''')
    result=phase.replace_once(result,"fn experiment_event(phase: &'static str, info: CheckpointInfo) {",'''fn experiment_event(phase: &'static str, info: CheckpointInfo) { experiment_record(phase, info, None, None); }
fn experiment_record(phase: &'static str, info: CheckpointInfo,
    credit: Option<super::telemetry::CreditAtWait>, reason: Option<WaitReason>) {''')
    result=phase.replace_once(result,'        monotonic_ns: ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64 };',
        '        bytes: info.payload_bytes.unwrap_or(0), credit, reason,\n        monotonic_ns: ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64 };')
    result=phase.replace_once(result,'serde_json::json!({"clock":"CLOCK_MONOTONIC", "capacity":131072,',
        'serde_json::json!({"clock":"CLOCK_MONOTONIC", "target_mib":EXPERIMENT_TARGET.load(std::sync::atomic::Ordering::Relaxed), "capacity":131072,')
    return result


def patch_driver(source):
    result=phase.patch_driver(source)
    result=phase.replace_once(result,'struct QueueState {', '''#[derive(serde::Serialize)]
struct ExperimentQueueSample { monotonic_ns: u64, queued_pairs: usize, oldest_age_ns: u64, sequence: u64 }
#[derive(Default)]
struct QueueState {''')
    # Existing Default annotation now belongs to the sample too; it is harmless.
    result=phase.replace_once(result,'    items: VecDeque<Message>,','    items: VecDeque<Message>,\n    experiment_history: Vec<ExperimentQueueSample>,\n    experiment_dropped: u64,')
    result=phase.replace_once(result,'        let mut s = self.state.lock().unwrap();\n        if s.closed {', '''        let mut s = self.state.lock().unwrap();
        let sample = ExperimentQueueSample { monotonic_ns: monotonic_ns(), queued_pairs: s.items.len(),
            oldest_age_ns: s.items.front().map_or(0, |p| p.enqueued.elapsed().as_nanos() as u64),
            sequence: message.pair.pair_sequence };
        if s.experiment_history.len() < 108000 { s.experiment_history.push(sample); }
        else { s.experiment_dropped += 1; }
        if s.closed {''')
    result=phase.replace_once(result,'    let queue = progress.queue.clone();',
        '    let queue = progress.queue.clone();\n    queue.state.lock().unwrap().experiment_history.reserve_exact(108000);')
    result=phase.replace_once(result,'    report["queue_final"] = progress.queue.observation();', '''    report["experiment_queue"] = {
        let s=progress.queue.state.lock().unwrap();
        json!({"clock":"CLOCK_MONOTONIC","capacity":108000,"dropped":s.experiment_dropped,
            "scope":"before each producer push including overflow, not a continuous scheduler trace", "samples":&s.experiment_history})
    };
    report["queue_final"] = progress.queue.observation();''')
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--revision',default='61642b8');p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    manifest=phase.build(Path(__file__).resolve().parents[2],a.output.absolute(),a.revision,
        patches=((phase.TARGET,patch_writer),(phase.DRIVER,patch_driver)),
        modes={'mib8':8,'mib6':6},env_name='GDP_EXPERIMENT_TARGET_MIB')
    manifest.update(experiment='fixed-checkpoint-target-v1',target_mib={'mib8':8,'mib6':6},
                    experiment_tool_sha256=phase.sha(Path(__file__)))
    (a.output/'build-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(json.dumps(manifest,indent=2))


if __name__=='__main__':main()
