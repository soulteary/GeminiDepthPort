#!/usr/bin/env bash
# Prepare independent source snapshots and build both with identical toolchain.
set -euo pipefail
umask 077
if [[ $# != 2 ]]; then
  echo 'usage: build.sh SDK_GIT_CHECKOUT NEW_ABSOLUTE_OUTPUT_DIRECTORY' >&2
  exit 2
fi
sdk_git=$(cd "$1" && pwd)
output=$2
case "$output" in /*) ;; *) echo 'absolute output required' >&2; exit 2 ;; esac
[[ ! -e "$output" ]] || { echo 'output must be new' >&2; exit 2; }
base=575347ca742d219ef61b1e214e97043f17f48d23
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
git -C "$sdk_git" cat-file -e "$base^{commit}"
mkdir -m 700 "$output"
git -C "$sdk_git" archive --format=tar "$base" > "$output/sdk-base.tar"
# Experimental builder only. It never replaces capture.Dockerfile or vendor SDK.
docker run --rm --init --network none \
  -v "$repo:/work:ro" -v "$output:/experiment" \
  gdp-sdk-diagnostics-build:local -c '
    set -euo pipefail
    mkdir /experiment/before-source /experiment/after-source
    tar --touch -xf /experiment/sdk-base.tar -C /experiment/before-source
    cp -R /experiment/before-source/. /experiment/after-source/
    git -C /experiment/after-source apply --check /work/experiments/sdk-2.10.6-diagnostics/sdk-diagnostics.patch
    git -C /experiment/after-source apply /work/experiments/sdk-2.10.6-diagnostics/sdk-diagnostics.patch
    cp -R /work/experiments/sdk-2.10.6-diagnostics /experiment/harness
    for variant in before after; do
      cmake -S /experiment/harness -B /experiment/$variant \
        -DSDK_SOURCE=/experiment/$variant-source -DCMAKE_BUILD_TYPE=Release
      cmake --build /experiment/$variant --parallel 4
    done
  '
