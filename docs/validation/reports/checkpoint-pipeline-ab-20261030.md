# Bounded checkpoint pipeline A/B — 2026-10-30

Baseline: `d9fe06270dcb303e995bf3f795b9d0c6669cacb6` (synchronous V3). After: accompanying commit with `raw_recording.checkpoint_pipeline: true` and `storage_format: "chunked-v3"`. Defaults remain unchanged. Library users enable it before append through `enable_checkpoint_pipeline()`.

The writer syncs each sealed chunk/index/directory, then a single joined worker validates that immutable prefix while the writer appends the next chunk. It reserves a combined 32-pair / 16 MiB payload window; an oversized indivisible pair waits for all pending work and runs alone. Explicit checkpoints and finish wait for verification and the existing durable commit barriers. Final full validation is retained. Smaller 16-pair / 8 MiB chunk targets provide overlap without doubling the crash-loss window.

## Measurement

Linux Docker, Rust 1.90 release with thin LTO. Seven alternating-order repeats per profile/delay: 56 isolated children. Compact: 640×400, 128 pairs, 125 MiB image payload. Wide: 1920×1080, 16 pairs, 126.5625 MiB. Both use identical SDK-count payload/evidence fixtures. Native-fsync timing has no LD_PRELOAD; delayed timing adds 2 ms to every fsync. The full test/build jobs finished before timing.

The same test-only `pipeline_benchmark.rs` driver is built against both source snapshots. The baseline cannot call the new opt-in API, so that disabled branch is replaced with a panic; its environment is 0. Production baseline sources are unchanged. The baseline executable is copied before cleaning/rebuilding the modified package to prevent Cargo cache aliasing at `/work`. The parent checks implementation identity (new credit metrics absent/present), two-round image readback counts and memory/frame budgets.

Values are medians of seven runs. Append P99 intervals are medians of logarithmic histogram bounds, not exact quantiles. Verification totals sum the individual validation stage durations; they overlap appends in pipeline mode and cannot be added to total elapsed time. RSS is whole-child VmHWM sampled immediately after finish and before the extra materialized comparison.

- compact, fsync +0 ms: throughput 841.29 → 1192.21 MiB/s (+41.7%); append P99 [6.816, 6.983] → [0.786, 0.811] ms; verification 96.87 → 98.91 ms; peak RSS 3632 → 4032 KiB.
- compact, fsync +2 ms: throughput 478.68 → 393.59 MiB/s (-17.8%); append P99 [18.874, 19.009] → [11.534, 12.244] ms; verification 105.98 → 122.23 ms; peak RSS 3668 → 4064 KiB.
- wide, fsync +0 ms: throughput 836.93 → 1168.67 MiB/s (+39.6%); append P99 [10.486, 10.690] → [4.194, 4.350] ms; verification 94.72 → 95.96 ms; peak RSS 7168 → 7572 KiB.
- wide, fsync +2 ms: throughput 491.10 → 400.55 MiB/s (-18.4%); append P99 [20.972, 21.744] → [15.729, 16.251] ms; verification 101.28 → 118.46 ms; peak RSS 7200 → 7604 KiB.

For both profiles: eight → sixteen checkpoints/payload files, 13 → 21 final files and **46 → 86 fsync calls**. All pairs are durable at finish. Peak uncommitted payload across all after runs: compact 16 pairs / 16,408,576 bytes; wide 2 pairs / 16,591,862 bytes. The separate blocked-verifier test reaches 31 uncommitted pairs without progressing the acknowledged checkpoint, then proves recovery ignores the tail.

## Interpretation and cost

The experiment supports lower append tail latency on this filesystem and these inputs. Native-fsync throughput also improves, but extra checkpoint barriers reduce throughput by about 18% with 2 ms fsync injection. Peak RSS increases about 400 KiB. This is a latency/throughput tradeoff, so the option is disabled by default. There is no claim of universal acceleration, physical-device bandwidth, lower camera CPU, or improvement to ROSbag2 when native recording is off. For wide profiles only sixteen appends are sampled per child, so each run’s P99 is effectively near its maximum.

The on-disk V3 schema is unchanged and the existing reader/exporter accepts the output. Chunk boundaries, index references and checkpoint hashes change legitimately with the smaller targets; they are not asserted byte-identical to synchronous output. Both image extents and evidence pass the real SHA256 validator twice; the post-timer consumer path also compares original depth bytes and record counts. Snapshot comparison checksums remain in the raw data for provenance, not as replacements for SHA256.

## Correctness and CI

Tests cover round trips across many boundaries, explicit checkpoint and graceful Drop, unchanged start constraints, total byte-budget failure, oversized whole pairs, real concurrent reads/writes under a blocked verifier, read errors, corruption, SIGKILL and pending checkpoint write/fsync/rename failures. The acknowledged prefix advances only after the final directory sync; a rename followed by failed directory sync may leave a newer visible checkpoint, which recovery revalidates without claiming the failed runtime acknowledgement. Failed writers join pending tasks without committing them.

The previous commit’s Ubuntu coverage job exposed independent supervision-test timing races. This change gates global-clock loss until readiness is observed, gives only the discovery-exclusion fixture startup heartbeat headroom, and holds status fsync behind a release file while checking actual publication progress. These changes preserve the assertions and do not alter production timeouts.

Rust 1.85 all-target Clippy passes. Final validation: 589 Rust tests and 63 external tests pass; production Rust line coverage is **17555/17721 = 99.0633%**, compared with baseline **17432/17594 = 99.0792%**. The repaired supervision suite passes 51/51. The coverage scope/exclusions and 99% threshold are unchanged.

## Reproduction

Build the same release test driver against baseline/current trees, copy the baseline executable, then invoke the after test executable with `GDP_VERIFY_BEFORE`, `GDP_VERIFY_AFTER`, `GDP_VERIFY_OUTPUT` and `--exact pipeline_ab_benchmark --ignored --nocapture`. The parent selects pipeline=false/true for the children and alternates order across seven repeats. Raw observations: [JSON](checkpoint-pipeline-ab-20261030.json).
