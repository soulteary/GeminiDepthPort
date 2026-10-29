"""Report Rust production *line* coverage without counting inline test code.

LLVM's raw JSON/LCOV remain the authority for functions, regions and branches.
This report deliberately emits only DA/LF/LH: changing line ranges cannot
correctly recalculate those other metrics. No Rust source is rewritten.
"""

import argparse
from collections import defaultdict
import hashlib
import html
import json
from pathlib import Path
import sys

from tree_sitter import Language, Parser
import tree_sitter_rust


def nodes(root):
    pending = [root]
    while pending:
        node = pending.pop()
        yield node
        pending.extend(reversed(node.children))


def test_lines(source):
    """Return exclusively-test and mixed line numbers using the Rust AST.

    Only exact cfg(test) and built-in #[test] functions are removed. Unknown
    cfg expressions are retained. A line with any production token survives,
    even when it also contains test code. Comments/strings cannot create attrs.
    """
    tree = Parser(Language(tree_sitter_rust.language())).parse(source)
    if tree.root_node.has_error:
        raise ValueError("Rust syntax is not understood by the pinned coverage parser")
    excluded = bytearray(len(source))
    comments = bytearray(len(source))
    containers = {
        "source_file", "declaration_list", "block", "field_declaration_list",
        "enum_variant_list", "ordered_field_declaration_list",
    }
    for node in nodes(tree.root_node):
        if node.type in {"line_comment", "block_comment"}:
            comments[node.start_byte:node.end_byte] = b"\1" * (node.end_byte - node.start_byte)
        if node.type != "attribute_item":
            continue
        attr = b"".join(node.text.split())
        if attr not in {b"#[cfg(test)]", b"#[test]"}:
            continue
        if node.parent.type == "field_initializer":
            item = node.parent
        elif node.parent.type in containers:
            item = node.next_named_sibling
            while item is not None and item.type in {"attribute_item", "line_comment", "block_comment"}:
                item = item.next_named_sibling
        else:
            raise ValueError(f"unsupported test attribute location: {node.parent.type}")
        if item is None:
            raise ValueError("test attribute has no associated Rust item")
        if attr == b"#[test]" and item.type != "function_item":
            raise ValueError("built-in test attribute is not attached to a function")
        end = item.end_byte
        # These separators belong to the annotated field/statement, not to a
        # following production item on the same line.
        if item.next_sibling is not None and item.next_sibling.type in {",", ";"}:
            end = item.next_sibling.end_byte
        excluded[node.start_byte:end] = b"\1" * (end - node.start_byte)
    only_test, mixed = set(), set()
    start = 0
    # LLVM source coordinates advance on LF, not Unicode line separators (or
    # the broader set understood by Python str.splitlines()).
    for number, line in enumerate(source.split(b"\n"), 1):
        end = min(start + len(line) + 1, len(source))
        if any(excluded[start:end]):
            production = any(
                byte not in b" \t\r\n" and not excluded[i] and not comments[i]
                for i, byte in enumerate(source[start:end], start)
            )
            (mixed if production else only_test).add(number)
        start = end
    return only_test, mixed


def read_lcov(text):
    """Read executable line counts; do not reuse LLVM's unfiltered totals."""
    files = defaultdict(dict)
    current = None
    for line in text.splitlines():
        if line.startswith("SF:"):
            if current is not None:
                raise ValueError("LCOV record is missing end_of_record")
            current = line[3:]
            if not current:
                raise ValueError("LCOV source path is empty")
        elif line.startswith("DA:"):
            if current is None:
                raise ValueError("LCOV line data has no source file")
            fields = line[3:].split(",")
            if len(fields) not in (2, 3):
                raise ValueError("invalid LCOV DA record")
            number, count = map(int, fields[:2])
            if number < 1 or count < 0:
                raise ValueError("invalid LCOV line number or execution count")
            files[current][number] = files[current].get(number, 0) + count
        elif line == "end_of_record":
            if current is None:
                raise ValueError("LCOV end_of_record has no source file")
            current = None
        # FN/FNDA/FNF/FNH and branch records intentionally remain raw-only.
    if current is not None:
        raise ValueError("LCOV input ended inside a record")
    return files


def line_summary(counts):
    found = len(counts)
    hit = sum(count > 0 for count in counts.values())
    return {"found": found, "hit": hit, "percent": round(100 * hit / found, 4) if found else None}


def package_name(path):
    return path.parts[1] if path.parts[0] == "crates" else "geminidepthport"


def metadata_packages(metadata):
    members = set(metadata["workspace_members"])
    packages = [package for package in metadata["packages"] if package["id"] in members]
    if not members or {package["id"] for package in packages} != members:
        raise ValueError("Cargo metadata does not describe all workspace members")
    return sorted(
        [(Path(package["manifest_path"]).resolve().parent, package["name"]) for package in packages],
        key=lambda item: len(item[0].parts), reverse=True,
    )


def production_report(raw, root, label, metadata=None):
    root = root.resolve()
    workspace = metadata_packages(metadata) if metadata is not None else None
    records, excluded_files = [], []
    for filename, counts in sorted(raw.items()):
        source_path = Path(filename)
        if not source_path.is_absolute():
            source_path = root / source_path
        source_path = source_path.resolve(strict=True)
        try:
            relative = source_path.relative_to(root)
        except ValueError:
            excluded_files.append({"path": filename, "reason": "outside repository"})
            continue
        # Scope includes every production Rust source file in this workspace;
        # no adapter, CLI, unsafe module or error path is excluded.
        production = relative.parts[0] == "src" or (
            len(relative.parts) >= 4 and relative.parts[0] == "crates" and relative.parts[2] == "src"
        )
        if source_path.suffix != ".rs" or not production:
            excluded_files.append({"path": relative.as_posix(), "reason": "outside src/ or crates/*/src/"})
            continue
        source = source_path.read_bytes()
        try:
            only_test, mixed = test_lines(source)
        except ValueError as error:
            raise ValueError(f"{relative}: {error}") from error
        source_lines = [line.decode("utf-8").removesuffix("\r") for line in source.split(b"\n")]
        if source_lines and not source_lines[-1]:
            source_lines.pop()
        if any(number > len(source_lines) for number in counts):
            raise ValueError(f"{relative}: LCOV does not match the current source file")
        kept = {number: count for number, count in counts.items() if number not in only_test}
        package = package_name(relative)
        if workspace is not None:
            package = next((name for directory, name in workspace if source_path.is_relative_to(directory)), None)
            if package is None:
                raise ValueError(f"{relative}: source file is not in the Cargo workspace")
        records.append({
            "path": relative.as_posix(),
            "package": package,
            "source_sha256": hashlib.sha256(source).hexdigest(),
            "lines": line_summary(kept),
            "raw_lines": line_summary(counts),
            "excluded_test_lines": sorted(counts.keys() & only_test),
            "mixed_test_lines_retained": sorted(counts.keys() & mixed),
            "uncovered_lines": sorted(number for number, count in kept.items() if count == 0),
            "counts": kept,
            "source": source_lines,
        })
    if not records or not sum(r["lines"]["found"] for r in records):
        raise ValueError("no production Rust executable lines were found")
    packages = {}
    for record in records:
        total = packages.setdefault(record["package"], {"found": 0, "hit": 0})
        for key in ("found", "hit"):
            total[key] += record["lines"][key]
    if workspace is not None:
        missing = sorted({name for _, name in workspace} - packages.keys())
        if missing:
            raise ValueError(f"workspace packages missing from coverage: {', '.join(missing)}")
    total = {key: sum(p[key] for p in packages.values()) for key in ("found", "hit")}
    for item in [total, *packages.values()]:
        item["percent"] = round(100 * item["hit"] / item["found"], 4) if item["found"] else None
    summary = {
        "schema_version": 1,
        "label": label,
        "metric": "production Rust line coverage",
        "source_scope": ["src/**/*.rs", "crates/*/src/**/*.rs"],
        "exclusion_policy": "exact cfg(test) items and built-in test functions; mixed lines and unknown cfg retained",
        "raw_only_metrics": ["functions", "regions", "branches"],
        "parser_versions": {"tree-sitter": "0.25.2", "tree-sitter-rust": "0.24.2"},
        "lines": total,
        "packages": dict(sorted(packages.items())),
        "workspace_packages": sorted(name for _, name in workspace) if workspace is not None else None,
        "files": [{key: value for key, value in r.items() if key not in {"counts", "source"}} for r in records],
        "excluded_files": excluded_files,
    }
    return records, summary


def write_lcov(records):
    output = []
    for record in records:
        output.extend(["TN:", f"SF:{record['path']}"])
        output.extend(f"DA:{number},{count}" for number, count in sorted(record["counts"].items()))
        output.extend([f"LF:{record['lines']['found']}", f"LH:{record['lines']['hit']}", "end_of_record"])
    return "\n".join(output) + "\n"


def display_percent(value):
    return f"{value:.2f}%" if value is not None else "n/a"


def markdown_summary(summary):
    lines = [f"## Rust coverage: {summary['label']}", "",
             "Production line coverage; inline test-only code is excluded. Function, region and branch metrics remain raw-only.", "",
             "| Package | Covered lines | Executable lines | Line coverage |", "| --- | ---: | ---: | ---: |"]
    for name, counts in [*summary["packages"].items(), ("TOTAL", summary["lines"])]:
        lines.append(f"| {name} | {counts['hit']} | {counts['found']} | {display_percent(counts['percent'])} |")
    return "\n".join(lines) + "\n"


def html_report(records, summary):
    esc = html.escape
    parts = ["<!doctype html><meta charset='utf-8'>",
             f"<title>Rust production coverage: {esc(summary['label'])}</title>",
             "<style>body{font:16px system-ui;margin:2rem;color:#18212b}table{border-collapse:collapse}td,th{padding:.3rem .7rem;text-align:left;border-bottom:1px solid #ddd}pre{margin:0;white-space:pre-wrap}details{margin-top:1rem}.hit{background:#e8f5e9}.miss{background:#ffebee}.test{color:#68737d;background:#f4f5f6}.num{color:#68737d;min-width:4rem}a{color:#145c9e}</style>",
             f"<h1>Rust production line coverage: {esc(summary['label'])}</h1>",
             "<p>Exact <code>#[cfg(test)]</code> items and built-in test functions are excluded. Mixed production/test lines and unknown cfg expressions are retained. Functions, regions and branches are available only in <a href='raw.json'>raw LLVM JSON</a>.</p>",
             "<p><a href='production.lcov'>Production LCOV</a> · <a href='summary.json'>Summary JSON</a> · <a href='raw.lcov'>Raw LCOV</a></p>",
             "<table><tr><th>Package</th><th>Covered</th><th>Executable</th><th>Line coverage</th></tr>"]
    for name, count in [*summary["packages"].items(), ("TOTAL", summary["lines"])]:
        parts.append(f"<tr><td>{esc(name)}</td><td>{count['hit']}</td><td>{count['found']}</td><td>{display_percent(count['percent'])}</td></tr>")
    parts.append("</table><h2>Files</h2>")
    for record in records:
        count = record["lines"]
        parts.append(f"<details><summary>{esc(record['path'])} — {count['hit']}/{count['found']} ({display_percent(count['percent'])})</summary>")
        parts.append(f"<p>Excluded test lines: {len(record['excluded_test_lines'])}. Mixed lines retained: {len(record['mixed_test_lines_retained'])}.</p><table>")
        excluded = set(record["excluded_test_lines"])
        for number, source in enumerate(record["source"], 1):
            executions = record["counts"].get(number)
            cls = "test" if number in excluded else "hit" if executions else "miss" if executions == 0 else ""
            marker = "test-only" if number in excluded else "" if executions is None else str(executions)
            parts.append(f"<tr class='{cls}'><td class='num'>{number}</td><td>{marker}</td><td><pre>{esc(source)}</pre></td></tr>")
        parts.append("</table></details>")
    return "\n".join(parts) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-lcov", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--label", choices=["unit", "combined"], required=True)
    parser.add_argument("--workspace-metadata", type=Path, required=True,
                        help="output of cargo metadata --locked --no-deps --format-version 1")
    args = parser.parse_args()
    try:
        metadata = json.loads(args.workspace_metadata.read_text())
        records, summary = production_report(read_lcov(args.input_lcov.read_text()), args.root, args.label, metadata)
        args.output_dir.mkdir(parents=True, exist_ok=True)
        (args.output_dir / "production.lcov").write_text(write_lcov(records))
        (args.output_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        (args.output_dir / "summary.md").write_text(markdown_summary(summary))
        (args.output_dir / "index.html").write_text(html_report(records, summary))
        print(markdown_summary(summary))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"coverage report failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
