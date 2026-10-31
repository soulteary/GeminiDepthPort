#!/usr/bin/env bash
# Run from any directory. No local base image, production SDK update or gate override.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
image="${1:?usage: build_runtime.sh IMAGE_TAG [linux/amd64|linux/arm64]}"
platform="${2:-linux/amd64}"
case "$platform" in linux/amd64|linux/arm64) ;; *) echo 'unsupported platform' >&2; exit 2;; esac
revision="$(git rev-parse HEAD)"
# The service/ROS/recorder represented by gdp_revision must match the checkout.
git diff --quiet HEAD -- Cargo.toml Cargo.lock src crates ros runtime native/usb-recovery scripts/capture-runtime
python3 experiments/sdk-2.10.6-diagnostics/experiment_runtime.py verify-patch \
  --patch experiments/sdk-2.10.6-diagnostics/sdk-diagnostics.patch
docker build --platform "$platform" --build-arg "GDP_REVISION=$revision" \
  -f experiments/sdk-2.10.6-diagnostics/capture.Dockerfile -t "$image" .
image_id="$(docker image inspect --format '{{.Id}}' "$image")"
docker run --rm --platform "$platform" --network none --read-only --cap-drop ALL \
  --tmpfs /tmp:rw,nosuid,nodev,size=128m "$image_id" \
  python3 /opt/gdp-experiment/experiment_runtime.py smoke
docker image inspect --format '{{.Id}} {{.Architecture}} {{json .Config.Labels}}' "$image_id"
