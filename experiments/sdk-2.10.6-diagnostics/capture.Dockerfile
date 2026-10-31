# Independent opt-in SDK experiment. Production capture.Dockerfile is untouched.
FROM rust:1.85.1-bullseye AS rust-build
WORKDIR /build
COPY Cargo.toml Cargo.lock driver_profiles.json ./
COPY src ./src
COPY crates ./crates
RUN cargo build --locked --release -p capture-probe --bin capture-service \
    && cargo build --locked --release -p offline-depth --example audit_imu_long_run

FROM ros:humble-ros-base-jammy AS runtime-base
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libusb-1.0-0 libudev1 libssl3 libgl1 \
    python3-yaml python3-numpy python3-opencv ros-humble-rclpy \
    ros-humble-sensor-msgs ros-humble-std-msgs ros-humble-geometry-msgs \
    ros-humble-tf2-msgs ros-humble-tf2-ros ros-humble-rmw-fastrtps-cpp \
    && rm -rf /var/lib/apt/lists/*

FROM runtime-base AS native-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    git build-essential cmake pkg-config binutils nasm \
    libusb-1.0-0-dev libudev-dev libssl-dev libgl1-mesa-dev \
    && rm -rf /var/lib/apt/lists/*
COPY experiments/sdk-2.10.6-diagnostics/sdk-diagnostics.patch /experiment/sdk-diagnostics.patch
COPY experiments/sdk-2.10.6-diagnostics/public-c-exports.txt /experiment/public-c-exports.txt
RUN set -e; git init /sdk; \
    git -C /sdk remote add origin https://github.com/orbbec/OrbbecSDK_v2.git; \
    fetched=0; for attempt in 1 2 3; do \
       if git -C /sdk fetch --depth 1 origin 575347ca742d219ef61b1e214e97043f17f48d23; then fetched=1; break; fi; \
       sleep 2; done; test "$fetched" = 1 \
    && git -C /sdk checkout --detach FETCH_HEAD \
    && test "$(git -C /sdk rev-parse HEAD)" = 575347ca742d219ef61b1e214e97043f17f48d23 \
    && echo '0db54c7c1bcafa9d18cd51bddcd4cf0d0c733c967ff0f9542f7796b770a0757c  /experiment/sdk-diagnostics.patch' | sha256sum -c - \
    && git -C /sdk apply --check /experiment/sdk-diagnostics.patch \
    && git -C /sdk apply /experiment/sdk-diagnostics.patch \
    && cmake -S /sdk -B /sdk-build -DCMAKE_BUILD_TYPE=Release \
       -DCMAKE_INSTALL_PREFIX=/opt/gdp-sdk-experiment -DOB_BUILD_MAIN_PROJECT=ON \
       -DOB_BUILD_EXAMPLES=OFF -DOB_BUILD_TOOLS=OFF -DOB_BUILD_TESTS=OFF \
       -DOB_BUILD_DOCS=OFF -DOB_INSTALL_EXAMPLES_SOURCE=OFF -DOB_INSTALL_LICENSES=ON \
       -DOB_BUILD_WITH_EXTENSIONS_COMMIT_HASH=OFF \
    && cmake --build /sdk-build --parallel 4 \
    && cmake --install /sdk-build \
    && cp "/sdk/End User License Agreement.txt" /opt/gdp-sdk-experiment/SDK-EULA.txt \
    && cp /sdk/extensions/license.txt /opt/gdp-sdk-experiment/extensions-LICENSE.txt \
    && mkdir -p /opt/gdp-experiment \
    && cp /experiment/sdk-diagnostics.patch /experiment/public-c-exports.txt /opt/gdp-experiment/
# Metadata/validator changes reuse the compiled fixed SDK layer, not host libraries.
COPY experiments/sdk-2.10.6-diagnostics/experiment_runtime.py /experiment/experiment_runtime.py
RUN python3 /experiment/experiment_runtime.py sdk-build-record --sdk-source /sdk \
       --patch /experiment/sdk-diagnostics.patch --exports /experiment/public-c-exports.txt \
       --output /opt/gdp-experiment/sdk-build.json
COPY ros /gdp-ros
RUN source /opt/ros/humble/setup.bash \
    && cmake -S /gdp-ros -B /gdp-ros-build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/gdp-ros \
    && cmake --build /gdp-ros-build --parallel 2 && cmake --install /gdp-ros-build
COPY native/usb-recovery /usb-recovery
RUN mkdir -p /opt/gdp-ros/bin \
    && c++ -std=c++17 -O2 -Wall -Wextra -Werror /usb-recovery/main.cpp \
       $(pkg-config --cflags --libs libusb-1.0) -o /opt/gdp-ros/bin/gdp-usb-recover

FROM runtime-base
ARG GDP_REVISION
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
COPY --from=native-build /opt/gdp-sdk-experiment /opt/gdp-sdk-experiment
COPY --from=native-build /opt/gdp-experiment /opt/gdp-experiment
COPY --from=native-build /opt/gdp-ros /opt/gdp-ros
COPY --from=rust-build /build/target/release/capture-service /usr/local/bin/capture-service
COPY --from=rust-build /build/target/release/examples/audit_imu_long_run /usr/local/bin/audit_imu_long_run
COPY runtime /app
RUN mv /app/health.py /app/ros_health.py
COPY scripts/capture-runtime/health.py /opt/gdp-experiment/base_health.py
COPY experiments/sdk-2.10.6-diagnostics/health.py /opt/gdp-experiment/health.py
COPY experiments/sdk-2.10.6-diagnostics/experiment_runtime.py /opt/gdp-experiment/experiment_runtime.py
COPY experiments/sdk-2.10.6-diagnostics/entrypoint.sh /opt/gdp-experiment/entrypoint.sh
COPY experiments/sdk-2.10.6-diagnostics/collect.py /opt/gdp-experiment/collect.py
COPY ros /opt/gdp-experiment/ros
ENV GDP_EXPERIMENT_REVISION=$GDP_REVISION \
    ROS_LOCALHOST_ONLY=0 RMW_IMPLEMENTATION=rmw_fastrtps_cpp \
    FASTRTPS_DEFAULT_PROFILES_FILE=/app/fastdds.xml ROS_LOG_DIR=/tmp/ros-logs \
    PYTHONDONTWRITEBYTECODE=1 HOME=/tmp ROS_HOME=/tmp/ros OB_GDP_DIAGNOSTICS=0 OB_GDP_DIAGNOSTICS_DIR=/sdk-diagnostics
RUN source /opt/ros/humble/setup.bash \
    && chmod +x /opt/gdp-experiment/entrypoint.sh \
    && chmod -R a+rX /opt/gdp-sdk-experiment /opt/gdp-ros /opt/gdp-experiment /app \
    && python3 /opt/gdp-experiment/experiment_runtime.py make-manifest \
    && python3 /opt/gdp-experiment/experiment_runtime.py smoke
LABEL org.opencontainers.image.revision=$GDP_REVISION \
    org.geminidepthport.capture-backend="rust-sdk-diagnostics-v1" \
    org.geminidepthport.sdk-experiment="sdk-2.10.6-diagnostics-v1" \
    org.geminidepthport.communication-profile="humble-fastdds-local-udp-v1"
ENTRYPOINT ["/opt/gdp-experiment/entrypoint.sh"]
