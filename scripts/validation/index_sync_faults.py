#!/usr/bin/env python3
"""Linux isolated index-sync delay/EIO matrix; real calls, no mocked writer."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import writer_contention_ab as ab


def run(build, output, variants=(('sync_all', 'fsync'), ('sync_data', 'fdatasync')), delay_ms=100):
    if sys.platform != 'linux':
        raise ValueError('LINUX_REQUIRED')
    if not 1 <= delay_ms <= 5000:
        raise ValueError('BOUNDED_DELAY_REQUIRED')
    manifest = json.loads((build / 'build-manifest.json').read_text())
    output.mkdir(parents=True, exist_ok=False)
    library = output / 'faults.so'
    source = Path(__file__).resolve().parents[2] / 'tests/support/io_faults.c'
    subprocess.run(['cc', '-shared', '-fPIC', '-std=c11', '-Wall', '-Wextra', '-Werror',
                    str(source), '-ldl', '-o', str(library)], check=True)
    rows = []
    for variant, operation in variants:
        binary = Path(manifest['variants'][variant]['binary'])
        if (ab.sha256(binary) != manifest['variants'][variant]['binary_sha256']
                or ab.sha256(manifest['variants'][variant]['payload']) != manifest['variants'][variant]['payload_sha256']):
            raise ValueError('BINARY_HASH_MISMATCH')
        for fault in ('delay', 'eio'):
            root = output / (variant + '-' + fault)
            root.mkdir()
            barrier = root / 'start.json'
            ab.write_json(barrier, dict(start_monotonic_ns=time.monotonic_ns()+500_000_000,
                                        start_unix_ns=time.time_ns()+500_000_000))
            env = ab.child_environment(root, 30, 'run', barrier)
            env.update(LD_PRELOAD=str(library), GDP_CONTENTION_BATCH_POLICY='half-credit',
                       GDP_IO_FAIL_OP=operation, GDP_IO_FAIL_SUFFIX='/records.jsonl',
                       GDP_IO_FAIL_NTH='2', GDP_IO_FAULT_MARKER=str(root / 'injected'))
            if fault == 'delay':
                env['GDP_IO_DELAY_MS'] = str(delay_ms)
            with (root / 'child.log').open('w') as log:
                subprocess.run([str(binary), '--exact', ab.TEST_NAME, '--ignored', '--nocapture'],
                               env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=30)
            if (root / 'injected').read_text() != operation:
                raise ValueError('FAULT_NOT_OBSERVED')
            result = json.loads((root / 'result.json').read_text())
            if fault == 'delay':
                ab.validate_result(result, 30)
            else:
                if result['writer_error'] is None or result['ok'] is not False:
                    raise ValueError('SYNC_ERROR_NOT_PROPAGATED')
                if (root / 'capture/COMPLETE').exists() or (root / 'capture.partial/COMPLETE').exists():
                    raise ValueError('FAILED_SYNC_PUBLISHED_COMPLETE')
                partial = root / 'capture.partial'
                before = {str(p.relative_to(partial)): ab.sha256(p) for p in partial.rglob('*') if p.is_file()}
                count = json.loads((partial / 'checkpoint.json').read_text())['record_count']
                clean = ab.child_environment(root, count, 'recover')
                with (root / 'recovery.log').open('w') as log:
                    subprocess.run([str(binary), '--exact', ab.TEST_NAME, '--ignored', '--nocapture'],
                                   env=clean, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=30)
                after = {str(p.relative_to(partial)): ab.sha256(p) for p in partial.rglob('*') if p.is_file()}
                if before != after:
                    raise ValueError('RECOVERY_MODIFIED_FAILED_EVIDENCE')
            rows.append(dict(variant=variant, fault=fault, result=result))
    ab.write_json(output / 'result.json', dict(ok=True, rows=rows,
        scope='second index sync delayed or fails; separate AB runner exercises SIGKILL prefix recovery'))
    return rows


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--build', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    a = p.parse_args()
    run(a.build.resolve(), a.output.absolute())


if __name__ == '__main__':
    main()
