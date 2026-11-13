#!/usr/bin/env python3
"""Bounded single/three-writer same-filesystem release experiment, without SDK/ROS.

Build the SAME tests/writer_contention_benchmark.rs against each revision:
  cargo test --release -p offline-depth --test writer_contention_benchmark --no-run
Pass its executable as --after; optionally pass --before for alternating A/B.
Each group starts one or three isolated children at one future-time barrier.
Every child uses real 30 FPS payloads, 200 Hz accel + gyro, a modeled nonblocking
16-pair FIFO, the production checkpoint pipeline and all normal sync/readbacks.
Independent SIGKILL + read-only prefix recovery runs once per binary. This is a
synthetic storage isolation test, not capture-service or hardware acceptance.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import resource
import re
import signal
import stat
import statistics
import subprocess
import sys
import time

PAIR_BYTES = 640 * 480 * 2 + 640 * 400 * 2
TEST_NAME = "writer_contention_child"
MAX_PAIRS = 108000
OWNED_EXIT_CONFIRM_SECONDS = .05
READY_MARKER_MAX_BYTES = 4096
LIMITS = (
    "Synthetic owned-buffer FIFO, no SDK/USB/ROS or status-persistence background load. "
    "Single/three writer groups share the output filesystem; observer-selected camera count is irrelevant. "
    "Process CPU includes synthetic generation/hash and extra readers; VmHWM includes the whole child. "
    "Simultaneous group RSS is sampled, not a sum of individual peaks. /proc PID IO includes that process "
    "and its threads but page-cache/writeback attribution is incomplete; diskstats are whole-device. "
    "No cache dropping, filesystem tuning, queue increase or durability weakening. "
    "SIGKILL after a durable checkpoint tests process-crash recovery, not physical power loss. "
    "Alternation/repeats improve reproducibility, not statistical significance or proof of unique cause. "
    "Overlay/tmpfs/container results do not establish MiniPC/ext4 behavior. "
    "Before/after writer-only differences cannot establish capture-service status-path performance."
)


def sha256(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def write_json(path, value):
    # No replacement: only this run's fresh output may be written.
    with Path(path).open("x") as f:
        json.dump(value, f, indent=2, sort_keys=True)
        f.write("\n")


def mount_evidence(path, mountinfo=Path("/proc/self/mountinfo")):
    resolved = Path(path).resolve(strict=True)
    entries = []
    try:
        for line in mountinfo.read_text().splitlines():
            left, right = line.split(" - ", 1)
            fields = left.split()
            mount = fields[4].replace("\\040", " ").replace("\\011", "\t").replace("\\134", "\\")
            if resolved == Path(mount) or Path(mount) in resolved.parents:
                filesystem, source, *options = right.split()
                entries.append((len(mount), dict(mountpoint=mount, filesystem=filesystem,
                    source=source, major_minor=fields[2], mount_options=fields[5], super_options=options)))
        selected = max(entries, key=lambda x: x[0])[1] if entries else {"error": "MOUNT_NOT_FOUND"}
    except (OSError, ValueError, IndexError) as e:
        selected = {"error": type(e).__name__ + ":" + str(e)}
    selected["st_dev"] = resolved.stat().st_dev
    selected["physical_ext4_verified"] = False
    selected["limits"] = "mountinfo identifies this namespace only; ext4 name alone does not verify physical device or MiniPC"
    return selected


def process_sample(pid, proc=Path("/proc")):
    """One bounded process batch; reject PID reuse across its reads."""
    base = proc / str(pid)
    try:
        first = (base / "stat").read_text()
        fields = first[first.rfind(")") + 2:].split()
        start_ticks = int(fields[19])
        result = {"pid": pid, "start_ticks": start_ticks,
                  "state_before": fields[0],
                  "cpu_ticks": int(fields[11]) + int(fields[12]),
                  "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE")}
        try:
            result["io"] = {k: int(v) for line in (base / "io").read_text().splitlines()
                            for k, v in [line.split(":", 1)]}
        except (OSError, ValueError) as e:
            result["io_error"] = type(e).__name__
        last = (base / "stat").read_text()
        after = last[last.rfind(")") + 2:].split()
        if int(after[19]) != start_ticks:
            return {"pid": pid, "error": "PID_REUSED"}
        result["state_after"] = after[0]
        if fields[0] in ("Z", "X") or after[0] in ("Z", "X"):
            result.update(error="EXITED", terminal=True)
        return result
    except FileNotFoundError:
        return {"pid": pid, "error": "EXITED"}
    except (OSError, ValueError, IndexError) as e:
        return {"pid": pid, "error": type(e).__name__}


def owned_process_sample(process, proc=Path("/proc")):
    """Confirm only an owned child's narrowly bounded exit-adjacent proc gap.

    This is not a retry or replacement for missing IO evidence. A live process's
    gap remains a diagnostic failure; identity/schema failures are never waived.
    No numeric PID is read after Popen reaped that child.
    """
    if process.poll() is not None:
        return {"pid": process.pid, "error": "EXITED", "terminal": True, "reaped": True}
    sample = process_sample(process.pid, proc)
    if sample.get("terminal") is True:
        return sample
    exit_errors = {"EXITED", "FileNotFoundError", "ProcessLookupError", "PermissionError"}
    error = sample.get("error")
    if error is not None and error not in exit_errors:
        return sample
    if error not in exit_errors and sample.get("io_error") not in exit_errors:
        return sample
    started = time.monotonic_ns()
    confirmation = {"start_monotonic_ns": started,
                    "limit_ns": round(OWNED_EXIT_CONFIRM_SECONDS * 1e9),
                    "scope": "owned child exit boundary only; unavailable proc/IO data are not filled in"}
    try:
        returncode = process.wait(timeout=OWNED_EXIT_CONFIRM_SECONDS)
    except subprocess.TimeoutExpired:
        exited = False
        returncode = None
    else:
        exited = True
    ended = time.monotonic_ns()
    elapsed = ended - started
    confirmation.update(end_monotonic_ns=ended, elapsed_ns=elapsed, returncode=returncode)
    if not exited:
        confirmation["state"] = "still-live-at-bound"
    elif elapsed > confirmation["limit_ns"]:
        # A scheduling pause may make a nominally bounded wait return much later.
        # Reaping proves exit, but cannot exempt the earlier proc gap in that case.
        confirmation["state"] = "exit-confirmation-overrun"
        sample["reaped"] = True
    else:
        confirmation["state"] = "owned-child-exit-confirmed"
        if error is not None:
            sample["proc_error_before_exit_confirmation"] = error
        sample.update(error="EXITED", terminal=True, reaped=True)
    sample["exit_confirmation"] = confirmation
    return sample


def disk_sample(proc=Path("/proc")):
    try:
        # All devices retained to avoid guessing which dm/partition is physical.
        return {v[2]: {"major": int(v[0]), "minor": int(v[1]),
                       "read_ops": int(v[3]), "read_sectors": int(v[5]),
                       "write_ops": int(v[7]), "write_sectors": int(v[9]),
                       "io_ms": int(v[12]), "weighted_io_ms": int(v[13])}
                for line in (proc / "diskstats").read_text().splitlines()
                if len(v := line.split()) >= 14 and not v[2].startswith(("loop", "ram", "zram"))}
    except (OSError, ValueError, IndexError) as e:
        return {"error": type(e).__name__}


def output_size(root):
    total = 0
    for path in root.rglob("*"):
        try:
            metadata = path.stat()
        except FileNotFoundError:
            # Atomic checkpoint publication may rename/unlink a temporary entry
            # between scandir and stat. Planned writer credits remain enforced.
            continue
        if stat.S_ISREG(metadata.st_mode):
            total += metadata.st_size
    return total


def stop_group(process):
    """Signal only the child process group created by this tool."""
    if process.poll() is None:
        signal_group(process, signal.SIGTERM)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            signal_group(process, signal.SIGKILL)
            process.wait(timeout=5)


def signal_group(process, sig):
    # A live or unreaped Popen child cannot have its PID reused. Never signal an
    # unrelated group if a child changed its process-group membership.
    if process.poll() is not None:
        return False
    try:
        if os.getpgid(process.pid) != process.pid:
            raise RuntimeError("OWN_PROCESS_GROUP_IDENTITY_REJECTED")
        os.killpg(process.pid, sig)
        return True
    except ProcessLookupError:
        return False


def child_environment(root, pairs, mode, barrier=None):
    # No inherited fault injection/preload or implicit benchmark settings.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("GDP_IO_", "GDP_CONTENTION_")) and k != "LD_PRELOAD"}
    env.update(GDP_CONTENTION_ROOT=str(root), GDP_CONTENTION_PAIRS=str(pairs),
               GDP_CONTENTION_MODE=mode)
    if barrier is not None:
        env["GDP_CONTENTION_BARRIER"] = str(barrier)
    return env


def launch(binary, root, pairs, mode, barrier=None):
    # Payload extents are <16 MiB. Index/log files also have a hard per-file cap;
    # aggregate usage is checked every second in addition to writer byte credits.
    file_limit = pairs * 32768 + 16 * 1024 * 1024
    def limits():
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, file_limit))
    log = (root / (mode + ".log")).open("xb")
    try:
        process = subprocess.Popen([str(binary), "--exact", TEST_NAME, "--ignored",
                                    "--nocapture", "--test-threads=1"],
            env=child_environment(root, pairs, mode, barrier), stdout=log,
            stderr=subprocess.STDOUT, start_new_session=True, preexec_fn=limits)
    finally:
        log.close()
    return process


def guard_budget(root, max_bytes, deadline):
    if time.monotonic() >= deadline:
        raise RuntimeError("WALL_TIME_LIMIT")
    observed = output_size(root)
    if observed > max_bytes:
        raise RuntimeError(f"OUTPUT_BYTE_LIMIT:observed={observed}:limit={max_bytes}")


def ready_marker(path):
    """Only handshake callers may wait for a live child's create/write window."""
    try:
        with path.open("rb") as f:
            raw = f.read(READY_MARKER_MAX_BYTES + 1)
    except FileNotFoundError:
        return False, None
    if len(raw) > READY_MARKER_MAX_BYTES:
        raise ValueError("READY_MARKER_BYTE_LIMIT")
    try:
        return True, json.loads(raw)
    except json.JSONDecodeError:
        return False, None


def run_batch(binary, root, count, pairs, output, max_bytes, deadline, sample_interval=.2, proc_root=Path("/proc")):
    root.mkdir()
    children = []
    child_roots = []
    sample_count = 0
    rss_peak = None
    previous_ns = None
    gap_max_ns = 0
    sample_errors = set()
    resource_gaps = {}
    try:
        for index in range(count):
            child_root = root / f"writer-{index}"
            child_root.mkdir()
            child_roots.append(child_root)
            children.append(launch(binary, child_root, pairs, "run", root / "start.json"))
        ready_deadline = min(deadline, time.monotonic() + 30)
        ready = set()
        while len(ready) != count:
            for process, path in zip(children, child_roots):
                if process.poll() is not None:
                    raise RuntimeError("CHILD_EXITED_BEFORE_BARRIER")
                if process.pid in ready:
                    continue
                available, value = ready_marker(path / "ready.json")
                if not available:
                    continue
                if type(value) is not dict or type(value.get("pid")) is not int:
                    raise ValueError("BARRIER_READY_SCHEMA")
                if value["pid"] != process.pid:
                    raise RuntimeError("BARRIER_PID_MISMATCH")
                ready.add(process.pid)
            if len(ready) == count:
                break
            guard_budget(output, max_bytes, ready_deadline)
            time.sleep(.01)
        target = time.monotonic_ns() + 200_000_000
        write_json(root / "start.tmp", {"start_monotonic_ns": target,
                                       "start_unix_ns": time.time_ns() + 200_000_000})
        (root / "start.tmp").rename(root / "start.json")
        with (root / "resources.jsonl").open("x") as log:
            next_disk = 0
            next_budget = 0
            resource_log_bytes = 0
            resource_log_limit = max(8 * 1024 * 1024, pairs // 30 * 65536)
            while True:
                now = time.monotonic_ns()
                # Do not follow a numeric PID after its owned child has been
                # reaped; it may already identify a different process.
                processes = [owned_process_sample(p, proc_root) for p in children]
                valid = [p for p in processes if "error" not in p]
                row = {"monotonic_ns": now, "unix_ns": time.time_ns(), "processes": processes,
                       "simultaneous_rss_bytes": sum(p["rss_bytes"] for p in valid),
                       "all_children_live": len(valid) == count}
                if now >= next_disk:
                    row["disks"] = disk_sample(proc_root)
                    if not row["disks"] or "error" in row["disks"]:
                        key = "diskstats:" + str(row["disks"].get("error", "EMPTY"))
                        resource_gaps[key] = resource_gaps.get(key, 0) + 1
                    next_disk = now + 1_000_000_000
                encoded = json.dumps(row, separators=(",", ":")) + "\n"
                if resource_log_bytes + len(encoded.encode()) > resource_log_limit:
                    raise RuntimeError("RESOURCE_LOG_BYTE_LIMIT_BEFORE_WRITE")
                log.write(encoded)
                resource_log_bytes += len(encoded.encode())
                sample_count += 1
                if len(valid) == count:
                    rss_peak = max(rss_peak or 0, row["simultaneous_rss_bytes"])
                if previous_ns is not None:
                    gap_max_ns = max(gap_max_ns, now - previous_ns)
                previous_ns = now
                sample_errors.update(p["error"] for p in processes if p.get("error") not in (None, "EXITED"))
                for sample in processes:
                    error = sample.get("error")
                    # Only the bounded owned-child check (or verified Z/X)
                    # exempts this sample. A later unrelated poll must not erase
                    # a gap observed while the child was still live at the bound.
                    finished = sample.get("terminal") is True
                    if error is not None and (error != "EXITED" or not finished):
                        key = "process:" + error
                        resource_gaps[key] = resource_gaps.get(key, 0) + 1
                    if sample.get("io_error") is not None and not finished:
                        key = "process_io:" + sample["io_error"]
                        resource_gaps[key] = resource_gaps.get(key, 0) + 1
                if time.monotonic() >= deadline:
                    raise RuntimeError("WALL_TIME_LIMIT")
                if now >= next_budget:
                    guard_budget(output, max_bytes, deadline)
                    next_budget = now + 1_000_000_000
                if all(p.poll() is not None for p in children):
                    break
                time.sleep(sample_interval)
        results = []
        for p, path in zip(children, child_roots):
            if p.returncode != 0:
                raise RuntimeError(f"CHILD_FAILED:{path.name}:{p.returncode}")
            results.append(json.loads((path / "result.json").read_text()))
        return {"writers": results, "simultaneous_rss_peak_bytes": rss_peak,
                "resource_samples": sample_count, "resource_gap_max_ns": gap_max_ns,
                "resource_log_bytes": resource_log_bytes, "resource_log_limit_bytes": resource_log_limit,
                "resource_sample_errors": sorted(sample_errors),
                "resource_gaps": resource_gaps,
                "resource_diagnostic_complete": rss_peak is not None and not resource_gaps,
                "start_skew_ns": max(r["actual_start_monotonic_ns"] for r in results) - min(r["actual_start_monotonic_ns"] for r in results)}
    finally:
        for process in children:
            stop_group(process)


def validate_result(row, pairs):
    if row.get("ok") is not True or row.get("pairs") != pairs or row.get("written_pairs") != pairs:
        raise ValueError("RECORDING_FAILED:" + str(row.get("reason") or row.get("writer_error")))
    if row.get("queue_capacity") != 16 or not 1 <= row.get("queue_peak", 0) <= 16 or row.get("overflow") != 0:
        raise ValueError("QUEUE_CONTRACT")
    if row.get("fps") != 30 or row.get("imu_hz_per_stream") != 200 or row.get("status_background_load") is not False:
        raise ValueError("WORKLOAD_CONTRACT")
    d = row["diagnostics"]
    if type(d.get("failed_calls")) is not dict or d["failed_calls"]:
        raise ValueError("DISK_STAGE_FAILURE_CONTRACT")
    if (d["durable_pairs"] != pairs or d["validation_image_bytes"] != 2 * PAIR_BYTES * pairs
            or d["failed_calls"] or d["uncommitted_pairs_peak"] > 32
            or d["uncommitted_payload_bytes_peak"] > 16 * 1024 * 1024):
        raise ValueError("DURABILITY_OR_CREDIT_CONTRACT")
    r = row["independent_readback"]
    if (type(r["passes"]) is not int or r["passes"] != 2
            or r["pairs_per_pass"] != [pairs, pairs]
            or r["per_frame_color_depth_evidence_hashes"] is not True):
        raise ValueError("READBACK_CONTRACT")
    artifacts = row.get("artifact_sha256")
    expected = {"snapshot.json", "records.jsonl", "checkpoint.json", "manifest.json", "COMPLETE"}
    valid_digest = lambda value: type(value) is str and re.fullmatch("[a-f0-9]{64}", value) is not None
    if (not valid_digest(row.get("input_sha256")) or type(artifacts) is not dict
            or set(artifacts) != expected or not all(valid_digest(value) for value in artifacts.values())):
        raise ValueError("SHA256_CONTRACT")
    for key in ("cpu_seconds", "cpu_cores", "peak_rss_kib", "append_p99_ns", "append_max_ns",
                "queue_wait_p99_ns", "producer_late_max_ns"):
        value = row.get(key)
        if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
            raise ValueError("PERFORMANCE_NUMBER_CONTRACT:" + key)
    return row["input_sha256"], row["artifact_sha256"]


def run_recovery(binary, root, output, max_bytes, deadline):
    root.mkdir()
    child = launch(binary, root, 32, "crash")
    try:
        while True:
            if child.poll() is not None:
                raise RuntimeError("CRASH_CHILD_EXITED_BEFORE_CHECKPOINT")
            available, ready = ready_marker(root / "recovery-ready.json")
            if available:
                if (type(ready) is not dict or type(ready.get("pairs")) is not int
                        or type(ready.get("pid")) is not int
                        or ready != {"pairs": 32, "pid": child.pid}):
                    raise RuntimeError("RECOVERY_PID_OR_PREFIX_MISMATCH")
                break
            guard_budget(output, max_bytes, deadline)
            time.sleep(.02)
        if not signal_group(child, signal.SIGKILL):
            raise RuntimeError("CRASH_CHILD_EXITED_BEFORE_SIGKILL")
        child.wait(timeout=5)
        if child.returncode != -signal.SIGKILL:
            raise RuntimeError("SIGKILL_NOT_OBSERVED")
    finally:
        stop_group(child)
    partial = root / "capture.partial"
    before = {str(p.relative_to(partial)): sha256(p) for p in partial.rglob("*") if p.is_file()}
    recovery = launch(binary, root, 32, "recover")
    try:
        while recovery.poll() is None:
            guard_budget(output, max_bytes, deadline)
            time.sleep(.02)
        if recovery.returncode != 0:
            raise RuntimeError("RECOVERY_READER_FAILED")
    finally:
        stop_group(recovery)
    after = {str(p.relative_to(partial)): sha256(p) for p in partial.rglob("*") if p.is_file()}
    result = json.loads((root / "recovery-result.json").read_text())
    if before != after or (root / "capture/COMPLETE").exists() or (partial / "COMPLETE").exists():
        raise ValueError("RECOVERY_MUTATED_INPUT_OR_PUBLISHED_COMPLETE")
    if result["passes"] != 2 or result["pairs_per_pass"] != [32, 32]:
        raise ValueError("RECOVERY_READBACK_CONTRACT")
    if result.get("ordinary_reader_rejected") is not True:
        raise ValueError("RECOVERY_ORDINARY_READER_CONTRACT")
    return {"ok": True, "signal": "SIGKILL", "after_explicit_durable_checkpoint": True,
            "source_unchanged_sha256": before, "readback": result,
            "limits": "independent process-crash test; not an arbitrary kill point or physical power cut"}


def estimate_bytes(pairs, repeats, variants):
    # Each production writer enforces this budget; reserve observer/log overhead.
    per_writer = pairs * (PAIR_BYTES + 32768) + 16 * 1024 * 1024
    recovery = 32 * (PAIR_BYTES + 32768) + 16 * 1024 * 1024
    # Sample logs are streamed rather than retained in observer memory.
    log_reserve = max(8 * 1024 * 1024, pairs // 30 * 65536)
    return variants * (repeats * (4 * per_writer + 2 * log_reserve) + recovery) + 64 * 1024 * 1024


def summarize_groups(rows, variants):
    groups = []
    for variant in variants:
        for count in (1, 3):
            selected = [r for r in rows if r["variant"] == variant and r["writer_count"] == count]
            if not selected:
                continue
            rss = [r["simultaneous_rss_peak_bytes"] for r in selected if r["simultaneous_rss_peak_bytes"] is not None]
            processes = []
            for row in selected:
                for child in row["writers"]:
                    report = {k: child.get(k) for k in ("cpu_seconds", "cpu_cores", "peak_rss_kib", "append_p99_ns", "append_max_ns", "queue_peak", "overflow", "queue_wait_p99_ns", "producer_late_max_ns")}
                    report["verification_ns"] = child.get("diagnostics", {}).get("verification", {}).get("total_ns")
                    readback = child.get("independent_readback")
                    report["extra_two_readers_ns"] = readback.get("wall_ns") if isinstance(readback, dict) else None
                    processes.append(report)
            groups.append({"variant": variant, "writer_count": count, "runs": len(selected),
                "median_group_peak_rss_bytes": statistics.median(rss) if rss else None,
                "group_rss_observed_runs": len(rss),
                "resource_diagnostic_complete": len(rss) == len(selected) and all(r.get("resource_diagnostic_complete", False) for r in selected),
                "resource_diagnostics_by_run": [{
                    "directory": row.get("directory"),
                    "resource_gaps": dict(sorted(row.get("resource_gaps", {}).items())[:16]),
                    "resource_gap_reasons_omitted": max(0, len(row.get("resource_gaps", {})) - 16),
                    "resource_sample_errors": row.get("resource_sample_errors", [])[:16],
                } for row in selected],
                "per_process_rows": processes})
    return groups


def run(args):
    if sys.platform != "linux":
        raise ValueError("LINUX_REQUIRED")
    if not 1 <= args.seconds <= 3600 or not 3 <= args.repeats <= 10:
        raise ValueError("seconds must be 1..3600 and repeats 3..10")
    pairs = args.seconds * 30
    binaries = {"after": args.after.resolve(strict=True)}
    if args.before is not None:
        binaries["before"] = args.before.resolve(strict=True)
    if any(not p.is_file() or not os.access(p, os.X_OK) for p in binaries.values()):
        raise ValueError("EXECUTABLE_BINARY_REQUIRED")
    if not 1 <= args.max_wall_seconds <= 86400 or not 128 * 1024 * 1024 <= args.max_output_bytes <= 1024 ** 4:
        raise ValueError("explicit bounded wall/capacity required")
    estimate = estimate_bytes(pairs, args.repeats, len(binaries))
    if estimate > args.max_output_bytes:
        raise ValueError(f"PLANNED_CAPACITY_EXCEEDS_LIMIT:{estimate}")
    output = args.output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("FRESH_OUTPUT_REQUIRED")
    output.parent.mkdir(parents=True, exist_ok=True)
    if os.statvfs(output.parent).f_bavail * os.statvfs(output.parent).f_frsize < estimate:
        raise ValueError("INSUFFICIENT_FREE_SPACE")
    output.mkdir(mode=0o700)
    output = output.resolve()
    source = Path(__file__).resolve().parents[2] / "crates/offline-depth/tests/writer_contention_benchmark.rs"
    manifest = {"kind": "writer-contention-ab-v1", "uname": list(platform.uname()),
        "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
        "filesystem": mount_evidence(output), "output": str(output),
        "seconds_per_recording": args.seconds, "pairs_per_writer": pairs,
        "fps": 30, "imu_hz_per_stream": 200, "queue_capacity": 16,
        "checkpoint_credit_pairs": 32, "checkpoint_credit_bytes": 16 * 1024 * 1024,
        "repeats": args.repeats, "counts": [1, 3], "status_background_load": False,
        "binaries": {k: {"path": str(p), "sha256": sha256(p)} for k, p in binaries.items()},
        "tool_sha256": sha256(__file__), "common_driver_sha256": sha256(source),
        "max_wall_seconds": args.max_wall_seconds, "max_output_bytes": args.max_output_bytes,
        "barrier_clock": "CLOCK_MONOTONIC across children in the same Linux time namespace; Unix time is display only",
        "capacity_guard": "planned writer byte credits include payload/records/checkpoint/failed partials/recovery; hard per-file RLIMIT_FSIZE; before-write resource-log cap; whole-tree watchdog at <=1s. No filesystem project quota: unexpected output may exceed the observed aggregate threshold before termination, and actual excess is reported",
        "planned_upper_bound_bytes": estimate, "limits": LIMITS}
    write_json(output / "manifest.json", manifest)
    rows = []
    recovery = {}
    error = None
    deadline = time.monotonic() + args.max_wall_seconds
    expected = None
    try:
        for repeat in range(args.repeats):
            counts = (1, 3) if repeat % 2 == 0 else (3, 1)
            variants = list(binaries) if repeat % 2 == 0 else list(reversed(binaries))
            for count in counts:
                for variant in variants:
                    name = f"round-{repeat:02d}-writers-{count}-{variant}"
                    row = run_batch(binaries[variant], output / name, count, pairs,
                                    output, args.max_output_bytes, deadline)
                    row.update(repeat=repeat, writer_count=count, variant=variant, directory=name)
                    rows.append(row)
                    # Preserve a failed group's raw child results before rejecting it.
                    write_json(output / (name + ".json"), row)
                    for child in row["writers"]:
                        if child.get("common_driver_sha256") != manifest["common_driver_sha256"]:
                            raise ValueError("COMMON_DRIVER_HASH_MISMATCH")
                        fingerprints = validate_result(child, pairs)
                        if expected is None:
                            expected = fingerprints
                        if expected != fingerprints:
                            raise ValueError("SAME_PAYLOAD_OR_METADATA_HASH_MISMATCH")
                    if not row["resource_diagnostic_complete"]:
                        raise ValueError("RESOURCE_DIAGNOSTIC_INCOMPLETE")
        for variant, binary in binaries.items():
            recovery[variant] = run_recovery(binary, output / ("recovery-" + variant), output,
                                             args.max_output_bytes, deadline)
    except Exception as e:
        error = type(e).__name__ + ":" + str(e)
    groups = summarize_groups(rows, binaries)
    summary = {"ok": error is None, "error": error, "completed_groups": len(rows),
               "planned_groups": args.repeats * 2 * len(binaries), "groups": groups,
               "recovery": recovery, "limits": LIMITS, "hardware_acceptance_verified": False,
               "output_bytes_before_summary_and_sha256": output_size(output),
               "observed_aggregate_overrun_bytes": max(0, output_size(output) - args.max_output_bytes)}
    write_json(output / "summary.json", summary)
    with (output / "SHA256SUMS").open("x") as f:
        for p in sorted(output.rglob("*")):
            if p.is_file() and p.name != "SHA256SUMS":
                f.write(f"{sha256(p)}  {p.relative_to(output)}\n")
    return summary


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--after", type=Path, required=True, help="new/common release test executable")
    p.add_argument("--before", type=Path, help="optional baseline executable built with identical common driver")
    p.add_argument("--output", type=Path, required=True, help="fresh directory on the filesystem under test")
    p.add_argument("--seconds", type=int, default=10, help="30 FPS recording seconds per writer, 1..3600")
    p.add_argument("--repeats", type=int, default=3, help="3..10; alternating single/three and before/after order")
    p.add_argument("--max-output-bytes", type=int, default=8 * 1024 ** 3)
    p.add_argument("--max-wall-seconds", type=int, default=900)
    args = p.parse_args()
    try:
        result = run(args)
    except (ValueError, OSError) as e:
        p.exit(2, str(e) + "\n")
    print(json.dumps({k: result[k] for k in ("ok", "error", "completed_groups", "planned_groups")}, indent=2))
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
