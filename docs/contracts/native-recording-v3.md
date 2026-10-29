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

## Durable checkpoint ordering

For each sealed chunk, the writer performs:

1. Sync the payload file and append-only record index.
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

Group commit deliberately allows loss of the last unacknowledged active chunk
on SIGKILL/power loss: at most one chunk (up to 32 pairs), in addition to frames
still in the acquisition queue/in flight. This is not per-frame synchronous
durability. Earlier acknowledged prefixes retain ordered fsync protection under
a filesystem/device that honors fsync. The old V2 path syncs individual payloads
but does not provide this explicit durable-index-prefix contract.

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
