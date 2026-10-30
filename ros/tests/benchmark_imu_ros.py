#!/usr/bin/env python3
"""Manual real ROS bridge benchmark; synthetic inputs, no SDK/device/recorder."""
import argparse
import ctypes as c
import json
import math
from pathlib import Path
import resource
import time
from smoke_ros import Config, Frame as ImageFrame
from smoke_imu import Frame
from smoke_context import image_config, configure, BASE


def rss():
    fields={line.split(':')[0]:line.split(':')[1].strip() for line in Path('/proc/self/status').read_text().splitlines() if ':' in line}
    return {key:int(fields[key].split()[0]) for key in ('VmRSS','VmHWM','Threads')}


def main():
    ap=argparse.ArgumentParser();ap.add_argument('--library',required=True);ap.add_argument('--mode',choices=['before','reuse','shared'],required=True);ap.add_argument('--output',required=True);ap.add_argument('--seconds',type=float,default=8.);ap.add_argument('--repeat',type=int,default=0);args=ap.parse_args()
    lib=c.CDLL(args.library)
    lib.gdp_ros_create.argtypes=[c.POINTER(Config),c.POINTER(c.c_void_p)]
    lib.gdp_ros_publish.argtypes=[c.c_void_p,c.POINTER(ImageFrame)]
    lib.gdp_ros_imu_create.argtypes=[c.c_char_p]*6+[c.POINTER(c.c_void_p)]
    lib.gdp_ros_imu_publish.argtypes=[c.c_void_p,c.POINTER(Frame)]
    lib.gdp_ros_destroy.argtypes=[c.c_void_p];lib.gdp_ros_destroy.restype=None
    lib.gdp_ros_imu_destroy.argtypes=[c.c_void_p];lib.gdp_ros_imu_destroy.restype=None
    context=c.c_void_p();images=[];imu=c.c_void_p();started=time.monotonic();before=rss()
    try:
        if args.mode=='shared':
            configure(lib);assert lib.gdp_ros_context_abi_version()==1
            assert lib.gdp_ros_context_create(c.byref(context))==0
        for depth in (False,True):
            ns='/gdp_imu_ros_benchmark';cfg=image_config(ns,640,400)
            if depth:
                cfg.node_name=b'context_depth';cfg.frame_id=b'camera_depth_optical_frame'
                cfg.image_topic=(ns+'/depth/image_raw').encode();cfg.camera_info_topic=(ns+'/depth/camera_info').encode();cfg.metadata_topic=(ns+'/depth/frame_metadata').encode()
            handle=c.c_void_p()
            code=lib.gdp_ros_create_in_context(context,c.byref(cfg),c.byref(handle)) if context.value else lib.gdp_ros_create(c.byref(cfg),c.byref(handle))
            assert code==0
            pixels=(c.c_uint8*(640*400*2))()
            frame=ImageFrame(1,c.sizeof(ImageFrame),2 if depth else 1,640,400,1280,0,0,0,0,1,1,1,BASE,int(depth),int(depth),.1 if depth else 0.,1. if depth else 0.,pixels,len(pixels))
            images.append((handle,cfg,pixels,frame))
        names=[b'context_imu',b'/gdp_imu_ros_benchmark',b'/gdp_imu_ros_benchmark/accel/sample',b'/gdp_imu_ros_benchmark/gyro/sample',b'camera_accel_frame',b'camera_gyro_frame']
        code=lib.gdp_ros_imu_create_in_context(context,*names,c.byref(imu)) if context.value else lib.gdp_ros_imu_create(*names,c.byref(imu));assert code==0
        create_ms=(time.monotonic()-started)*1000
        n=0;video_n=0;next_imu=time.monotonic();next_video=next_imu
        frames=[Frame(1,c.sizeof(Frame),k,1,0,0,0,0,BASE,0,(1.,2.,3.)) for k in (1,2)]
        def run(duration,measure=False):
            nonlocal n,video_n,next_imu,next_video
            latencies=[];video=0;end=time.monotonic()+duration
            while time.monotonic()<end:
                now=time.monotonic()
                if now<next_imu:time.sleep(min(next_imu-now,.005));continue
                n+=1;us=BASE+n*5000
                for f in frames:
                    f.stamp_sec=us//1_000_000;f.stamp_nanosec=us%1_000_000*1000;f.sequence=n;f.device_timestamp_us=n*5000;f.system_timestamp_us=us
                    at=time.monotonic_ns();code=lib.gdp_ros_imu_publish(imu,c.byref(f));elapsed=time.monotonic_ns()-at;assert code==0
                    if measure:latencies.append(elapsed)
                if now>=next_video:
                    video_n+=1;video+=1;stamp=BASE+video_n*33333
                    for handle,_,_,f in images:
                        f.stamp_sec=stamp//1_000_000;f.stamp_nanosec=stamp%1_000_000*1000;f.sequence=video_n;f.device_timestamp_us=video_n*33333;f.system_timestamp_us=stamp
                        assert lib.gdp_ros_publish(handle,c.byref(f))==0
                    next_video=max(next_video+1/30,now)
                next_imu=max(next_imu+.005,now)
            return latencies,video
        run(1.)
        cpu=lambda:resource.getrusage(resource.RUSAGE_SELF).ru_utime+resource.getrusage(resource.RUSAGE_SELF).ru_stime
        c0=cpu();at=time.monotonic();latencies,video=run(args.seconds,True);wall=time.monotonic()-at;c1=cpu();steady=rss()
        samples=sorted(latencies);p99=samples[math.ceil(len(samples)*.99)-1]
        shutdown=time.monotonic()
        if context.value:lib.gdp_ros_context_destroy(context);context.value=None
        for handle,_,_,_ in images:lib.gdp_ros_destroy(handle)
        images=[];lib.gdp_ros_imu_destroy(imu);imu.value=None
        result={'mode':args.mode,'repeat':args.repeat,'seconds':wall,'cpu_cores':(c1-c0)/wall,'imu_publish_calls':len(samples),'imu_calls_per_second':len(samples)/wall,'image_pairs':video,'image_pairs_per_second':video/wall,'imu_publish_p99_us':p99/1000,'imu_publish_mean_us':sum(samples)/len(samples)/1000,'create_ms':create_ms,'destroy_ms':(time.monotonic()-shutdown)*1000,'rss_before':before,'rss_steady':steady,'context_count_for_three_nodes':1 if args.mode=='shared' else 3,'scope':'real Humble/Fast DDS, Python ctypes driver, synthetic 640x400 YUYV/mm-depth 30 FPS and accel/gyro 200 Hz; no subscribers, SDK, USB or native/ROSbag2 recorder; AMD64 may be emulated'}
        Path(args.output).write_text(json.dumps(result,indent=2)+'\n')
    finally:
        for handle,_,_,_ in images:lib.gdp_ros_destroy(handle)
        if imu.value:lib.gdp_ros_imu_destroy(imu)
        if context.value:lib.gdp_ros_context_destroy(context)
if __name__=='__main__':main()
