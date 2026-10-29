# Streaming recording validation: before/after release experiment

Date: 2026-10-29 (Asia/Shanghai). Baseline: `72e86742300ee51fb723e1398214f653b1967679`.
[Raw runs, source/binary fingerprints and summaries](streaming-verification-ab-20261029.json).

## Implementation and unchanged guarantees

Checkpoint validation and final full-job validation now call the same record
validator in validation-only mode. It reads and SHA-256-hashes both image streams
through a reusable 64 KiB scratch buffer, without returning a RecordedFrame or
allocating image-sized vectors. Index-line storage is also reused. Evidence and
other metadata still undergo their existing bounded parsing and checks; this is
not a zero-allocation or no-parsing claim.

The original next_frame API still returns depth/evidence for consumers and
exports. The two modes share layout, encoding, original metadata, progression,
range, hash, binding and EOF validation, and may be interleaved on one reader.
V2/observed associations remain supported. Format bytes, chunk limits, whole-pair
queues, byte budgets, SHA-256, fsync order, checkpoint publication, cancellation
and COMPLETE rules are unchanged. No durability barrier/readback pass was removed.

Successful V3 jobs report validation_image_bytes exactly twice the image payload:
new-chunk checkpoint readback plus the final entire-recording readback. The
validation_buffer_peak_bytes diagnostic covers image scratch capacity only,
not total process memory or metadata. V2's final verification also uses the new
mode; its existing one-pass semantics are unchanged. Prefix recovery uses the
streaming mode and still reports complete=false. Validation-only EOF does not by
itself establish COMPLETE, physical image validity or D2C/numerical conversion;
those existing boundaries remain separate.

## Measurement

A single shared integration-test driver was compiled against the pristine
baseline production sources and the changed sources, with Rust 1.90 release +
thin LTO in Linux arm64 Docker. The baseline executable was copied out before
rebuilding the modified package. Binary hashes differ; the driver enforces absent
baseline streaming diagnostics versus exactly two-pass bytes and <=64 KiB image
scratch after optimization. Use separate target directories when reproducing to
avoid Cargo fingerprint reuse between different sources mounted at the same path.

The final experiment ran after task-owned heavy checks completed. The host was
shared and not CPU-pinned. There were 24 valid runs: two sizes, two sync conditions,
two versions and three repetitions, with version order alternated. Conditions:

- Compact: 128 pairs, 640x400, 512,000 bytes per image, 125 MiB payload.
- Wide: 16 pairs, 1920x1080, 4,147,200 bytes per image, 126.5625 MiB payload.
- Native fsync without LD_PRELOAD, or an asserted per-child 2ms delay on each fsync.

Both versions write V3 with the same original metadata and producer pixel pattern.
Throughput includes initialization, appends, both readback rounds and the complete
commit. Append P99 comes from fixed histogram intervals. Verification time is the
cumulative checkpoint/final validation stage; stages are nested and must not be
summed as independent totals. Another post-timer pass compares all depth buffers
byte-for-byte; both image streams/evidence are hash-checked inside writer finish.

Peak memory is the isolated Linux child's whole-process VmHWM in KiB, sampled
immediately after writer finish and before that extra materialized comparison.
It includes the harness, metadata and one reusable producer buffer; it is not
anonymous-only RSS, allocator live bytes, page cache or three-camera memory.

## Results

Medians of three runs follow; P99 lower/upper bounds are medians of the respective
histogram interval endpoints, not exact percentiles or confidence intervals.

- 640x400, fsync delay 0ms: throughput 887.7 → 878.7 MiB/s; append P99 6.29–6.68 → 6.29–6.68ms; validation 93.96 → 93.67ms; peak RSS 4.46 → 3.54 MiB.
- 640x400, fsync delay 2ms: throughput 482.0 → 478.3 MiB/s; append P99 18.87–19.31 → 18.87–19.56ms; validation 101.18 → 103.20ms; peak RSS 4.48 → 3.55 MiB.
- 1920x1080, fsync delay 0ms: throughput 783.1 → 918.1 MiB/s; append P99 12.58–12.95 → 8.39–8.91ms; validation 107.35 → 89.63ms; peak RSS 14.62 → 6.98 MiB.
- 1920x1080, fsync delay 2ms: throughput 467.0 → 493.0 MiB/s; append P99 23.07–24.69 → 20.97–22.02ms; validation 112.67 → 99.21ms; peak RSS 14.66 → 7.02 MiB.

All before/after runs kept 13 files, 46 fsync calls, eight checkpoints, every target
pair and both readback passes. The wide, native-fsync case improved throughput
about 17%, reduced verification time about 16.5%, and reduced process peak RSS
about 52%. Compact-case memory fell about 21%; its throughput/latency did not
show a stable improvement. Three runs on a cached shared filesystem do not prove
an improvement on every device/profile. The clear benefit is bounded validation
memory, with stronger throughput/latency benefit for the larger images here.

## Hardware scope

The supplied a7772d4/b03173f physical trials had no raw_recording configuration;
recording was external ROSbag2 + SQLite3. They do not execute this native validation
path. These results therefore neither explain nor overturn the supplied capture
CPU comparison. VmHWM here must not be compared directly with those anonymous-RSS
aggregates. There is no SDK/USB/ROS throughput, physical power-cut or capture-CPU
improvement claim. V2 remains the default; V3 is still explicitly selected.

## Reproduce

In a pristine baseline checkout, add only the identical
`crates/offline-depth/tests/verification_benchmark.rs` from the changed checkout;
keep production sources at the baseline revision above. Build with a separate
CARGO_TARGET_DIR and copy the executable printed by Cargo:

```sh
CARGO_TARGET_DIR=/tmp/gdp-verify-before-target \
cargo +1.90.0 test --release --locked -p offline-depth \
  --test verification_benchmark --no-run
```

Build the modified checkout in another target directory, then run its driver
with the absolute old/new executable paths printed by Cargo:

```sh
CARGO_TARGET_DIR=/tmp/gdp-verify-after-target \
cargo +1.90.0 test --release --locked -p offline-depth \
  --test verification_benchmark --no-run

GDP_VERIFY_BEFORE=/absolute/old/verification_benchmark-executable \
GDP_VERIFY_AFTER=/absolute/new/verification_benchmark-executable \
GDP_VERIFY_OUTPUT=/tmp/verification-ab.json \
CARGO_TARGET_DIR=/tmp/gdp-verify-after-target \
cargo +1.90.0 test --release --locked -p offline-depth \
  --test verification_benchmark verification_ab_benchmark -- --ignored --nocapture
```

Requires Linux /proc and a C compiler for the delay fixture. The performance
experiment is explicitly ignored during ordinary CI; no noisy speed/RSS threshold
is imposed on shared runners.

## Validation

579 Rust tests and 63 external tests passed, as did Rust 1.85 workspace/all-targets
Clippy and formatting checks. Combined production Rust line coverage is
17,337/17,499 = 99.0742% with unchanged toolchain/exclusion rules. New tests compare
both APIs on malformed V2 SDK/observed and V3 inputs, mixed-mode progress, hash
buffer boundaries and exact extent consumption, and payload corruption both before
checkpoint and after a prior checkpoint. Existing actual SIGKILL, write/sync/rename
failure, torn-tail, budget, cancellation, export and incomplete-marker tests remain
in the normal suite. Explicit performance runs are additional to those test totals.
