#!/usr/bin/env python3
"""Experiment health: immutable binding, real ROS consumption, IMU and SDK diagnostics."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import experiment_runtime as contract


def validate_extra(status,binding,diagnostics):
    contract.require(status['message_timestamp']=='global' and status['imu']['ready'] is True
                     and status['imu']['shared_ros_context'] is True and status['imu']['error'] is None,'IMU_GLOBAL_CONTEXT_HEALTH')
    contract.require(all(n>0 for n in status['imu']['published']) and status['restart_attempt']==0,'IMU_PROGRESS_OR_RESTART')
    contract.require(status['publish_failures']==0 and status['stamp_failures']==0,'PUBLICATION_HEALTH')
    contract.require(status['raw_recording']['state']!='failed' and status['raw_recording']['whole_pair_queue_overflows']==0,'RECORDING_HEALTH')
    if binding['diagnostics']:
        valid=[]
        for path in list(diagnostics.glob('sdk-diag-*.json'))[:256]:
            if not path.stat().st_size:continue
            value=contract.read_json(path)
            if (value.get('pid')==status['pid'] and value.get('sdk_base')==contract.BASE
                    and 0<=time.monotonic_ns()-value['snapshot_monotonic_ns']<=15_000_000_000
                    and value.get('sink_errors')==0 and value['counters']['input_frames']>0):valid.append(value)
        contract.require(valid,'SDK_DIAGNOSTICS_NOT_FRESH_OR_DISABLED')


def main():
    try:
        spec=importlib.util.spec_from_file_location('gdp_base_capture_health',Path(__file__).with_name('base_health.py'))
        base=importlib.util.module_from_spec(spec);spec.loader.exec_module(base)
        raw=Path('/config/capture.json').read_bytes();config=contract.read_json('/config/capture.json')
        manifest=contract.read_json(contract.MANIFEST);binding=contract.read_json('/config/experiment.json')
        contract.validate_config(config,binding,manifest,os.environ)
        contract.require(contract.sha('/config/health.json')==binding['health_sha256'],'HEALTH_CONFIG_BINDING')
        contract.validate_health(config,contract.read_json('/config/health.json'))
        path=Path(config['service']['status_path'])
        first=base.validate_snapshot(raw,contract.read_json(path),time.time_ns()//10**6)
        validate_extra(first,binding,Path('/sdk-diagnostics'))
        result=subprocess.run([sys.executable,'/app/ros_health.py'],env=dict(os.environ,GDP_VALIDATE_HOST_STAMPS='1'),timeout=24)
        contract.require(result.returncode==0,'ACTUAL_ROS_CONSUMPTION')
        second=base.validate_snapshot(raw,contract.read_json(path),time.time_ns()//10**6)
        base.validate_transition(first,second);validate_extra(second,binding,Path('/sdk-diagnostics'))
        contract.require(all(b>a for a,b in zip(first['imu']['published'],second['imu']['published'])),'IMU_FROZEN')
        contract.require(Path('/config/capture.json').read_bytes()==raw,'CONFIG_CHANGED')
        return 0
    except Exception as error:
        print(json.dumps({'ok':False,'code':'SDK_EXPERIMENT_UNHEALTHY','detail':str(error)}));return 1

if __name__=='__main__':raise SystemExit(main())
