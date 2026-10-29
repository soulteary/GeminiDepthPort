"""Run Rust-backed Python suites, failing on missing binaries or skipped tests."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
SUITES = [
    ("management", "tests/rust/test_management.py", 33),
    ("native_gate", "tests/test_native_gate_acceptance.py", 7),
    ("sdk_grid", "tests/test_sdk_grid_comparison.py", 5),
    ("recording", "tools/recording/test_recording.py", 5),
]


def binary_environment(directory):
    directory = directory.resolve(strict=True)
    binaries = {
        "GDP_BINARY": directory / "geminidepthport",
        "GDP_MANAGER_PROGRAM": directory / "geminidepthport",
        "GDP_GATE_PROTOCOL_PROGRAM": directory / "examples/gate_protocol",
        "GDP_DESCRIPTOR_WARP": directory / "examples/descriptor_warp",
    }
    required = [*binaries.values(), directory / "capture-observer", directory / "offline-depth"]
    for binary in required:
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError(f"required Rust executable is missing or not executable: {binary}")
    return {**{key: str(value) for key, value in binaries.items()}, "GDP_RECORDING_BIN_DIR": str(directory)}


def profiles():
    template = os.environ.get("LLVM_PROFILE_FILE")
    if not template:
        raise ValueError("LLVM_PROFILE_FILE must be set when --require-profiles is used")
    directory = Path(template).parent
    return {str(path): (path.stat().st_mtime_ns, path.stat().st_size) for path in directory.glob("*.profraw")}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--require-profiles", action="store_true")
    parser.add_argument("--real-compose", action="store_true", help="also validate generated YAML with Docker Compose; no daemon or devices")
    args = parser.parse_args()
    results = []
    failed = False
    completed = False
    try:
        os.environ.update(binary_environment(args.bin_dir))
        if args.real_compose:
            subprocess.run(["docker", "compose", "version"], check=True, stdout=subprocess.DEVNULL)
            os.environ["GDP_REAL_COMPOSE"] = "1"
        os.chdir(ROOT)
        sys.path[:0] = [str(ROOT), str(ROOT / "tests"), str(ROOT / "tools/recording")]
        for name, relative, minimum in SUITES:
            before = profiles() if args.require_profiles else None
            spec = importlib.util.spec_from_file_location(f"gdp_coverage_{name}", ROOT / relative)
            module = importlib.util.module_from_spec(spec)
            sys.modules[spec.name] = module
            spec.loader.exec_module(module)
            suite = unittest.defaultTestLoader.loadTestsFromModule(module)
            if suite.countTestCases() < minimum:
                raise ValueError(f"{name}: fewer than the required {minimum} tests discovered")
            result = unittest.TextTestRunner(verbosity=2).run(suite)
            profile_changed = profiles() != before if args.require_profiles else None
            passed = result.wasSuccessful() and not result.skipped and profile_changed is not False
            results.append({"suite": name, "tests": result.testsRun, "skipped": len(result.skipped),
                            "errors": len(result.errors), "failures": len(result.failures),
                            "instrumented_profiles_changed": profile_changed, "passed": passed})
            if not passed:
                print(f"{name}: failed, skipped tests, or no instrumented Rust profile was produced", file=sys.stderr)
                failed = True
        completed = True
    except (OSError, ValueError, ImportError, subprocess.CalledProcessError) as error:
        print(f"external Rust tests failed: {error}", file=sys.stderr)
        failed = True
    finally:
        passed = completed and not failed and len(results) == len(SUITES) and all(r["passed"] for r in results)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps({"passed": passed, "suites": results}, indent=2) + "\n")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
