# Background status persistence: release A/B comparison

Date: 2026-10-29 (Asia/Shanghai). Baseline: `0729384ce9b09a68a22aa3d774577e8f6848bce5`.
Raw observations, exact source fingerprints and aggregation are in
[the JSON report](status-persistence-20261029.json).

## Method and limits

Both programs were built with Rust 1.90, release + thin LTO, in the same Linux
arm64 Docker environment. The same dynamic SDK/ROS ABI fixtures, configuration,
quality collection and 50ms heartbeat were used. The SDK fixture returns 2x2
frames after a 10ms wait. No USB device or real DDS transport was involved.

Each variant ran three times per condition, with before/after order alternated.
After readiness, the test enabled a per-process interposer that delays each fsync
of the service status directory by 0, 25 or 100ms. It leaves image publication,
SDK waits and quality-directory fsync calls unchanged. Each measurement counts
depth image publication calls during a fixed two-second window. Stop/drain occurs
after that window, and the test verifies final stopped status. The delay marker
is checked for every run. All 18 runs completed successfully. The final run followed all task-owned test/build/coverage jobs; the shared host was not CPU-pinned.

This measures resistance to status persistence stalls, not physical camera FPS,
image conversion throughput, ROS subscriber receipt or storage throughput.
The zero-delay condition still uses the same interposer and atomic fsync path.
The fixture's nominal 100 returns/second explains rates above camera 30 FPS.

## Results

The following are medians of three runs. P99 values are logarithmic histogram
interval bounds from each run's SDK between-polls metric, including its short
startup/shutdown period. They are not exact per-window quantiles; median lower
and upper bounds are reported separately.

- 0ms injected fsync delay: publication progress 71.45 → 90.99 images/s (27.3% increase). Between-polls P99 bounds 16.777–18.874ms → 1.049–1.180ms.
- 25ms injected fsync delay: publication progress 49.99 → 92.48 images/s (85.0% increase). Between-polls P99 bounds 46.137–49.026ms → 0.983–1.049ms.
- 100ms injected fsync delay: publication progress 29.99 → 87.97 images/s (193.3% increase). Between-polls P99 bounds 117.441–122.723ms → 1.835–1.966ms.

At 100ms delay, the background pending-slot coalescing count was 18, 18, 17;
the slot remained bounded. The disk operation itself was not accelerated: the
capture loop proceeds while ordinary status snapshots are serialized and synced
elsewhere. Critical transitions and graceful finalization still wait for durable
writes, so those pauses are intentionally not eliminated.

## Reproduction

Build the baseline and modified `capture-service` binaries separately with
`cargo +1.90.0 build --release --locked -p capture-probe --bin capture-service`.
Use the modified checkout's integration test as the shared driver:

```sh
GDP_STATUS_BENCH_BEFORE=/absolute/baseline/capture-service \
GDP_STATUS_BENCH_AFTER=/absolute/modified/capture-service \
GDP_STATUS_BENCH_OUTPUT=/tmp/status-persistence-ab.json \
cargo +1.90.0 test --release --locked -p capture-probe \
  --test service_supervision status_persistence_ab_benchmark -- --ignored --nocapture
```

Run on Linux with C/C++ compilers available. Normal CI leaves this benchmark
ignored to avoid asserting noisy performance thresholds. Deterministic queue,
barrier, error and finalization tests run in the normal test suite. The native
slow-fsync regression separately checks that image publication continues during
a 500ms status-directory stall. Hardware three-camera 600-second validation is
still needed to assess real frame continuity and end-to-end latency.

## Validation

The final production implementation passed 560 Rust tests, 63 external tests,
Rust 1.85 workspace/all-targets Clippy and formatting checks. Combined production
Rust line coverage is 16,821/16,977 = 99.0811%, using the existing toolchain and
source exclusion rules. The benchmark is an additional explicit run, not part of
those normal test totals.
