#!/usr/bin/env bash
# Build the shared release driver and run the bounded three-writer comparison.
set -euo pipefail

writer_repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$writer_repo_root"
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  exec python3 scripts/validation/writer_long_run.py --help
fi
if [[ $(uname -s) != Linux ]]; then
  echo 'Linux is required; run this script on the recording host.' >&2
  exit 2
fi
for writer_arg in "$@"; do
  if [[ $writer_arg == --binary || $writer_arg == --binary=* ]]; then
    # A prebuilt release ELF lets root collect tracefs events without rebuilding
    # the repository or inheriting the invoking user's Rust environment.
    exec python3 scripts/validation/writer_long_run.py "$@"
  fi
done

# Keep compiler output outside the experiment's fresh root. The controller owns
# its output directory and performs the capacity and trace capability checks.
writer_build_root=$(mktemp -d "${TMPDIR:-/tmp}/gdp-writer-long-build.XXXXXX")
echo "Release build logs: $writer_build_root" >&2
cargo +1.90.0 test --locked --release -p capture-probe --lib --no-run \
  --message-format=json > "$writer_build_root/compiler.jsonl" 2> "$writer_build_root/build.log"
writer_binary=$(python3 - "$writer_build_root/compiler.jsonl" <<'PY'
import json
from pathlib import Path
import sys
matches = []
for line in Path(sys.argv[1]).read_text().splitlines():
    row = json.loads(line)
    if (row.get('reason') == 'compiler-artifact' and row.get('profile', {}).get('test') is True
            and row.get('target', {}).get('name') == 'capture_probe' and row.get('executable')):
        matches.append(Path(row['executable']).resolve(strict=True))
if len(matches) != 1 or not matches[0].is_file():
    raise SystemExit('Expected one capture-probe library test executable')
print(matches[0])
PY
)
exec python3 scripts/validation/writer_long_run.py --binary "$writer_binary" "$@"
