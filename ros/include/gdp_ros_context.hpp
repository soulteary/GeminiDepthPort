// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <rclcpp/rclcpp.hpp>
#include <memory>
namespace gdp_ros {
// Nodes/publishers keep this owner alive. Only its final release shuts down ROS.
struct ContextOwner {
  std::shared_ptr<rclcpp::Context> context = std::make_shared<rclcpp::Context>();
  ContextOwner() { context->init(0, nullptr); }
  ~ContextOwner() { if (context->is_valid()) { try { context->shutdown("bridge context released"); } catch (...) {} } }
};
struct ContextHandle { std::shared_ptr<ContextOwner> owner; };
inline std::shared_ptr<ContextOwner> shared_context(void *handle) {
  return handle ? static_cast<ContextHandle *>(handle)->owner : nullptr;
}
}
