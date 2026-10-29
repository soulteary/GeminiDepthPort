#!/usr/bin/env bash
set -euo pipefail

coverage_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$coverage_root"
coverage_mode=${1:-all}
case "$coverage_mode" in
  all) coverage_scopes=(unit combined) ;;
  unit|combined) coverage_scopes=("$coverage_mode") ;;
  *) echo "usage: $0 [unit|combined|all]" >&2; exit 2 ;;
esac

for coverage_scope in "${coverage_scopes[@]}"; do
  coverage_report="$coverage_root/target/coverage-reports/$coverage_scope"
  mkdir -p "$coverage_report"
  # Remove only this script's previous outputs before any fallible preflight.
  # A failed rerun must not leave an apparently successful old report behind.
  rm -f -- "$coverage_report/raw.lcov" "$coverage_report/raw.json" \
    "$coverage_report/production.lcov" "$coverage_report/summary.json" \
    "$coverage_report/summary.md" "$coverage_report/index.html" \
    "$coverage_report/external-tests.json" "$coverage_report/workspace-metadata.json"
done

if [[ $(cargo +1.90.0 llvm-cov --version) != "cargo-llvm-cov 0.9.1" ]]; then
  echo "coverage requires cargo-llvm-cov 0.9.1" >&2
  exit 1
fi

for coverage_scope in "${coverage_scopes[@]}"; do
  (
    # Separate object/profile directories prevent unit and combined runs from
    # contaminating one another. Existing non-coverage builds are untouched.
    export CARGO_TARGET_DIR="$coverage_root/target/coverage-$coverage_scope"
    export CARGO_TERM_COLOR=never
    coverage_environment=$(cargo +1.90.0 llvm-cov show-env --sh)
    eval "$coverage_environment"
    cargo +1.90.0 llvm-cov clean --workspace
    coverage_report="$coverage_root/target/coverage-reports/$coverage_scope"
    mkdir -p "$coverage_report"

    if [[ "$coverage_scope" == unit ]]; then
      cargo +1.90.0 test --locked --workspace --lib --bins
    else
      cargo +1.90.0 test --locked --workspace
      cargo +1.90.0 build --locked --workspace --bins --examples
      coverage_external_args=()
      if [[ ${GDP_COVERAGE_REAL_COMPOSE:-0} == 1 ]]; then
        coverage_external_args+=(--real-compose)
      fi
      python3 scripts/testing/run_external_tests.py \
        --bin-dir "$CARGO_TARGET_DIR/debug" \
        --report "$coverage_report/external-tests.json" \
        --require-profiles "${coverage_external_args[@]}"
    fi

    cargo +1.90.0 metadata --locked --no-deps --format-version 1 > "$coverage_report/workspace-metadata.json"
    cargo +1.90.0 llvm-cov report --workspace --lcov --output-path "$coverage_report/raw.lcov"
    cargo +1.90.0 llvm-cov report --workspace --json --output-path "$coverage_report/raw.json"
    python3 scripts/testing/production_coverage.py \
      --root "$coverage_root" --label "$coverage_scope" \
      --workspace-metadata "$coverage_report/workspace-metadata.json" \
      --input-lcov "$coverage_report/raw.lcov" --output-dir "$coverage_report"
    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
      cat "$coverage_report/summary.md" >> "$GITHUB_STEP_SUMMARY"
    fi
  )
done
