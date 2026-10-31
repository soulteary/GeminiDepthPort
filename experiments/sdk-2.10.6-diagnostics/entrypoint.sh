#!/usr/bin/env bash
set -eo pipefail
source /opt/ros/humble/setup.bash
if [ "$#" -gt 0 ]; then exec "$@"; fi
cd /tmp
python3 /opt/gdp-experiment/experiment_runtime.py validate
exec /usr/local/bin/capture-service --config /config/capture.json
