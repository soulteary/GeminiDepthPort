import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from production_coverage import html_report, production_report, read_lcov, test_lines, write_lcov
from run_external_tests import binary_environment
import run_external_tests


class RustTestRanges(unittest.TestCase):
    def test_nested_module_stops_at_its_real_closing_brace(self):
        source = b'''fn before() {}
#[cfg(test)]
mod tests {
    mod nested {
        #[test]
        fn unit() { let text = "} #[cfg(test)] fn fake() {"; }
    }
}
fn after() {}
'''
        only_test, mixed = test_lines(source)
        self.assertEqual(only_test, set(range(2, 9)))
        self.assertEqual(mixed, set())

    def test_strings_comments_and_raw_identifiers_are_not_attributes(self):
        source = b'''fn real() {
    let raw = r#"#[cfg(test)] mod tests { }"#;
    consume(&raw);
    // #[cfg(test)] fn imaginary() {}
    let text = "#[test]";
}
'''
        self.assertEqual(test_lines(source), (set(), set()))

    def test_unknown_cfg_is_preserved(self):
        source = b'''#[cfg(any(test, feature = "public"))]
fn dual_use() {}
#[cfg(all(test, feature = "special"))]
fn unknown_cfg() {}
#[cfg_attr(test, inline)]
fn normal() {}
#[cfg(not(test))]
fn production_only() {}
'''
        self.assertEqual(test_lines(source), (set(), set()))

    def test_production_and_test_on_same_line_are_retained(self):
        source = b'''fn before() {} #[cfg(test)] fn helper() {} fn after() {}
#[test] fn unit() {}
'''
        only_test, mixed = test_lines(source)
        self.assertEqual(only_test, {2})
        self.assertEqual(mixed, {1})

    def test_test_only_fields_initializers_and_statements(self):
        source = b'''struct S {
    #[cfg(test)]
    gate: usize,
    real: usize,
}
fn value() {
    #[cfg(test)]
    let gate = 1;
    let s = S {
        #[cfg(test)]
        gate: gate,
        real: 2,
    };
    #[cfg(test)]
    if true { consume(gate); }
    consume(s);
}
'''
        only_test, mixed = test_lines(source)
        self.assertEqual(only_test, {2, 3, 7, 8, 10, 11, 14, 15})
        self.assertEqual(mixed, set())

    def test_adjacent_attributes_and_comments_do_not_hide_the_item(self):
        source = b'''#[cfg(test)]
#[allow(dead_code)]
// only used in a test
fn helper() {} // commentary
fn production() {}
'''
        self.assertEqual(test_lines(source), ({1, 2, 3, 4}, set()))

    def test_unparseable_source_fails_instead_of_guessing_ranges(self):
        with self.assertRaisesRegex(ValueError, "syntax"):
            test_lines(b"#[cfg(test)] mod broken {")


class ProductionLcov(unittest.TestCase):
    def test_recomputes_line_counts_and_does_not_export_raw_function_metrics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            source = root / "src/lib.rs"
            source.write_text('''fn production() { consume("<script>"); }
#[cfg(test)]
mod tests {
    #[test]
    fn unit() { production(); }
}
fn uncovered() {}
''')
            raw = read_lcov(f'''TN:
SF:{source}
FN:1,production
FN:5,tests::unit
FNDA:1,production
FNDA:100,tests::unit
FNF:2
FNH:2
DA:1,1
DA:5,100
DA:7,0
LF:3
LH:2
BRDA:5,0,0,100
BRF:1
BRH:1
end_of_record
''')
            records, summary = production_report(raw, root, "unit")
            self.assertEqual(summary["lines"], {"found": 2, "hit": 1, "percent": 50.0})
            self.assertEqual(summary["files"][0]["excluded_test_lines"], [5])
            self.assertEqual(summary["files"][0]["uncovered_lines"], [7])
            lcov = write_lcov(records)
            self.assertEqual(lcov, "TN:\nSF:src/lib.rs\nDA:1,1\nDA:7,0\nLF:2\nLH:1\nend_of_record\n")
            self.assertNotIn("FN", lcov)
            self.assertNotIn("BR", lcov)
            report = html_report(records, summary)
            self.assertIn("&lt;script&gt;", report)
            self.assertNotIn("<script>", report)
            self.assertIn("raw LLVM JSON", report)

    def test_merges_line_records_and_preserves_unexecuted_adapter_code(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/orbbec-sys/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn adapter() {}\nfn cleanup_error() {}\n")
            test = root / "crates/orbbec-sys/tests/integration.rs"
            test.parent.mkdir(parents=True)
            test.write_text("#[test] fn test() {}\n")
            raw = read_lcov(f"SF:{source}\nDA:1,0\nDA:2,0\nend_of_record\n"
                            f"SF:{source}\nDA:1,2\nend_of_record\n"
                            f"SF:{test}\nDA:1,10\nend_of_record\n")
            records, summary = production_report(raw, root, "combined")
            self.assertEqual(summary["packages"]["orbbec-sys"], {"found": 2, "hit": 1, "percent": 50.0})
            self.assertEqual(len(records), 1)
            self.assertEqual(summary["files"][0]["uncovered_lines"], [2])

    def test_mixed_line_is_kept_even_if_a_test_also_executed_on_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src/lib.rs"
            source.parent.mkdir()
            source.write_text("#[cfg(test)] fn test_helper() {} fn production() {}\n")
            _, summary = production_report({str(source): {1: 7}}, root, "unit")
            self.assertEqual(summary["lines"]["found"], 1)
            self.assertEqual(summary["files"][0]["mixed_test_lines_retained"], [1])

    def test_truncated_and_invalid_lcov_fail(self):
        for text in ("DA:1,1\n", "SF:a.rs\nDA:1,1\n", "SF:a.rs\nDA:0,1\nend_of_record\n",
                     "SF:a.rs\nDA:1,-1\nend_of_record\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                read_lcov(text)

    def test_source_mismatch_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src/lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")
            with self.assertRaisesRegex(ValueError, "current source"):
                production_report({str(source): {10: 1}}, root, "unit")

    def test_missing_workspace_package_is_rejected_and_names_come_from_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src/lib.rs"
            source.parent.mkdir()
            source.write_text("fn manager() {}\n")
            other = root / "crates/folder-name/src/lib.rs"
            other.parent.mkdir(parents=True)
            other.write_text("fn core() {}\n")
            metadata = {
                "workspace_members": ["root-id", "core-id"],
                "packages": [
                    {"id": "root-id", "name": "manager", "manifest_path": str(root / "Cargo.toml")},
                    {"id": "core-id", "name": "actual-core", "manifest_path": str(other.parents[1] / "Cargo.toml")},
                    {"id": "dependency-id", "name": "external", "manifest_path": "/outside/Cargo.toml"},
                ],
            }
            with self.assertRaisesRegex(ValueError, "workspace packages missing from coverage: actual-core"):
                production_report({str(source): {1: 1}}, root, "unit", metadata)
            _, summary = production_report({str(source): {1: 1}, str(other): {1: 0}}, root, "unit", metadata)
            self.assertEqual(set(summary["packages"]), {"manager", "actual-core"})
            self.assertEqual(summary["workspace_packages"], ["actual-core", "manager"])

    def test_unicode_separators_in_strings_do_not_change_llvm_line_numbers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src/lib.rs"
            source.parent.mkdir()
            text = 'fn value() {\n    consume("left\u2028right\u0085end");\n}\n#[test] fn unit() {}\n'
            source.write_text(text)
            records, summary = production_report({str(source): {2: 1, 4: 1}}, root, "unit")
            self.assertEqual(records[0]["source"], text.rstrip("\n").split("\n"))
            self.assertEqual(summary["files"][0]["excluded_test_lines"], [4])
            self.assertEqual(summary["lines"]["found"], 1)

    def test_required_example_cannot_silently_skip_external_suite(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("geminidepthport", "capture-observer", "offline-depth", "examples/gate_protocol"):
                binary = root / name
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_text("#!/bin/sh\nexit 0\n")
                binary.chmod(0o700)
            with self.assertRaisesRegex(ValueError, "descriptor_warp"):
                binary_environment(root)

    def test_external_import_error_records_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "broken.py").write_text("raise ImportError('synthetic unavailable module')\n")
            report = root / "external-tests.json"
            with patch.object(run_external_tests, "ROOT", root), \
                 patch.object(run_external_tests, "SUITES", [("broken", "broken.py", 1)]), \
                 patch.object(run_external_tests, "binary_environment", return_value={}), \
                 patch.object(run_external_tests.os, "chdir"), \
                 patch.object(sys, "path", sys.path.copy()), \
                 patch.object(sys, "argv", ["run_external_tests", "--bin-dir", str(root), "--report", str(report)]):
                self.assertEqual(run_external_tests.main(), 1)
            self.assertFalse(json.loads(report.read_text())["passed"])

    def test_failed_rerun_removes_only_generated_reports(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "scripts/testing/rust_coverage.sh"
            script.parent.mkdir(parents=True)
            shutil.copyfile(Path(__file__).with_name("rust_coverage.sh"), script)
            binary = root / "bin/cargo"
            binary.parent.mkdir()
            binary.write_text("#!/bin/sh\nexit 1\n")
            binary.chmod(0o700)
            report = root / "target/coverage-reports/unit"
            report.mkdir(parents=True)
            (report / "summary.json").write_text('{"old":true}')
            (report / "raw.lcov").write_text("old")
            (report / "notes.txt").write_text("preserve this file")
            result = subprocess.run(["bash", str(script), "unit"], capture_output=True,
                                    env={**os.environ, "PATH": f"{binary.parent}{os.pathsep}{os.environ['PATH']}"})
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((report / "summary.json").exists())
            self.assertFalse((report / "raw.lcov").exists())
            self.assertEqual((report / "notes.txt").read_text(), "preserve this file")


if __name__ == "__main__":
    unittest.main()
