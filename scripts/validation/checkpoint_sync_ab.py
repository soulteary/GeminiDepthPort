#!/usr/bin/env python3
"""Isolated release before/after checkpoint experiment; no camera or SDK.

Compile tests/checkpoint_sync_benchmark.rs against each revision with the same
compiler/profile. Both children opt into checkpoint_pipeline. Long-tail injection
is confined to payload fsync; it delays real I/O and does not replace it.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--before', type=Path, required=True)
    p.add_argument('--after', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--repeats', type=int, default=3)
    a = p.parse_args()
    if not 3 <= a.repeats <= 10:
        p.error('repeats must be 3..10')
    binaries = {'before': a.before.resolve(strict=True), 'after': a.after.resolve(strict=True)}
    a.output.mkdir(parents=True, exist_ok=False)
    source = Path(__file__).resolve().parents[2] / 'tests/support/io_faults.c'
    library = (a.output / 'io-faults.so').resolve()
    subprocess.run(['cc', '-shared', '-fPIC', '-std=c11', '-Wall', '-Wextra', '-Werror',
                    str(source), '-ldl', '-o', str(library)], check=True)
    rows = []
    for repeat in range(a.repeats):
        for profile in ('burst', 'paced'):
            for delay in (0, 200):
                order = ('before', 'after') if repeat % 2 == 0 else ('after', 'before')
                for variant in order:
                    root = (a.output / f'{repeat}-{profile}-{delay}-{variant}').resolve()
                    root.mkdir()
                    env = {k: v for k, v in os.environ.items()
                           if not k.startswith('GDP_IO_') and k != 'LD_PRELOAD'}
                    env.update(GDP_VERIFY_ROOT=str(root), GDP_VERIFY_PROFILE=profile,
                               GDP_CHECKPOINT_PIPELINE='1')
                    if delay:
                        env.update(LD_PRELOAD=str(library), GDP_IO_FAIL_OP='fsync',
                                   GDP_IO_FAIL_CONTAINS='/payload-', GDP_IO_REPEAT='1',
                                   GDP_IO_DELAY_MS=str(delay), GDP_IO_FAULT_MARKER=str(root/'injected'))
                    with (root/'child.log').open('w') as log:
                        subprocess.run([str(binaries[variant]), '--exact', 'checkpoint_sync_benchmark_child',
                                        '--ignored', '--nocapture'], env=env, stdout=log,
                                       stderr=subprocess.STDOUT, check=True, timeout=90)
                    if delay and (root/'injected').read_text() != 'fsync':
                        raise ValueError('injection was not observed')
                    row = json.loads((root/'result.json').read_text())
                    d = row['diagnostics']
                    if (row['pairs_read_back'] != row['pairs'] or d['durable_pairs'] != row['pairs']
                            or d['validation_image_bytes'] != 2*row['payload_bytes']
                            or d['failed_calls']):
                        raise ValueError('readback/durability verification failed')
                    if d['uncommitted_pairs_peak'] > 32 or d['uncommitted_payload_bytes_peak'] > 16*1024*1024:
                        raise ValueError('credit limit exceeded')
                    row.update(variant=variant, profile=profile, payload_fsync_delay_ms=delay,
                               repeat=repeat, verification_ns=int(d['verification']['total_ns']))
                    rows.append(row)
                    (a.output/'rows.json').write_text(json.dumps(rows, indent=2)+'\n')
    summary = []
    for profile in ('burst', 'paced'):
        for delay in (0, 200):
            medians = {}
            for variant in binaries:
                group = [r for r in rows if r['profile'] == profile
                         and r['payload_fsync_delay_ms'] == delay and r['variant'] == variant]
                medians[variant] = {k: statistics.median(r[k] for r in group) for k in (
                    'append_p99_ns', 'cpu_seconds', 'cpu_cores', 'peak_rss_kib', 'verification_ns',
                    'total_seconds_including_commit_verification', 'payload_mib_per_second')}
            summary.append(dict(profile=profile, payload_fsync_delay_ms=delay, medians=medians))
    (a.output/'summary.json').write_text(json.dumps(dict(
        repeats=a.repeats, runs=len(rows), summary=summary,
        limits='Synthetic single writer; process CPU and VmHWM before extra materialized readback; '
               'verification includes checkpoint + final real readback, excludes payload fsync; '
               'cached filesystem, no physical NVMe or three-camera performance claim.'), indent=2)+'\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
