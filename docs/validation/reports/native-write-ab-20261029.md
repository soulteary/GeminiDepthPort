# Native V3 write batching A/B — 2026-10-29

Baseline: `905d77077e06e5ef2c8e3e43f8654749b6a7e08a`; after: accompanying commit. Same release driver in `crates/offline-depth/tests/write_benchmark.rs`, Rust 1.90 thin LTO, Linux Docker. Baseline executable copied before rebuilding; package cleaned between trees to prevent stale Cargo reuse. Three alternating-order repeats per profile/delay (24 timed runs).

Compact: 640×400, 128 pairs, 125 MiB image payload. Wide: 1920×1080, 16 pairs, 126.5625 MiB. Native fsync timings use no interposer; delayed timings add 2 ms per fsync. Values below are medians of three isolated children; P99 bounds are medians of histogram bounds, not exact percentiles.

- compact, fsync +0 ms: throughput 667.15 → 753.06 MiB/s (+12.9%); append P99 [7.340, 7.864] → [7.340, 7.565] ms; verification 107.71 → 99.98 ms; whole-child peak RSS 3620 → 3516 KiB.
- compact, fsync +2 ms: throughput 327.69 → 395.38 MiB/s (+20.7%); append P99 [25.166, 27.217] → [18.874, 19.743] ms; verification 170.31 → 154.81 ms; whole-child peak RSS 3648 → 3548 KiB.
- wide, fsync +0 ms: throughput 465.21 → 698.63 MiB/s (+50.2%); append P99 [18.874, 20.696] → [14.680, 15.160] ms; verification 162.76 → 108.01 ms; whole-child peak RSS 7156 → 7048 KiB.
- wide, fsync +2 ms: throughput 478.26 → 465.30 MiB/s (-2.7%); append P99 [20.972, 22.909] → [20.972, 22.898] ms; verification 101.69 → 103.48 ms; whole-child peak RSS 7192 → 7080 KiB.

Separate count-only compact runs use the C interposer without injected delay and are excluded from timing. Payload writes: 384 scalar → 128 vector; index writes: 128 → 8. Total payload/index write calls: **512 → 136 (−73.44%)**. Both retain 46 fsync calls, eight payload chunks/checkpoints, 13 final files and 128 durable pairs.

All runs validate twice through real reads and per-extent SHA256; a post-timer materialized depth comparison also passes. Snapshot/index/checkpoint/manifest/COMPLETE comparison checksums match for every before/after run. These noncryptographic comparison checksums supplement, rather than replace, the recording SHA256 checks.

The stable result is fewer write calls. Throughput improves in three measured conditions but wide +2ms regresses slightly; three short cached-filesystem repetitions do not establish a universal speedup. RSS changes are small and not evidence of a memory improvement: the implementation explicitly adds an index staging allocation capped at 64 KiB and reuses bounded metadata buffers. Peak RSS includes the whole process and is sampled before the extra materialized readback. Verification is unchanged and remains a major cost. No physical-device throughput or camera/ROS CPU claim follows. When raw_recording is disabled and external ROSbag2 records, this path is inactive.

Validation: complete workspace/external coverage suite passes; production Rust lines 17432/17594 = 99.0792% (baseline 17337/17499 = 99.0742%). Rust 1.85 all-target Clippy passes. Actual short writev, EINTR, zero-write, partial payload/index failure, SIGKILL and previous-prefix recovery tests pass. Coverage scope/exclusions and the 99% gate are unchanged.

Reproduce: build this same test-only driver against baseline/current sources, copy the baseline executable, then run the after binary with `GDP_VERIFY_BEFORE`, `GDP_VERIFY_AFTER`, `GDP_VERIFY_OUTPUT` and `--exact write_ab_benchmark --ignored --nocapture`. `tests/support/io_faults.c` optionally emits independent write counters via `GDP_IO_WRITE_COUNTS`; do not enable it in native-fsync timing runs. Raw results: [JSON](native-write-ab-20261029.json).
