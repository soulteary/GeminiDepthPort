FROM ubuntu:22.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake git ca-certificates python3 pkg-config binutils \
    libudev-dev libssl-dev libgl1-mesa-dev nasm \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["bash"]
