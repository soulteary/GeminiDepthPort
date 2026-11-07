"""Check repository Markdown navigation and shell examples without executing them."""
from collections import Counter
from pathlib import Path
import re
import subprocess
import unittest
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]


def documents():
    paths = list(ROOT.glob("*.md"))
    for directory in ("docs", "examples", "crates", "ros", "tools", "tests"):
        paths.extend((ROOT / directory).rglob("*.md"))
    return sorted(paths)


def heading_anchors(path):
    # GitHub heading IDs omit punctuation and suffix duplicate headings.
    source = re.sub(r"^```.*?^```[^\n]*", "", path.read_text(), flags=re.M | re.S)
    seen = Counter()
    anchors = set(re.findall(r'''<a\s+(?:id|name)=["']([^"']+)''', source))
    for heading in re.findall(r"^#{1,6}\s+(.+?)\s*#*$", source, re.M):
        slug = re.sub(r"[^\w\-\s]", "", heading.lower()).replace(" ", "-")
        count = seen[slug]
        seen[slug] += 1
        anchors.add(slug + (f"-{count}" if count else ""))
    return anchors


class Documentation(unittest.TestCase):
    def test_local_links_and_heading_anchors(self):
        anchors = {}
        for path in documents():
            for target in re.findall(r"\]\(([^\s)]+)\)", path.read_text()):
                if re.match(r"[A-Za-z][A-Za-z0-9+.-]*:", target):
                    continue
                filename, _, fragment = target.partition("#")
                resolved = (path.parent / unquote(filename)).resolve() if filename else path
                with self.subTest(document=path.relative_to(ROOT), target=target):
                    self.assertTrue(resolved.is_relative_to(ROOT), "link escapes repository")
                    self.assertTrue(resolved.exists(), "missing local link target")
                    if fragment and resolved.suffix == ".md" and resolved.is_file():
                        if resolved not in anchors:
                            anchors[resolved] = heading_anchors(resolved)
                        self.assertIn(unquote(fragment), anchors[resolved], "missing heading anchor")

    def test_user_docs_and_history_are_reachable_from_entry_points(self):
        pending = [ROOT / "README.md", ROOT / "docs/README.md"]
        visited = set()
        while pending:
            path = pending.pop().resolve()
            if path in visited:
                continue
            visited.add(path)
            for target in re.findall(r"\]\(([^\s)]+)\)", path.read_text()):
                if re.match(r"[A-Za-z][A-Za-z0-9+.-]*:", target):
                    continue
                filename = unquote(target.partition("#")[0])
                resolved = (path.parent / filename).resolve() if filename else path
                if resolved.is_relative_to(ROOT) and resolved.suffix == ".md" and resolved.is_file():
                    pending.append(resolved)
        expected = {path.resolve() for directory in ("docs", "examples")
                    for path in (ROOT / directory).rglob("*.md")}
        missing = sorted(str(path.relative_to(ROOT)) for path in expected - visited)
        self.assertEqual(missing, [], "documents need an entry in the current or historical index")

    def test_shell_examples_parse_without_execution(self):
        for path in documents():
            for block in re.findall(r"```(?:sh|bash)\n(.*?)```", path.read_text(), re.S):
                with self.subTest(document=path.relative_to(ROOT), block=block):
                    result = subprocess.run(["bash", "-n"], input=block, text=True,
                                            capture_output=True, check=False)
                    self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
