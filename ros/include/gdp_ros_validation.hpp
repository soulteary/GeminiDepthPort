// SPDX-License-Identifier: Apache-2.0
#pragma once
#include "gdp_ros_bridge.h"
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <limits>
#include <string>

namespace gdp_ros {
inline bool bounded_string(const char *s, size_t maximum = 256) {
  return s && ::strnlen(s, maximum + 1) > 0 && ::strnlen(s, maximum + 1) <= maximum;
}
inline bool token(const std::string &s) {
  if (s.empty() || !((s[0] >= 'A' && s[0] <= 'Z') || (s[0] >= 'a' && s[0] <= 'z') || s[0] == '_')) return false;
  return std::all_of(s.begin(), s.end(), [](unsigned char c) {
    return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
  });
}
inline bool absolute_name(const std::string &s, bool root_allowed) {
  if (s == "/") return root_allowed;
  if (s.size() < 2 || s[0] != '/' || s.back() == '/') return false;
  size_t begin = 1;
  while (begin < s.size()) {
    const auto end = s.find('/', begin);
    if (!token(s.substr(begin, end == std::string::npos ? end : end - begin))) return false;
    if (end == std::string::npos) return true;
    begin = end + 1;
  }
  return false;
}
template<size_t N> inline bool finite(const double (&v)[N]) {
  return std::all_of(v, v + N, [](double x) { return std::isfinite(x); });
}
inline bool near(double a, double b, double tolerance = 1e-6) { return std::abs(a - b) <= tolerance; }
inline bool config_valid(const gdp_ros_config *c) {
  if (!c || c->abi_version != GDP_ROS_ABI_VERSION || c->struct_size != sizeof(*c)) return false;
  if (!bounded_string(c->node_name) || !bounded_string(c->namespace_name) || !bounded_string(c->image_topic)
      || !bounded_string(c->camera_info_topic) || !bounded_string(c->metadata_topic)
      || !bounded_string(c->frame_id) || !bounded_string(c->distortion_model, 32)) return false;
  if (!token(c->node_name) || !token(c->frame_id) || !absolute_name(c->namespace_name, true)) return false;
  const std::string ns(c->namespace_name), image(c->image_topic), info(c->camera_info_topic), metadata(c->metadata_topic);
  const std::string prefix = ns == "/" ? "/" : ns + "/";
  for (const auto &topic : {image, info, metadata}) {
    if (!absolute_name(topic, false) || topic.compare(0, prefix.size(), prefix) != 0) return false;
  }
  if (image == info || image == metadata || info == metadata) return false;
  if (!c->width || !c->height || c->width > 16384 || c->height > 16384) return false;
  if (!finite(c->k) || !finite(c->r) || !finite(c->p)) return false;
  if (c->k[0] <= 0 || c->k[4] <= 0 || !near(c->k[6], 0) || !near(c->k[7], 0) || !near(c->k[8], 1)) return false;
  if (c->p[0] <= 0 || c->p[5] <= 0 || !near(c->p[8], 0) || !near(c->p[9], 0) || !near(c->p[10], 1) || !near(c->p[11], 0)) return false;
  const std::string model(c->distortion_model);
  const uint32_t count = model == "plumb_bob" ? 5 : model == "rational_polynomial" ? 8 : model == "equidistant" ? 4 : 0;
  if (!count || c->d_len != count || !c->d) return false;
  for (uint32_t i = 0; i < count; ++i) if (!std::isfinite(c->d[i])) return false;
  for (size_t row = 0; row < 3; ++row) for (size_t col = 0; col < 3; ++col) {
    double product = 0;
    for (size_t j = 0; j < 3; ++j) product += c->r[row * 3 + j] * c->r[col * 3 + j];
    if (!near(product, row == col ? 1. : 0., 1e-5)) return false;
  }
  const auto &r = c->r;
  const double determinant = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6]) + r[2] * (r[3] * r[7] - r[4] * r[6]);
  return near(determinant, 1., 1e-5);
}
inline bool frame_valid_timestamped(const gdp_ros_frame *f, uint32_t width, uint32_t height, uint64_t global_timestamp_us, uint32_t source) {
  if (source != 1 && source != 2) return false;
  if (!f || f->abi_version != GDP_ROS_ABI_VERSION || f->struct_size != sizeof(*f)) return false;
  if (f->width != width || f->height != height || !width || !height || width > 16384 || height > 16384) return false;
  if (f->encoding != GDP_ROS_ENCODING_YUYV && f->encoding != GDP_ROS_ENCODING_DEPTH16) return false;
  if (f->is_bigendian != 0 || f->normalized > 1 || f->has_depth_scale > 1 || f->has_device_timestamp > 1 || f->has_system_timestamp > 1) return false;
  const uint64_t row = uint64_t(width) * 2, span = uint64_t(f->step) * height;
  if (f->step < row || f->step % 2 || span != f->data_len || !span || span > 512u * 1024u * 1024u || !f->data) return false;
  if (f->encoding == GDP_ROS_ENCODING_YUYV && (width % 2 || f->step != row || f->normalized || f->has_depth_scale)) return false;
  if (f->encoding == GDP_ROS_ENCODING_DEPTH16) {
    if (!f->has_depth_scale || !std::isfinite(f->source_depth_scale_mm) || f->source_depth_scale_mm <= 0 || f->output_depth_scale_mm != 1.f) return false;
    if (f->normalized ? f->step != row : f->source_depth_scale_mm != f->output_depth_scale_mm) return false;
  }
  if (f->stamp_sec < 0 || f->stamp_nanosec >= 1000000000 || (f->stamp_sec == 0 && f->stamp_nanosec == 0)) return false;
  // Device time zero may be a valid clock origin. Only the mapped Unix/system
  // timestamp must be positive; retain raw device zero distinctly from absent.
  if (!f->has_system_timestamp || !f->system_timestamp_us) return false;
  const uint64_t mapped = source == 1 ? f->system_timestamp_us : global_timestamp_us;
  const uint64_t stamp_ns = uint64_t(f->stamp_sec) * 1000000000 + f->stamp_nanosec;
  if (!mapped || mapped > std::numeric_limits<uint64_t>::max() / 1000 || stamp_ns != mapped * 1000) return false;
  return true;
}
inline bool frame_valid(const gdp_ros_frame *f, uint32_t width, uint32_t height) {
  return frame_valid_timestamped(f, width, height, 0, 1);
}
} // namespace gdp_ros
