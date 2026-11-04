"""Bounded storage experiment orchestration; real subprocesses, no camera required."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("writer_contention_ab", ROOT / "scripts/validation/writer_contention_ab.py")
ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ab)


def result(pairs=30):
    return {"ok": True, "pairs": pairs, "written_pairs": pairs, "queue_capacity": 16,
        "common_driver_sha256": ab.sha256(ROOT / "crates/offline-depth/tests/writer_contention_benchmark.rs"),
        "queue_peak": 1, "overflow": 0, "fps": 30, "imu_hz_per_stream": 200,
        "status_background_load": False, "input_sha256": "a" * 64,
        "artifact_sha256": {key: "b" * 64 for key in
                            ("snapshot.json", "records.jsonl", "checkpoint.json", "manifest.json", "COMPLETE")},
        "cpu_seconds": .01,
        "cpu_cores": .1, "peak_rss_kib": 1000, "append_p99_ns": 10,
        "append_max_ns": 20, "queue_wait_p99_ns": 30, "producer_late_max_ns": 40,
        "diagnostics": {"durable_pairs": pairs, "validation_image_bytes": 2 * ab.PAIR_BYTES * pairs,
            "failed_calls": {}, "uncommitted_pairs_peak": 32,
            "uncommitted_payload_bytes_peak": 16 * 1024 * 1024,
            "verification": {"total_ns": 100}},
        "independent_readback": {"passes": 2, "pairs_per_pass": [pairs, pairs],
            "per_frame_color_depth_evidence_hashes": True, "wall_ns": 100}}


def fake_binary(path, behavior="ok"):
    # This fixture tests process/barrier/cleanup contracts, not Rust durability.
    body = """#!SHEBANG
import json, os, pathlib, time
root=pathlib.Path(os.environ['GDP_CONTENTION_ROOT']); pairs=int(os.environ['GDP_CONTENTION_PAIRS'])
mode=os.environ['GDP_CONTENTION_MODE']; behavior=BEHAVIOR
def write(name,value): (root/name).write_text(json.dumps(value))
if mode=='crash':
    p=root/'capture.partial'; p.mkdir(); (p/'checkpoint.json').write_text('prefix32')
    write('recovery-ready.json',{'pairs':32,'pid':os.getpid()})
    while True: time.sleep(.01)
elif mode=='recover':
    if behavior=='mutate': (root/'capture.partial/checkpoint.json').write_text('changed')
    write('recovery-result.json',{'passes':2,'pairs_per_pass':[32,32],'ordinary_reader_rejected':behavior!='ordinary-accepted'})
else:
    if behavior=='delayed-ready':
        (root/'ready.json').write_text('')
        (root/'empty-ready-created').write_text('1')
        while not (root/'ready-release').exists(): time.sleep(.001)
    if behavior=='invalid-ready': (root/'ready.json').write_text('{')
    elif behavior=='large-ready': (root/'ready.json').write_text(' '*4097)
    elif behavior=='schema-ready': write('ready.json',{'pid':str(os.getpid())})
    else: write('ready.json',{'pid':os.getpid()+(1 if behavior=='wrong-ready-pid' else 0)})
    barrier=pathlib.Path(os.environ['GDP_CONTENTION_BARRIER'])
    while not barrier.exists(): time.sleep(.002)
    target=json.loads(barrier.read_text())['start_monotonic_ns']
    while time.monotonic_ns()<target: time.sleep(.0001)
    if behavior=='hang':
        while True: time.sleep(.01)
    value=RESULT
    value['pairs']=pairs; value['written_pairs']=pairs
    value['diagnostics']['durable_pairs']=pairs
    value['diagnostics']['validation_image_bytes']=2*PAIR_BYTES*pairs
    value['independent_readback']['pairs_per_pass']=[pairs,pairs]
    value['actual_start_unix_ns']=time.time_ns()
    value['actual_start_monotonic_ns']=time.monotonic_ns()
    if behavior=='overflow': value.update(ok=False,reason='RAW_QUEUE_OVERFLOW',overflow=1,queue_peak=16)
    if behavior=='hash' and root.parent.name.endswith('before'): value['input_sha256']='c'*64
    write('result.json',value)
"""
    body = body.replace("SHEBANG", sys.executable).replace("BEHAVIOR", repr(behavior))
    body = body.replace("RESULT", repr(result())).replace("PAIR_BYTES", str(ab.PAIR_BYTES))
    path.write_text(body)
    path.chmod(0o700)
    return path


def args(root, binary):
    return argparse.Namespace(after=binary, before=None, output=root / "new-output", seconds=1,
                              repeats=3, max_output_bytes=2 * 1024 ** 3, max_wall_seconds=30)


class WriterABTests(unittest.TestCase):
    def test_ready_marker_reads_are_small_and_only_incomplete_json_is_pending(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "ready.json"
            self.assertEqual(ab.ready_marker(path), (False, None))
            for raw in (b"", b"{", b'{"pid":'):
                path.write_bytes(raw)
                self.assertEqual(ab.ready_marker(path), (False, None))
            path.write_text("null")
            self.assertEqual(ab.ready_marker(path), (True, None))
            path.write_bytes(b" " * 4097)
            with self.assertRaisesRegex(ValueError, "READY_MARKER_BYTE_LIMIT"):
                ab.ready_marker(path)

    @unittest.skipUnless(sys.platform == "linux", "Linux owned subprocess orchestration")
    def test_real_three_children_empty_ready_gate_precedes_common_barrier(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); output = root / "output"; output.mkdir(); batch = output / "batch"
            binary = fake_binary(root / "driver", "delayed-ready")
            results = []; errors = []
            def run():
                try:
                    results.append(ab.run_batch(binary, batch, 3, 30, output, 100000000,
                                                time.monotonic() + 10, .01))
                except BaseException as e:
                    errors.append(e)
            worker = threading.Thread(target=run, daemon=True); worker.start()
            until = time.monotonic() + 5
            children = [batch / f"writer-{i}" for i in range(3)]
            while not all((p / "empty-ready-created").exists() for p in children):
                self.assertFalse(errors, errors)
                self.assertLess(time.monotonic(), until)
                time.sleep(.001)
            self.assertFalse((batch / "start.json").exists())
            for p in children[:2]: (p / "ready-release").write_text("1")
            while not all((p / "ready.json").stat().st_size > 0 for p in children[:2]):
                self.assertLess(time.monotonic(), until)
                time.sleep(.001)
            self.assertEqual((children[2] / "ready.json").read_bytes(), b"")
            self.assertFalse((batch / "start.json").exists())
            (children[2] / "ready-release").write_text("1")
            worker.join(timeout=10)
            self.assertFalse(worker.is_alive())
            self.assertFalse(errors, errors)
            self.assertEqual(len(results[0]["writers"]), 3)
            self.assertGreaterEqual((batch / "start.json").stat().st_mtime_ns,
                                    max((p / "ready.json").stat().st_mtime_ns for p in children))

    @unittest.skipUnless(sys.platform == "linux", "Linux owned subprocess orchestration")
    def test_ready_valid_wrong_pid_schema_oversize_and_persistent_invalid_are_rejected(self):
        for behavior, error in (("wrong-ready-pid", "BARRIER_PID_MISMATCH"),
                                ("schema-ready", "BARRIER_READY_SCHEMA"),
                                ("large-ready", "READY_MARKER_BYTE_LIMIT"),
                                ("invalid-ready", "WALL_TIME_LIMIT")):
            with self.subTest(behavior=behavior), tempfile.TemporaryDirectory() as d:
                root = Path(d); output = root / "output"; output.mkdir()
                with self.assertRaisesRegex((RuntimeError, ValueError), error):
                    ab.run_batch(fake_binary(root / "driver", behavior), output / "batch", 1, 30,
                                 output, 100000000, time.monotonic() + (.3 if behavior == "invalid-ready" else 5), .01)
                self.assertFalse((output / "batch/start.json").exists())

    def test_budget_scan_tolerates_entries_that_no_longer_resolve(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "retained").write_bytes(b"1234")
            (root / "vanished-checkpoint-temp").symlink_to(root / "renamed-away")
            self.assertEqual(ab.output_size(root), 4)

    def test_missing_all_rss_yields_null_without_mutating_failed_reports(self):
        failed = {"ok": False, "reason": "RAW_QUEUE_OVERFLOW", "overflow": 1}
        row = {"variant": "after", "writer_count": 1, "writers": [failed],
               "simultaneous_rss_peak_bytes": None, "resource_diagnostic_complete": False,
               "directory": "failed-group", "resource_gaps": {"process_io:PermissionError": 1}}
        before = json.dumps(row, sort_keys=True)
        summary = ab.summarize_groups([row], ["after"])
        self.assertIsNone(summary[0]["median_group_peak_rss_bytes"])
        self.assertEqual(summary[0]["group_rss_observed_runs"], 0)
        self.assertFalse(summary[0]["resource_diagnostic_complete"])
        self.assertEqual(summary[0]["per_process_rows"][0]["overflow"], 1)
        self.assertEqual(summary[0]["resource_diagnostics_by_run"][0]["resource_gaps"],
                         {"process_io:PermissionError": 1})
        self.assertEqual(json.dumps(row, sort_keys=True), before)

    def test_durability_workload_credit_and_overflow_guards(self):
        self.assertEqual(ab.validate_result(result(), 30)[0], "a" * 64)
        faults = [("overflow", 1), ("queue_capacity", 32), ("fps", 29),
                  ("imu_hz_per_stream", 100), ("status_background_load", True)]
        for key, value in faults:
            row = result(); row[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                ab.validate_result(row, 30)
        row = result(); row["diagnostics"]["uncommitted_payload_bytes_peak"] += 1
        with self.assertRaisesRegex(ValueError, "CREDIT"):
            ab.validate_result(row, 30)
        row = result(); row["independent_readback"]["passes"] = 1
        with self.assertRaisesRegex(ValueError, "READBACK"):
            ab.validate_result(row, 30)

    def test_real_rust_failure_map_hash_and_finite_number_contracts_are_strict(self):
        row = result(); row["ok"] = "true"
        with self.assertRaisesRegex(ValueError, "RECORDING_FAILED"):
            ab.validate_result(row, 30)
        row = result(); row["independent_readback"]["per_frame_color_depth_evidence_hashes"] = "true"
        with self.assertRaisesRegex(ValueError, "READBACK"):
            ab.validate_result(row, 30)
        for invalid in ([], 0, False, None, "", {"fsync": 0}, {"fsync": 1}):
            row = result(); row["diagnostics"]["failed_calls"] = invalid
            with self.subTest(failed_calls=invalid), self.assertRaisesRegex(ValueError, "DISK_STAGE"):
                ab.validate_result(row, 30)
        for invalid in ({}, {"records.jsonl": "b" * 64}, [], None):
            row = result(); row["artifact_sha256"] = invalid
            with self.subTest(artifacts=invalid), self.assertRaisesRegex(ValueError, "SHA256"):
                ab.validate_result(row, 30)
        for invalid in (True, float("nan"), float("inf"), -1):
            row = result(); row["cpu_seconds"] = invalid
            with self.subTest(cpu=invalid), self.assertRaisesRegex(ValueError, "PERFORMANCE_NUMBER"):
                ab.validate_result(row, 30)
        row = result(); row["input_sha256"] = "g" * 64
        with self.assertRaisesRegex(ValueError, "SHA256"):
            ab.validate_result(row, 30)

    def test_mount_namespace_evidence_is_not_physical_ext4_acceptance(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d); info = p / "mountinfo"
            info.write_text("1 0 0:1 / / rw - overlay overlay rw\n2 1 8:1 / " + str(p.resolve()) + " rw - ext4 /dev/sda1 rw\n")
            observed = ab.mount_evidence(p, info)
            self.assertEqual(observed["filesystem"], "ext4")
            self.assertFalse(observed["physical_ext4_verified"])

    def test_real_proc_batch_and_explicit_exit(self):
        if sys.platform != "linux":
            self.skipTest("real procfs is Linux-only")
        sample = ab.process_sample(os.getpid())
        self.assertGreater(sample["rss_bytes"], 0)
        self.assertGreater(sample["start_ticks"], 0)
        self.assertEqual(ab.process_sample(999999999)["error"], "EXITED")

    def test_proc_fixture_missing_io_is_explicit(self):
        with tempfile.TemporaryDirectory() as d:
            proc = Path(d); base = proc / "123"; base.mkdir()
            fields = ["0"] * 22; fields[0] = "S"; fields[19] = "101"; fields[21] = "5"
            (base / "stat").write_text("123 (writer with spaces) " + " ".join(fields))
            row = ab.process_sample(123, proc)
            self.assertEqual(row["start_ticks"], 101)
            self.assertEqual(row["io_error"], "FileNotFoundError")
            self.assertNotIn("io", row)

    def test_terminal_proc_fixture_is_excluded_from_live_resource_samples(self):
        with tempfile.TemporaryDirectory() as d:
            proc = Path(d); base = proc / "123"; base.mkdir()
            fields = ["0"] * 22; fields[0] = "Z"; fields[19] = "101"; fields[21] = "0"
            (base / "stat").write_text("123 (finished writer) " + " ".join(fields))
            row = ab.process_sample(123, proc)
            self.assertEqual(row["error"], "EXITED")
            self.assertTrue(row["terminal"])
            self.assertEqual(row["state_before"], "Z")
            self.assertEqual(row["state_after"], "Z")
            self.assertEqual(row["io_error"], "FileNotFoundError")

    @unittest.skipUnless(sys.platform == "linux" and hasattr(os, "WNOWAIT"), "Linux zombie identity")
    def test_actual_exited_child_is_terminal_before_parent_reaps_it(self):
        child = subprocess.Popen([sys.executable, "-c", "pass"])
        try:
            os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
            row = ab.process_sample(child.pid)
            self.assertEqual(row["error"], "EXITED")
            self.assertTrue(row["terminal"])
            self.assertIn("Z", (row["state_before"], row["state_after"]))
        finally:
            child.wait(timeout=5)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "controlled proc FIFO requires POSIX")
    def test_owned_exit_after_missing_proc_read_is_bounded_and_live_gap_is_retained(self):
        # A real child remains alive at the initial poll. The proc server removes
        # stat between its first and final reads, then releases the child to exit
        # after a short delay. No Popen methods or summarizers are mocked.
        for exits in (True, False):
            with self.subTest(exits=exits), tempfile.TemporaryDirectory() as d:
                root = Path(d); ready = root / "child-ready"; release = root / "child-release"
                body = ("import os,pathlib,sys,time; ready=pathlib.Path(sys.argv[1]); release=pathlib.Path(sys.argv[2]); "
                        "ready.write_text('1');\nwhile not release.exists(): time.sleep(.001)\ntime.sleep(.001)\nos._exit(0)\n")
                child = subprocess.Popen([sys.executable, "-c", body, str(ready), str(release)])
                server = None
                try:
                    until = time.monotonic() + 5
                    while not ready.exists():
                        self.assertIsNone(child.poll())
                        self.assertLess(time.monotonic(), until)
                        time.sleep(.001)
                    proc = root / "proc"; base = proc / str(child.pid); base.mkdir(parents=True)
                    os.mkfifo(base / "stat"); os.mkfifo(base / "io")
                    fields = ["0"] * 22; fields[0] = "S"; fields[19] = "101"; fields[21] = "5"
                    raw = f"{child.pid} (writer) " + " ".join(fields)
                    def serve():
                        with (base / "stat").open("w") as f: f.write(raw)
                        (base / "stat").unlink()
                        if exits: release.write_text("1")
                        # Opening IO follows the first stat close; the final stat
                        # is guaranteed missing before its reader can proceed.
                        with (base / "io").open("w") as f: f.write("write_bytes: 10\n")
                    server = threading.Thread(target=serve, daemon=True); server.start()
                    row = ab.owned_process_sample(child, proc)
                    server.join(timeout=2); self.assertFalse(server.is_alive())
                    confirmation = row["exit_confirmation"]
                    self.assertEqual(confirmation["limit_ns"], 50_000_000)
                    self.assertEqual(confirmation["elapsed_ns"], confirmation["end_monotonic_ns"] - confirmation["start_monotonic_ns"])
                    self.assertEqual(row["error"], "EXITED")
                    if exits:
                        self.assertTrue(row["terminal"])
                        self.assertTrue(row["reaped"])
                        self.assertEqual(row["proc_error_before_exit_confirmation"], "EXITED")
                        self.assertEqual(confirmation["state"], "owned-child-exit-confirmed")
                        self.assertEqual(confirmation["returncode"], 0)
                    else:
                        self.assertNotIn("terminal", row)
                        self.assertIsNone(child.poll())
                        self.assertEqual(confirmation["state"], "still-live-at-bound")
                        self.assertIsNone(confirmation["returncode"])
                finally:
                    if child.poll() is None: child.kill()
                    child.wait(timeout=5)
                    if server is not None: server.join(timeout=2)

    def test_owned_process_does_not_waive_io_parse_errors_or_read_a_reaped_pid(self):
        with tempfile.TemporaryDirectory() as d:
            proc = Path(d)
            child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
            try:
                base = proc / str(child.pid); base.mkdir()
                fields = ["0"] * 22; fields[0] = "S"; fields[19] = "101"; fields[21] = "5"
                (base / "stat").write_text(f"{child.pid} (writer) " + " ".join(fields))
                (base / "io").write_text("write_bytes: invalid\n")
                row = ab.owned_process_sample(child, proc)
                self.assertEqual(row["io_error"], "ValueError")
                self.assertNotIn("terminal", row)
                self.assertNotIn("exit_confirmation", row)
            finally:
                if child.poll() is None: child.kill()
                child.wait(timeout=5)
            # This fake numeric PID now contains apparently valid resources.
            # The owned Popen is reaped, so those files must never be consulted.
            (base / "io").write_text("write_bytes: 10\n")
            row = ab.owned_process_sample(child, proc)
            self.assertTrue(row["reaped"])
            self.assertNotIn("start_ticks", row)

    def test_parent_wait_pause_past_exit_bound_never_waives_real_proc_gap(self):
        for gap in ("stat", "io"):
            with self.subTest(gap=gap), tempfile.TemporaryDirectory() as d:
                root = Path(d); ready = root / "ready"; release = root / "release"
                body = ("import os,pathlib,sys,time; ready=pathlib.Path(sys.argv[1]); release=pathlib.Path(sys.argv[2]); "
                        "ready.write_text('1');\nwhile not release.exists(): time.sleep(.001)\nos._exit(0)\n")
                child = subprocess.Popen([sys.executable, "-c", body, str(ready), str(release)])
                try:
                    until = time.monotonic() + 5
                    while not ready.exists():
                        self.assertIsNone(child.poll())
                        self.assertLess(time.monotonic(), until)
                        time.sleep(.001)
                    proc = root / "proc"; base = proc / str(child.pid); base.mkdir(parents=True)
                    if gap == "io":
                        fields = ["0"] * 22; fields[0] = "S"; fields[19] = "101"; fields[21] = "5"
                        (base / "stat").write_text(f"{child.pid} (writer) " + " ".join(fields))
                    real_wait = child.wait
                    def paused_wait(*args, **kwargs):
                        release.write_text("1")
                        time.sleep(.06)  # Actual parent pause; the child really exits.
                        return real_wait(*args, **kwargs)
                    with patch.object(child, "wait", new=paused_wait):
                        row = ab.owned_process_sample(child, proc)
                    confirmation = row["exit_confirmation"]
                    self.assertEqual(confirmation["state"], "exit-confirmation-overrun")
                    self.assertGreater(confirmation["elapsed_ns"], confirmation["limit_ns"])
                    self.assertEqual(confirmation["returncode"], 0)
                    self.assertTrue(row["reaped"])
                    self.assertNotIn("terminal", row)
                    if gap == "stat":
                        self.assertEqual(row["error"], "EXITED")
                    else:
                        self.assertEqual(row["io_error"], "FileNotFoundError")
                    # Even though the overrun remains a gap, this owned child is
                    # reaped and its numeric PID must never be followed again.
                    again = ab.owned_process_sample(child, proc)
                    self.assertTrue(again["reaped"])
                    self.assertTrue(again["terminal"])
                    self.assertNotIn("start_ticks", again)
                finally:
                    if child.poll() is None: child.kill()
                    child.wait(timeout=5)

    @unittest.skipUnless(sys.platform == "linux", "Linux dumpability controls proc IO permissions")
    def test_real_proc_io_permission_denied_is_live_gap_or_confirmed_owned_exit(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); ready = root / "ready"; release = root / "release"
            body = ("import ctypes,os,pathlib,sys,time; "
                    "assert ctypes.CDLL(None).prctl(4,0,0,0,0)==0; "
                    "ready=pathlib.Path(sys.argv[1]); release=pathlib.Path(sys.argv[2]); ready.write_text('1');\n"
                    "while not release.exists(): time.sleep(.001)\ntime.sleep(.001)\nos._exit(0)\n")
            child = subprocess.Popen([sys.executable, "-c", body, str(ready), str(release)])
            try:
                until = time.monotonic() + 5
                while not ready.exists():
                    self.assertIsNone(child.poll())
                    self.assertLess(time.monotonic(), until)
                    time.sleep(.001)
                probe = ab.process_sample(child.pid)
                if probe.get("io_error") != "PermissionError":
                    self.skipTest("proc IO is not denied after PR_SET_DUMPABLE=0; environment may grant CAP_SYS_PTRACE")
                live = ab.owned_process_sample(child)
                self.assertEqual(live["io_error"], "PermissionError")
                self.assertNotIn("terminal", live)
                self.assertEqual(live["exit_confirmation"]["state"], "still-live-at-bound")
                self.assertIsNone(child.poll())
                real_sample = ab.process_sample
                def release_after_real_io_read(pid, proc):
                    row = real_sample(pid, proc)
                    self.assertEqual(row["io_error"], "PermissionError")
                    release.write_text("1")
                    return row
                # Only add a release notification after the actual proc read;
                # Popen.poll/wait, process ownership and all IO remain real.
                with patch.object(ab, "process_sample", side_effect=release_after_real_io_read):
                    ended = ab.owned_process_sample(child)
                self.assertEqual(ended["io_error"], "PermissionError")
                self.assertTrue(ended["terminal"])
                self.assertEqual(ended["exit_confirmation"]["state"], "owned-child-exit-confirmed")
                self.assertEqual(ended["exit_confirmation"]["returncode"], 0)
                self.assertEqual(ended["exit_confirmation"]["limit_ns"], 50_000_000)
            finally:
                if child.poll() is None: child.kill()
                child.wait(timeout=5)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO fixture requires POSIX")
    def test_pid_reuse_between_real_proc_fixture_reads_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            proc = Path(d); base = proc / "123"; base.mkdir()
            os.mkfifo(base / "stat"); os.mkfifo(base / "io")
            fields = ["0"] * 22; fields[0] = "S"; fields[19] = "101"; fields[21] = "5"
            first = "123 (writer) " + " ".join(fields)
            fields[19] = "102"; last = "123 (writer) " + " ".join(fields)
            def writer():
                with (base / "stat").open("w") as f: f.write(first)
                # The IO read guarantees the first stat descriptor has closed.
                with (base / "io").open("w") as f: f.write("write_bytes: 10\n")
                with (base / "stat").open("w") as f: f.write(last)
            worker = threading.Thread(target=writer, daemon=True); worker.start()
            row = ab.process_sample(123, proc); worker.join(timeout=2)
            self.assertFalse(worker.is_alive())
            self.assertEqual(row["error"], "PID_REUSED")

    @unittest.skipUnless(sys.platform == "linux", "subprocess limits and SIGKILL are Linux-only")
    def test_alternating_barriers_recovery_hashes_fresh_output_and_no_original_mutation(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); binary = fake_binary(root / "driver")
            original = root / "old-evidence"; original.mkdir(); (original / "keep").write_bytes(b"unchanged")
            settings = args(root, binary); settings.before = binary
            summary = ab.run(settings)
            self.assertTrue(summary["ok"], summary)
            self.assertEqual(summary["completed_groups"], 12)
            self.assertTrue(summary["recovery"]["after"]["ok"])
            self.assertTrue(summary["recovery"]["before"]["ok"])
            self.assertEqual((original / "keep").read_bytes(), b"unchanged")
            manifest = json.loads((settings.output / "manifest.json").read_text())
            self.assertFalse(manifest["status_background_load"])
            names = sorted(settings.output.glob("round-*.json"))
            self.assertEqual(len(names), 12)
            # On round 1 three writers run before single and before runs before after.
            self.assertLess((settings.output / "round-01-writers-3-before/start.json").stat().st_mtime_ns,
                            (settings.output / "round-01-writers-1-before/start.json").stat().st_mtime_ns)
            for line in (settings.output / "SHA256SUMS").read_text().splitlines():
                digest, name = line.split("  ", 1)
                self.assertEqual(ab.sha256(settings.output / name), digest)
            with self.assertRaisesRegex(ValueError, "FRESH_OUTPUT"):
                ab.run(settings)

    @unittest.skipUnless(sys.platform == "linux", "Linux process groups")
    def test_failed_group_is_preserved_and_does_not_run_recovery(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); summary = ab.run(args(root, fake_binary(root / "driver", "overflow")))
            self.assertFalse(summary["ok"])
            self.assertIn("RAW_QUEUE_OVERFLOW", summary["error"])
            self.assertEqual(summary["completed_groups"], 1)
            self.assertEqual(summary["recovery"], {})
            self.assertTrue((root / "new-output/round-00-writers-1-after/writer-0/result.json").exists())

    @unittest.skipUnless(sys.platform == "linux", "Linux process groups")
    def test_real_subprocess_missing_proc_data_remains_missing_not_zero(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); output = root / "output"; output.mkdir()
            proc = root / "empty-proc"; proc.mkdir()
            row = ab.run_batch(fake_binary(root / "driver"), output / "batch", 1, 30,
                               output, 100000000, time.monotonic() + 10, .01, proc)
            self.assertIsNone(row["simultaneous_rss_peak_bytes"])
            self.assertFalse(row["resource_diagnostic_complete"])
            self.assertIn("process:EXITED", row["resource_gaps"])
            self.assertTrue(row["writers"][0]["ok"])
            row.update(variant="after", writer_count=1)
            self.assertIsNone(ab.summarize_groups([row], ["after"])[0]["median_group_peak_rss_bytes"])

    @unittest.skipUnless(sys.platform == "linux", "Linux process groups")
    def test_timeout_cleans_own_group_and_recovery_refuses_mutation(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); binary = fake_binary(root / "driver", "hang")
            output = root / "timeout"; output.mkdir()
            with self.assertRaisesRegex(RuntimeError, "WALL_TIME_LIMIT"):
                ab.run_batch(binary, output / "batch", 1, 30, output, 100000000, time.monotonic() + .5, .01)
            pid = json.loads((output / "batch/writer-0/ready.json").read_text())["pid"]
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            mutate = fake_binary(root / "mutate-driver", "mutate")
            with self.assertRaisesRegex(ValueError, "RECOVERY_MUTATED"):
                ab.run_recovery(mutate, output / "recovery", output, 100000000, time.monotonic() + 10)
            accepted = fake_binary(root / "ordinary-accepted-driver", "ordinary-accepted")
            with self.assertRaisesRegex(ValueError, "RECOVERY_ORDINARY_READER_CONTRACT"):
                ab.run_recovery(accepted, output / "ordinary-accepted", output, 100000000, time.monotonic() + 10)

    def test_capacity_and_fresh_directory_rejected_before_children(self):
        if sys.platform != "linux":
            self.skipTest("Linux orchestration")
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); settings = args(root, fake_binary(root / "driver"))
            settings.max_output_bytes = 128 * 1024 ** 2
            with self.assertRaisesRegex(ValueError, "PLANNED_CAPACITY"):
                ab.run(settings)
            self.assertFalse(settings.output.exists())
            settings.max_output_bytes = 2 * 1024 ** 3
            settings.output.symlink_to(root / "missing")
            with self.assertRaisesRegex(ValueError, "FRESH_OUTPUT"):
                ab.run(settings)


if __name__ == "__main__":
    unittest.main()
