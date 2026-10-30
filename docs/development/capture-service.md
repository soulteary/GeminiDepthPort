# Resident RGB-D capture service

Use the [Rust backend guide](../guides/rust-backend.md) for managed deployment.
This reference describes standalone configuration, process supervision and shutdown.

`capture-service --config /absolute/private/capture.json` runs a supervisor and
one thread-confined SDK worker process. `--validate-config` accepts the same path
and checks schema and any supplied artifact hashes without opening a camera.
The service uses the [RGB-D probe schema](../../crates/capture-probe/README.md#single-pipeline-rgb-d-probe)
with `seconds` **omitted**, requires ROS
output, and adds this explicit policy object:

## Service configuration

```json
{
  "service": {
    "status_path": "/tmp/capture/status.json",
    "generation": "IMMUTABLE_GENERATION",
    "idle_timeout_ms": 2000,
    "startup_timeout_ms": 15000,
    "heartbeat_ms": 500,
    "heartbeat_timeout_ms": 10000,
    "max_restarts": 3,
    "restart_backoff_ms": 1000,
    "shutdown_timeout_ms": 5000,
    "ready_min_pairs": 5
  }
}
```

All policy fields are required and bounded. An optional `runtime_contract` object
contains exactly `sdk_sha256`, `bridge_sha256`, and `service_sha256` (lowercase
SHA-256 hashes). If present, the worker verifies actual SDK library, bridge, and
running executable bytes before loading the SDK. Generated deployment configs
should always include this contract. Configuration bytes are read at startup,
then the supervisor requires the same SHA-256 bytes before every worker restart.
There is no live config reload, silent profile change, device-property write, or
replacement calibration fallback.

## Status and readiness

The worker writes a mode-0600 JSON status file using fsync and atomic rename in a
private mode-0700 directory. Status states are `starting`, `ready`, `degraded`, and
`stopped`. Readiness requires `ready_min_pairs` consecutive valid, fully published
RGB-D pairs after the explicitly configured startup discovery window. A missing,
malformed, skewed or nonadvancing pair immediately degrades readiness; subsequent
consecutive valid pairs can recover it. No valid published pair within
`idle_timeout_ms` ends the session. Failure to establish readiness within
`startup_timeout_ms` also ends it. Counters are session-local and include startup
observations, while startup valid pairs are discarded rather than published.
Health consumers must check status heartbeat freshness and actual ROS output;
`ready` alone does not establish downstream delivery.

Every worker start/reopen creates a new random 32-character lowercase hexadecimal
`session_id`. It reloads both exact profiles, resets pairing and ROS stamp guards,
reads calibration again from the first complete actual SDK frameset, and sets
that session ID on both frame metadata publishers and pair metadata before any
image publication. Consumers must require matching session IDs across frame and
pair evidence and treat a session change as a discontinuity; sequence reset is
not continuous recording. Raw SDK errors, device serials, library/config paths,
and raw calibration are absent from the status report. The private generation,
config digest, process IDs and session ID remain for local lifecycle correlation.

## Shutdown and recovery

SIGINT/SIGTERM stop the worker loop, release queued pairs, and call SDK stop on its
own thread. SDK calls cannot be safely interrupted in-process. The supervisor
therefore watches the worker's changing status heartbeat using its own monotonic
clock, sends SIGTERM on a stale heartbeat, and kills the isolated worker process
if `shutdown_timeout_ms` expires. It also bounds signal-driven shutdown. The
heartbeat timeout is a watchdog limit, not a claim that a native call itself has
that timeout. No worker threads are forcibly cancelled and no SDK handles cross
threads. Missing SDK frames with returning polls use the shorter idle timeout;
a completely blocked native call uses the process watchdog.

After worker failure, at most `max_restarts` additional sessions are attempted,
with interruptible backoff `restart_backoff_ms × min(attempt, 10)`. The total retry
budget does not silently reset after a successful interval. A status-directory
lock prevents two service supervisors for the same status location. Shutdown
during backoff transitions to `stopped`; exhausted retries leave `degraded` and
exit nonzero. An external container/systemd supervisor may then apply its own
explicit restart policy. On Linux, each worker registers `PR_SET_PDEATHSIG=SIGKILL` before configuration
or SDK access and then verifies its expected supervisor PID, closing the
parent-died-before-registration race. Killing the supervisor therefore also
kills its worker; abrupt death does not pretend to run SDK destructors. The old
status snapshot can remain `ready` and must be rejected once its heartbeat is
stale. Deployment should still stop the complete container/cgroup.

## Recording and additional policies

Configuration changes, generation switching and recording protection are coordinated
by the management service. The capture service enforces the native restart gate,
marks session boundaries and recovers the same immutable settings. It does not
clear a recording guard or certify recorder continuity across a reopen.

Recording is disabled by default. Explicit `raw_recording` configuration enables
the optional native-count sink; completion, byte budgets and failure behavior are
defined in the [native recording contract](../contracts/native-sdk-recording-v1.md).
Additional configuration includes `quality_policy`, `quality_runtime`,
`usb_recovery` and `service.recording_control_path`; see the
[quality adapter](../contracts/service-quality-v1.md),
[USB recovery](../contracts/usb-stream-recovery-v1.md) and
[recording protection](../contracts/recording-protection-v2.md) contracts.
Managed deployments generate these fields; do not copy live bindings into examples.

## Supervision checks

Linux black-box supervision tests compile deterministic mock SDK/ROS ABI libraries
and launch the actual service binary without USB or ROS access. They cover worker
SIGSTOP → bounded watchdog kill → fresh session readiness; supervisor SIGKILL →
worker death; expected-parent PID mismatch before SDK access; SIGTERM native
handle cleanup and no surviving child; immutable config restart/refusal; and
correct/incorrect runtime artifact hashes. Run:

```sh
cargo test -p capture-probe --test service_supervision --offline -- --nocapture
```

These Linux-only tests are excluded on macOS, where live workers intentionally
refuse to start. They verify supervision mechanics, not physical camera recovery.

Checkpoint phase timing and a bounded Linux host sampler are documented in
[Recording checkpoint observation](checkpoint-observation.md).
