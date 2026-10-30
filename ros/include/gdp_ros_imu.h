// SPDX-License-Identifier: Apache-2.0
#ifndef GDP_ROS_IMU_H
#define GDP_ROS_IMU_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#define GDP_ROS_IMU_ABI_VERSION 1u
#define GDP_ROS_IMU_ACCEL 1u
#define GDP_ROS_IMU_GYRO 2u
#define GDP_ROS_IMU_SYSTEM_TIME 1u
#define GDP_ROS_IMU_GLOBAL_TIME 2u
/* xyz: accel m/s^2 or gyro rad/s; no orientation/extrinsic conversion. */
typedef struct gdp_ros_imu_frame {
 uint32_t abi_version,struct_size,kind,timestamp_source;
 int32_t stamp_sec;uint32_t stamp_nanosec;
 uint64_t sequence,device_timestamp_us,system_timestamp_us,global_timestamp_us;
 double xyz[3];
} gdp_ros_imu_frame;
// All strings/frames borrowed for the call; create copies them. Same-thread use.
int32_t gdp_ros_imu_create(const char *node,const char *ns,const char *accel_topic,const char *gyro_topic,const char *accel_frame,const char *gyro_frame,void **out);
int32_t gdp_ros_imu_publish(void *handle,const gdp_ros_imu_frame *frame);
void gdp_ros_imu_destroy(void *handle);
#ifdef __cplusplus
}
#endif
#endif
