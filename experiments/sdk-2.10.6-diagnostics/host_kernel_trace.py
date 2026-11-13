#!/usr/bin/env python3
"""Independent bounded host tracefs observer for the unchanged SDK retest.

This is not writer_syscall_trace.TraceSession and does not relax its guards.
The caller starts this observer before Popen(run_retest), polls failure at least
every 0.2 s, and retains the original driver's writer cleanup on trace failure.
It calls finish after that cleanup. This module never signals a target or uses Docker.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import select
import stat
import subprocess
import sys
import threading
import time
import uuid

sys.dont_write_bytecode = True
REVISION = "host-observation-v2"
SYSCALL_EVENTS = tuple("syscalls:" + prefix + call
    for call in ("fsync", "fdatasync")
    for prefix in ("sys_enter_", "sys_exit_"))
SCHED_EVENTS = ("sched:sched_switch", "sched:sched_wakeup",
    "sched:sched_process_fork", "sched:sched_process_exec", "sched:sched_process_exit")
MAX_LINE = 8192
MAX_JSON_LINE = 16 * 1024**2
MAX_CPUS = 256
RESERVE = 2 * 1024**2
LOSS_KEYS = ("overrun", "commit overrun", "dropped events")
SCOPE = (
    "Independent host-namespace PID-filtered syscall/scheduler trace, with a second "
    "device-filtered ext4/jbd2/block instance. Host events follow pinned writer TIDs and forks. Raw trace may "
    "contain unrelated task names, paths and syscall pointer values and must "
    "remain private. Only subsequently proven host TID generations may be "
    "associated with writers. Device overlap is not unique SSD attribution. "
    "Read syscalls are disabled to prevent trace_pipe reader self-excitation. "
    "Each writer owns its own credit limits; no cross-writer credit pool.")


def stream_sha(stream):
    digest=hashlib.sha256()
    for block in iter(lambda:stream.read(1024**2),b""):digest.update(block)
    return digest.hexdigest()


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def text(path, limit=65536):
    with Path(path).open("rb") as stream:
        raw = stream.read(limit + 1)
    require(len(raw) <= limit, "INPUT_LIMIT:" + str(path))
    return raw.decode("utf-8", "strict")


def root_ring_upper_kib(raw):
    """Accept only tracefs decimal sizing, including its lazy-expansion form."""
    match = re.fullmatch(r"([0-9]+)(?: \(expanded: ([0-9]+)\))?\n?", raw)
    require(match is not None, "ROOT_RING_SIZE_UNAVAILABLE")
    return max(int(match[1]), int(match[2] or match[1]))


def document(path, limit=2 * 1024**2):
    return json.loads(text(path, limit))


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def file_sha(path):
    with Path(path).open("rb") as stream:
        return stream_sha(stream)


def canonical(path):
    path = Path(path)
    require(path.is_absolute() and path == path.resolve(), "NONCANONICAL_OR_SYMLINK_PATH:" + str(path))
    return path


def proc_record(path):
    raw = text(path / "stat", 16384)
    fields = raw[raw.rfind(")") + 2:].split()
    require(len(fields) > 21, "PROC_STAT_SCHEMA")
    return {"tid": int(path.name), "start_ticks": int(fields[19]),
            "state": fields[0], "user_ticks": int(fields[11]),
            "system_ticks": int(fields[12]), "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE")}


def stable_identity(path):
    first = proc_record(path)
    require(first["state"] not in ("Z", "X"), "THREAD_NOT_LIVE")
    status = text(path / "status", 32768)
    fields = {}
    for name in ("NSpid", "Tgid"):
        match = re.search(r"^" + name + r":\s*([0-9 \t]+)$", status, re.M)
        require(match is not None, "PROC_" + name + "_MISSING")
        fields[name] = list(map(int, match[1].split()))
    namespace = os.readlink(path / "ns/pid")
    cgroup = text(path / "cgroup", 16384)
    # Linux exposes timens_offsets on /proc/TID, not task/TID.
    offsets_path = Path("/proc") / path.name / "timens_offsets" if path.parent.name == "task" else path / "timens_offsets"
    offsets = text(offsets_path, 4096)
    mono = [line.split()[1:] for line in offsets.splitlines() if line.split()[:1] == ["monotonic"]]
    require(mono == [["0", "0"]], "TARGET_MONOTONIC_OFFSET_NONZERO")
    after = proc_record(path)
    require((first["tid"], first["start_ticks"]) == (after["tid"], after["start_ticks"]),
            "THREAD_IDENTITY_CHANGED")
    require(namespace == os.readlink(path / "ns/pid") and cgroup == text(path / "cgroup", 16384),
            "THREAD_NAMESPACE_OR_CGROUP_MOVED")
    return {**first, "nspid": fields["NSpid"], "tgid": fields["Tgid"][0],
            "pid_namespace": namespace, "cgroup": cgroup, "monotonic_offset_ns": 0}


def worker_terminal_status(process, proc=Path("/proc")):
    """Recheck the pinned group leader; a task/parent read race is not exit."""
    if process is None:
        return {"retire_sampling": False, "basis": "no-pinned-worker-read"}
    try:
        current = proc_record(proc / str(process["worker_host_pid"]))
    except (FileNotFoundError, ProcessLookupError):
        return {"retire_sampling": True, "basis": "pinned-worker-proc-stat-unavailable"}
    if current["start_ticks"] != process["worker_start_ticks"]:
        return {"retire_sampling": True, "basis": "numeric-pid-reused-old-worker-not-present"}
    return {"retire_sampling": current["state"] in ("Z", "X"),
            "basis": "pinned-worker-terminal" if current["state"] in ("Z", "X") else "pinned-worker-still-live"}


class Budget:
    """One synchronized aggregate limit, including all metadata and manifests."""
    def __init__(self, output, maximum):
        self.output = Path(output)
        require(16 * 1024**2 <= maximum <= 128 * 1024**3, "TOTAL_FILE_BUDGET")
        self.maximum = maximum
        self.used = 0
        self.lock = threading.Lock()
        self.streams = {}

    def write(self, name, raw, terminal=False):
        require(name == "SHA256SUMS" or re.fullmatch(r"[a-z0-9_.-]{1,80}", name) is not None, "OUTPUT_NAME")
        with self.lock:
            require(self.used + len(raw) <= self.maximum - (0 if terminal else RESERVE), "TOTAL_FILE_BYTE_LIMIT")
            if name not in self.streams:
                self.streams[name] = (self.output / name).open("xb", buffering=0)
                os.fchmod(self.streams[name].fileno(), 0o600)
            stream = self.streams[name]
            offset = 0
            while offset < len(raw):
                count = stream.write(raw[offset:])
                require(count is not None and count > 0, "OUTPUT_SHORT_WRITE")
                offset += count
                self.used += count

    def json(self, name, row, terminal=False):
        raw = (json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n").encode()
        require(len(raw) <= MAX_JSON_LINE, "JSON_LINE_LIMIT")
        self.write(name, raw, terminal)

    def close(self):
        for stream in self.streams.values():
            stream.close()
        self.streams.clear()


class Instance:
    def __init__(self, parent, name, events):
        self.parent, self.name, self.events = parent, name, events
        self.path = None
        self.fd = None
        self.fragment = b""
        self.thread = None
        self.stop_reader = threading.Event()
        self.active = False
        self.start_ns = None
        self.stop_ns = None
        self.initial = None
        self.last = None
        self.final = None
        self.stream_lost = 0
        self.stream_lost_by_cpu = {}
        self.stream_lost_unlocated = 0
        self.raw_bytes = 0
        self.lines = 0
        self.lifecycle_events = 0
        self.live_children = set()
        self.stats_error = None
        self.stop_lock = threading.Lock()
        self.cpu_ns = 0
        self.proof = None
        self.filters = {}
        self.ring_bytes = 0
        self.ring_sizing = None

    def control(self, name, value):
        require(self.path is not None and not Path(name).is_absolute()
                and ".." not in Path(name).parts, "PRIVATE_CONTROL_PATH")
        path = self.path / name
        require(not path.is_symlink(), "PRIVATE_CONTROL_SYMLINK")
        with path.open("w") as stream:
            stream.write(value + "\n")

    def configure(self):
        owner = self.parent
        # Conservative pre-allocation bound: a new instance can inherit the
        # root's configured buffer size. Never change that root configuration.
        root_cpus = list((owner.tracefs / "per_cpu").glob("cpu[0-9]*"))
        require(0 < len(root_cpus) <= MAX_CPUS, "CPU_COUNT_LIMIT")
        self.ring_sizing = {
            "root_cpu_count": len(root_cpus),
            "root_buffer_total_size_kb_raw": text(owner.tracefs / "buffer_total_size_kb", 1024),
            "root_buffer_size_kb_raw": text(owner.tracefs / "buffer_size_kb", 1024)}
        root_total = root_ring_upper_kib(self.ring_sizing["root_buffer_total_size_kb_raw"])
        root_per_cpu = root_ring_upper_kib(self.ring_sizing["root_buffer_size_kb_raw"])
        inherited = max(root_total, len(root_cpus) * root_per_cpu) * 1024
        requested = len(root_cpus) * owner.buffer_kib * 1024
        transient = max(inherited, requested)
        allocated = sum(item.ring_bytes for item in owner.instances)
        self.ring_sizing.update(
            root_total_upper_kib=root_total, root_per_cpu_upper_kib=root_per_cpu,
            root_inherited_upper_bytes=inherited, requested_per_cpu_kib=owner.buffer_kib,
            requested_private_bytes=requested, allocated_before_bytes=allocated,
            transient_upper_bytes=transient, max_total_ring_bytes=owner.max_ring_bytes)
        require(allocated + transient <= owner.max_ring_bytes,
                "AGGREGATE_RING_MEMORY_LIMIT_BEFORE_INSTANCE")
        self.path = owner.tracefs / "instances" / ("gdp-host-v2-" + self.name + "-" + uuid.uuid4().hex)
        self.path.mkdir(mode=0o700)
        self.control("tracing_on", "0")
        self.control("events/enable", "0")
        self.control("current_tracer", "nop")
        self.control("trace_clock", "mono")
        require("[mono]" in text(self.path / "trace_clock"), "MONO_CLOCK_REJECTED")
        cpus = list((self.path / "per_cpu").glob("cpu[0-9]*"))
        require(0 < len(cpus) <= MAX_CPUS, "CPU_COUNT_LIMIT")
        requested = len(cpus) * owner.buffer_kib * 1024
        allocated = sum(item.ring_bytes for item in owner.instances if item is not self)
        require(allocated + requested <= owner.max_ring_bytes, "AGGREGATE_RING_MEMORY_LIMIT_BEFORE_WRITE")
        self.control("buffer_size_kb", str(owner.buffer_kib))
        self.ring_sizing["private_buffer_total_size_kb_raw"] = text(self.path / "buffer_total_size_kb", 1024)
        total = self.ring_sizing["private_buffer_total_size_kb_raw"].strip()
        require(total.isdigit(), "RING_TOTAL_UNAVAILABLE")
        self.ring_bytes = int(total) * 1024
        self.ring_sizing["private_verified_bytes"] = self.ring_bytes
        require(sum(item.ring_bytes for item in owner.instances) <= owner.max_ring_bytes,
                "AGGREGATE_RING_MEMORY_LIMIT")
        self.control("options/overwrite", "0")
        self.control("options/record-tgid", "0")
        self.control("options/event-fork", "0")
        self.control("set_event_pid", "")
        self.control("set_event_notrace_pid", "")
        require(not text(self.path / "set_event_pid").strip()
                and not text(self.path / "set_event_notrace_pid").strip(), "HOST_TRACE_PID_FILTER")
        require(threading.get_native_id() == os.getpid(), "SETUP_MUST_BE_MAIN_THREAD")
        token = "gdp-host-pid-proof-" + uuid.uuid4().hex
        self.control("options/markers", "1")
        self.control("trace", "")
        self.control("tracing_on", "1")
        try:
            before = time.monotonic_ns()
            self.control("trace_marker", token)
            after = time.monotonic_ns()
        finally:
            self.control("tracing_on", "0")
        raw = text(self.path / "trace", 65536)
        self.proof = owner.syscalls.marker_pid_proof(raw, token, os.getpid())
        matched = [owner.syscalls.PREFIX.match(row) for row in raw.splitlines() if token in row]
        require(len(matched) == 1 and matched[0] is not None, "MARKER_CLOCK_PROOF")
        stamp = matched[0]
        resolution = 10 ** (9 - len(stamp[4]))
        at = int(stamp[3]) * 10**9 + int(stamp[4]) * resolution
        require(before - resolution <= at <= after + resolution, "TRACE_MONO_CALIBRATION_MISMATCH")
        self.proof.update(marker_monotonic_ns=at, clock_bracket_ns=[before, after],
                          printed_resolution_ns=resolution)
        self.control("trace", "")
        for event in self.events:
            category, name = event.split(":")
            if self.name == "storage":
                dev = owner.binding["block_kernel_dev" if category == "block" else "filesystem_kernel_dev"]
                self.control(f"events/{category}/{name}/filter", f"dev == {dev}")
                actual = text(self.path / f"events/{category}/{name}/filter").strip()
                require(re.fullmatch(r"dev\s*==\s*" + str(dev), actual) is not None, "DEVICE_FILTER_REJECTED")
                self.filters[event] = actual
            self.control(f"events/{category}/{name}/enable", "1")
        self.initial = self.stats("initial")
        require(all(row[key] == 0 for row in self.initial["cpu"].values() for key in LOSS_KEYS),
                "DIRTY_INITIAL_LOSS_COUNTERS")
        self.fd = os.open(self.path / "trace_pipe", os.O_RDONLY | os.O_NONBLOCK)

    def stats(self, phase):
        started = time.monotonic_ns()
        cpus = sorted((self.path / "per_cpu").glob("cpu[0-9]*"))
        require(0 < len(cpus) <= MAX_CPUS, "CPU_COUNT_LIMIT")
        values, raw_values = {}, {}
        for cpu in cpus:
            raw = text(cpu / "stats", 16384)
            raw_values[cpu.name] = raw
            fields = {}
            for line in raw.splitlines():
                key, sep, value = line.partition(":")
                if sep:
                    key, value = key.strip(), value.strip()
                    fields[key] = int(value) if value.isdigit() else value
            require(all(type(fields.get(key)) is int for key in LOSS_KEYS), "MISSING_PER_CPU_LOSS_COUNTERS")
            values[cpu.name] = fields
        deltas = {}
        if self.last:
            require(set(self.last["cpu"]) == set(values), "CPU_SET_CHANGED")
            for cpu, fields in values.items():
                delta = {key: fields[key] - self.last["cpu"][cpu][key] for key in LOSS_KEYS}
                require(all(value >= 0 for value in delta.values()), "LOSS_COUNTER_REGRESSION")
                deltas[cpu] = delta
        row = {"kind": "trace-loss-counters", "instance": self.name, "phase": phase,
               "sample_started_monotonic_ns": started, "sample_finished_monotonic_ns": time.monotonic_ns(),
               "cpu": values, "raw_per_cpu_stats": raw_values, "delta": deltas,
               "stream_lost_events": self.stream_lost,
               "stream_lost_annotations_by_cpu": {name: self.stream_lost_by_cpu.get(name, 0) for name in values},
               "stream_lost_annotations_without_cpu": self.stream_lost_unlocated,
               "scope": "Complete exposed per-CPU counters; stream LOST annotations are separate. "
                        "Overrun, commit overrun, dropped events and annotations may overlap; do not sum as unique loss."}
        self.parent.budget.json(self.name + "-loss.jsonl", row, terminal=phase == "final")
        self.last = row
        if phase == "final":
            self.final = row
        if any(value for delta in deltas.values() for value in delta.values()):
            self.parent.fail("KERNEL_LOSS:" + self.name)
        return row

    def start(self):
        self.control("tracing_on", "1")
        if self.name == "host":
            self.control("set_event_pid", str(os.getpid()))
            self.control("options/event-fork", "1")
        self.start_ns = time.monotonic_ns()
        self.active = True
        self.thread = threading.Thread(target=self.read_loop, name="host-trace-" + self.name, daemon=True)
        self.thread.start()

    def stop_capture(self):
        with self.stop_lock:
            if self.path is not None and self.stop_ns is None:
                try:
                    self.control("tracing_on", "0")
                    self.stop_ns = time.monotonic_ns()
                except Exception as error:
                    self.parent.fail("TRACE_DISABLE_FAILED:" + self.name + ":" + str(error))
        self.active = False

    def chunk(self, raw):
        joined = self.fragment + raw
        lines = joined.split(b"\n")
        self.fragment = lines.pop()
        require(len(self.fragment) <= MAX_LINE, "RAW_LINE_LIMIT")
        complete = []
        for line in lines:
            require(len(line) <= MAX_LINE, "RAW_LINE_LIMIT")
            lost = re.search(rb"\bLOST\s+([0-9]+)\s+EVENTS\b", line)
            if lost:
                self.stream_lost += int(lost[1])
                cpu = re.search(rb"\bCPU:\s*(\d+)", line)
                if cpu:
                    require(int(cpu[1]) < MAX_CPUS, "LOSS_ANNOTATION_CPU_LIMIT")
                    name = "cpu" + str(int(cpu[1]))
                    self.stream_lost_by_cpu[name] = self.stream_lost_by_cpu.get(name, 0) + int(lost[1])
                else:
                    self.stream_lost_unlocated += int(lost[1])
                self.parent.fail("STREAM_LOST_EVENTS:" + self.name)
            if self.name == "host" and b"sched_process_fork" in line:
                self.parent.anchor_fork(line.decode("utf-8", "strict"))
            if self.name == "host" and b"sched_process_" in line:
                self.lifecycle_events += 1
                require(self.lifecycle_events <= self.parent.max_generations, "PROCESS_GENERATION_LIMIT")
                if b"sched_process_fork:" in line:
                    child = re.search(rb"\bchild_pid=(\d+)", line)
                    require(child is not None, "FORK_SCHEMA")
                    self.live_children.add(int(child[1]))
                    require(len(self.live_children) <= self.parent.max_processes, "ACTIVE_PROCESS_MAPPING_LIMIT")
                elif b"sched_process_exit:" in line:
                    child = re.search(rb"\bpid=(\d+)", line)
                    require(child is not None, "EXIT_SCHEMA")
                    self.live_children.discard(int(child[1]))
            complete.append(line + b"\n")
        data = b"".join(complete)
        if data:
            self.parent.budget.write(self.name + "-raw.trace", data)
            self.raw_bytes += len(data)
            self.lines += len(lines)

    def read_once(self):
        try:
            raw = os.read(self.fd, 65536)
        except BlockingIOError:
            return False
        if not raw:
            return False
        self.chunk(raw)
        return True

    def read_loop(self):
        started = time.thread_time_ns()
        try:
            while not self.stop_reader.is_set():
                if not self.read_once():
                    select.select([self.fd], [], [], .02)
        except Exception as error:
            self.parent.fail("READER_ERROR:" + self.name + ":" + type(error).__name__ + ":" + str(error))
        finally:
            self.cpu_ns = time.thread_time_ns() - started

    def finish(self):
        self.stop_capture()
        self.stop_reader.set()
        if self.thread:
            self.thread.join(3)
            require(not self.thread.is_alive(), "READER_STOP_TIMEOUT:" + self.name)
        try:
            if self.fd is not None:
                deadline = time.monotonic() + 2
                while self.read_once():
                    require(time.monotonic() < deadline, "FINAL_DRAIN_TIMEOUT")
                require(not self.fragment, "FINAL_PARTIAL_LINE")
        except Exception as error:
            self.parent.fail("FINAL_DRAIN_ERROR:" + self.name + ":" + str(error))
        finally:
            # Counter collection is mandatory even if a decoder/write/budget failed.
            if self.path is not None:
                try:
                    self.stats("final")
                except Exception as error:
                    self.stats_error = type(error).__name__ + ":" + str(error)
                    self.parent.fail("FINAL_COUNTER_ERROR:" + self.name + ":" + self.stats_error)
            if self.fd is not None:
                os.close(self.fd)
                self.fd = None
            if self.path is not None:
                try:
                    self.path.rmdir()
                except Exception as error:
                    self.parent.fail("INSTANCE_CLEANUP_ERROR:" + self.name + ":" + str(error))
        return {"instance": self.name, "path": str(self.path), "removed": self.path is not None and not self.path.exists(),
                "start_monotonic_ns": self.start_ns, "stop_monotonic_ns": self.stop_ns,
                "raw_bytes": self.raw_bytes, "lines": self.lines, "reader_thread_cpu_ns": self.cpu_ns,
                "kernel_ring_bytes": self.ring_bytes, "stream_lost_events": self.stream_lost,
                "ring_sizing": self.ring_sizing,
                "stream_lost_annotations_by_cpu": self.stream_lost_by_cpu,
                "stream_lost_annotations_without_cpu": self.stream_lost_unlocated,
                "complete_initial_and_final_counters": self.initial is not None and self.final is not None,
                "stats_error": self.stats_error, "pid_clock_proof": self.proof,
                "events": self.events, "filters": self.filters, "set_event_pid": "writer roots and existing threads; kernel event-fork follows descendants" if self.name == "host" else ""}


class HostKernelTrace:
    def __init__(self, source, output, recording_path, retest_root, disk=None,
                 tracefs=Path("/sys/kernel/tracing"), max_bytes=1024**3,
                 max_seconds=7200, buffer_kib=256, max_ring_bytes=64 * 1024**2,
                 max_processes=32768, max_generations=262144, expected_uuid=None, expected_revision=None):
        require(math.isfinite(max_seconds) and 1 <= max_seconds <= 86400, "TRACE_DURATION_BOUND")
        require(4 <= buffer_kib <= 8192 and 1024**2 <= max_ring_bytes <= 128 * 1024**2, "RING_BOUND")
        require(128 <= max_processes <= 65536 and max_processes <= max_generations <= 1048576, "MAPPING_BOUND")
        require(type(expected_revision) is str and re.fullmatch("[a-f0-9]{40}",expected_revision), "EXPLICIT_SOURCE_REVISION_REQUIRED")
        require(type(expected_uuid) is str and bool(expected_uuid), "EXPLICIT_FILESYSTEM_UUID_REQUIRED")
        require(type(disk) is str and bool(disk), "EXPLICIT_DISK_REQUIRED")
        self.revision = expected_revision
        self.source = Path(source).absolute()
        self.output = Path(output).absolute()
        self.recording_path = Path(recording_path).absolute()
        self.retest_root = Path(retest_root).absolute()
        self.disk, self.tracefs, self.expected_uuid = disk, Path(tracefs), expected_uuid
        self.max_seconds, self.buffer_kib, self.max_ring_bytes = max_seconds, buffer_kib, max_ring_bytes
        self.max_processes, self.max_generations = max_processes, max_generations
        self.budget = Budget(self.output, max_bytes)
        self.failure = None
        self.errors = []
        self.lock = threading.Lock()
        self.done = threading.Event()
        self.stats_thread = None
        self.identity_thread = None
        self.started_ns = None
        self.finished = None
        self.instances = []
        self.bound = {}
        self.mapping_count = 0
        self.mapping_keys = set()
        self.known_threads = {}
        self.thread_pins = {}
        self.seeded_workers = set()
        self.identity_gaps = 0
        self.identity_samples = 0
        self.terminal_workers = set()
        self.last_stats_ns = None
        self.max_stats_gap_ns = 0
        self.binding = None
        self.source_hashes = {}

    def source_proof(self):
        paths = [Path(__file__).resolve()]
        paths += [self.source / "scripts/validation" / name for name in ("writer_syscall_trace.py", "writer_storage_trace.py")]
        paths += [self.source / "experiments/sdk-2.10.6-diagnostics" / name
                  for name in ("run_retest.py", "hardware.py", "sdk_lifecycle.py", "analyze_checkpoint_tails.py")]
        return {str(path): file_sha(path) for path in paths}

    def fail(self, reason):
        with self.lock:
            if self.failure is None:
                self.failure = reason
            if len(self.errors) < 64 and reason not in self.errors:
                self.errors.append(reason[:1024])

    def start(self):
        require(self.started_ns is None and not self.output.exists(), "FRESH_TRACE_OUTPUT_REQUIRED")
        require(not self.retest_root.exists(), "TRACE_MUST_START_BEFORE_ORIGINAL_DRIVER")
        for path in (self.output, self.retest_root, self.recording_path, self.source):
            canonical(path)
        require(not self.output.is_relative_to(self.retest_root) and not self.retest_root.is_relative_to(self.output)
                and not self.output.is_relative_to(self.source) and self.recording_path.is_dir(), "TRACE_PATH_BOUNDARY")
        self.output.mkdir(mode=0o700, parents=True)
        self.started_ns = time.monotonic_ns()
        try:
            require(sys.platform == "linux" and os.geteuid() == 0, "HOST_ROOT_REQUIRED")
            git = ["git", "--no-optional-locks", "-c", "safe.directory=" + str(self.source), "-C", str(self.source)]
            require(subprocess.check_output(git + ["rev-parse", "HEAD"], timeout=30, text=True).strip() == self.revision
                    and subprocess.check_output(git + ["status", "--porcelain", "--untracked-files=all"],
                                                timeout=30, text=True) == "", "FIXED_CLEAN_SOURCE_REQUIRED")
            self.source_hashes = self.source_proof()
            diag = self.source / "experiments/sdk-2.10.6-diagnostics"
            sys.path.insert(0, str(diag))
            self.hardware = load("host_trace_original_hardware", diag / "hardware.py")
            require(os.readlink("/proc/self/ns/pid") == os.readlink("/proc/1/ns/pid"), "HOST_PID_NAMESPACE_REQUIRED")
            self.syscalls = load("host_trace_pure_syscalls", self.source / "scripts/validation/writer_syscall_trace.py")
            self.storage = load("host_trace_pure_storage", self.source / "scripts/validation/writer_storage_trace.py")
            self.syscalls.zero_monotonic_offset()
            self.syscalls.tracefs_mount(self.tracefs)
            # Read-only binding: st_dev + ext4 + direct whole-disk/partition.
            self.binding = self.storage.device_binding(self.recording_path, self.disk)
            uuid_path = Path("/dev/disk/by-uuid") / self.expected_uuid
            uuid_info = uuid_path.stat()
            require(stat.S_ISBLK(uuid_info.st_mode), "EXPECTED_UUID_NOT_BLOCK_DEVICE")
            require([os.major(uuid_info.st_rdev), os.minor(uuid_info.st_rdev)] == self.binding["filesystem_dev"],
                    "ORIGINAL_RECORDING_UUID_MISMATCH")
            self.binding.update(expected_uuid=self.expected_uuid, uuid_device=str(uuid_path.resolve()))
            available = set(text(self.tracefs / "available_events", 4 * 1024**2).split())
            events = set(SYSCALL_EVENTS + SCHED_EVENTS + self.storage.EVENTS)
            require(events <= available, "REQUIRED_TRACEPOINTS_MISSING:" + ",".join(sorted(events - available)))
            formats = {}
            for event in sorted(events):
                category, name = event.split(":")
                raw = text(self.tracefs / "events" / category / name / "format")
                formats[event] = hashlib.sha256(raw.encode()).hexdigest()
                if category in ("ext4", "jbd2", "block"):
                    require(re.search(r"field:[^;]*\bdev;\s*offset:\d+;\s*size:4;\s*signed:0;", raw),
                            "DEVICE_TRACEPOINT_SCHEMA:" + event)
            self.instances = [Instance(self, "host", SYSCALL_EVENTS + SCHED_EVENTS),
                              Instance(self, "storage", self.storage.EVENTS)]
            for instance in self.instances:
                instance.configure()
            for instance in self.instances:
                instance.start()
            self.stats_thread = threading.Thread(target=self.statistics_loop, name="host-trace-stats", daemon=True)
            self.identity_thread = threading.Thread(target=self.identity_loop, name="host-trace-identity", daemon=True)
            self.stats_thread.start()
            self.identity_thread.start()
            ready = {"ok": True, "schema": "gdp-independent-host-trace-v1", "revision": self.revision,
                "observer_pid": os.getpid(), "started_monotonic_ns": self.started_ns, "clock": "mono",
                "identity_basis": "Original hardware runtime-identity pin plus repeated proc executable/instance, "
                                  "host start_ticks, NSpid, namespace and exact container cgroup checks",
                "limits": {"max_total_file_bytes": self.budget.maximum, "terminal_reserve_bytes": RESERVE,
                    "max_seconds": self.max_seconds, "max_line_bytes": MAX_LINE,
                    "max_json_line_bytes": MAX_JSON_LINE, "max_cpus": MAX_CPUS,
                    "max_total_ring_bytes": self.max_ring_bytes, "max_processes": self.max_processes,
                    "max_lifecycle_events": self.max_generations, "max_distinct_thread_identities": self.max_generations},
                "binding": self.binding, "event_format_sha256": formats,
                "helper_and_source_sha256_before": self.source_hashes,
                "instances": [{"name": v.name, "path": str(v.path), "start_monotonic_ns": v.start_ns,
                               "ring_bytes": v.ring_bytes, "ring_sizing": v.ring_sizing,
                               "pid_clock_proof": v.proof} for v in self.instances],
                "scope": SCOPE, "original_trace_session_used": False, "source_modified": False}
            self.budget.json("ready.json", ready)
            require(self.failure is None, self.failure)
            return ready
        except BaseException as error:
            self.fail("SETUP_ERROR:" + type(error).__name__ + ":" + str(error))
            self.finish()
            raise

    def statistics_loop(self):
        deadline = time.monotonic()
        try:
            while not self.done.is_set():
                now = time.monotonic_ns()
                if self.last_stats_ns is not None:
                    gap = now - self.last_stats_ns
                    self.max_stats_gap_ns = max(self.max_stats_gap_ns, gap)
                    require(gap <= 2_000_000_000, "LOSS_SAMPLE_DEADLINE_MISSED")
                self.last_stats_ns = now
                for instance in self.instances:
                    instance.stats("periodic")
                self.budget.json("observer-resources.jsonl", {
                    "monotonic_ns": time.monotonic_ns(), "observer": proc_record(Path("/proc") / str(os.getpid())),
                    "scope": "Whole host observer process includes both readers and identity/statistics threads; cumulative ticks, not utilization."})
                require(now - self.started_ns < self.max_seconds * 1e9, "TRACE_MAXIMUM_DURATION_REACHED")
                # Stop recording on first failure; keep collecting counter receipts
                # until the original controller completes its failure cleanup.
                if self.failure:
                    for instance in self.instances:
                        instance.stop_capture()
                deadline += 1
                self.done.wait(max(0, deadline - time.monotonic()))
        except Exception as error:
            self.fail("STATISTICS_ERROR:" + type(error).__name__ + ":" + str(error))
            for instance in self.instances:
                instance.stop_capture()

    def sample_identities(self):
        roots = [(f"{mode}-{number:02d}", self.retest_root / "ab" / f"{mode}-{number:02d}" / "capture")
                 for number in range(1, 4) for mode in ("on", "off")]
        roots.append(("crash", self.retest_root / "crash"))
        for group, root in roots:
            if not (root / "run.json").exists():
                continue
            run = document(root / "run.json")
            require(len(run["cameras"]) == 3 and {v["name"] for v in run["cameras"]} == {"a", "b", "c"},
                    "THREE_CAMERA_IDENTITY_SCHEMA")
            for camera in run["cameras"]:
                label = camera["name"]
                if (group, label) in self.terminal_workers:
                    continue
                pin_path = root / label / "runtime-identity.json"
                if not pin_path.exists():
                    continue
                began = time.monotonic_ns()
                process = None
                try:
                    pin = self.hardware._read_runtime_identity(pin_path)
                    binding = document(root / label / "config/experiment.json", 16384)
                    require(pin["run_id"] == run["run_id"] == binding["run_id"]
                            and pin["image_id"] == run["image_id"] == binding["image_id"]
                            and pin["config_sha256"] == binding["capture_sha256"]
                            and pin["launch_instance_id"] == binding["instance_id"], "ORIGINAL_PIN_BINDING_CHANGED")
                    process = pin["process"]
                    worker = process["worker_host_pid"]
                    current = proc_record(Path("/proc") / str(worker))
                    if current["state"] in ("Z", "X"):
                        raise ProcessLookupError("worker-terminal")
                    root_identity = stable_identity(Path("/proc") / str(worker))
                    require(root_identity["start_ticks"] == process["worker_start_ticks"]
                            and root_identity["nspid"][-1] == pin["worker_pid"]
                            and root_identity["pid_namespace"] == process["pid_namespace"], "WORKER_PID_START_NAMESPACE_CHANGED")
                    require(re.search(r"(?:^|[/:-])" + re.escape(pin["container_id"]) + r"(?:\.scope)?(?:/|\n|$)",
                                      root_identity["cgroup"]), "EXACT_CONTAINER_CGROUP_MISSING")
                    # Reuse only the read-only, already pinned runtime process
                    # checker. Never call the helper that creates runtime pins.
                    status = {"pid": pin["worker_pid"], "instance_id": pin["instance_id"]}
                    info = {"State": {"Pid": process["container_init_pid"]}}
                    checked = self.hardware.observe_runtime_process(info, status, binding, previous=process)
                    require(checked == process, "ORIGINAL_RUNTIME_PROCESS_CHANGED")
                    thread_dir = Path("/proc") / str(worker) / "task"
                    threads = []
                    thread_races = []
                    thread_race_count = 0
                    for entry in os.scandir(thread_dir):
                        require(len(threads) < self.max_processes, "WORKER_THREAD_MAPPING_LIMIT")
                        if not entry.name.isdigit():
                            continue
                        try:
                            cheap = proc_record(Path(entry.path))
                            key = (int(entry.name), cheap["start_ticks"])
                            if key in self.known_threads:
                                continue
                            row = stable_identity(Path(entry.path))
                        except (FileNotFoundError, ProcessLookupError) as error:
                            thread_race_count += 1
                            if len(thread_races) < 64:
                                thread_races.append({"tid": int(entry.name), "error": type(error).__name__})
                            continue
                        except ValueError as error:
                            if str(error) == "THREAD_NOT_LIVE":
                                thread_race_count += 1
                                if len(thread_races) < 64:
                                    thread_races.append({"tid": int(entry.name), "error": str(error)})
                                continue
                            raise
                        require(row["tgid"] == worker and row["pid_namespace"] == root_identity["pid_namespace"]
                                and row["cgroup"] == root_identity["cgroup"], "THREAD_WORKER_BINDING_CHANGED")
                        key = (row["tid"], row["start_ticks"])
                        self.mapping_keys.add(key)
                        require(len(self.mapping_keys) <= self.max_generations, "DISTINCT_THREAD_MAPPING_LIMIT")
                        threads.append(row)
                        self.known_threads[key] = row
                        self.thread_pins[row["tid"]] = (group, label, pin)

                    require(stable_identity(Path("/proc") / str(worker))["start_ticks"] == process["worker_start_ticks"],
                            "WORKER_CHANGED_DURING_THREAD_ENUMERATION")
                    row = {"kind": "identity-sample", "group": group, "camera": label,
                        "sample_started_monotonic_ns": began, "sample_finished_monotonic_ns": time.monotonic_ns(),
                        "runtime_identity": pin, "threads": threads, "thread_race_count": thread_race_count,
                        "thread_races_first_64": thread_races}
                    if worker not in self.seeded_workers:
                        host = next(instance for instance in self.instances if instance.name == "host")
                        tids = [int(entry.name) for entry in thread_dir.iterdir() if entry.name.isdigit()]
                        require(len(tids) <= self.max_processes, "WORKER_THREAD_MAPPING_LIMIT")
                        with (host.path / "set_event_pid").open("a") as selected:
                            selected.write(" ".join(map(str,tids))+"\n")
                        self.seeded_workers.add(worker)
                        self.budget.json("identities.jsonl", {"kind":"pid-filter-seeded","worker":worker,
                            "monotonic_ns":time.monotonic_ns(),"host_tids":tids,"scope":"existing tasks seeded; event-fork follows future children"})
                    self.bound[(group, label)] = {"worker_host_pid": worker, "start_ticks": process["worker_start_ticks"]}
                    self.identity_samples += 1
                except (FileNotFoundError, ProcessLookupError):
                    # Exited workers are expected during normal cleanup; retained
                    # lifecycle events determine their proven terminal boundary.
                    terminal = worker_terminal_status(process)
                    row = {"kind": "identity-unavailable", "group": group, "camera": label,
                           "sample_started_monotonic_ns": began, "sample_finished_monotonic_ns": time.monotonic_ns(),
                           "reason": "process-exited-or-proc-race", "terminal_is_not_assumed_proven": True,
                           "pinned_worker_recheck": terminal}
                    if terminal["retire_sampling"] and (group, label) in self.bound:
                        self.terminal_workers.add((group, label))
                except Exception as error:
                    if str(error) in ("THREAD_NOT_LIVE", "RUNTIME_PROCESS_EXITED"):
                        terminal = worker_terminal_status(process)
                        self.budget.json("identities.jsonl", {
                            "kind": "identity-unavailable", "group": group, "camera": label,
                            "sample_started_monotonic_ns": began, "sample_finished_monotonic_ns": time.monotonic_ns(),
                            "reason": "worker-exit-adjacent", "terminal_is_not_assumed_proven": True,
                            "pinned_worker_recheck": terminal})
                        if terminal["retire_sampling"] and (group, label) in self.bound:
                            self.terminal_workers.add((group, label))
                        continue
                    self.identity_gaps += 1
                    row = {"kind": "identity-error", "group": group, "camera": label,
                           "sample_started_monotonic_ns": began, "sample_finished_monotonic_ns": time.monotonic_ns(),
                           "reason": type(error).__name__ + ":" + str(error)}
                    self.fail("IDENTITY_ERROR:" + group + ":" + label + ":" + str(error))
                self.budget.json("identities.jsonl", row)

    def anchor_fork(self, line):
        match = self.syscalls.PREFIX.match(line)
        if not match: return
        _, _, _, _, body = match.groups()
        event, fields = self.syscalls.event_text(body)
        if event != "sched_process_fork": return
        parent = self.syscalls.scalar(fields,"parent_pid" if "parent_pid=" in fields else "pid")
        child = self.syscalls.scalar(fields,"child_pid")
        pinning = self.thread_pins.get(parent)
        if pinning is None: return
        group, camera, pin = pinning
        began = time.monotonic_ns()
        try:
            worker = pin["process"]
            before = proc_record(Path("/proc")/str(worker["worker_host_pid"]))
            require(before["start_ticks"] == worker["worker_start_ticks"], "FORK_ROOT_PID_REUSED")
            row = stable_identity(Path("/proc")/str(child))
            require(proc_record(Path("/proc")/str(worker["worker_host_pid"]))["start_ticks"] == before["start_ticks"], "FORK_ROOT_CHANGED")
            require(re.search(r"(?:^|[/:-])"+re.escape(pin["container_id"])+r"(?:\.scope)?(?:/|\n|$)",row["cgroup"]), "FORK_CONTAINER_CGROUP_MISMATCH")
            require(row["tgid"] == worker["worker_host_pid"] and row["pid_namespace"] == worker["pid_namespace"],
                    "FORK_CHILD_NOT_PROVEN_WORKER_THREAD")
            key = (row["tid"],row["start_ticks"])
            require(len(self.mapping_keys) < self.max_generations, "DISTINCT_THREAD_MAPPING_LIMIT")
            self.mapping_keys.add(key)
            self.thread_pins[child] = pinning
            self.budget.json("identities.jsonl", {"kind":"identity-sample","basis":"fork-triggered-proc",
                "group":group,"camera":camera,"runtime_identity":pin,"threads":[row],
                "sample_started_monotonic_ns":began,"sample_finished_monotonic_ns":time.monotonic_ns()})
        except (OSError, ValueError) as error:
            self.budget.json("identities.jsonl", {"kind":"fork-identity-unavailable","group":group,"camera":camera,
                "host_tid":child,"sample_started_monotonic_ns":began,"sample_finished_monotonic_ns":time.monotonic_ns(),
                "reason":type(error).__name__+":"+str(error)})

    def identity_loop(self):
        try:
            while not self.done.is_set():
                self.sample_identities()
                self.done.wait(1)
        except Exception as error:
            self.fail("IDENTITY_OBSERVER_ERROR:" + type(error).__name__ + ":" + str(error))

    def poll(self):
        if self.started_ns and time.monotonic_ns() - self.started_ns >= self.max_seconds * 1e9:
            self.fail("TRACE_MAXIMUM_DURATION_REACHED")
        if self.failure:
            for instance in self.instances:
                instance.stop_capture()
        return self.failure

    def finish(self):
        if self.finished is not None:
            return self.finished
        self.done.set()
        for instance in self.instances:
            instance.stop_capture()
            instance.stop_reader.set()
        threads = [self.stats_thread, self.identity_thread] + [v.thread for v in self.instances]
        for thread in threads:
            if thread:
                thread.join(5)
                if thread.is_alive():
                    self.fail("OBSERVER_THREAD_STOP_TIMEOUT:" + thread.name)
        # A live writer might still own a descriptor or append to evidence.
        # Retain this incomplete session for a later finish attempt; never
        # close/reuse descriptors or issue immutable manifests underneath it.
        require(not any(thread and thread.is_alive() for thread in threads), "LIVE_OBSERVER_PREVENTS_FINALIZATION")
        rows = []
        for instance in self.instances:
            try:
                rows.append(instance.finish())
            except Exception as error:
                self.fail("FINISH_ERROR:" + instance.name + ":" + str(error))
        capture_ok = self.failure is None and len(rows) == 2 and all(
            row["complete_initial_and_final_counters"] and row["removed"] for row in rows)
        try:
            source_after = self.source_proof() if self.source_hashes else {}
        except Exception as error:
            source_after = {"error": type(error).__name__ + ":" + str(error)}
        if self.source_hashes != source_after:
            self.fail("HELPER_OR_SOURCE_CHANGED_DURING_TRACE")
            capture_ok = False
        result = {"schema": "gdp-independent-host-trace-v1", "capture_complete": capture_ok,
            "state": "raw-capture-complete-pending-identity-analysis" if capture_ok else "incomplete",
            "association_complete": False, "association_status": "requires-independent-offline-analysis",
            "hardware_acceptance_verified": False, "failure": self.failure, "errors": self.errors,
            "started_monotonic_ns": self.started_ns, "finished_monotonic_ns": time.monotonic_ns(),
            "instances": rows, "binding": self.binding, "max_statistics_start_gap_ns": self.max_stats_gap_ns,
            "bound_workers": [{"group": k[0], "camera": k[1], **v} for k, v in sorted(self.bound.items())],
            "identity_samples": self.identity_samples, "identity_errors": self.identity_gaps,
            "distinct_thread_identities": len(self.mapping_keys), "max_total_file_bytes": self.budget.maximum,
            "helper_and_source_sha256_before": self.source_hashes, "helper_and_source_sha256_after": source_after,
            "scope": SCOPE}
        try:
            self.budget.json("summary.json", result, terminal=True)
            self.budget.close()
            lines = []
            for path in sorted(self.output.iterdir()):
                require(path.is_file() and not path.is_symlink(), "OUTPUT_CHANGED_BEFORE_MANIFEST")
                with path.open("rb") as stream:
                    digest = stream_sha(stream)
                lines.append(digest + "  " + path.name + "\n")
            self.budget.write("SHA256SUMS", "".join(lines).encode(), terminal=True)
            self.budget.close()
            require(sum(p.stat().st_size for p in self.output.iterdir()) == self.budget.used
                    and self.budget.used <= self.budget.maximum, "AGGREGATE_OUTPUT_BYTES_MISMATCH")
        except Exception as error:
            self.fail("FINALIZATION_ERROR:" + type(error).__name__ + ":" + str(error))
            result.update(capture_complete=False, state="incomplete", failure=self.failure,
                          finalization_error=str(error), errors=list(self.errors))
            self.finished = result
            raise
        self.finished = result
        return self.finished


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--recording-path", type=Path, required=True)
    parser.add_argument("--retest-root", type=Path, required=True)
    parser.add_argument("--execute", action="store_true")
    parser.add_argument("--max-bytes", type=int, default=16 * 1024**3)
    parser.add_argument("--max-seconds", type=int, default=86400)
    args = parser.parse_args()
    require(args.execute, "EXPLICIT_EXECUTE_REQUIRED")
    # Library mode provides driver coupling; stand-alone mode deliberately refuses
    # to create an observer whose failure nobody can propagate to run_retest.
    raise SystemExit("Use HostKernelTrace from the reviewed formal controller; standalone capture is intentionally disabled.")


if __name__ == "__main__":
    main()
