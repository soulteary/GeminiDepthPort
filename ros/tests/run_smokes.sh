#!/usr/bin/env bash
set -euo pipefail
python3 /ws/ros/tests/smoke_ros.py \
  --library /opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so \
  --namespace /gdp_synthetic_smoke
python3 /ws/ros/tests/smoke_ros.py \
  --library /opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so \
  --namespace /gdp_synthetic_global_smoke --stamp-source global --observed-timing
python3 /ws/ros/tests/smoke_aux.py \
  --library /opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so \
  --namespace /gdp_synthetic_aux_smoke

python3 /ws/ros/tests/smoke_imu.py
python3 /ws/ros/tests/smoke_context.py

python3 /ws/ros/tests/diagnostics_smoke.py

exec python3 /ws/ros/tests/multicamera_static_tf.py \
  --library /opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so
