// SPDX-License-Identifier: Apache-2.0
// Standalone native boundary tests: c++ -std=c++17 -Iros/include ...
#include "gdp_ros_validation.hpp"
#include "gdp_ros_aux_validation.hpp"
#include <cassert>
#include <limits>
int main() {
  double d[5]{};
  gdp_ros_config c{};
  c.abi_version=1; c.struct_size=sizeof(c); c.node_name="test"; c.namespace_name="/test";
  c.image_topic="/test/image_raw"; c.camera_info_topic="/test/camera_info"; c.metadata_topic="/test/frame_metadata"; c.frame_id="test_optical";
  c.width=2; c.height=2; c.distortion_model="plumb_bob"; c.d=d; c.d_len=5;
  c.k[0]=1; c.k[4]=1; c.k[8]=1; c.r[0]=1; c.r[4]=1; c.r[8]=1; c.p[0]=1; c.p[5]=1; c.p[10]=1;
  assert(gdp_ros::config_valid(&c));
  c.k[0]=std::numeric_limits<double>::quiet_NaN(); assert(!gdp_ros::config_valid(&c)); c.k[0]=1;
  c.r[0]=-1; assert(!gdp_ros::config_valid(&c)); c.r[0]=1;
  c.d_len=4; assert(!gdp_ros::config_valid(&c)); c.d_len=5;
  c.metadata_topic="/other/frame_metadata"; assert(!gdp_ros::config_valid(&c)); c.metadata_topic="/test/frame_metadata";
  uint8_t pixels[8]{};
  gdp_ros_frame f{};
  f.abi_version=1; f.struct_size=sizeof(f); f.encoding=GDP_ROS_ENCODING_YUYV; f.width=2; f.height=2; f.step=4;
  f.stamp_sec=1700000000; f.stamp_nanosec=1000; f.sequence=1; f.has_system_timestamp=1;
  f.system_timestamp_us=1700000000000001; f.data=pixels; f.data_len=8;
  assert(gdp_ros::frame_valid(&f,2,2));
  const auto global = f.system_timestamp_us - 1;
  assert(!gdp_ros::frame_valid_timestamped(&f,2,2,global,2));
  f.stamp_nanosec=0;
  assert(gdp_ros::frame_valid_timestamped(&f,2,2,global,2));
  assert(!gdp_ros::frame_valid(&f,2,2));
  assert(!gdp_ros::frame_valid_timestamped(&f,2,2,0,2));
  assert(!gdp_ros::frame_valid_timestamped(&f,2,2,global,0));
  assert(!gdp_ros::frame_valid_timestamped(&f,2,2,std::numeric_limits<uint64_t>::max(),2));
  assert(!gdp_ros::frame_valid_timestamped(nullptr,2,2,global,2));
  f.stamp_nanosec=1000;
  assert(gdp_ros::frame_valid_timestamped(&f,2,2,global,1));
  f.has_device_timestamp=1; f.device_timestamp_us=0;
  assert(gdp_ros::frame_valid(&f,2,2)); // device-clock origin differs from Unix
  f.data_len=7; assert(!gdp_ros::frame_valid(&f,2,2)); f.data_len=8;
  f.stamp_nanosec=1001; assert(!gdp_ros::frame_valid(&f,2,2)); f.stamp_nanosec=1000;
  f.normalized=1; assert(!gdp_ros::frame_valid(&f,2,2)); f.normalized=0;
  f.encoding=GDP_ROS_ENCODING_DEPTH16; f.has_depth_scale=1; f.source_depth_scale_mm=.1f; f.output_depth_scale_mm=.1f;
  assert(!gdp_ros::frame_valid(&f,2,2));
  f.normalized=1; f.output_depth_scale_mm=1; assert(gdp_ros::frame_valid(&f,2,2));
  f.source_depth_scale_mm=std::numeric_limits<float>::infinity(); assert(!gdp_ros::frame_valid(&f,2,2));
  gdp_ros_aux_config aux{};
  aux.abi_version=1; aux.struct_size=sizeof(aux); aux.node_name="test_aux"; aux.namespace_name="/test";
  aux.pair_metadata_topic="/test/rgbd/pair_metadata"; aux.parent_frame="color_optical"; aux.child_frame="depth_optical"; aux.stamp_sec=1700000000;
  aux.rotation_row_major[1]=-1;aux.rotation_row_major[3]=1;aux.rotation_row_major[8]=1;
  aux.translation_m[0]=.1;aux.translation_m[1]=.2;aux.translation_m[2]=.3;
  assert(gdp_ros::aux_config_valid(&aux));
  const auto q=gdp_ros::quaternion(aux.rotation_row_major);
  assert(gdp_ros::near(q[2],std::sqrt(.5),1e-12)&&gdp_ros::near(q[3],std::sqrt(.5),1e-12));
  aux.rotation_row_major[1]=1;assert(!gdp_ros::aux_config_valid(&aux));aux.rotation_row_major[1]=-1;
  aux.translation_m[0]=std::numeric_limits<double>::infinity();assert(!gdp_ros::aux_config_valid(&aux));
  gdp_ros_pair pair{};pair.abi_version=1;pair.struct_size=sizeof(pair);pair.clock=1;pair.same_frameset=1;
  pair.color_stamp_ns=1700000000000001000;pair.depth_stamp_ns=1700000000000002000;pair.color_timestamp_us=1;pair.depth_timestamp_us=11;pair.delta_us=10;
  assert(gdp_ros::pair_valid(&pair));pair.hardware_sync_verified=1;assert(!gdp_ros::pair_valid(&pair));pair.hardware_sync_verified=0;
  pair.delta_us=11;assert(!gdp_ros::pair_valid(&pair));pair.delta_us=10;
  pair.color_timestamp_us=0;pair.delta_us=11;assert(!gdp_ros::pair_valid(&pair));pair.color_timestamp_us=1;pair.delta_us=10;
  auto next=pair;next.pair_sequence++;next.color_sequence++;next.depth_sequence++;next.color_stamp_ns+=1000;next.depth_stamp_ns+=1000;
  assert(!gdp_ros::pair_advances(next,pair));next.color_timestamp_us++;next.depth_timestamp_us++;assert(gdp_ros::pair_advances(next,pair));
  next.clock=2;assert(!gdp_ros::pair_advances(next,pair));
  pair.clock=2;assert(!gdp_ros::pair_valid(&pair));
}
