# Bound SDK frameset evidence v1

## Identity and source clocks

`frame-core::BoundPairGuard` binds both owned child frames to one verified runtime `CaptureSnapshot`, one receive-clock origin and one SDK frameset assertion. It retains independent source sequences, device/system timestamps, layouts, source/output scales and normalized flags. It never changes either source timestamp to manufacture synchrony. The serialized association is `sdk_frameset` and `hardware_sync_verified` remains false.

## Pair validation

Validation happens before state commit: packet format, exact payload span, independent frame sequences/timestamps, explicit skew budget, snapshot stream dimensions, source scale and monotonic receive domain. Failures reject the whole pair. The old `PairGuard::accept` remains millimetres-only. The explicit native-count variant preserves pixels and their SDK scale and cannot change unit policy mid-session; it is intended for native sources and offline callers, not a change to existing ROS depth semantics. Lossless PNG output of normalized millimetres cannot recover fractional SDK counts.

## Runtime snapshots

The resident Rust service, when given explicit `quality_policy`, builds an immutable runtime snapshot from the enabled SDK profiles and actual depth-to-color transform. No fallback camera model is supplied. It stores snapshots in the private status directory's `snapshots/<id>.json`, and reports snapshot ID/path in status. Session and supervisor instance are distinct identities. Source-scale or calibration/profile changes require a new session/snapshot; they are not patched into an old record. Snapshot provenance is SDK-active-profile evidence and structural validation does not certify physical accuracy.

## ROS publication

The existing Image, CameraInfo and v1 pair metadata topics remain unchanged. An additive reliable/volatile KeepLast(10) String topic `<pair_metadata_topic>/evidence` carries versioned bound evidence plus both actual ROS header stamps. Pixels and evidence occupy the same bounded queue item, so realtime overflow evicts the whole pair. The evidence is submitted only after both Image submissions and the matching v1 pair submission. A later failure remains a publication failure: ROS publication is not atomic, successful calls cannot be rolled back, and no consumer receipt is certified. A bridge lacking the additive evidence symbol returns a clear submission error rather than silently omit evidence.

The compatibility `publish_bound_evidence` entry point requires SDK system-mapped
Image stamps. `publish_bound_evidence_with_timestamp_source` checks the explicitly
selected system or global fields instead; it never substitutes one clock for the
other. Global Image stamps currently require device-clock pair metadata, while
the legacy system-clock pair metadata contract still binds its stamps to system.
Source timestamps, evidence JSON and sequence/session/pair-clock checks are unchanged.

## Management binding

Management reads the SDK worker's exact snapshot rather than rebuilding a different ROS-observed snapshot for the recording guard. It verifies live identity before/after reading, config bytes, fixed private path, owner/mode/link count and the complete envelope. The official driver remains the default; its timestamp-only observer has a separate association contract and does not claim SDK frameset provenance.

## Validation scope

Tests cover native/mm interpretation, unchanged source clocks, different source indices, wrong session/receive domain, whole-pair queue ownership, strict JSON, SDK calibration conversion, malformed source/profile rejection and dynamic C ABI evidence copy/clock/session guards, including a partial publication error. Hardware synchronization, physical calibration accuracy and real consumer receipt require separate acceptance; no new hardware or performance result is claimed here.
