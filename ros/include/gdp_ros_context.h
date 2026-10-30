// SPDX-License-Identifier: Apache-2.0
#ifndef GDP_ROS_CONTEXT_H
#define GDP_ROS_CONTEXT_H
#include "gdp_ros_bridge.h"
#include "gdp_ros_imu.h"
#ifdef __cplusplus
extern "C" {
#endif
// Optional v1 extension. Explicit lease, same library/thread, no global registry.
// Publishers retain the context after the caller destroys its lease.
uint32_t gdp_ros_context_abi_version(void);
int32_t gdp_ros_context_create(void **out);
void gdp_ros_context_destroy(void *context);
int32_t gdp_ros_create_in_context(void *context, const gdp_ros_config *config, void **out);
int32_t gdp_ros_imu_create_in_context(void *context, const char *node,const char *ns,const char *accel_topic,const char *gyro_topic,const char *accel_frame,const char *gyro_frame,void **out);
#ifdef __cplusplus
}
#endif
#endif
