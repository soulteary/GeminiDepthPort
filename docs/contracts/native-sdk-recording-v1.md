# Optional native SDK recording in the resident service

`capture-service` accepts an optional private root configuration object:

```json
{
  "raw_recording": {
    "directory": "/private/native-recordings",
    "name": "capture",
    "queue_capacity": 2,
    "max_pairs": 30,
    "max_bytes": 100000000
  }
}
```

All five fields are explicit. Optional `storage_format` is `files-v2` (default, unchanged legacy layout)
or `chunked-v3` (explicit group commit); other values are rejected. The directory must be absolute and cannot be `/`,
`/tmp` or contain parent traversal. Names use 1–64 ASCII alphanumeric, underscore
or hyphen characters. Queue capacity is 1–16; both limits are positive u64.
Enabling this feature requires an explicit matching `quality_policy` and
`quality_runtime`. Old configurations omit the object and do no raw payload
copies, create no raw writer, and keep their existing ROS behavior.

The service appends its independently generated worker session ID to the job
name. A restarted worker never resumes a previous job. It waits for actual
SDK profiles/calibration and a same-frameset native pair; startup/waiting framesets
excluded before recording starts are counted explicitly in raw status. Once
active, a missing route, rejected native pair, whole-pair queue overflow, SDK
session error, storage error or byte-limit failure is terminal for this recorder.
It does not stitch a missing route from a later frameset. The existing configured
pair clock/tolerance applies unchanged; no skew budget is adjusted.

## Native identity and representation

The raw branch owns a distinct sealed snapshot with `DepthRepresentation::SdkCounts`
and exact f64 promotion of the actual SDK f32 millimetres-per-count scale. Every
other snapshot content/binding field is identical to the source ROS millimetre
snapshot: session, supervisor instance, generation, configuration and actual
calibration remain the original observation. The native SnapshotId differs
because representation differs; it is not substituted into ROS packets.

`ros-snapshot.json` preserves the original ROS envelope and
`snapshot-association.json` explicitly links both IDs. The versioned reader independently
verifies that they differ only in depth representation. The recorder does not
rewrite ROS topics, frames, packet evidence, source indices, either raw SDK
clock, calibration, exposure or device properties. All physical accuracy flags
remain false; this feature is storage evidence, not physical certification.

On the acquisition thread, SDK frame handles remain borrowed/owned in their
original non-Send SDK domain. Only when the raw recorder is active are original
color and depth bytes copied into owned vectors and bound to a separate native
`BoundPairEvidence`: normalized=false, output scale equals actual source scale,
original indices and both SDK clocks retained. Source payloads are not recovered
from already-quantized ROS depth. The current SDK API exposes packed stride only;
source format/dimensions/exact payload length must validate as packed YUYV/Y16.
Unknown padding is rejected rather than inferred or stripped. The supported SDK
service host is explicitly little-endian Linux; recorded byte order is explicit.

A bounded whole-pair channel sends only owned pixels/evidence to one disk worker.
When full it rejects the new whole pair and terminates the recorder. It never
silently drops a pair and later marks the job complete. Capacity bounds pending
pairs; one producer and one consumer may additionally own an in-flight pair.
Per-image payloads are capped at 64 MiB. This is a bounded ownership design,
not a claim about total process RSS or performance.

## Completion, budget and cancellation

The disk worker uses `RawRecordingWriter::begin_with_limit`; `max_bytes` accounts
for actual snapshot, association, pixel, evidence, record-index, manifest and
completion-file bytes, checked before each write. It is not a raw-payload-only
budget. `payload_bytes_submitted` is separately named and is not advertised as
all on-disk bytes. Limit failure is terminal even if all target pairs were queued.

Reaching `max_pairs` closes only the raw producer. The camera and ordinary ROS
publication continue; the disk worker drains accepted pairs, validates the versioned
recording from disk, and performs an atomic no-replace commit. It never releases
recording protection or assigns business validity. Signal/session exit before
the explicit target leaves an unfinished `.partial` job. Missing source metadata
before initialization has no invented snapshot or successful empty output.

Finalization uses a cancellation/commit guard after full disk verification.
Recorder failures before that gate prevent COMPLETE/rename. The short final
commit gate linearizes the finished target transaction; an SDK error after that
boundary cannot undo an already-committed, earlier capture. File/directory/parent
sync and no-replace behavior come from the common transactional writer. Failed jobs retain
`.partial` without a completion marker. Hard process kills cannot invent a
final summary; the existing process supervisor bounds shutdown/native-I/O hangs.
This adapter does not expand supervisor restart limits or recovery permissions.

## Private status and quality events

`status.raw_recording` includes state (waiting-for-snapshot, recording, draining,
committing, complete, failed), stable reason, session and both snapshot IDs, limits,
submitted/written pair counts, submitted payload bytes, whole-pair overflows,
startup/waiting exclusions, final/partial paths, and explicit
`business_validity:null`/`recording_protection_released:false`.

A failure opens a quality episode using fixed `Scope::Recorder`
(`{"kind":"recorder"}`), with the existing QueueOverflow or FrameContractInvalid
rule and severity. It does not become DeviceDisconnected merely because storage
failed. Recorder scope is independent of Group/Stream; subsequent healthy ROS
frames cannot recover a terminal raw recorder. No new runtime policy thresholds
or rule enum values are required. Existing Group/Stream DTOs serialize unchanged.
Query clients must preserve unknown scope facts instead of dropping events.

The producer never prints SDK errors, serials, raw configuration or calibration.
These private status/evidence files contain opaque IDs and original clock data;
they are not a public telemetry endpoint.

## Offline verification boundary

Actual channel/worker tests verify original 0.1-mm SDK-count fixtures (including
65535), unequal color/depth source indices, preservation of both original clocks,
distinct linked native/ROS snapshots, target completion, a held real bounded
channel overflowing, explicit SDK abort, and total-byte-limit failure. Interrupted
jobs have no final directory/COMPLETE. A real service subprocess with missing SDK
verifies failed raw status and Recorder-scope events without starting a device.
All fixtures are synthetic and physical accuracy is false. No hardware, scale,
30-minute, publication, performance or physical-accuracy acceptance is claimed.

## Chunked native storage

The service uses V3 chunked storage when `storage_format` is `chunked-v3`. Source identity, whole-pair
semantics, bounds and target completion above remain unchanged. The new durable
checkpoint distinguishes appended from committed pairs and enables read-only
recovery of a verified prefix without claiming a complete target. See
[Native recording V3](native-recording-v3.md) for exact fsync ordering, range
checks, byte budgets, bounded uncommitted-tail loss and recovery commands.
