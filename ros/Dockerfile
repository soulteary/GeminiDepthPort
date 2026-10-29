FROM ros:humble-ros-base-jammy
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
COPY ros /ws/ros
COPY runtime/qos_contract.py runtime/pipeline_diagnostics.py runtime/host_pressure.py /ws/runtime/
RUN source /opt/ros/humble/setup.bash \
    && cmake -S /ws/ros -B /ws/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/gdp-ros \
    && cmake --build /ws/build --parallel 2 \
    && cmake --install /ws/build
COPY runtime/fastdds.xml /opt/gdp-ros/fastdds.xml
RUN chmod -R a+rX /ws/ros /opt/gdp-ros
ENV ROS_LOCALHOST_ONLY=0 \
    RMW_IMPLEMENTATION=rmw_fastrtps_cpp \
    FASTRTPS_DEFAULT_PROFILES_FILE=/opt/gdp-ros/fastdds.xml \
    ROS_LOG_DIR=/tmp/ros-logs
CMD ["bash", "/ws/ros/tests/run_smokes.sh"]
