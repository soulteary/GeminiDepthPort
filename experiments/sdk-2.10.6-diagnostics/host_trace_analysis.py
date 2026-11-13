#!/usr/bin/env python3
"""Read-only host TID-generation association beside original wait-chain output.

Requires the unchanged analyze_checkpoint_tails.py output. Original evidence
and original analysis are never rewritten. No process, tracefs or Docker access.
"""
import argparse
from bisect import bisect_right
import hashlib
import json
import os
from pathlib import Path
import re
import sys

sys.dont_write_bytecode = True
from host_kernel_trace import Budget, MAX_LINE, REVISION, document, load, require, stream_sha


def digest(path):
    with path.open("rb") as stream:
        return stream_sha(stream)


def verify_manifest(root, maximum):
    raw = (root / "SHA256SUMS").read_bytes()
    require(len(raw) <= 16384, "TRACE_MANIFEST_BOUND")
    names = set()
    total = len(raw)
    for line in raw.decode("ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([a-z0-9_.-]{1,80})", line)
        require(match is not None and match[2] not in names and match[2] != "SHA256SUMS", "TRACE_MANIFEST_SCHEMA")
        path = root / match[2]
        require(path.is_file() and not path.is_symlink(), "TRACE_MANIFEST_FILE_TYPE")
        total += path.stat().st_size
        require(total <= maximum and digest(path) == match[1], "TRACE_MANIFEST_SHA_OR_SIZE")
        names.add(match[2])
    require({path.name for path in root.iterdir()} == names | {"SHA256SUMS"}, "TRACE_MANIFEST_FILE_SET")
    return digest(root / "SHA256SUMS")


def original_observer_gaps(original):
    """The kernel stream cannot repair missing original application events."""
    gaps = []
    coverage = original.get("event_union_coverage")
    if not isinstance(coverage, list) or not coverage:
        gaps.append({"reason": "original-application-event-coverage-unavailable"})
    else:
        for row in coverage:
            missing = row.get("missing_through_last_observed")
            if type(missing) is not int or missing != 0:
                gaps.append({"reason": "original-application-event-prefix-missing", "camera": row.get("target"),
                             "missing_through_last_observed": missing})
    sources = original.get("source_summaries") or {}
    lifecycle = sources.get("lifecycle") or {}
    if lifecycle.get("sampling_ok") is not True or lifecycle.get("errors") not in ([], {}):
        gaps.append({"reason": "original-lifecycle-sampling-incomplete",
                     "sampling_ok": lifecycle.get("sampling_ok"), "errors": lifecycle.get("errors")})
    targets = lifecycle.get("event_targets")
    if not isinstance(targets, dict) or set(targets) != {"a", "b", "c"}:
        gaps.append({"reason": "original-lifecycle-target-coverage-unavailable"})
    else:
        for camera, row in targets.items():
            for field in ("lost_events_after_start",):
                if type(row.get(field)) is not int or row[field] != 0:
                    gaps.append({"reason": "original-lifecycle-event-history-incomplete",
                                 "camera": camera, "field": field, "value": row.get(field)})
    checkpoint = sources.get("checkpoint") or {}
    if checkpoint.get("errors") not in ([], {}):
        gaps.append({"reason": "original-checkpoint-sampling-errors", "errors": checkpoint.get("errors")})
    targets = checkpoint.get("targets")
    if not isinstance(targets, list) or len(targets) != 3:
        gaps.append({"reason": "original-checkpoint-target-coverage-unavailable"})
    else:
        for row in targets:
            for field in ("lost_events_after_start",):
                if type(row.get(field)) is not int or row[field] != 0:
                    gaps.append({"reason": "original-checkpoint-event-history-incomplete",
                                 "target_index": row.get("index"), "field": field, "value": row.get(field)})
            if row.get("observation_supported") is not True or row.get("observation_enabled") is not True:
                gaps.append({"reason": "original-checkpoint-observation-unavailable", "target_index": row.get("index")})
    return gaps


def raw_lines(path, maximum=128 * 1024**3):
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= maximum, "RAW_TRACE_INPUT_BOUND")
    with path.open("rb") as stream:
        while True:
            line = stream.readline(MAX_LINE + 2)
            if not line:
                break
            require(len(line) <= MAX_LINE + 1 and line.endswith(b"\n"), "RAW_TRACE_LINE_BOUND_OR_TRUNCATION")
            yield line.decode("utf-8", "strict").rstrip("\n")


def json_lines(path, maximum=128 * 1024**3):
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= maximum, "MAPPING_INPUT_BOUND")
    with path.open("rb") as stream:
        for _ in range(1_000_000):
            line = stream.readline(16 * 1024**2 + 1)
            if not line:
                return
            require(len(line) <= 16 * 1024**2 and line.endswith(b"\n"), "MAPPING_LINE_BOUND")
            yield json.loads(line)
        require(not stream.read(1), "MAPPING_ROWS_BOUND")


class Generations:
    """Lifetime/image boundaries from host fork/exec/exit, never comm matching."""
    def __init__(self, maximum):
        self.maximum = maximum
        self.rows = []
        self.live = {}
        self.by_tid = {}
        self.gaps = []
        self.exits = {}
        self.exit_count = 0
        self.gap_count = 0

    def gap(self, reason, **context):
        self.gap_count += 1
        if len(self.gaps) < 1024:
            self.gaps.append({"reason": reason, **context})

    def born(self, tid, at, basis, parent=None):
        require(len(self.rows) < self.maximum, "GENERATION_ANALYSIS_LIMIT")
        require(not self.by_tid.get(tid) or self.by_tid[tid][-1]["start"] <= at, "GENERATION_CLOCK_REGRESSION")
        if tid in self.live:
            self.gap("birth-without-prior-exit", tid=tid, monotonic_ns=at)
            self.live[tid]["end"] = at
        row = {"generation": len(self.rows) + 1, "tid": tid, "start": at, "end": None,
               "basis": basis, "parent_generation": parent, "owner": None}
        self.rows.append(row)
        self.live[tid] = row
        self.by_tid.setdefault(tid, []).append(row)
        return row

    def event(self, kind, fields, at, scalar):
        if kind == "sched_process_fork":
            parent = scalar(fields, "parent_pid" if re.search(r"\bparent_pid\s*=", fields) else "pid")
            child = scalar(fields, "child_pid")
            born=self.born(child, at, "fork", self.live.get(parent, {}).get("generation"))
            born["parent_tid"]=parent
        elif kind == "sched_process_exec":
            tid, old = scalar(fields, "pid"), scalar(fields, "old_pid")
            for value in {tid, old}:
                prior = self.live.pop(value, None)
                if prior:
                    prior["end"] = at
            self.born(tid, at, "exec")
        elif kind == "sched_process_exit":
            tid = scalar(fields, "pid")
            self.exits.setdefault(tid, []).append(at)
            self.exit_count += 1
            require(self.exit_count <= self.maximum, "EXIT_ANALYSIS_LIMIT")
            row = self.live.pop(tid, None)
            if row:
                row["end"] = at
            # Existing host tasks can predate this capture. Their unobserved
            # births are expected, not evidence that they belong to a writer.

    def at(self, tid, start, end=None):
        end = start if end is None else end
        rows = self.by_tid.get(tid, ())
        if not rows:
            return None
        index = bisect_right([row["start"] for row in rows], start) - 1
        if index < 0:
            return None
        row = rows[index]
        if row["start"] >= start or (row["end"] is not None and row["end"] <= end):
            return None
        return row

    def resolve_parents(self):
        for row in self.rows:
            if "parent_tid" in row:
                parent=self.at(row["parent_tid"],row["start"])
                if parent is not None:row["parent_generation"]=parent["generation"]

    def anchor(self, sample, expected_workers):
        group, camera = sample["group"], sample["camera"]
        pin = sample["runtime_identity"]
        worker = pin["process"]
        worker_key = (worker["worker_host_pid"], worker["worker_start_ticks"])
        if (camera, *worker_key) not in expected_workers:
            return
        start, end = sample["sample_started_monotonic_ns"], sample["sample_finished_monotonic_ns"]
        for thread in sample["threads"]:
            require(thread["tgid"] == worker_key[0] and thread["pid_namespace"] == worker["pid_namespace"]
                    and thread["monotonic_offset_ns"] == 0, "SNAPSHOT_WORKER_IDENTITY_SCHEMA")
            require(thread["nspid"][0] == thread["tid"], "SNAPSHOT_HOST_TID_SCHEMA")
            row = self.at(thread["tid"], start, end)
            if row is None:
                # A PID-filtered stream attaches to existing tasks. The stable
                # proc snapshot anchors identity from its END, never from birth.
                prior=self.by_tid.get(thread["tid"],[])
                boundaries=[r["start"] for r in prior]+self.exits.get(thread["tid"],[])
                overlaps=any(r["start"]<=end and (r["end"] is None or r["end"]>=start) for r in prior)
                if not overlaps and not any(start<=at<=end for at in boundaries):
                    require(len(self.rows)<self.maximum,"GENERATION_ANALYSIS_LIMIT")
                    row={"generation":len(self.rows)+1,"tid":thread["tid"],"start":end,
                        "end":min((at for at in boundaries if at>end),default=None),
                        "basis":"stable-proc-anchor-after-attach; birth unknown","parent_generation":None,"owner":None}
                    self.rows.append(row);self.by_tid.setdefault(thread["tid"],[]).append(row)
                    self.by_tid[thread["tid"]].sort(key=lambda r:r["start"])
            if row is None:
                self.gap("snapshot-without-enclosing-fork-or-exec-generation", group=group, camera=camera,
                         tid=thread["tid"], start_ticks=thread["start_ticks"], sample_started_monotonic_ns=start, sample_finished_monotonic_ns=end)
                continue
            owner = {"group": group, "camera": camera, "worker_host_pid": worker_key[0],
                     "worker_start_ticks": worker_key[1], "tid_start_ticks": thread["start_ticks"],
                     "namespace_tid": thread["nspid"][-1], "container_id": pin["container_id"],
                     "pid_namespace": thread["pid_namespace"]}
            if row["owner"] is not None:
                require(row["owner"] == owner, "CONFLICTING_THREAD_GENERATION_ANCHOR")
            else:
                row["owner"] = owner
                row["identity_observed_bracket_ns"] = [start, end]


class Association:
    def __init__(self, syscalls, generations, chain, emit):
        self.syscalls, self.generations, self.chain, self.emit = syscalls, generations, chain, emit
        self.start, self.end = chain["window_start_monotonic_ns"], chain["window_end_monotonic_ns"]
        self.calls, self.offcpu, self.wakes = {}, {}, {}
        self.strict_sync = set()
        self.gaps = []
        self.events = self.intervals = 0
        self.by_camera = {name: {"syscall_intervals": 0, "fsync_intervals": 0, "scheduler_events": 0} for name in "abc"}

    def owner(self, tid, at):
        generation = self.generations.at(tid, at)
        return generation if generation and generation["owner"] else None

    def output(self, value, row, first, last):
        if last < self.start or first > self.end:
            return
        owner = row["owner"]
        value.update(owner, generation=row["generation"], host_tid=row["tid"])
        value["checkpoint_temporal_overlaps"] = [
            {"event_id": event["event_id"], "checkpoint_id": event["checkpoint_id"], "phase": event["phase"],
             "same_namespace_tid": event.get("tid") == owner["namespace_tid"]}
            for event in self.chain["events"] if event["target"] == owner["camera"]
            and event["start_monotonic_ns"] <= last
            and event["start_monotonic_ns"] + event["wall_ns"] >= first]
        if value.get("syscall") in ("fsync","fdatasync"):
            for link in value["checkpoint_temporal_overlaps"]:
                if link["same_namespace_tid"] and link["phase"] in ("payload_sync","index_sync","directory_sync","checkpoint_directory_sync"):
                    self.strict_sync.add((owner["camera"],link["event_id"]))
        self.emit(value)
        self.events += 1

    def event(self, tid, cpu, at, resolution, kind, fields):
        scalar = self.syscalls.scalar
        base = {"kind": kind, "monotonic_ns": at, "cpu": cpu, "printed_resolution_ns": resolution}
        if kind.startswith(("sys_enter_", "sys_exit_")):
            row = self.owner(tid, at)
            if row is None:
                return
            key = row["generation"]
            name = kind.replace("sys_enter_", "").replace("sys_exit_", "")
            if kind.startswith("sys_enter_"):
                if key in self.calls:
                    self.gaps.append({"reason": "syscall-entry-without-exit", "generation": key, "at": at})
                self.calls[key] = (name, at, scalar(fields, "fd"))
            else:
                entry = self.calls.pop(key, None)
                if entry is None:
                    self.gaps.append({"reason": "syscall-exit-without-entry", "generation": key, "at": at})
                    return
                require(entry[0] == name and at >= entry[1], "SYSCALL_PAIR_MISMATCH")
                result = re.search(r"(?:\bret\s*(?:=|:)\s*)?(-?(?:0x[0-9a-fA-F]+|\d+))\s*$", fields)
                require(result is not None, "SYSCALL_RETURN_SCHEMA")
                raw = result[1]
                result = int(raw, 16 if "0x" in raw else 10)
                if result >= 2**63:
                    result -= 2**64
                value = {**base, "syscall": name, "fd": entry[2], "entry_monotonic_ns": entry[1],
                         "duration_ns": at - entry[1], "return_value": result,
                         "scope": "Syscall wall time overlaps scheduler intervals; never sum them."}
                self.output(value, row, entry[1], at)
                if at >= self.start and entry[1] <= self.end:
                    self.by_camera[row["owner"]["camera"]]["syscall_intervals"] += 1
                    if name in ("fsync", "fdatasync"):
                        self.by_camera[row["owner"]["camera"]]["fsync_intervals"] += 1
                    self.intervals += 1
        elif kind == "sched_switch":
            previous, following = scalar(fields, "prev_pid"), scalar(fields, "next_pid")
            prior, after = self.owner(previous, at), self.owner(following, at)
            if prior and previous != following:
                state = re.search(r"\bprev_state=(\S+)", fields)
                require(state is not None, "SCHED_STATE_SCHEMA")
                if prior["generation"] in self.offcpu:
                    self.gaps.append({"reason": "switch-out-without-resume", "generation": prior["generation"], "at": at})
                self.offcpu[prior["generation"]] = (at, state[1])
                self.wakes.pop(prior["generation"], None)
                self.output({**base, "direction": "switch-out", "prev_state": state[1],
                             "peer_host_tid": following}, prior, at, at)
                if self.start <= at <= self.end:
                    self.by_camera[prior["owner"]["camera"]]["scheduler_events"] += 1
            if after:
                key = after["generation"]
                start = self.offcpu.pop(key, None)
                wake = self.wakes.pop(key, None)
                value = {**base, "direction": "switch-in", "peer_host_tid": previous}
                if start:
                    require(at >= start[0], "SCHED_CLOCK_REGRESSION")
                    value.update(off_cpu_start_monotonic_ns=start[0], off_cpu_ns=at - start[0], previous_state=start[1])
                    if wake is not None:
                        require(start[0] <= wake <= at, "WAKE_CLOCK_REGRESSION")
                        value["wakeup_to_run_ns"] = at - wake
                else:
                    value["left_censored_off_cpu"] = True
                self.output(value, after, start[0] if start else at, at)
                if at >= self.start and (start[0] if start else at) <= self.end:
                    self.by_camera[after["owner"]["camera"]]["scheduler_events"] += 1
        elif kind == "sched_wakeup":
            woken = scalar(fields, "pid")
            row = self.owner(woken, at)
            if row:
                if row["generation"] in self.offcpu:
                    self.wakes.setdefault(row["generation"], at)
                self.output(base, row, at, at)
        require(len(self.gaps) <= 1024, "ASSOCIATION_GAP_LIMIT")

    def finish(self):
        for generation, (name, at, fd) in self.calls.items():
            row = self.generations.rows[generation - 1]
            end = row["end"] if row["end"] is not None else self.end
            if end >= self.start and at <= self.end:
                self.gaps.append({"reason": "right-censored-syscall", "generation": generation, "at": at})
                self.output({"kind": "right-censored-syscall", "syscall": name, "fd": fd,
                             "entry_monotonic_ns": at, "observed_until_monotonic_ns": end,
                             "return_value": None}, row, at, end)


def parse(syscalls, line):
    if not line.strip() or line.lstrip().startswith("#"):
        return None
    require("LOST " not in line, "RAW_STREAM_LOSS")
    match = syscalls.PREFIX.match(line)
    require(match is not None, "RAW_TRACE_PREFIX")
    tid, cpu, seconds, fraction, body = match.groups()
    require(1 <= len(fraction) <= 9, "RAW_TRACE_CLOCK_RESOLUTION")
    resolution = 10 ** (9 - len(fraction))
    kind, fields = syscalls.event_text(body)
    return int(tid), int(cpu), int(seconds) * 10**9 + int(fraction) * resolution, resolution, kind, fields


def split_generation_gaps(gaps, start, end):
    inside, outside = [], []
    for gap in gaps:
        first=gap.get('sample_started_monotonic_ns',gap.get('monotonic_ns'))
        last=gap.get('sample_finished_monotonic_ns',first)
        if type(first) is int and type(last) is int and (last<start or first>end):outside.append(gap)
        else:inside.append(gap)
    return inside,outside



def identity_history_status(generations, summary, expected):
    owned={(r['owner']['camera'],r['owner']['worker_host_pid'],r['owner']['worker_start_ticks'])
           for r in generations.rows if r['owner']}
    bound={(r['camera'],r['worker_host_pid'],r['start_ticks']) for r in summary.get('bound_workers',[])}
    reasons=[]
    if type(summary.get('identity_samples')) is not int or summary['identity_samples']<=0:
        reasons.append('zero-or-invalid-identity-samples')
    if type(summary.get('identity_errors')) is not int or summary['identity_errors']!=0:
        reasons.append('identity-capture-errors-or-unavailable-count')
    if {r[0] for r in expected}!={'a','b','c'} or not expected<=owned or not expected<=bound:
        reasons.append('expected-writer-identities-not-all-proven')
    if 'identity_binding_complete' in summary and summary['identity_binding_complete'] is not True:
        reasons.append('capture-identity-binding-incomplete')
    binding_complete=not reasons
    if generations.gaps or any('birth unknown' in r['basis'] for r in generations.rows):
        reasons.append('generation-history-gaps-or-unknown-births')
    return {'complete':not reasons,'binding_complete':binding_complete,'reasons':reasons,'expected_workers':len(expected),
            'proven_workers':len(owned),'capture_bound_workers':len(bound)}


def analyze(source, trace, official, output, max_output=256 * 1024**2):
    source, trace, official, output = map(lambda p: Path(p).resolve(), (source, trace, official, output))
    require(not output.exists() and not output.is_relative_to(trace)
            and not output.is_relative_to(official), "FRESH_INDEPENDENT_OUTPUT_REQUIRED")
    summary = document(trace / "summary.json")
    ready = document(trace / "ready.json")
    require(summary["schema"] == ready["schema"] == "gdp-independent-host-trace-v1"
            and re.fullmatch("[a-f0-9]{40}",ready["revision"]) is not None, "CURRENT_TRACE_SCHEMA")
    manifest_sha = verify_manifest(trace, ready["limits"]["max_total_file_bytes"])
    original = document(official / "summary.json")
    official_manifest_sha = verify_manifest(official, 512 * 1024**2)
    chain = original["three_writer_wait_chain"]
    require(chain["window_start_monotonic_ns"] <= chain["anchor_monotonic_ns"] - 1_000_000_000
            and chain["window_end_monotonic_ns"] >= chain["anchor_monotonic_ns"], "REQUIRED_PRE_OVERFLOW_WINDOW")
    expected = {(row["target"], row["host_pid"], row["start_ticks"]) for row in original["worker_epochs"]}
    paths = [trace / name for name in ("summary.json", "ready.json", "identities.jsonl", "host-raw.trace", "storage-raw.trace")]
    paths.append(official / "summary.json")
    paths.append(official / "resources.json")
    resource_points = document(official / "resources.json", 64 * 1024**2)
    require(isinstance(resource_points, list) and len(resource_points) <= 200000, "ORIGINAL_RESOURCE_POINTS_BOUND")
    refs = {str(path): digest(path) for path in paths}
    for relative in ("scripts/validation/writer_syscall_trace.py", "scripts/validation/writer_storage_trace.py",
                     "experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py"):
        matches = [value for path, value in ready["helper_and_source_sha256_before"].items()
                   if path.endswith("/" + relative)]
        require(len(matches) == 1 and digest(source / relative) == matches[0], "CURRENT_SOURCE_HELPER_SHA_MISMATCH")
    syscalls = load("host_analysis_original_syscalls", source / "scripts/validation/writer_syscall_trace.py")
    storage = load("host_analysis_original_storage", source / "scripts/validation/writer_storage_trace.py")
    generations = Generations(ready["limits"]["max_lifecycle_events"])
    for line in raw_lines(trace / "host-raw.trace"):
        event = parse(syscalls, line)
        if event and event[4].startswith("sched_process_"):
            generations.event(event[4], event[5], event[2], syscalls.scalar)
    for sample in json_lines(trace / "identities.jsonl"):
        if sample["kind"] == "identity-sample":
            generations.anchor(sample, expected)
    output.mkdir(mode=0o700, parents=True)
    budget = Budget(output, max_output)
    try:
        generations.resolve_parents()
        association = Association(syscalls, generations, chain, lambda row: budget.json("writer-events.jsonl", row))
        for line in raw_lines(trace / "host-raw.trace"):
            event = parse(syscalls, line)
            if event:
                association.event(*event)
        association.finish()
        storage_count = 0
        def storage_emit(row):
            nonlocal storage_count
            if row["monotonic_ns"] >= chain["window_start_monotonic_ns"] and row.get(
                    "entry_monotonic_ns", row["monotonic_ns"]) <= chain["window_end_monotonic_ns"]:
                generation = generations.at(row["context_tid"], row["monotonic_ns"])
                if generation and generation["owner"]:
                    row = {**row, "proven_writer_context": generation["owner"],
                           "host_tid_generation": generation["generation"]}
                budget.json("device-events.jsonl", row)
                storage_count += 1
        decoder = storage.StorageDecoder(ready["binding"], storage_emit)
        for line in raw_lines(trace / "storage-raw.trace"):
            decoder.line(line)
        for row in generations.rows:
            if row["owner"]:
                budget.json("proven-generations.jsonl", row)
        application_gaps = original_observer_gaps(original)
        interval_start, interval_end = chain["window_start_monotonic_ns"], chain["window_end_monotonic_ns"]
        window_gaps, outside_gaps = split_generation_gaps(generations.gaps, interval_start, interval_end)
        identity_status=identity_history_status(generations,summary,expected)
        gaps = window_gaps + association.gaps + application_gaps
        if not identity_status["binding_complete"]:
            gaps.append({"reason":"identity-history-incomplete", "details":identity_status})
        interval_start, interval_end = chain["window_start_monotonic_ns"], chain["window_end_monotonic_ns"]
        queue_brackets = {}
        for name in "abc":
            owned = [row for row in generations.rows if row["owner"] and row["owner"]["camera"] == name
                     and row["tid"] == row["owner"]["worker_host_pid"]]
            if not any(row["start"] < interval_start and row["end"] is not None
                       and row["end"] >= chain["anchor_monotonic_ns"] for row in owned):
                gaps.append({"reason": "worker-generation-does-not-cover-pre-overflow-window-with-proven-exit", "camera": name})
            queues = [row for row in chain["queue_samples"] if row["target"] == name]
            usable = [row for row in queues if isinstance(row.get("writer_queue"), dict)]
            if not usable:
                gaps.append({"reason": "no-official-queue-samples-in-window", "camera": name})
            # Original fast event polls deliberately omit writer_queue. Preserve
            # that absence; require actual resource samples, never fill the
            # absent fast rows from a different timestamp.
            for point in usable:
                queue = point.get("writer_queue")
                if not (isinstance(queue, dict) and queue.get("capacity_pairs") == 16
                        and type(queue.get("queued_pairs")) is int and 0 <= queue["queued_pairs"] <= 16
                        and "oldest_queued_age_ns" in queue
                        and (queue["oldest_queued_age_ns"] is None and queue["queued_pairs"] == 0
                             or type(queue["oldest_queued_age_ns"]) is int and queue["oldest_queued_age_ns"] >= 0)):
                    gaps.append({"reason": "missing-or-invalid-queue-depth-oldest-age", "camera": name,
                                 "sample_id": point.get("sample_id")})
                    break
            peers = sorted((point for point in resource_points if point["target"] == name
                and point.get("identity") and (name, *point["identity"]) in expected
                and isinstance(point.get("writer_queue"), dict)), key=lambda point: point["monotonic_ns"])
            before = [point for point in peers if point["monotonic_ns"] <= interval_start]
            after = [point for point in peers if point["monotonic_ns"] >= chain["anchor_monotonic_ns"]]
            def compact(point):
                return {key: point.get(key) for key in ("sample_id", "source", "epoch", "monotonic_ns",
                        "identity", "writer_queue", "active", "active_append")}
            queue_brackets[name] = {"before_window": compact(before[-1]) if before else None,
                "at_or_after_overflow": compact(after[0]) if after else None,
                "available_queue_samples_inside_window": len(usable),
                "fast_rows_without_queue_not_filled": len(queues) - len(usable),
                "scope": "Enclosing original resource reads; queue/status publication is not atomic with internal overflow time."}
            if not before or not after:
                gaps.append({"reason": "queue-resource-reads-do-not-bracket-pre-overflow-second", "camera": name})
            if not any(event["target"] == name for event in chain["events"]):
                gaps.append({"reason": "no-official-checkpoint-events-in-window", "camera": name})
            if not association.by_camera[name]["scheduler_events"]:
                gaps.append({"reason": "no-proven-scheduler-events-in-window", "camera": name})
        for event in chain["events"]:
            if event["phase"] in ("payload_sync","index_sync") and (event["target"],event["event_id"]) not in association.strict_sync:
                gaps.append({"reason":"checkpoint-sync-without-proven-tid-fsync","camera":event["target"],"event_id":event["event_id"]})
        for name in "abc":
            history=[h for h in chain.get("internal_queue_history",[]) if h["target"]==name]
            if not history or not all(h["window_bracketed"] and h["missing_between_selected_events"]==0 for h in history):
                gaps.append({"reason":"internal-queue-window-unproven","camera":name})
        if chain["anchor_basis"] != "overflow_detection":
            gaps.append({"reason": "no-internal-exact-overflow-clock"})
        if not summary.get("capture_complete"):
            gaps.append({"reason": "raw-capture-incomplete", "failure": summary.get("failure")})
        for instance in summary.get("instances", []):
            if (instance.get("start_monotonic_ns") is None or instance["start_monotonic_ns"] > interval_start
                    or instance.get("stop_monotonic_ns") is None or instance["stop_monotonic_ns"] < interval_end):
                gaps.append({"reason": "capture-armed-interval-does-not-cover-window", "instance": instance.get("instance")})
        if not storage_count:
            gaps.append({"reason": "no-device-events-in-window"})
        credit_checks = []
        credit_fields = {"pending_payload_bytes", "pending_pairs", "active_payload_bytes",
                         "active_pairs", "payload_limit_bytes", "pair_limit"}
        for wait in chain["wait_links"]:
            credit = wait.get("credit_at_wait")
            valid = isinstance(credit, dict) and set(credit) == credit_fields and all(
                type(value) is int and value >= 0 for value in credit.values())
            valid = valid and credit["pair_limit"] == 32 and credit["payload_limit_bytes"] == 16 * 1024**2
            if not valid:
                gaps.append({"reason": "per-writer-credit-snapshot-missing-or-invalid", "camera": wait["target"],
                             "wait_event_id": wait["wait_event_id"]})
            credit_checks.append({"camera": wait["target"], "wait_event_id": wait["wait_event_id"],
                "credit_at_wait": credit, "schema_and_limits_valid": valid,
                "pairs_within_limit": credit["pending_pairs"] + credit["active_pairs"] <= credit["pair_limit"] if valid else None,
                "bytes_within_limit": credit["pending_payload_bytes"] + credit["active_payload_bytes"] <= credit["payload_limit_bytes"] if valid else None,
                "scope": "This writer's own credit; no cross-writer sum or shared pool."})
        # An observed fork from a proven writer without an identity snapshot
        # cannot be silently equated to a writer thread.
        owned_ids = {row["generation"] for row in generations.rows if row["owner"]}
        unbound = [row for row in generations.rows if row["parent_generation"] in owned_ids and not row["owner"]
                   and row["start"] <= interval_end and (row["end"] is None or row["end"] >= interval_start)]
        for row in unbound[:1024]:
            gaps.append({"reason": "unbound-descendant-overlaps-window", "tid": row["tid"], "generation": row["generation"]})
        value = {"schema": "gdp-independent-host-association-v1", "revision": ready["revision"], "analysis_only": True,
            "outside_window_identity_gaps":outside_gaps,
            "observer_history_notes":original.get("source_summaries"),
            "overall_identity_history_complete":identity_status["complete"],
            "identity_history":identity_status,
            "association_complete": not gaps, "state": "complete" if not gaps else "incomplete",
            "hardware_acceptance_verified": False, "original_analysis_unchanged": True,
            "input_sha256": refs, "three_writer_wait_chain": chain, "gaps": gaps,
            "verified_trace_manifest_sha256": manifest_sha, "analysis_helper_sha256": digest(Path(__file__)),
            "verified_original_analysis_manifest_sha256": official_manifest_sha, "per_writer_credit_checks": credit_checks,
            "queue_resource_brackets": queue_brackets,
            "application_observer_complete": not application_gaps, "application_observer_gaps": application_gaps,
            "generation_gap_count": generations.gap_count, "unbound_descendants_in_window": len(unbound),
            "by_camera": association.by_camera, "associated_events": association.events,
            "device_events": storage_count, "storage_decoder": decoder.report(),
            "limits": {"max_output_bytes": max_output, "generation_limit": generations.maximum},
            "scope": "Only proc-anchored host TID generations, bounded by captured fork/exec/exit, are associated. "
                     "Comm names never establish identity. Checkpoint links are temporal overlap; same_namespace_tid "
                     "states an additional thread match. Wall, off-CPU and checkpoint spans overlap and must not be "
                     "summed. Device events include unrelated and kernel tasks and do not uniquely identify SSD latency. "
                     "Pending/active pair and byte credit belong separately to each writer."}
        require(refs == {str(path): digest(path) for path in paths}
                and digest(trace / "SHA256SUMS") == manifest_sha, "INPUT_CHANGED_DURING_ANALYSIS")
        budget.json("summary.json", value, terminal=True)
        budget.close()
        budget.write("SHA256SUMS", "".join(digest(path) + "  " + path.name + "\n"
                     for path in sorted(output.iterdir())).encode(), terminal=True)
        budget.close()
        return value
    except Exception as error:
        budget.json("analysis-error.json", {"state": "incomplete", "error": type(error).__name__ + ":" + str(error),
                                          "hardware_acceptance_verified": False}, terminal=True)
        budget.close()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--trace", type=Path, required=True)
    parser.add_argument("--official-analysis", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = analyze(args.source, args.trace, args.official_analysis, args.output)
    print(json.dumps({"state": result["state"], "output": str(args.output),
                      "association_complete": result["association_complete"]}))
    return 0 if result["association_complete"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
