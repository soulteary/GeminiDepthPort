# Native V3 chunk recording: release comparison

Date: 2026-10-29 (Asia/Shanghai). Baseline source revision: `2721f7c294356a2371bd71c178212380ec352e3b`.
[All observations and source fingerprints](native-recording-v3-ab-20261029.json).
[Format, durability ordering and recovery contract](../../contracts/native-recording-v3.md).

## Method

The experiment compares the retained V2 small-file implementation path and V3
chunked implementation in the same Rust 1.90 release + thin-LTO test executable,
on the same Linux arm64 Docker filesystem. Both receive the same 128 pairs:
640x400 YUYV color and 16-bit SDK-count depth, 512,000 bytes per image, 125 MiB
payload total. Snapshots, original indices/timestamps, depth scale and bound pair
evidence are identical. Optional ROS snapshot association files are omitted for
both; a real service recording adds that common metadata.

There are three repetitions per format/condition, with order alternated, for 12
successful runs. Zero-delay runs use native fsync without the injection library.
The second condition adds 2ms to each actual fsync through a per-child interposer
and checks its injection marker. All task-owned heavy checks finished before the
experiment; the shared host was not CPU-pinned or otherwise hardware-isolated.

Total time includes writer initialization, appends, checkpoints, metadata, final
full readback verification and COMPLETE/directory/parent commit. An additional
post-timer pass compares all 128 depth buffers byte-for-byte; writer verification
also checks color and evidence hashes. No encoding, SDK, USB, ROS/DDS or producer
queue load is measured. Cached-filesystem MiB/s is not physical device throughput.

## Results and tradeoffs

Medians of three runs follow. Append P99 is a fixed-histogram interval, not an
exact percentile; median lower/upper bounds are reported separately.

- Fsync delay 0ms: total 105.37 → 145.46ms; payload throughput 1186.26 → 859.33 MiB/s (0.72×). Append P99 bounds 0.49–0.52 → 6.29–6.82ms.
- Fsync delay 2ms: total 1133.50 → 253.06ms; payload throughput 110.28 → 493.95 MiB/s (4.48×). Append P99 bounds 9.44–10.49 → 16.78–18.49ms.

Every run produced 388 files / 391 fsync calls in V2 versus 13 files / 46 fsync
calls in V3: file count fell 96.65% and fsync calls fell 88.24%. V3 used eight
payload chunks and eight verified checkpoints. No target pair was omitted to
obtain these savings.

This is not a universal speedup: without injected delay, V3 throughput is about
28% lower here because verified checkpoints add a readback pass; V3 append P99
is also higher because group commit periodically performs multiple syncs and
validates a chunk. Under 2ms sync latency, total throughput is about 4.5× higher,
but append P99 still rises. Finite queue capacity and real capture backpressure
must therefore be validated on hardware. Existing V2 remains the default; set
`raw_recording.storage_format` to `chunked-v3` for the sync-bound case, or retain
`files-v2` when fast-cache throughput/per-append latency or old layout semantics
matter more.

V3 guarantees only acknowledged checkpoints across abrupt loss: the latest
unacknowledged chunk and acquisition queue may be lost. Recovery is read-only,
validates the prefix and reports complete=false. Graceful completion still
requires every target pair, hashes, bindings and the full commit transaction.
The test suite uses SIGKILL and I/O faults; no physical power-cut claim is made.

## Reproduce

On Linux with C/C++ compilers, from the modified checkout:

```sh
GDP_CHUNK_BENCH_OUTPUT=/tmp/native-recording-ab.json \
cargo +1.90.0 test --release --locked -p offline-depth \
  --test chunked_recording recording_chunk_ab_benchmark -- --ignored --nocapture
```

This explicit benchmark has no CI performance threshold. Normal tests cover
cross-chunk readback, original evidence, export, bad ranges/hashes/indices,
geometry/source regressions before checkpoint publication, real SIGKILL,
checkpoint write/sync/rename failures, dynamic truncation/growth, byte budgets,
commit cancellation, missing COMPLETE and legacy-mode compatibility.

## Validation

Final checks passed: 575 Rust tests, 63 external tests, Rust 1.85 workspace/all-targets
Clippy and formatting. Combined production Rust line coverage is
17,240/17,401 = 99.0748% with the existing toolchain/source exclusions. Explicit
release experiments are additional runs, not part of those normal test totals.
