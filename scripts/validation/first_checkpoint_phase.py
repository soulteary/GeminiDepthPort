#!/usr/bin/env python3
"""Build a synthetic-only first-checkpoint phase experiment from a pinned archive."""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

TARGET = Path('crates/offline-depth/src/recording/chunked.rs')
DRIVER = Path('crates/offline-depth/tests/writer_contention_benchmark.rs')


def replace_once(source, old, new):
    if source.count(old) != 1:
        raise ValueError('PHASE_SOURCE_CONTRACT_CHANGED:' + old[:60])
    return source.replace(old, new, 1)


def patch_writer(source):
    source = replace_once(source, '    full_credit_batches: bool,',
                          '    full_credit_batches: bool,\n    experiment_first_pairs: u64,')
    source = replace_once(source, '        state.pipeline = true;', '''        state.experiment_first_pairs = std::env::var("GDP_EXPERIMENT_FIRST_PAIRS")
            .unwrap_or_else(|_| "7".into()).parse().expect("first pairs integer");
        assert!([3,5,7].contains(&state.experiment_first_pairs));
        state.pipeline = true;''')
    source = replace_once(source, '            (CHUNK_PAIRS / 2, CHUNK_BYTES / 2)', '''            let first = self.chunked.as_ref().unwrap();
            (if first.chunks <= 1 { first.experiment_first_pairs } else { CHUNK_PAIRS / 2 }, CHUNK_BYTES / 2)''')
    source = replace_once(source, '        let diagnostics = self.diagnostics.clone();',
                          '        experiment_event("dispatch", info);\n        let diagnostics = self.diagnostics.clone();')
    source = replace_once(source, '        let work = move || -> Result<VerifiedPrefix> {',
                          '        let work = move || -> Result<VerifiedPrefix> {\n            experiment_event("payload_start", info);')
    source = replace_once(source, '            work_diagnostics.1.measure(info, Phase::IndexSync, || {',
                          '            experiment_event("index_start", info);\n            work_diagnostics.1.measure(info, Phase::IndexSync, || {')
    source = replace_once(source, '            work_diagnostics.1.measure(info, Phase::DirectorySync, || {',
                          '            experiment_event("index_end", info);\n            work_diagnostics.1.measure(info, Phase::DirectorySync, || {')
    source = replace_once(source, '            Ok(verified)\n        };',
                          '            experiment_event("commit_end", info);\n            Ok(verified)\n        };')
    return source + '''
// Experimental process-local bounded observations, serialized only after finish.
#[derive(Clone, serde::Serialize)]
struct ExperimentEvent { phase: &'static str, checkpoint: u64, pairs: u64, monotonic_ns: u64 }
static EXPERIMENT_EVENTS: std::sync::Mutex<Vec<ExperimentEvent>> = std::sync::Mutex::new(Vec::new());
static EXPERIMENT_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn experiment_event(phase: &'static str, info: CheckpointInfo) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) }, 0);
    let row = ExperimentEvent { phase, checkpoint: info.checkpoint_id, pairs: info.prefix_pairs,
        monotonic_ns: ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64 };
    let mut rows = EXPERIMENT_EVENTS.lock().unwrap();
    if rows.len() < 131072 { rows.push(row); }
    else { EXPERIMENT_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
}
impl RawRecordingWriter {
    pub fn experiment_phase_report() -> serde_json::Value {
        serde_json::json!({"clock":"CLOCK_MONOTONIC", "capacity":131072,
            "dropped":EXPERIMENT_DROPPED.load(std::sync::atomic::Ordering::Relaxed),
            "events": &*EXPERIMENT_EVENTS.lock().unwrap()})
    }
}
'''


def patch_driver(source):
    return replace_once(source, '    report["queue_final"] = progress.queue.observation();',
                        '    report["experiment_phase"] = RawRecordingWriter::experiment_phase_report();\n'
                        '    report["queue_final"] = progress.queue.observation();')


def build(repo, output, revision, patches=None, modes=None, env_name=None):
    if sys.platform != 'linux':
        raise ValueError('LINUX_REQUIRED')
    commit = subprocess.check_output(['git', 'rev-parse', '--verify', revision + '^{commit}'], cwd=repo, text=True).strip()
    output.mkdir(parents=True, exist_ok=False)
    archive = output / 'source.tar'
    subprocess.run(['git', 'archive', '--format=tar', '-o', str(archive), commit], cwd=repo, check=True)
    root = output / 'source'; root.mkdir()
    subprocess.run(['tar', '-xf', str(archive), '-C', str(root)], check=True)
    for path, patch in (patches or ((TARGET, patch_writer), (DRIVER, patch_driver))):
        original = (root / path).read_text()
        changed = patch(original)
        (root / path).write_text(changed)
        import difflib
        (output / (path.name + '.patch')).write_text(''.join(difflib.unified_diff(
            original.splitlines(True), changed.splitlines(True), fromfile=str(path), tofile=str(path))))
    with (output / 'build.stderr').open('w') as log:
        result = subprocess.run(['cargo', 'test', '--locked', '--release', '-p', 'offline-depth',
            '--test', 'writer_contention_benchmark', '--no-run', '--message-format=json'], cwd=root,
            text=True, stdout=subprocess.PIPE, stderr=log, check=True)
    (output / 'build.jsonl').write_text(result.stdout)
    artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
    binaries = [a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'
                and a.get('target', {}).get('name') == 'writer_contention_benchmark' and a.get('executable')]
    if len(binaries) != 1: raise ValueError('AMBIGUOUS_BINARY')
    payload = output / 'phase.elf'; shutil.copy2(binaries[0], payload)
    variants = {}
    for mode in (modes or ('aligned', 'staggered', 'first3', 'first5', 'first7')):
        binary = output / (mode + '.bin')
        choose = ('case "${GDP_CONTENTION_ROOT##*/}" in writer-0) first=3;; writer-1) first=5;; writer-2) first=7;; *) first=7;; esac\n'
                  if mode == 'staggered' else 'first=' + (mode[5:] if mode.startswith('first') else '7') + '\n')
        binary.write_text('#!/bin/sh\n' + choose + 'exec env GDP_CONTENTION_BATCH_POLICY=half-credit '
                          'GDP_EXPERIMENT_FIRST_PAIRS="$first" ' + shlex.quote(str(payload)) + ' "$@"\n')
        if modes is not None:
            binary.write_text('#!/bin/sh\nexec env GDP_CONTENTION_BATCH_POLICY=half-credit '
                              + env_name + '=' + str(modes[mode]) + ' '
                              + shlex.quote(str(payload)) + ' "$@"\n')
        binary.chmod(0o700)
        variants[mode] = dict(binary=str(binary), binary_sha256=sha(binary), payload=str(payload), payload_sha256=sha(payload))
    manifest = dict(commit=commit, variants=variants, patched_files=[str(TARGET), str(DRIVER)],
                    common_driver_sha256=sha(root / DRIVER), rustc=subprocess.check_output(['rustc','-Vv'],text=True),
                    production_modified=False, collector='identical bounded in-memory collector in both variants; no sampling sleeps',
                    tool_sha256=sha(Path(__file__)))
    (output / 'build-manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    return manifest


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--revision',default='61642b8'); p.add_argument('--output',type=Path,required=True)
    a=p.parse_args(); print(json.dumps(build(Path(__file__).resolve().parents[2],a.output.absolute(),a.revision),indent=2))


if __name__ == '__main__': main()
