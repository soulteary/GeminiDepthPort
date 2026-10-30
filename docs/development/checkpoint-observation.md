# Recording checkpoint observation

`raw_recording.checkpoint_observation` is an optional boolean, default `false`.
Set `"checkpoint_observation": true` inside the existing `raw_recording` object
for the diagnostic capture run. Library callers pass
`RawRecordingDiagnostics::with_checkpoint_observation(true)` to the writer.
It adds bounded, runtime-only diagnostics; it does not change the recording
format or the durability protocol. Keep it `false` before starting a separate
A/B run to bypass the additional clocks and observation locks. Existing aggregate
I/O counters remain enabled. Validate the entire config with
`capture-service --config /absolute/capture.json --validate-config`.

The status field is `raw_recording.disk_stages.checkpoint_observation` (version 1).
It reports index buffer flush; payload, index and directory fsync; checkpoint
preparation; verifier dispatch/join; prefix readback; checkpoint file write and
fsync; atomic replacement; checkpoint directory fsync; final whole-recording
readback. Each completed span has a monotonic start, elapsed wall time, thread CPU
time, process/thread IDs, checkpoint ID, prefix pair count and success flag.
`info.payload_bytes` is the sealed chunk size including evidence; it is `null`
for final whole-recording verification. It is not total image bytes or bandwidth.
Linux IDs are in the worker's PID namespace. Unsupported clock/ID fields are null.

Histograms accumulate per stage; exact recent events are bounded to 64 and active
spans to 4. Overwrites and active-limit hits are explicit. Completed event IDs are
assigned on completion, so concurrent verification cannot hide a late completion.
The operation runs outside the observation lock. Report serialization also runs
outside that lock. Timing includes some instrumentation/scheduling overhead.
Stages can overlap and **must not be summed** as end-to-end latency. A sync wall
span is a syscall/scheduling measurement, not physical disk latency; thread CPU
helps distinguish computation from blocked time. No per-frame event log is written.

V3 still performs actual prefix readback and final whole-recording readback,
including the existing per-frame SHA256 checks. Checkpoint acknowledgement still
follows all sync and replacement barriers. Existing recovery validates the durable
prefix and ignores uncommitted tails. Legacy files-v2 retains its existing final
verification; these diagnostics do not add a second V2 pass.

## Linux host sampling

Run the standard-library Python script on the **Linux Docker host**, with read
access to the private status files, worker `/proc` entries and Docker metadata.
Use the configured `service.status_path` inside each container, not a host guess.
The example status path below comes from the standalone service example; replace
container names, status paths, disk name and output path with actual values.

```bash
sudo python3 scripts/validation/checkpoint_probe.py \
  --target camera-a:/tmp/capture/status.json \
  --target camera-b:/tmp/capture/status.json \
  --target camera-c:/tmp/capture/status.json \
  --disk nvme0n1 \
  --seconds 180 --interval 0.2 --thread-interval 1 \
  --output /var/tmp/gdp-checkpoints-run-01
```

Start after readiness, before the checkpoint interval of interest, and keep the
capture running until the script ends. Ctrl-C stops only the sampler and its own
optional perf process group. It does not stop capture. The script does not edit
configs, restart containers, access the SDK, drop caches or change kernel settings.
Choose a new output directory on each run; existing directories are refused.
Outputs are private (directory 0700, files 0600), capped and never uploaded.
Default limits: 256 MiB JSONL, 1 hour, 8 targets/disks, at most 100,000 ticks.
Only requested devices are sampled. Use `findmnt -T /actual/recording/directory`
and `lsblk` to identify the storage stack. Avoid counting both a mapped device
and its underlying disk as independent traffic.

`samples.jsonl` contains process CPU ticks/I/O/scheduling counters, bounded thread
CPU samples, cgroup-v2 CPU/memory/I/O counters, queue depth/high-water marks,
active spans and requested `/proc/diskstats` counters/rates. Device request mean
latency and utilization cover the **whole selected device**, including other
processes; they are not a request P99 or the duration of a particular fsync.
Anonymous/file cache bytes can be inspected separately in cgroup `memory.stat`.
Missing cgroup-v2 files are null; missing requested disks are listed explicitly.

Completed `checkpoint_event` rows carry their own start/duration plus the sample
observation time. Correlate the event interval with nearby `sample` rows' queue,
CPU and disk rates. Status is a heartbeat snapshot, so repeated reads do not
increase its temporal resolution. `summary.json` records sampler CPU, child CPU,
RSS, output size, observation support, initial historical omissions and event
loss after sampling began. Nonzero loss means the diagnostic ring was overwritten
between observations; it does not mean recording payload loss. Shorten the status
heartbeat/sample interval in a separate validated run if necessary. A nonzero exit
reports errors, post-start event loss or failed requested perf acquisition; retain
and inspect the summary even when sampling exits nonzero.

For a non-container service use `--host-status /absolute/status.json --pid WORKER_PID`
instead of `--target`. Docker worker resolution checks PID namespaces and process
start times. PID reuse/container replacement fails closed; start a new sampler
for a new container. Linux time namespaces can offset monotonic clocks: verify the
worker anchor (`anchor_monotonic_ns`, `anchor_unix_ns`) against sample clock pairs
before overlaying raw timestamps. Avoid wall-clock alignment across NTP jumps.

## Optional CPU stacks

Add these arguments to the same command when host `perf` is installed and the
host's existing perf permissions allow attaching to the workers:

```bash
--perf --perf-after 90 --perf-seconds 30 --perf-frequency 49 --perf-max-mib 64
```

This requests software `cpu-clock` events, monotonic timestamps and 4096-byte
DWARF call chains, scoped to the resolved worker PIDs. The perf file and stderr
are each limited to the requested file size; this is separate from the JSONL
limit. The script does not elevate privileges or relax `perf_event_paranoid`.
A missing/denied perf leaves the other samples available and returns nonzero.
`recorded_not_validated` means only that perf returned success and a nonempty file
exists; inspect it before drawing conclusions:

```bash
perf report --stdio -i /var/tmp/gdp-checkpoints-run-01/perf.data
perf script --ns -i /var/tmp/gdp-checkpoints-run-01/perf.data
```

Preserve matching worker binaries and symbols. Container paths may need a suitable
`--symfs` or `/proc/CONTAINER_INIT_PID/root` while the container is alive. Match perf
host TIDs to the script's namespace-TID mapping, then to checkpoint event TIDs.
Stacks can contain private addresses, paths and stack bytes: keep artifacts local.
The sampler does not automatically export container binaries or configuration.

For overhead acceptance, alternate equal-duration recording runs with observation
disabled, enabled, enabled plus basic sampling, then enabled plus perf. Keep SDK,
profiles, IMU, queue/checkpoint settings, disks, cache state policy and recorder
load equal. Compare acquisition CPU, append histogram bounds, checkpoint/final
verification time, throughput, queue high-water and memory; include sampler and
perf CPU separately. The accompanying synthetic report measures basic sampling;
real-camera/perf acceptance must use the actual Linux host and storage load.
