# Native recording directory V3: chunks and recoverable prefixes

The resident service supports native SDK counts in `recording-directory-v3`.
Set `raw_recording.storage_format` to `chunked-v3` to enable it. Omission or
`files-v2` retains the existing layout and sync behavior. This opt-in is deliberate:
very fast fsync/cache workloads can favor V2 and V3 changes the unacknowledged-tail
loss boundary. V3 reduces inode/sync pressure and adds verified-prefix recovery.
The public `RawRecordingWriter::begin*` constructors keep emitting V2 for callers
that explicitly need the old layout. New `begin_chunked*` constructors emit V3.
The reader supports both versions; older readers reject V3 rather than guessing.
`offline-depth --recording` selects version detection. `--recording-v2` remains a
legacy alias. Capability negotiation advertises offline input versions 1, 2, 3.
No camera/SDK settings, ROS publication, snapshots, calibration, original indices,
source clocks, units, pair provenance or business validity are rewritten.

## Files and bounds

V3 retains `snapshot.json`, optional ROS snapshot/association, `records.jsonl`,
`manifest.json` and the final `COMPLETE` marker. Instead of three files per pair,
`payload-00000000000000000001.bin`, etc. contain consecutive color bytes, depth
bytes and tagged evidence JSON for whole pairs. Each index FileRef includes
`path`, `sha256`, and `range: {offset, length}`. SHA-256 covers each exact extent,
not an imagined reconstructed image. V2 FileRefs omit range and keep their
original serialized form.

A chunk is sealed at 32 pairs or a 16 MiB target. Before adding a pair that would
cross that target, the previous chunk is sealed. A single larger pair stays
whole in one chunk. Each image remains capped at 64 MiB and evidence at 64 KiB;
the absolute chunk limit is 128 MiB + 64 KiB. There is one append-only active
file, no accumulated pixel batch in RAM, and no compression. The existing bounded
owned whole-pair acquisition queue and terminal overflow policy remain unchanged.

Readers require canonical chunk names, contiguous non-overlapping extents in
color/depth/evidence order, consecutive chunk numbers, exact file lengths, valid
individual hashes, valid complete index hash, monotonic source progress and the
existing snapshot/calibration/representation/pair checks. V3 permits native SDK
pairs only; observed ROS associations remain V2. Missing/truncated/corrupt
chunks, aliases, gaps, range overflow and oversized reads fail closed. Readers
reuse one chunk file handle, not cached pixels; exact reads and final length
checks still reject truncation or growth during validation.

## Bounded write staging

V3 writes the color, depth and evidence extents with vectored I/O, without
concatenating image buffers. Short writes advance across slice boundaries;
interrupted calls retry and zero-length writes fail. Evidence and index JSON
serialization reuse separate metadata buffers. V2 retains its per-file writes.

The append-only index normally flushes at a 64 KiB threshold. While a
checkpoint task is pending, later index records stay in RAM so the append thread
does not write the inode being synced/read back. This metadata buffer is capped
at **2 MiB**: the existing 32 unconfirmed pair credits times the 64 KiB maximum
index line. It holds no additional image pairs. Capacity grows on demand and is
retained for reuse, including after a pending task has finished.

Every checkpoint flushes its entire index prefix before payload/index sync or
validation. Buffered bytes count against the recording byte budget when staged,
not again at flush. A failed partial flush poisons the writer; Drop does not
retry that failed flush or promote its tail. Existing confirmed prefixes remain
recoverable. Diagnostics report `index_buffer_peak_bytes`,
`index_buffer_capacity_peak_bytes`, `pipelined_index_buffer_limit_bytes`, write
attempts and successful flushes. The separate validation image scratch buffer
remains at most 64 KiB. Neither number describes total process RSS.

The layout, hashes, two actual readback rounds and durability barriers are
unchanged. This staging applies to native recording, not external ROSbag2.

## Durable checkpoint ordering

For each sealed chunk, the writer performs:

1. Completely flush the bounded index buffer, then sync the payload file and append-only record index.
2. Sync the staging directory so both names are durable before the checkpoint.
3. Read back the new chunk through the real recording validator. Cached verified
   index digest/progress preserves cross-chunk checks without rescanning old
   records at every checkpoint. Invalid pairs cannot supersede a good prefix.
4. Write and sync `.checkpoint.tmp`, containing binding, snapshot SHA-256,
   committed record count, index byte length/hash, and sealed chunk count.
5. Rename it over `checkpoint.json` and sync the staging directory again.

Only then does `disk_stages.durable_pairs` advance. `written_pairs` continues to
mean successful append calls, not durable acknowledgements. Diagnostics expose
checkpoint count, payload-file count, sync/write/verification/append timings.
Sealed chunks are never reopened for append. The byte budget charges payload,
index and metadata contents, including the temporary coexistence of old/new
checkpoint files; obsolete checkpoint bytes are released only after successful
replacement. It does not claim to measure filesystem metadata/journal space.

Without the checkpoint pipeline, group commit allows loss of the last
unacknowledged active chunk on SIGKILL/power loss: at most one chunk (up to
32 pairs), in addition to frames
still in the acquisition queue/in flight. This is not per-frame synchronous
durability. Earlier acknowledged prefixes retain ordered fsync protection under
a filesystem/device that honors fsync. The old V2 path syncs individual payloads
but does not provide this explicit durable-index-prefix contract.

<a id="optional-bounded-background-checkpoint-transaction"></a>

## Optional checkpoint pipeline

`raw_recording.checkpoint_pipeline: true` requires `storage_format: "chunked-v3"`.
Omitted/false keeps synchronous checkpoints. Library callers use
`RawRecordingWriter::enable_checkpoint_pipeline()` before the first append.

The default pipeline batching policy is `half-credit`, with dispatch targets of
16 pairs / 8 MiB. If the single preceding checkpoint is unfinished, reaching
that target does not block or start another task: the current active chunk may
continue within the remaining global credits. Before **every** pair admission,
pending plus active payload remains within **32 pairs and 16 MiB combined**.
The next admission waits when those credits are exhausted. A single indivisible
larger pair retains the existing per-image/evidence limits, with no earlier or
following pair overlapping it. These credits are separate from the acquisition
FIFO and the total recording byte budget.

`raw_recording.checkpoint_batching: "full-credit"` explicitly selects a
32-pair / 16 MiB dispatch target; specifying either batching value also requires
the pipeline. Library callers use `enable_checkpoint_batching()` before the first
append. This candidate reduces transaction frequency at the cost of overlap;
it does not increase global credits and is not the default. Diagnostics report
the effective `checkpoint_batching`, `uncommitted_pairs_peak` and
`uncommitted_payload_bytes_peak`.

The append thread flushes the sealed index prefix and transfers the payload
handle, digest and fixed index length to one background task. That task executes
the complete [checkpoint ordering](#durable-checkpoint-ordering), including
payload/index/directory sync, readback/hash checks, checkpoint write/sync,
rename and final directory sync. It advances `durable_pairs` only after the final
sync. Later records remain in the bounded metadata buffer; the task never reads
the new active payload chunk or acknowledges its tail.

A subsequent append collects a finished task's verified progress and accounting;
it performs no checkpoint commit I/O. Explicit `checkpoint()`, finish and graceful
Drop join pending work; checkpoint/finish synchronously seal any final active
tail. Errors or panics fail the writer. Drop joins even on failure, so no task
outlives directory publication. A valid submitted prefix may finish committing
during abort; the overall failed job still has no COMPLETE.

Long sync stalls or insufficient average confirmation throughput can fill the
unchanged bounded acquisition queue and cause terminal overflow. Current hardware
acceptance and performance limits are listed in [current status](../guides/current-status.md).
The pipeline does not affect capture with native recording disabled or external
ROSbag2. Recovery and COMPLETE semantics are the same for synchronous and
pipelined V3 directories.

## Recovery is not completion

```
offline-depth --recover-recording-prefix /absolute/path/capture.partial
```

This read-only command validates the last checkpoint, limits the index to its
committed byte length, and verifies all referenced original data/evidence. It
ignores later incomplete index bytes and unreferenced chunks without truncating,
renaming, deleting or repairing the source. Success reports
`recovered_prefix_valid=true`, `complete=false`, validated pair count and
`business_validity=null`. No checkpoint/zero committed pairs is not a successful
empty recovery. A committed input with COMPLETE is rejected by the recovery
entry point. The library exposes `RecordingDirectorySource::open_recovered`.

Recovered prefixes cannot be exported as completed jobs through the ordinary
conversion API. Normal V3 validation without COMPLETE reports complete=false;
normal V3 export requires the exact V3 COMPLETE marker and no partial/interrupted
state. V2 conversion semantics remain unchanged.

Graceful incomplete shutdown attempts to seal only valid appended pairs and
keeps `.partial`/INTERRUPTED, never COMPLETE. An append/checkpoint failure is
terminal and does not advance the checkpoint; recovery can still use the earlier
prefix. If checkpoint rename succeeded but its final directory sync fails, the
visible checkpoint may describe a newer prefix whose data/index were already
synced and verified. Recovery validates what is actually present; runtime durable
counters never acknowledge that failed sync.


## Complete transaction

Reaching the explicit target closes only the native producer. Finish seals the
last chunk, syncs the index, writes the V3 manifest and reads back the entire
recording again, including all hashes and bindings. The existing cancellation
commit guard, no-replace directory publication, COMPLETE sync and final-directory
and parent sync are retained. Pre-gate cancellation, storage/budget/verification
failure or an interrupted target never reports complete. Publication failures
retain the existing best-effort marker removal/rollback and explicit failure.
There is no resume/append into an interrupted job and no silent trimming of bad
frames to satisfy the target.

Tests exercise actual SIGKILL, corrupted/torn tails, range/index/snapshot
corruption, byte-budget failure, cross-chunk source regressions, missing COMPLETE,
valid exports, and I/O faults at payload write/sync, index sync, directory sync,
checkpoint sync/rename and final commit. These are fault-injection tests, not a
claim of a physical power-cut test or measured hardware crash survival.

## Bounded validation-only reader

`RecordingDirectorySource::validate_next()` advances one record and returns a
boolean, without returning image buffers. It shares the existing validator with
`next_frame()`, which remains available for consumers/exports. Both APIs check
EOF/index hashes and original bounds/bindings. The image scratch buffer is at
most 64 KiB; metadata parsing retains its existing bounds. This is storage/evidence
validation, not physical certification or a substitute for numerical conversion.

The checkpoint and final whole-job readback both use this path, as does read-only
prefix recovery. `validation_image_bytes` counts image bytes from completed
successful validation passes; a complete V3 job has twice its image payload.
`validation_buffer_peak_bytes` reports peak image scratch capacity only. These
are runtime diagnostics, not recording content or durability acknowledgements.
See [release comparison](../validation/reports/streaming-verification-ab-20261029.md)
for throughput, append P99, verification time, peak RSS and reproduction details.

## Optional raw IMU samples

Explicit camera IMU support adds an optional nonempty `imu` array to each index
record in both V2 and V3. Samples retain independent source indices/timestamps
and sensor frame IDs; the receive batch does not imply RGB-D exposure pairing.
The index hash covers them, checkpoint validators carry per-sensor progress,
and final validation still reads the entire recording again. Recovery ignores
IMU samples outside the acknowledged index prefix. Legacy empty records keep
their serialized form; strict older readers reject the new field. See
[camera IMU v1](camera-imu-v1.md) for bounds, configuration and ROSbag2 checks.
