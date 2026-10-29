# Resident SDK quality adapter v1

The optional root `quality_runtime` field is a `quality_core::RuntimeQualityPolicy`.
Its nested `quality` must equal root `quality_policy` exactly, including action
and thresholds. The service rejects inconsistent, incomplete, duplicate-key or
unsupported policies. This adapter accepts **report-only** action; it does not
claim fail-closed behavior while continuing publication. Old configurations
without `quality_runtime` retain their previous behavior.

The complete runtime policy is included in the actual SDK capture snapshot's
configuration content and hash. Each worker owns a fresh acquisition session and
binds its quality evidence to the supervisor's explicit instance ID. Reopening
the SDK creates a new worker/session and reads calibration again; old counters,
source-time high-water marks and recovery streaks do not cross into that epoch.
This adapter does not change restart budgets, warmup allowances, recording
protection, pairing tolerances, queue capacity, publishing or device properties.
A report-only quality degradation is separate from the existing capture
`ready` state; inspect `status.quality.state` as well as `status.ready`.

## Observations and limits

Every present SDK color/depth frame increments that stream's received count
before complete-pair, pixel-format or pairing acceptance. Counts are independent
of published/accepted pair counts. Missing streams receive no invented frame.
Before an actual SDK snapshot exists, receipts are explicitly counted as
`pre_snapshot_received_counts`; they do not invent calibration IDs or enter
snapshot-bound rolling frame windows. The latest metadata retains original
sequence, device microseconds, SDK system microseconds, format, size, scale,
monotonic receive offset and host Unix receive time. Metadata getter failures
increment received counts and open a contract violation without invented values.

After binding, each stream's rolling window checks observed format/dimensions,
layout, depth-scale validity, source mapping/age and SDK system clock progression.
Depth receipts are counted once before their pixel-validation phase. The
normalization loop supplies source/output zero, nonzero and u16-maximum counts;
strict/native and missing-color paths measure the validated source payload.
`valid_fraction` means numerically nonzero, **not** physically valid range or
calibration certification. Original SDK counts and scale remain untouched.
Raw/source and normalized-output 65535 counts are explicitly distinguished.
Color pixels and unobserved depth payloads remain `None`; missing CameraInfo
observation remains `None`. Conversion/pairing/publication errors are additional
group episodes; stream statistics do not certify consumer delivery. Source sequence and
device-time high-water diagnostics are independent of the SDK system age map.
Pair delta is observed in the explicitly configured device/system clock even
when the pair is rejected. A per-loop tick detects stale streams without relying
on new frames arriving. Startup observations are counted, not silently erased.

Source age uses only the existing explicit `sdk-system-unix` mapping: SDK system
microseconds → Unix nanoseconds, compared with independently observed host Unix
receipt time. There is no device-time-to-now guess or ROS-header substitution.
Missing/overflowing/future/old mappings become quality violations. Raw clock
values are retained for diagnosis and are not converted in the source metadata.
SDK load/open/poll failure is reported as SDK availability/disconnection evidence;
this classification does not certify a physical unplug event.

Actual whole-pair queue eviction opens `QUEUE_OVERFLOW`; successful queue
admission provides healthy evidence. The current service drains synchronously
and still uses the existing bounded whole-pair queue. Publication is wrapped at
the actual `rgbd::drain` boundary: only successfully returned bridge publish
calls increment `publication_calls`; a later failing call opens
`PUBLISH_PARTIAL_FAILURE` and increments `partial_publications` if any prior call
succeeded. Neither counter means DDS delivery. All existing drain errors remain
nonzero failures; quality reporting does not suppress them. Consecutive healthy
observations satisfy the explicit recovery hysteresis. Sustained faults update
episode counters without producing an event on every frame.

## Private machine-readable files

`status.json` adds a safe `quality` object containing state, latest event sequence
and paths under its private status directory:

- `quality/<session>/current.json`: instance/session/snapshot binding, current
  quality state and bounded rolling-window summary, received/pre-snapshot counts,
  latest original stream metadata, and publication counters.
- `quality/<session>/query.json`: the typed `QualityQueryResponse` snapshot DTO,
  using the same quality engine as current state and events.
- `quality/<session>/events.json`: bounded transition page, explicit cursor and
  `EVENT_GAP` evidence if older events were evicted. No unbounded append log.
- `quality/<session>/summary.json`: final per-session summary and receipt counts,
  written on a normal/error worker exit. A hard process kill/native hang cannot
  invent a final summary; absence remains explicit in the last status reference.

Files use the existing atomic private writer (0600 files, 0700 leaf directory,
write/sync/rename). Each file is atomic independently, not a multi-file database
transaction. Readers must check session/instance and `latest_event_sequence`
when correlating current state and events, retrying if a concurrent update is
observed. Status is written after the corresponding current/events files. Read `/tmp/capture/status.json` inside the matching capture container, then
read its referenced private quality paths. There is no new management
`capture-status` command or HTTP query endpoint in this version; these JSON
files and the Rust query API are the machine interfaces. Calibration/serial
data is not exposed in the status object. The quality files themselves
are private acquisition evidence and contain opaque identity/clock metadata.

Generated Rust configurations explicitly include all runtime limits: stale/age
budget equal to existing `idle_timeout_ms`, existing pairing delta, 100 ms future
budget, valid fraction 0 and zero fraction 1, report-only action; window capacity
1024 and 10-second horizon; transition capacity 256; one session/summary per
worker engine; all nine rules open after one violation, recover after three
healthy observations, severity degraded. These values are serialized in the
private generated config, not hidden engine defaults. They do not loosen any
capture acceptance or recovery policy.

## Verification scope

Tests exercise the actual service adapter with synthetic SDK metadata and an
actual immutable snapshot fixture, independent receipts, tick-driven stale
transitions, repeated partial-publication failures through the runtime drain
wrapper, bounded transition behavior/recovery hysteresis, and private final
files. A real capture-service subprocess with a missing SDK verifies error exit,
redacted status references and finalized zero-frame quality files. Generation
tests verify idle/pair budget retention and every explicit runtime field. These
are offline checks, not hardware or ROS delivery acceptance.

## Bounded measurements and missing-route diagnostics

Each `StreamQuality.window_metrics` exposes active-window host receipt FPS
`(n-1)*1e9/(last-first)` only with at least two distinct receipt offsets, the
maximum adjacent receive gap, and signed SDK-system source-age min/max/mean at
receipt (future values are negative). Measurements use the declared rolling
window capacity/horizon; an expired/insufficient window is `None`. Pixel totals
and zero/nonzero/65535 fractions aggregate only measured samples retained in
that window. They are not filled with zero for missing streams. Query fields
are added with serde defaults, so existing policies/configurations need no new
thresholds. Source-age arithmetic retains original raw clocks separately.

`record_frame_deferred_pixels` counts metadata once and leaves pixel contract
health pending. `record_pixel_metrics` targets the exact retained frame context,
updates that receipt, and rejects duplicate, old or cross-bound observations.
It cannot reset a sustained pixel violation's configured hysteresis merely by
receiving the next metadata frame. Existing min-valid/max-zero thresholds apply
at the measured pixel phase under both ReportOnly and FailClosed engine policy;
the service still supports ReportOnly only, without filtering or new restarts.

Fixed diagnostic enums bound session and recent classification maps. Missing
routes are `MISSING_COLOR`, `MISSING_DEPTH` or `MISSING_BOTH`, counted per actual
frameset, with independent per-stream received counts and source sequence
high-water marks. There is no cross-frameset fill/matching. Pair skew keeps the
configured 20 ms (or explicitly configured) policy; no tolerance is adjusted to
pass. Binding/dimension/source-scale changes are contract diagnostics and do not
produce synthetic timestamp regressions from an invalid clock binding.
`PAIR_BINDING_OR_POLICY_INVALID` is distinct from `SOURCE_NOT_ADVANCING`.
Recent classification counts use a bounded diagnostic-observation window, not
an unlimited list; session counters remain independent of window eviction.

No hardware, scale, long-duration or performance validation is implied by the
synthetic measurement/phase/diagnostic tests.

## Available images in incomplete framesets

After startup discovery, `capture-service` and `rgbd-probe` publish an available
color or depth image even when the other SDK frameset route is missing. Payload,
profile, depth unit conversion and ROS stamp validation still apply. Older queued
pairs are drained first; successful single-image publication advances only that
stream's stamp guard. SDK settings, source indices, original timestamps and pair
clock/tolerance are unchanged.

`unpaired_images_published` counts successful image publication calls from these
incomplete framesets. It contributes to `individual_publish_calls` (the finite
probe calls this `individual_publish_calls_succeeded`), but never to
`pairs_published`, pair evidence or bound evidence. It does not prove downstream
receipt. Missing-pair diagnostics, degraded state, startup readiness and pair idle
timeouts remain based on complete accepted pairs. Startup discovery frames remain
excluded. Complete but skew-rejected framesets retain the existing pair policy.

This preserves available images for independent ROS consumers/recorders without
inventing a color/depth association. The optional native whole-pair recorder
retains its strict terminal failure on a missing route; an incomplete native
recording is never presented as complete. That recorder is not an independent
stream archive. Hardware exposure synchronization and elimination of SDK pairing
slips require separate hardware validation; a shared device clock alone does not
establish exposure synchronization.
