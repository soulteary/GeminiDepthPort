FROM gdp-coverage-90-env:local
RUN apt-get -o Acquire::Max-FutureTime=14400 update && apt-get install -y --no-install-recommends cmake pkg-config libudev-dev libssl-dev libgl1-mesa-dev nasm && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["bash"]
