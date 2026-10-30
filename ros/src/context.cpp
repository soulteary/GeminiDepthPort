// SPDX-License-Identifier: Apache-2.0
#include "gdp_ros_context.h"
#include "gdp_ros_context.hpp"
#include <new>
extern "C" uint32_t gdp_ros_context_abi_version(void) { return 1; }
extern "C" int32_t gdp_ros_context_create(void **out) {
  if (!out) return 1;
  *out = nullptr;
  try {
    auto handle = std::make_unique<gdp_ros::ContextHandle>();
    handle->owner = std::make_shared<gdp_ros::ContextOwner>();
    *out = handle.release(); return 0;
  } catch (const std::bad_alloc &) { return 3; } catch (...) { return 2; }
}
extern "C" void gdp_ros_context_destroy(void *context) {
  try { delete static_cast<gdp_ros::ContextHandle *>(context); } catch (...) {}
}
