// SPDX-License-Identifier: Apache-2.0
// Dynamic ABI fixture, uses the real independent validation header but no ROS.
#include "gdp_ros_bridge.h"
#include "gdp_ros_session.hpp"
#include "gdp_ros_validation.hpp"
#include "gdp_ros_aux_validation.hpp"
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <string>
#include <vector>
#include <numeric>
#include <thread>
#include <chrono>
#ifndef MOCK_ABI_VERSION
#define MOCK_ABI_VERSION GDP_ROS_ABI_VERSION
#endif
struct Mock {
  uint32_t width, height;
  std::string frame_id;
  double focal;
  std::vector<uint8_t> owned;
  uint64_t calls = 0;
  std::string session_id;
};
static void event(const char *value) {
  const char *path = std::getenv("GDP_ROS_MOCK_EVENTS");
  if (path) { auto *f = std::fopen(path, "a"); if (f) { std::fprintf(f, "%s\n", value); std::fclose(f); } }
}
extern "C" uint32_t gdp_ros_abi_version(void) { return MOCK_ABI_VERSION; }
extern "C" int32_t gdp_ros_create(const gdp_ros_config *c, void **out) {
  if (!out) return 1;
  *out = nullptr;
  if (!gdp_ros::config_valid(c)) return 1;
  auto p = std::make_unique<Mock>();
  p->width = c->width; p->height = c->height; p->frame_id = c->frame_id; p->focal = c->k[0];
  *out = p.release(); event("create");
  return std::string(c->node_name) == "mock_fail_create" ? 2 : 0;
}
extern "C" int32_t gdp_ros_publish(void *handle, const gdp_ros_frame *frame) {
  if (!handle) return 1;
  auto &p = *static_cast<Mock *>(handle);
  if (!gdp_ros::frame_valid(frame, p.width, p.height)) return 1;
  if (frame->sequence == 1 && (!frame->has_device_timestamp || frame->device_timestamp_us != 0)) return 2;
  if (frame->sequence == 999) { event("publish-failed"); return 2; }
  // Caller changes/frees source pixels between calls. Owned first bytes must stay.
  if (!p.owned.empty() && p.owned[0] != 16) return 2;
  if (p.frame_id != "test_optical" || p.focal != 1.) return 2;
  p.owned.assign(frame->data, frame->data + frame->data_len);
  p.calls++; event("publish-copied");
  return 0;
}
extern "C" void gdp_ros_destroy(void *handle) {
  delete static_cast<Mock *>(handle); event("destroy");
}
struct AuxMock {std::string session_id;std::string parent,child;double translation[3];bool published=false;gdp_ros_pair last{};};
extern "C" uint32_t gdp_ros_aux_abi_version(void){return MOCK_ABI_VERSION;}
extern "C" int32_t gdp_ros_aux_create(const gdp_ros_aux_config *c,void **out){
  if(!out)return 1;
  *out=nullptr;
  if(!gdp_ros::aux_config_valid(c))return 1;
  auto p=std::make_unique<AuxMock>();p->parent=c->parent_frame;p->child=c->child_frame;std::copy(c->translation_m,c->translation_m+3,p->translation);
  *out=p.release();event("aux-create");return std::string(c->node_name)=="mock_fail_aux_create"?2:0;
}
extern "C" int32_t gdp_ros_aux_publish_pair(void *handle,const gdp_ros_pair *pair){
  if(!handle||!gdp_ros::pair_valid(pair))return 1;
  auto &p=*static_cast<AuxMock *>(handle);
  if(p.published && !gdp_ros::pair_advances(*pair,p.last))return 1;
  if(p.parent!="color_optical"||p.child!="depth_optical"||p.translation[0]!=.1||p.translation[1]!=.2||p.translation[2]!=.3)return 2;
  if(pair->pair_sequence==999){event("aux-publish-failed");return 2;}
  p.last=*pair;p.published=true;event("aux-publish");return 0;
}
extern "C" void gdp_ros_aux_destroy(void *handle){delete static_cast<AuxMock *>(handle);event("aux-destroy");}

extern "C" int32_t gdp_ros_set_session_id(void *handle,const char *id){
 if(!handle||!gdp_ros::session_valid(id))return 1;
 auto &p=*static_cast<Mock *>(handle);
 if(p.calls||!p.session_id.empty())return 1;
 p.session_id=id;event("session-bound");return 0;
}
extern "C" int32_t gdp_ros_aux_set_session_id(void *handle,const char *id){
 if(!handle||!gdp_ros::session_valid(id))return 1;
 auto &p=*static_cast<AuxMock *>(handle);
 if(p.published||!p.session_id.empty())return 1;
 p.session_id=id;event("aux-session-bound");return 0;
}

extern "C" int32_t gdp_ros_aux_publish_evidence(void *handle,const char *json,size_t length){
  if(!handle||!json||length==0||length>65536)return 1;
  auto &p=*static_cast<AuxMock *>(handle);if(!p.published)return 1;
  std::string owned(json,length);if(owned.find("consumer_receipt_verified") == std::string::npos)return 1;
  if(p.last.pair_sequence==2){event("evidence-failed");return 2;}
  event("evidence-copied");return 0;
}

#ifndef MOCK_NO_TIMESTAMP_EXTENSION
extern "C" int32_t gdp_ros_publish_timestamped(void *handle, const gdp_ros_frame *frame, uint64_t global_timestamp_us, uint32_t source) {
  if (!handle) return 1;
  auto &p = *static_cast<Mock *>(handle);
  if (!gdp_ros::frame_valid_timestamped(frame, p.width, p.height, global_timestamp_us, source)) return 1;
  if (frame->sequence == 999) return 2;
  p.owned.assign(frame->data, frame->data + frame->data_len);
  event(source == 2 ? "publish-global" : "publish-system-with-global");
  return 0;
}
#endif

#if !defined(MOCK_NO_OBSERVATION_EXTENSION) && !defined(MOCK_NO_TIMESTAMP_EXTENSION)
extern "C" int32_t gdp_ros_publish_observed(void *handle, const gdp_ros_frame *frame, uint64_t global_timestamp_us, uint32_t source, uint64_t sdk_return_unix_ns, uint32_t sdk_system_clock_verified) {
  if (!sdk_return_unix_ns || sdk_system_clock_verified>1) return 1;
#ifdef MOCK_EXPECT_CLOCK_VERIFIED
  if (sdk_system_clock_verified!=MOCK_EXPECT_CLOCK_VERIFIED) return 1;
#endif
  event("sdk-return-observed");
  event(sdk_system_clock_verified ? "sdk-system-clock-verified" : "sdk-system-clock-unverified");
  return source==1 && !global_timestamp_us ? gdp_ros_publish(handle, frame) : gdp_ros_publish_timestamped(handle, frame, global_timestamp_us, source);
}
#endif

#ifndef MOCK_NO_IMU
#include "gdp_ros_imu.h"
struct ImuMock {std::string accel,gyro;bool delayed=false;};
extern "C" int32_t gdp_ros_imu_create(const char*,const char*,const char*,const char*,const char *a,const char *g,void **out){
 *out=nullptr;if(std::getenv("GDP_ROS_IMU_CREATE_ERROR"))return 2;*out=new ImuMock{a,g};if(std::getenv("GDP_ROS_IMU_CREATE_WITH_HANDLE"))return 2;event("imu-create");return 0;}
extern "C" int32_t gdp_ros_imu_publish(void *h,const gdp_ros_imu_frame *f){
 if(!h||!f||f->abi_version!=1||f->struct_size!=sizeof(*f)||f->kind<1||f->kind>2)return 1;
 auto *publisher=static_cast<ImuMock*>(h);
 if(!publisher->delayed){publisher->delayed=true;const auto *ms=std::getenv("GDP_ROS_IMU_FIRST_PUBLISH_DELAY_MS");if(ms)std::this_thread::sleep_for(std::chrono::milliseconds(std::strtoul(ms,nullptr,10)));}
 if(std::getenv("GDP_ROS_IMU_PUBLISH_ERROR"))return 2;
 event(f->kind==1?"imu-accel-publish":"imu-gyro-publish");return 0;}
extern "C" void gdp_ros_imu_destroy(void *h){delete static_cast<ImuMock*>(h);event("imu-destroy");}
#endif

#if !defined(MOCK_NO_CONTEXT) && !defined(MOCK_NO_IMU)
#include "gdp_ros_context.h"
extern "C" uint32_t gdp_ros_context_abi_version(void){return std::getenv("GDP_ROS_CONTEXT_BAD_ABI")?2:1;}
extern "C" int32_t gdp_ros_context_create(void **out){
 if(!out)return 1;
 *out=nullptr;if(std::getenv("GDP_ROS_CONTEXT_CREATE_ERROR"))return 2;
 if(std::getenv("GDP_ROS_CONTEXT_NULL"))return 0;
 *out=new int(1);event("context-create");return std::getenv("GDP_ROS_CONTEXT_WITH_HANDLE")?2:0;}
extern "C" void gdp_ros_context_destroy(void *p){delete static_cast<int*>(p);event("context-destroy");}
extern "C" int32_t gdp_ros_create_in_context(void *context,const gdp_ros_config *config,void **out){
 if(!context)return 1;
 event("image-shared-context");return gdp_ros_create(config,out);}
#ifndef MOCK_PARTIAL_CONTEXT
extern "C" int32_t gdp_ros_imu_create_in_context(void *context,const char *n,const char *ns,const char *a,const char *g,const char *af,const char *gf,void **out){
 if(!context)return 1;
 event("imu-shared-context");return gdp_ros_imu_create(n,ns,a,g,af,gf,out);}
#endif
#endif
