// SPDX-License-Identifier: Apache-2.0
#include "gdp_ros_imu.h"
#include "gdp_ros_validation.hpp"
#include "gdp_ros_context.h"
#include "gdp_ros_context.hpp"
#include <rclcpp/rclcpp.hpp>
#include <sensor_msgs/msg/imu.hpp>
#include <array>
#include <cmath>
#include <memory>
#include <string>
namespace {
struct ImuPublisher {
 std::shared_ptr<gdp_ros::ContextOwner> owner;
 std::shared_ptr<rclcpp::Context> context;
 std::shared_ptr<rclcpp::Node> node;
 std::array<rclcpp::Publisher<sensor_msgs::msg::Imu>::SharedPtr,2> publishers;
 std::array<sensor_msgs::msg::Imu,2> messages;
 std::array<bool,2> seen{};
 std::array<uint64_t,2> stamps{},sequences{},device{};
 ~ImuPublisher(){publishers={};node.reset();}
};
}
static int32_t create_imu(const char *node,const char *ns,const char *accel_topic,const char *gyro_topic,const char *accel_frame,const char *gyro_frame,void **out,std::shared_ptr<gdp_ros::ContextOwner> owner){
 if(!out)return 1;
 *out=nullptr;
 try {
  for(const auto *s:{node,ns,accel_topic,gyro_topic,accel_frame,gyro_frame})if(!gdp_ros::bounded_string(s))return 1;
  const std::string n=node,space=ns,a=accel_topic,g=gyro_topic,af=accel_frame,gf=gyro_frame;
  const std::string prefix=space=="/"?"/":space+"/";
  if(!gdp_ros::token(n)||!gdp_ros::token(af)||!gdp_ros::token(gf)||!gdp_ros::absolute_name(space,true)||!gdp_ros::absolute_name(a,false)||!gdp_ros::absolute_name(g,false)||a.compare(0,prefix.size(),prefix)||g.compare(0,prefix.size(),prefix)||a==g||af==gf)return 1;
  auto p=std::make_unique<ImuPublisher>();
  p->messages[0].header.frame_id=af;p->messages[1].header.frame_id=gf;
  for(auto &message:p->messages){message.orientation_covariance[0]=-1.;message.orientation.w=1.;}
  p->messages[0].angular_velocity_covariance[0]=-1.;p->messages[1].linear_acceleration_covariance[0]=-1.;
  p->owner=owner?std::move(owner):std::make_shared<gdp_ros::ContextOwner>();
  p->context=p->owner->context;
  rclcpp::NodeOptions options;options.context(p->context).use_intra_process_comms(false).use_global_arguments(false).start_parameter_services(false).start_parameter_event_publisher(false);
  p->node=std::make_shared<rclcpp::Node>(n,space,options);
  const auto qos=rclcpp::QoS(rclcpp::KeepLast(100)).reliable().durability_volatile();
  p->publishers={p->node->create_publisher<sensor_msgs::msg::Imu>(a,qos),p->node->create_publisher<sensor_msgs::msg::Imu>(g,qos)};
  for(const auto &pub:p->publishers){const auto q=pub->get_actual_qos().get_rmw_qos_profile();if(q.depth!=100||q.history!=RMW_QOS_POLICY_HISTORY_KEEP_LAST||q.reliability!=RMW_QOS_POLICY_RELIABILITY_RELIABLE||q.durability!=RMW_QOS_POLICY_DURABILITY_VOLATILE)return 2;}
  *out=p.release();return 0;
 }catch(const std::bad_alloc &){return 3;}catch(...){return 2;}
}
extern "C" int32_t gdp_ros_imu_create(const char *node,const char *ns,const char *a,const char *g,const char *af,const char *gf,void **out){
 return create_imu(node,ns,a,g,af,gf,out,nullptr);
}
extern "C" int32_t gdp_ros_imu_create_in_context(void *context,const char *node,const char *ns,const char *a,const char *g,const char *af,const char *gf,void **out){
 if(!context){if(out)*out=nullptr;return 1;}
 return create_imu(node,ns,a,g,af,gf,out,gdp_ros::shared_context(context));
}
extern "C" int32_t gdp_ros_imu_publish(void *handle,const gdp_ros_imu_frame *f){
 if(!handle||!f||f->abi_version!=1||f->struct_size!=sizeof(*f)||(f->kind!=1&&f->kind!=2)||f->stamp_sec<0||f->stamp_nanosec>=1000000000||!f->device_timestamp_us||!f->system_timestamp_us)return 1;
 for(double x:f->xyz)if(!std::isfinite(x))return 1;
 if(f->timestamp_source!=1&&f->timestamp_source!=2)return 1;
 const uint64_t source=f->timestamp_source==1?f->system_timestamp_us:f->global_timestamp_us;
 if(!source||source>uint64_t(INT32_MAX)*1000000+999999)return 1;
 auto *p=static_cast<ImuPublisher*>(handle);const size_t slot=f->kind-1;
 const uint64_t stamp=uint64_t(f->stamp_sec)*1000000000+f->stamp_nanosec;
 if(stamp!=source*1000||!stamp||(p->seen[slot]&&(stamp<=p->stamps[slot]||f->sequence<=p->sequences[slot]||f->device_timestamp_us<=p->device[slot])))return 1;
 try {
  auto &message=p->messages[slot];message.header.stamp.sec=f->stamp_sec;message.header.stamp.nanosec=f->stamp_nanosec;
  if(slot==0){message.linear_acceleration.x=f->xyz[0];message.linear_acceleration.y=f->xyz[1];message.linear_acceleration.z=f->xyz[2];}
  else{message.angular_velocity.x=f->xyz[0];message.angular_velocity.y=f->xyz[1];message.angular_velocity.z=f->xyz[2];}
  p->publishers[slot]->publish(message);p->seen[slot]=true;p->stamps[slot]=stamp;p->sequences[slot]=f->sequence;p->device[slot]=f->device_timestamp_us;return 0;
 }catch(const std::bad_alloc &){return 3;}catch(...){return 2;}
}
extern "C" void gdp_ros_imu_destroy(void *handle){delete static_cast<ImuPublisher*>(handle);}
