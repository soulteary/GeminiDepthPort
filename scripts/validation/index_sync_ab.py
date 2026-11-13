#!/usr/bin/env python3
"""Build isolated index-sync variants from a pinned commit; never patch the checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import shlex
import sys
import subprocess

TARGET = Path('crates/offline-depth/src/recording/chunked.rs')
OLD = 'work_diagnostics.measure("fsync", || records.sync_all())'
NEW = 'work_diagnostics.measure("fdatasync", || records.sync_data())'


def candidate(source):
    if source.count(OLD) != 1 or NEW in source:
        raise ValueError('INDEX_SYNC_SOURCE_CONTRACT_CHANGED')
    return source.replace(OLD, NEW, 1)


def build(repo, output, revision):
    if sys.platform != 'linux':
        raise ValueError('LINUX_REQUIRED')
    commit = subprocess.check_output(['git', 'rev-parse', '--verify', revision + '^{commit}'], cwd=repo, text=True).strip()
    output.mkdir(parents=True, exist_ok=False)
    archive = output / 'source.tar'
    subprocess.run(['git', 'archive', '--format=tar', '-o', str(archive), commit], cwd=repo, check=True)
    rows = {}
    for mode in ('sync_all', 'sync_data'):
        root = output / mode
        root.mkdir()
        subprocess.run(['tar', '-xf', str(archive), '-C', str(root)], check=True)
        target = root / TARGET
        original = target.read_text()
        if mode == 'sync_data':
            target.write_text(candidate(original))
        command = ['cargo', 'test', '--locked', '--release', '-p', 'offline-depth', '--test',
                   'writer_contention_benchmark', '--no-run', '--message-format=json']
        with (output / (mode + '-build.stderr')).open('w') as log:
            result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=log, check=True)
        (output / (mode + '-build.jsonl')).write_text(result.stdout)
        artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
        binaries = [a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'
                    and a.get('target', {}).get('name') == 'writer_contention_benchmark' and a.get('executable')]
        if len(binaries) != 1:
            raise ValueError('AMBIGUOUS_TEST_BINARY')
        binary = output / (mode + '.bin')
        payload = output / (mode + '.elf')
        shutil.copy2(binaries[0], payload)
        binary.write_text('#!/bin/sh\nexec env GDP_CONTENTION_BATCH_POLICY=half-credit '
                          + shlex.quote(str(payload)) + ' "$@"\n')
        binary.chmod(0o700)
        rows[mode] = dict(payload=str(payload), payload_sha256=hashlib.sha256(payload.read_bytes()).hexdigest(), binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                          source_sha256=hashlib.sha256(target.read_bytes()).hexdigest())
    manifest = dict(commit=commit, variants=rows, batch_policy='half-credit',
                    rustc=subprocess.check_output(['rustc', '-Vv'], text=True),
                    tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), only_candidate_change={str(TARGET): [OLD, NEW]},
                    production_checkout_modified=False, hardware_acceptance=False)
    (output / 'build-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    return manifest


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--revision', default='61642b8')
    p.add_argument('--output', type=Path, required=True)
    a = p.parse_args()
    print(json.dumps(build(Path(__file__).resolve().parents[2], a.output.absolute(), a.revision), indent=2))


if __name__ == '__main__':
    main()
