# Bounded checkpoint synchronization comparison — 2026-11-01

Baseline: `f65c0ee40e6a2e457ebfa1f7365557a3d7ca01ff`. Candidate: this working-tree change. Only the checkpoint implementation changes between the two release drivers. Both enable the existing pipeline flag. Production SDK and the recording format are unchanged.

## Change and guarantees

The append thread still flushes the bounded index buffer and seals a whole-pair payload. One task owns the sealed payload, index descriptor and immutable checkpoint prefix, then performs payload/index/directory synchronization, actual checkpoint readback and SHA256 validation, checkpoint temporary write/sync/rename and final directory sync. Only then does it acknowledge the reliable prefix. Collection transfers verified progress/accounting without further checkpoint disk I/O.

At most one pending task and one active chunk exist. The unchanged global allowance is 32 pairs / 16 MiB, except one indivisible oversized pair; a full allowance blocks the writer. No pixels are copied into the task. Explicit checkpoint, finish and Drop join pending work; finish retains its second actual whole-recording read/hash pass and guarded COMPLETE publication. A valid submitted prefix may finish during an abort, but an aborted job cannot become complete.

`checkpoint_pipeline` remains opt-in, default false. For the next physical experiment set `raw_recording.storage_format` to `chunked-v3` and `raw_recording.checkpoint_pipeline` to true, retaining the remaining capture settings; keep these values fixed in both SDK on/off groups. This change does not accelerate an explicitly false configuration. `verify_dispatch`/`verify_wait` telemetry names remain compatible and now cover the entire submitted checkpoint task, not only readback.

## Method and results

Linux ARM64 Docker VM, Rust 1.90.0, release + thin LTO, same cached filesystem, same 640×400 two-image fixture, three alternating before/after repeats: **24 isolated children**. Burst profile writes 128 pairs without pacing; paced profile writes 64 pairs on a 33,333 µs schedule. Payload-fsync injection is either zero or 200 ms on every payload segment; real synchronization and reads still execute. Both variants keep identical pipeline chunk/credit limits. This is not the physical NVMe or three-camera workload.

The following are medians across three children, not pooled percentiles. Append P99 is computed from individual measured append durations (64/128 samples; for 64 it is the maximum), not a histogram midpoint. CPU seconds include all process threads during writing and finish; CPU cores divide that time by elapsed wall. VmHWM is process peak before the extra materialized consumer check, not cgroup/cache or anonymous RSS. Verification wall time sums checkpoint and final readback spans, excludes fsync, and is not CPU time.

- Burst, no injected delay: append P99 **10.70 → 7.29 ms**; CPU **0.315 → 0.316 s** (**0.699 → 0.684 cores**); VmHWM **4164 → 4164 KiB**; verification **447.30 → 360.58 ms**; throughput **206.52 → 264.36 MiB/s**.
- Burst, payload fsync +200 ms: append P99 **214.80 → 216.78 ms**; CPU **0.511 → 0.361 s** (**0.128 → 0.099 cores**); VmHWM **4188 → 4192 KiB**; verification **615.95 → 326.37 ms**; throughput **31.34 → 34.37 MiB/s**.
- Paced, no injected delay: append P99 **36.15 → 11.67 ms**; CPU **0.347 → 0.436 s** (**0.149 → 0.175 cores**); VmHWM **4156 → 4168 KiB**; verification **464.38 → 504.46 ms**; total write/finish wall **2.258 → 2.489 s**.
- Paced, payload fsync +200 ms: append P99 **225.39 → 57.35 ms** (**−74.6%**); CPU **0.296 → 0.357 s** (**0.116 → 0.145 cores**); VmHWM **4188 → 4192 KiB**; verification **345.14 → 407.74 ms**; total write/finish wall **2.541 → 2.473 s**.

The evidence supports overlap and lower paced append tails, not universal throughput/CPU improvement. Burst writers can immediately consume credits and still wait on the same durability barrier; their injected-delay P99 does not improve. CPU and validation timings are variable on this shared VM; the paced medians show additional CPU and verification cost. Three repeats and short synthetic windows do not establish statistical significance or SDK diagnostic overhead. Memory peak changed by only 0–12 KiB in these medians; this does not predict multi-camera cgroup memory.

All 24 children completed their target, acknowledged every pair, and recorded exactly twice the image payload in successful real validation passes. Each also performed an additional post-timer materialized readback and depth-byte/count comparison. No credit peak exceeded 32 pairs / 16 MiB. Per-run diagnostics, binary SHA256 and summary are in [raw results](checkpoint-sync-ab-20261101.json). `artifact_hashes` are supplementary DefaultHasher fixture comparisons, not SHA256 evidence; actual per-frame SHA256 checks are performed by the recording reader.

## Regressions and limits

The gated checkpoint test now blocks payload sync, index sync, directory sync, checkpoint write and rename, in addition to readback. It proves append overlap, recovery of the old reliable prefix during the block, background commit without another frontend append, error propagation, corruption refusal and actual SIGKILL prefix recovery. Pending-task failures also cover thread creation, payload/index sync, checkpoint write/sync/rename/directory sync. Previous byte-budget, oversized-pair credits, IMU monotonicity and double-readback tests remain.

The service integration test injects a single real payload fsync delay at a 33 ms SDK cadence with a queue of 16: synchronous checkpoints fail with RAW_QUEUE_OVERFLOW at 800 ms; the candidate pipeline completes 64 pairs with no overflow at 800 ms; at 1800 ms it still fails explicitly and creates no COMPLETE. These use mock SDK/ROS implementations with real recording I/O, not cameras. Acquisition queue overflow/SDK abort tests exercise both legacy and pipeline recording modes.

No physical power-loss test, six-run hardware acceptance, or claim to fix every storage stall is made. A sustained storage deficit still needs separate buffering/storage investigation; the global credit and queue limits deliberately remain finite.

Final validation: workspace Clippy with `--all-targets -- -D warnings` passed; **633 Rust tests passed, 30 ignored** across 70 suites. **397 Python tests ran, one conditional skip, no failures**. The additional Linux telemetry assertion confirms that asynchronous payload/index/directory sync, readback and checkpoint publication use a different TID from frontend preparation.

## Reproduction

Use native Linux with Rust 1.90, Python 3 and a C compiler. Compile the identical new `crates/offline-depth/tests/checkpoint_sync_benchmark.rs` in a baseline checkout at `f65c0ee` and the candidate checkout (`cargo test --release -p offline-depth --test checkpoint_sync_benchmark --no-run`). Copy this driver into the baseline checkout first, leaving its runtime sources unchanged. Preserve each resulting executable from its target/release/deps directory under a separate name; use separate target directories or copy the baseline binary before rebuilding. In the candidate repository run:

```bash
python3 scripts/validation/checkpoint_sync_ab.py \
  --before /absolute/path/before-benchmark \
  --after /absolute/path/after-benchmark \
  --output /absolute/path/fresh-results --repeats 3
```

The output must be fresh. The runner compiles the existing child-scoped fault shim, alternates order by repeat, verifies injection markers, checks target/durability/two-pass counters and credit limits, and retains per-child logs/results and actual recording files on failure. It does not access cameras, Docker sockets or production configurations. Allow several GiB for retained fixture recordings. Do not run compilation or other intentional disk stress concurrently with measurement.
