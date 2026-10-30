#!/usr/bin/env python3
"""Synthetic three-session real DDS exercise of the long-run evidence observer."""
import ctypes as c
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from smoke_context import configure, image_config, Frame
from smoke_ros import Frame as ImageFrame


def main():
    lib=c.CDLL('/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so');configure(lib)
    lib.gdp_ros_publish_timestamped.argtypes=[c.c_void_p,c.POINTER(ImageFrame),c.c_uint64,c.c_uint32]
    groups=[]
    try:
        for ns in ('/long_a','/long_b','/long_c'):
            context=c.c_void_p();assert lib.gdp_ros_context_create(c.byref(context))==0
            images=[]
            for stream in ('color','depth'):
                cfg=image_config(ns)
                cfg.node_name=(stream+"_long").encode()
                cfg.image_topic=(ns+'/'+stream+'/image_raw').encode()
                cfg.camera_info_topic=(ns+'/'+stream+'/camera_info').encode()
                cfg.metadata_topic=(ns+'/'+stream+'/frame_metadata').encode()
                handle=c.c_void_p();assert lib.gdp_ros_create_in_context(context,c.byref(cfg),c.byref(handle))==0
                images.append((handle,cfg))
            imu=c.c_void_p()
            assert lib.gdp_ros_imu_create_in_context(context,b'long_imu',ns.encode(),(ns+'/accel/sample').encode(),(ns+'/gyro/sample').encode(),b'accel_frame',b'gyro_frame',c.byref(imu))==0
            groups.append((context,images,imu))
        with tempfile.TemporaryDirectory() as directory:
            out=Path(directory)/'wire.db'
            child=subprocess.Popen(['python3','/ws/ros/imu_long_run_observer.py','--namespaces','/long_a','/long_b','/long_c','--seconds','4','--max-mib','16','--output',str(out)],env=os.environ.copy())
            try:
                n=0;deadline=time.monotonic()+20
                while child.poll() is None:
                    assert time.monotonic()<deadline
                    n+=1;us=1_700_000_000_000_000+n*33333
                    for _,images,imu in groups:
                        for handle,cfg in images:
                            pixels=(c.c_uint8*8)()
                            frame=ImageFrame(1,c.sizeof(ImageFrame),1,2,2,4,0,us//1_000_000,us%1_000_000*1000,n,1,1,n*33333,us,0,0,0.,0.,pixels,8)
                            assert lib.gdp_ros_publish_timestamped(handle,c.byref(frame),us,2)==0
                        for kind in (1,2):
                            system=1_700_000_000_000_000+((n-1)//2)*66666
                            frame=Frame(1,c.sizeof(Frame),kind,2,us//1_000_000,us%1_000_000*1000,n,n*33333,system,us,(1.,2.,3.))
                            assert lib.gdp_ros_imu_publish(imu,c.byref(frame))==0
                    time.sleep(.033)
                result=json.loads(Path(str(out)+'.summary.json').read_text())
                assert child.wait()==0,result
                assert result['ok'] and len(result['counts'])==18,result
                print(json.dumps({'ok':True,'kind':'three-camera-long-run-observer','actual_dds':True,'shared_sessions':3,'hardware_verified':False}))
            finally:
                if child.poll() is None:child.kill();child.wait()
    finally:
        for context,images,imu in groups:
            for image,_ in images:lib.gdp_ros_destroy(image)
            lib.gdp_ros_imu_destroy(imu);lib.gdp_ros_context_destroy(context)

if __name__=='__main__':main()
