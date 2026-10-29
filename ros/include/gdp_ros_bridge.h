/* SPDX-License-Identifier: Apache-2.0 */
#ifndef GDP_ROS_BRIDGE_H
#define GDP_ROS_BRIDGE_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
#define GDP_ROS_ABI_VERSION 1u
#define GDP_ROS_ENCODING_YUYV 1u
#define GDP_ROS_ENCODING_DEPTH16 2u
typedef struct gdp_ros_config {
    uint32_t abi_version;
    uint32_t struct_size;
    const char *node_name;
    const char *namespace_name;
    const char *image_topic;
    const char *camera_info_topic;
    const char *metadata_topic;
    const char *frame_id;
    uint32_t width;
    uint32_t height;
    const char *distortion_model;
    double k[9];
    double r[9];
    double p[12];
    const double *d;
    uint32_t d_len;
} gdp_ros_config;
typedef struct gdp_ros_frame {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t encoding;
    uint32_t width;
    uint32_t height;
    uint32_t step;
    uint32_t is_bigendian;
    int32_t stamp_sec;
    uint32_t stamp_nanosec;
    uint64_t sequence;
    uint32_t has_device_timestamp;
    uint32_t has_system_timestamp;
    uint64_t device_timestamp_us;
    uint64_t system_timestamp_us;
    uint32_t has_depth_scale;
    uint32_t normalized;
    float source_depth_scale_mm;
    float output_depth_scale_mm;
    const uint8_t *data;
    uint64_t data_len;
} gdp_ros_frame;
/* All pointer spans must be readable for their declared sizes for the duration
 * of the call. Strings are NUL-terminated. No pointer is retained after return.
 * 0=success; 1=invalid argument; 2=ROS/runtime exception; 3=allocation failure.
 * Destroy must be called on the same thread after all publish calls finish.
 * A publish error may follow partial publication; no atomic multi-topic claim.
 */
uint32_t gdp_ros_abi_version(void);
int32_t gdp_ros_create(const gdp_ros_config *config, void **out_handle);
// Optional session extension: bind exactly once before the first publish.
int32_t gdp_ros_set_session_id(void *handle, const char *session_id);
int32_t gdp_ros_publish(void *handle, const gdp_ros_frame *frame);
/* Optional extension, retaining ABI v1 frame layout. source: 1=system, 2=global.
 * global_timestamp_us=0 means unavailable; source=global then fails closed. */
int32_t gdp_ros_publish_timestamped(void *handle, const gdp_ros_frame *frame,
                                  uint64_t global_timestamp_us, uint32_t source);
/* Optional timing extension: sdk_return_unix_ns is sampled by the SDK waiter.
 * 0 means unavailable. It does not change the frame's timestamp mapping. */
int32_t gdp_ros_publish_observed(void *handle, const gdp_ros_frame *frame,
                               uint64_t global_timestamp_us, uint32_t source,
                               uint64_t sdk_return_unix_ns, uint32_t sdk_system_clock_verified);
void gdp_ros_destroy(void *handle);
#ifdef __cplusplus
}
#endif
#endif
