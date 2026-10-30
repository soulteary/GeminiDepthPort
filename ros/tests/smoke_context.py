#!/usr/bin/env python3
"""Real DDS context identity, peer lifetime, and reused-message wire semantics."""
import ctypes as c
import json
import time
import rclpy
from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy
from sensor_msgs.msg import Image, Imu
from smoke_ros import Config, Frame as ImageFrame
from smoke_imu import Frame, validation

BASE = 1_700_000_000_000_000

def image_config(ns, width=2, height=2):
    d = (c.c_double * 5)(0., 0., 0., 0., 0.)
    return Config(1, c.sizeof(Config), b'context_image', ns.encode(), (ns+'/color/image_raw').encode(),
                  (ns+'/color/camera_info').encode(), (ns+'/color/frame_metadata').encode(),
                  b'camera_color_optical_frame', width, height, b'plumb_bob',
                  (c.c_double*9)(1.,0.,1.,0.,1.,1.,0.,0.,1.),
                  (c.c_double*9)(1.,0.,0.,0.,1.,0.,0.,0.,1.),
                  (c.c_double*12)(1.,0.,1.,0.,0.,1.,1.,0.,0.,0.,1.,0.),d,5)

def configure(lib):
    lib.gdp_ros_context_create.argtypes=[c.POINTER(c.c_void_p)]
    lib.gdp_ros_context_destroy.argtypes=[c.c_void_p]
    lib.gdp_ros_create_in_context.argtypes=[c.c_void_p,c.POINTER(Config),c.POINTER(c.c_void_p)]
    lib.gdp_ros_imu_create_in_context.argtypes=[c.c_void_p]+[c.c_char_p]*6+[c.POINTER(c.c_void_p)]
    lib.gdp_ros_imu_publish.argtypes=[c.c_void_p,c.POINTER(Frame)]
    lib.gdp_ros_publish.argtypes=[c.c_void_p,c.POINTER(ImageFrame)]
    lib.gdp_ros_destroy.argtypes=[c.c_void_p]
    lib.gdp_ros_imu_destroy.argtypes=[c.c_void_p]
    for name in ('gdp_ros_context_destroy','gdp_ros_destroy','gdp_ros_imu_destroy'):getattr(lib,name).restype=None

def send_image(lib,handle,config,n):
    pixels=(c.c_uint8*(config.width*config.height*2))()
    us=BASE+n*33333
    frame=ImageFrame(1,c.sizeof(ImageFrame),1,config.width,config.height,config.width*2,0,
                     us//1_000_000,us%1_000_000*1000,n,1,1,n*33333,us,0,0,0.,0.,pixels,len(pixels))
    assert lib.gdp_ros_publish(handle,c.byref(frame))==0

def send_imu(lib,handle,n):
    us=BASE+n*5000
    source=2 if n%2 else 1
    mapped=us+1 if source==2 else us
    for kind in (1,2):
        values=(float(n),-float(n),n+.5) if kind==1 else (-float(n),n*2.,n+10.)
        system=us if source==1 else us-5000
        frame=Frame(1,c.sizeof(Frame),kind,source,mapped//1_000_000,mapped%1_000_000*1000,n,n*5000,system,mapped if source==2 else 0,values)
        if source==2 and n>1:
            bad=Frame.from_buffer_copy(frame);bad.timestamp_source=1
            bad.stamp_sec=system//1_000_000;bad.stamp_nanosec=system%1_000_000*1000
            assert lib.gdp_ros_imu_publish(handle,c.byref(bad))==1
        bad=Frame.from_buffer_copy(frame);bad.xyz[0]=float('nan')
        assert lib.gdp_ros_imu_publish(handle,c.byref(bad))==1
        bad=Frame.from_buffer_copy(frame);bad.timestamp_source=2;bad.global_timestamp_us=0
        assert lib.gdp_ros_imu_publish(handle,c.byref(bad))==1
        assert lib.gdp_ros_imu_publish(handle,c.byref(frame))==0

def main():
    lib=c.CDLL('/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so');configure(lib)
    assert lib.gdp_ros_context_abi_version()==1
    rclpy.init();node=rclpy.create_node('gdp_context_reader');groups=[];received={};subs=[]
    qos=QoSProfile(depth=100,reliability=ReliabilityPolicy.RELIABLE,durability=DurabilityPolicy.VOLATILE)
    try:
        for ns in ('/gdp_context_a','/gdp_context_b'):
            context,image,imu=c.c_void_p(),c.c_void_p(),c.c_void_p()
            assert lib.gdp_ros_context_create(c.byref(context))==0
            cfg=image_config(ns)
            assert lib.gdp_ros_create_in_context(None,c.byref(cfg),c.byref(image))==1 and not image.value
            assert lib.gdp_ros_create_in_context(context,c.byref(cfg),c.byref(image))==0
            cfg.abi_version=99;bad=c.c_void_p()
            assert lib.gdp_ros_create_in_context(context,c.byref(cfg),c.byref(bad))==1 and not bad.value
            cfg.abi_version=1
            assert lib.gdp_ros_imu_create_in_context(context,b'context_imu',ns.encode(),(ns+'/accel/sample').encode(),(ns+'/gyro/sample').encode(),b'camera_accel_frame',b'camera_gyro_frame',c.byref(imu))==0
            groups.append((ns,context,image,imu,cfg))
            for suffix,typ in [('color/image_raw',Image),('accel/sample',Imu),('gyro/sample',Imu)]:
                topic=ns+'/'+suffix;received[topic]=[]
                subs.append(node.create_subscription(typ,topic,lambda msg,t=topic:received[t].append(msg),qos))
        n=0;deadline=time.monotonic()+15
        while min(map(len,received.values()))<5:
            assert time.monotonic()<deadline
            n+=1
            for _,_,image,imu,cfg in groups:send_image(lib,image,cfg,n);send_imu(lib,imu,n)
            rclpy.spin_once(node,timeout_sec=.03)
        prefixes=[]
        for ns,_,_,_,_ in groups:
            gids=[]
            for suffix in ('color/image_raw','color/camera_info','color/frame_metadata','accel/sample','gyro/sample'):
                infos=node.get_publishers_info_by_topic(ns+'/'+suffix);assert len(infos)==1
                assert infos[0].qos_profile.reliability==ReliabilityPolicy.RELIABLE
                assert infos[0].qos_profile.durability==DurabilityPolicy.VOLATILE
                gids.append(bytes(infos[0].endpoint_gid)[:12])
            assert len(set(gids))==1 and gids[0]!=bytes(12),gids
            prefixes.append(gids[0])
        assert prefixes[0]!=prefixes[1]  # explicit sessions never share implicitly
        ns,context,image,imu,cfg=groups[0]
        lib.gdp_ros_context_destroy(context);context.value=None
        lib.gdp_ros_destroy(image);image.value=None
        before=len(received[ns+'/accel/sample'])
        for _ in range(8):n+=1;send_imu(lib,imu,n);rclpy.spin_once(node,timeout_sec=.02)
        assert len(received[ns+'/accel/sample'])>before
        lib.gdp_ros_imu_destroy(imu);imu.value=None
        ns,context,image,imu,cfg=groups[1]
        lib.gdp_ros_imu_destroy(imu);imu.value=None
        lib.gdp_ros_context_destroy(context);context.value=None
        before=len(received[ns+'/color/image_raw'])
        for _ in range(8):n+=1;send_image(lib,image,cfg,n);rclpy.spin_once(node,timeout_sec=.02)
        assert len(received[ns+'/color/image_raw'])>before
        for ns,_,_,_,_ in groups:
            progress={}
            for kind in ('accel','gyro'):
                topic=ns+'/'+kind+'/sample'
                for msg in received[topic]:
                    validation.validate_message(topic,msg,progress,ns)
                    us=msg.header.stamp.sec*1_000_000+msg.header.stamp.nanosec//1000
                    seq=(us-BASE)//5000
                    assert us==BASE+seq*5000+(1 if seq%2 else 0)
                    vector=msg.linear_acceleration if kind=='accel' else msg.angular_velocity
                    expected=(float(seq),-float(seq),seq+.5) if kind=='accel' else (-float(seq),seq*2.,seq+10.)
                    assert (vector.x,vector.y,vector.z)==expected
                    assert msg.header.frame_id=='camera_'+kind+'_frame'
        print(json.dumps({'ok':True,'same_participant_within_session':True,'isolated_sessions':True,'both_destruction_orders':True,'reused_messages_valid':True,'hardware_opened':False}))
    finally:
        for _,context,image,imu,_ in groups:
            if imu.value:lib.gdp_ros_imu_destroy(imu)
            if image.value:lib.gdp_ros_destroy(image)
            if context.value:lib.gdp_ros_context_destroy(context)
        for sub in subs:node.destroy_subscription(sub)
        node.destroy_node();rclpy.shutdown()
if __name__=='__main__':main()
