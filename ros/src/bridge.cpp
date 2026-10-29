// SPDX-License-Identifier: Apache-2.0
#include "gdp_ros_bridge.h"
#include "gdp_ros_validation.hpp"
#include "gdp_ros_session.hpp"
#include <rclcpp/rclcpp.hpp>
#include <sensor_msgs/msg/image.hpp>
#include <sensor_msgs/msg/camera_info.hpp>
#include <std_msgs/msg/string.hpp>
#include <algorithm>
#include <chrono>
#include <iomanip>
#include <limits>
#include <locale>
#include <memory>
#include <new>
#include <sstream>
#include <stdexcept>
#include <string>

namespace {
uint64_t unix_now_ns() {
  const auto ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::system_clock::now().time_since_epoch()).count();
  return ns > 0 ? static_cast<uint64_t>(ns) : 0;
}
struct Publisher {
  std::shared_ptr<rclcpp::Context> context;
  std::shared_ptr<rclcpp::Node> node;
  rclcpp::Publisher<sensor_msgs::msg::Image>::SharedPtr image;
  rclcpp::Publisher<sensor_msgs::msg::CameraInfo>::SharedPtr info;
  rclcpp::Publisher<std_msgs::msg::String>::SharedPtr metadata;
  sensor_msgs::msg::CameraInfo calibration;
  std::string session_id;
  bool published = false;
  uint64_t last_stamp = 0, last_sequence = 0;
  ~Publisher() {
    metadata.reset(); info.reset(); image.reset(); node.reset();
    if (context && context->is_valid()) { try { context->shutdown("bridge destroyed"); } catch (...) {} }
  }
};
std::string frame_metadata(const gdp_ros_frame &f, const std::string &frame_id, const std::string &session_id, uint64_t global_timestamp_us, uint32_t source, uint64_t sdk_return_unix_ns, uint64_t publication_start_unix_ns, uint64_t image_publish_finished_unix_ns, uint64_t image_publish_wall_ns, bool sdk_system_clock_verified) {
  std::ostringstream out;
  out.imbue(std::locale::classic());
  out << std::setprecision(std::numeric_limits<float>::max_digits10)
      << "{\"version\":1,\"sequence\":" << f.sequence << ",\"stamp_sec\":" << f.stamp_sec
      << ",\"stamp_nanosec\":" << f.stamp_nanosec << ",\"frame_id\":\"" << frame_id
      << "\",\"encoding\":\"" << (f.encoding == GDP_ROS_ENCODING_YUYV ? "yuyv" : "16UC1")
      << "\",\"width\":" << f.width << ",\"height\":" << f.height << ",\"step\":" << f.step
      << ",\"device_timestamp_us\":";
  if (f.has_device_timestamp) out << f.device_timestamp_us; else out << "null";
  out << ",\"system_timestamp_us\":" << f.system_timestamp_us
      << ",\"global_timestamp_us\":";
  if (global_timestamp_us) out << global_timestamp_us; else out << "null";
  out << ",\"stamp_mapping\":\"" << (source == 2 ? "sdk-global-unix" : "sdk-system-unix")
      << "\",\"publisher_qos_verified\":true,\"normalized\":" << (f.normalized ? "true" : "false")
      << ",\"source_depth_scale_mm\":";
  if (f.has_depth_scale) out << f.source_depth_scale_mm; else out << "null";
  out << ",\"output_depth_scale_mm\":";
  if (f.has_depth_scale) out << f.output_depth_scale_mm; else out << "null";
  if (!session_id.empty()) out << ",\"session_id\":\"" << session_id << "\"";
  out << ",\"sdk_return_unix_ns\":";
  if (sdk_return_unix_ns) out << sdk_return_unix_ns; else out << "null";
  out << ",\"publication_start_unix_ns\":" << publication_start_unix_ns
      << ",\"image_publish_finished_unix_ns\":" << image_publish_finished_unix_ns
      << ",\"image_publish_wall_ns\":" << image_publish_wall_ns
      << ",\"sdk_system_clock_verified\":" << (sdk_system_clock_verified ? "true" : "false")
      << ",\"timing_clock\":\"host-realtime-unix-ns\",\"metadata_available_after_image_publish\":true";
  out << '}';
  return out.str();
}
}
extern "C" uint32_t gdp_ros_abi_version(void) { return GDP_ROS_ABI_VERSION; }
extern "C" int32_t gdp_ros_create(const gdp_ros_config *c, void **out) {
  if (!out) return 1;
  *out = nullptr;
  try {
    if (!gdp_ros::config_valid(c)) return 1;
    auto p = std::make_unique<Publisher>();
    p->context = std::make_shared<rclcpp::Context>();
    p->context->init(0, nullptr);
    rclcpp::NodeOptions options;
    options.context(p->context).use_global_arguments(false).start_parameter_services(false).start_parameter_event_publisher(false);
    p->node = std::make_shared<rclcpp::Node>(c->node_name, c->namespace_name, options);
    const auto qos = rclcpp::QoS(rclcpp::KeepLast(10)).reliable().durability_volatile();
    p->image = p->node->create_publisher<sensor_msgs::msg::Image>(c->image_topic, qos);
    p->info = p->node->create_publisher<sensor_msgs::msg::CameraInfo>(c->camera_info_topic, qos);
    p->metadata = p->node->create_publisher<std_msgs::msg::String>(c->metadata_topic, qos);
    // Discovered DDS graph endpoints on Humble may expose UNKNOWN history and
    // depth 0. Query each local publisher's actual RMW QoS instead; successful
    // creation is the invariant behind publisher_qos_verified in metadata.
    const auto actual_qos_valid = [](const auto &publisher) {
      const auto actual = publisher->get_actual_qos().get_rmw_qos_profile();
      return actual.reliability == RMW_QOS_POLICY_RELIABILITY_RELIABLE
          && actual.durability == RMW_QOS_POLICY_DURABILITY_VOLATILE
          && actual.history == RMW_QOS_POLICY_HISTORY_KEEP_LAST && actual.depth == 10;
    };
    if (!actual_qos_valid(p->image) || !actual_qos_valid(p->info) || !actual_qos_valid(p->metadata)) {
      throw std::runtime_error("actual publisher QoS differs from required contract");
    }
    p->calibration.header.frame_id = c->frame_id;
    p->calibration.width = c->width; p->calibration.height = c->height;
    p->calibration.distortion_model = c->distortion_model;
    std::copy(c->k, c->k + 9, p->calibration.k.begin());
    std::copy(c->r, c->r + 9, p->calibration.r.begin());
    std::copy(c->p, c->p + 12, p->calibration.p.begin());
    p->calibration.d.assign(c->d, c->d + c->d_len);
    *out = p.release();
    return 0;
  } catch (const std::bad_alloc &) { return 3; } catch (...) { return 2; }
}
extern "C" int32_t gdp_ros_set_session_id(void *handle, const char *id) {
  try {
    if (!handle || !gdp_ros::session_valid(id)) return 1;
    auto &p=*static_cast<Publisher *>(handle);
    if (p.published || !p.session_id.empty()) return 1;
    p.session_id=id;return 0;
  } catch (...) {return 2;}
}
extern "C" int32_t gdp_ros_publish_observed(void *handle, const gdp_ros_frame *f, uint64_t global_timestamp_us, uint32_t source, uint64_t sdk_return_unix_ns, uint32_t sdk_system_clock_verified) {
  if (!handle || sdk_system_clock_verified > 1) return 1;
  try {
    const auto publication_start_unix_ns = unix_now_ns();
    auto &p = *static_cast<Publisher *>(handle);
    if (!gdp_ros::frame_valid_timestamped(f, p.calibration.width, p.calibration.height, global_timestamp_us, source)) return 1;
    const uint64_t stamp = uint64_t(f->stamp_sec) * 1000000000 + f->stamp_nanosec;
    if (p.published && (stamp <= p.last_stamp || f->sequence <= p.last_sequence)) return 1;
    sensor_msgs::msg::Image image;
    image.header.frame_id = p.calibration.header.frame_id;
    image.header.stamp.sec = f->stamp_sec; image.header.stamp.nanosec = f->stamp_nanosec;
    image.width = f->width; image.height = f->height; image.step = f->step;
    image.encoding = f->encoding == GDP_ROS_ENCODING_YUYV ? "yuyv" : "16UC1";
    image.is_bigendian = f->is_bigendian;
    // Copy while the caller's SDK frame/normalization buffer is alive. Nothing
    // from the caller's payload or metadata is retained after this function.
    image.data.assign(f->data, f->data + static_cast<size_t>(f->data_len));
    auto info = p.calibration; info.header = image.header;
    // Capture callback/transport timing without claiming delivery completion.
    // The subscriber may run before this publish call returns.
    const auto frame_id = image.header.frame_id;
    const auto image_publish_started = std::chrono::steady_clock::now();
    p.image->publish(std::move(image));
    const auto image_publish_wall_ns = static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now() - image_publish_started).count());
    const auto image_publish_finished_unix_ns = unix_now_ns();
    p.info->publish(std::move(info));
    std_msgs::msg::String metadata;
    metadata.data = frame_metadata(*f, frame_id, p.session_id, global_timestamp_us, source,
        sdk_return_unix_ns, publication_start_unix_ns, image_publish_finished_unix_ns, image_publish_wall_ns, sdk_system_clock_verified == 1);
    if (metadata.data.size() > 4096) return 1;
    p.metadata->publish(std::move(metadata));
    p.published = true; p.last_stamp = stamp; p.last_sequence = f->sequence;
    return 0;
  } catch (const std::bad_alloc &) { return 3; } catch (...) { return 2; }
}
extern "C" int32_t gdp_ros_publish_timestamped(void *handle, const gdp_ros_frame *f, uint64_t global_timestamp_us, uint32_t source) {
  return gdp_ros_publish_observed(handle, f, global_timestamp_us, source, 0, 0);
}
extern "C" int32_t gdp_ros_publish(void *handle, const gdp_ros_frame *f) {
  return gdp_ros_publish_timestamped(handle, f, 0, 1);
}
extern "C" void gdp_ros_destroy(void *handle) {
  try { delete static_cast<Publisher *>(handle); } catch (...) {}
}
