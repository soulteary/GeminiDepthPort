# Recording reader CPU profile

The validation reader retains actual per-frame SHA256 verification. Completed V3
recordings still pass sealed-prefix readback plus final whole-recording readback.
This optimization does not change the format, sync barriers, acknowledgement,
queue/credit limits, or crash recovery. V2 retains its existing final verification.

Validation-only decoding now checks unique JSON keys without constructing a full
intermediate `Value` tree, then deserializes the typed record. The duplicate check
borrows unescaped keys and uses a BTreeSet, retaining logarithmic lookup rather
than quadratic comparisons. Size/recursion/finite-number checks, escaped-key
identity, unknown-field rejection, IMU progression and all bindings remain checked.
The materialized consumer path retains the original parser for independent parity
checks. V3 validation reuses its bounded evidence buffer; frame contexts are
borrowed during checks. No successful hash is cached across frames or readback passes.

Cached payload file positions skip a seek only when the requested offset is the
known current offset. Every extent is still read in full. A seek/read failure
invalidates position knowledge; the next access must reposition. Chunk paths are
still canonical 20-digit positive u64 names, checked without formatting a temporary
string. File hashes and on-disk bytes remain unchanged.

## Read-only diagnostic tool

Build and run against an existing completed recording:

```bash
cargo build --release --locked -p offline-depth --example profile_recording
target/release/examples/profile_recording /absolute/recording
```

For an interrupted V3 recording, explicitly request only the validated checkpoint
prefix:

```bash
target/release/examples/profile_recording /absolute/recording.partial --recovered
```

The tool writes only its JSON result to stdout. It does not modify the recording,
create COMPLETE, acknowledge a checkpoint or certify physical durability. Its
additional diagnostic pass does not replace either production readback pass.
Use a recording from the actual camera/IMU configuration for representative costs.

Library users call `enable_validation_cpu_profiling()` before the first read,
consume `validate_next()`, then inspect `validation_cpu_profile()`. Enabling after
reading starts is rejected. Profiling is off by default and is independent of
`raw_recording.checkpoint_observation`; it does not enlarge capture status reports.

Stages expose calls, errors, cumulative wall and thread CPU nanoseconds:

- `index_read`: bounded JSONL read.
- `index_decode`: strict and typed record decoding, including IMU fields.
- `evidence_read_hash`: evidence file access and digest check.
- `evidence_decode`: strict and typed evidence decoding.
- `image_read_hash`: complete image check, including access, reads and SHA256.
- `image_read` and `image_sha256`: nested 64 KiB reads and digest updates.
- `read_call`: complete reader call, including EOF, binding checks, progression,
  buffer handling and profiling overhead; excludes opening/dropping the source.

`image_read` and `image_sha256` are subsets of `image_read_hash`; all are inside
`read_call`. Do not sum parent and child stages. Digest finalization/hex comparison
and path/range checks are included in the parent, not SHA256-update-only timing.
`cpu_samples == 0` means no supported thread CPU samples, not measured zero CPU.
Buffer capacities are current retained capacities, not total/peak RSS.

Fine clocks and per-stage bookkeeping have measurable overhead. The accompanying
[report](../validation/reports/reader-optimization-ab-20261030.md) separately measures
it. Use this tool for diagnosis; compare production CPU/P99 with profiling disabled.
Page-cache state, concurrent writes and storage scheduling affect results. A warm,
post-capture profile cannot replace live checkpoint/disk/queue correlation using
the [checkpoint sampler](checkpoint-observation.md).

## Reproducing the experiments

`reader_benchmark.rs` contains an explicitly ignored common release driver and A/B
parent. Both implementations use identical synthetic fixtures, chunked-v3 pipeline
settings, pair counts and artifact checks. Build the current test driver against
the baseline checkout too, removing only its `PROFILE_BEGIN`/`PROFILE_END` test-only
block because the old production API has no fine profiling method. Do not change
baseline production sources for the CPU/P99 A/B.

Set `GDP_READER_BEFORE` and `GDP_READER_AFTER` to the isolated release test executables,
`GDP_READER_OUTPUT` to a new JSON path, and invoke the after executable with
`--exact reader_ab_benchmark --ignored --nocapture`. It alternates order for seven
repeats and checks two-round image/IMU counts and identical metadata artifact hashes.
The child also independently materializes and verifies the recorded frames after
capturing CPU and memory metrics.

For baseline CPU *breakdown only*, apply the published
[instrumentation patch](../validation/reports/reader-profile-baseline-20261030.patch)
to baseline `8fae5ed`; it adds the same clocks without the reader optimizations.
The raw JSON records separate baseline, instrumented-baseline and after hashes.
`GDP_READER_PROFILE=1` asks the child for extra profiling-off/on validation passes;
`GDP_PROFILE_FIRST=1` reverses their order. Those diagnostic passes are outside
production A/B timing. `GDP_READER_IMU=1` adds two 200 Hz-timestamped IMU streams,
with 6/7/7 samples per stream per three nominal video pairs.

`reader_allocations.rs --exact reader_allocation_child --ignored` is a separate
allocator experiment, not a CPU benchmark. `GDP_VERIFY_ROOT` supplies a private
existing temporary directory. It measures 128 validation records through EOF,
excluding source open/drop. Requests include reallocations; cumulative requested
bytes are not live bytes, an RSS peak, or a count of OS allocations.

`tests/support/reader_seek_counts.c` traces payload lseek/lseek64 calls in a separate
LD_PRELOAD run. It arms only after the explicit `GDP_READER_SEEK_ARMED` COMPLETE
path exists, and writes counts to a new private `GDP_READER_SEEK_COUNTS` path.
It never changes offsets or injects failures. The normal Linux test proves 256
payload seeks for 128 records; the original reader makes 384. Tracing and allocator
instrumentation are not enabled during CPU/P99 measurements.
