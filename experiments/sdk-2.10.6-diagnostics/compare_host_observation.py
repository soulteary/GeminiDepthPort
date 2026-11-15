#!/usr/bin/env python3
"""Read-only observer-on/off receipts; wrapper CPU is not total instrumentation cost."""
import argparse
import hashlib
import json
import math
from pathlib import Path
from host_kernel_trace import document, require, stream_sha


def receipt(row):
    value=row.get('workload_receipt')
    return value if isinstance(value,dict) else {}


def valid_receipt(row):
    value=receipt(row);profile=value.get('profile')
    return (value.get('supported') is True and isinstance(profile,dict) and
            value.get('fingerprint')==hashlib.sha256(json.dumps(profile,sort_keys=True,separators=(',',':')).encode()).hexdigest())


def compare(on_path,off_path,output):
    on_path,off_path,output=map(lambda p:Path(p).resolve(),(on_path,off_path,output))
    require(not output.exists() and not output.is_relative_to(on_path.parent) and not output.is_relative_to(off_path.parent),'FRESH_INDEPENDENT_OUTPUT_REQUIRED')
    paths={'on':on_path,'off':off_path};rows={};refs={};reasons=[]
    for mode,path in paths.items():
        with path.open('rb') as stream:refs[mode]=stream_sha(stream)
        row=document(path,4*1024**2);require(row.get('observer_mode')==mode,'OBSERVER_MODE_MISMATCH')
        for key in ('observer_user_cpu_seconds','observer_system_cpu_seconds','wall_seconds'):
            require(type(row.get(key)) in (int,float) and math.isfinite(row[key]) and row[key]>=0,'RESOURCE_NUMBER:'+key)
        require(row['wall_seconds']>0,'ZERO_WALL_TIME')
        cpu=row['observer_user_cpu_seconds']+row['observer_system_cpu_seconds']
        rows[mode]={'cpu_seconds':cpu,'cpu_cores':cpu/row['wall_seconds'],'wall_seconds':row['wall_seconds'],
                    'raw_trace_bytes':row.get('raw_trace_bytes'),'driver_returncode':row.get('driver_returncode')}
        if row.get('driver_returncode')!=0 or row.get('driver_failure') is not None:reasons.append(mode+':driver-not-successful')
        if row.get('workload_unchanged') is not True or not valid_receipt(row):
            reasons.append(mode+':workload-equivalence-unproven')
        if mode=='on' and (not isinstance(row.get('observation'),dict) or row['observation'].get('capture_complete') is not True):
            reasons.append('on:trace-incomplete')
    on=document(on_path);off=document(off_path)
    def fingerprint(row):return receipt(row).get('fingerprint')
    if not fingerprint(on) or fingerprint(on)!=fingerprint(off):reasons.append('workload-fingerprint-mismatch-or-missing')
    if on.get('source_revision')!=off.get('source_revision'):reasons.append('tool-revision-mismatch')
    comparable=not reasons
    value={'comparable':comparable,'inconclusive_reasons':reasons,'resources':rows,
        'observed_wrapper_cpu_core_difference':rows['on']['cpu_cores']-rows['off']['cpu_cores'] if comparable else None,
        'hardware_acceptance_verified':False,'input_sha256':refs,
        'scope':'Wrapper/observer self CPU only, excludes driver workers and kernel impact. Matching successful workloads permit descriptive paired comparison, not causal attribution or statistical significance.'}
    for mode,path in paths.items():
        with path.open('rb') as stream:require(stream_sha(stream)==refs[mode],'INPUT_CHANGED')
    output.mkdir(parents=True,mode=0o700)
    path=output/'comparison.json';path.write_text(json.dumps(value,indent=2)+'\n');path.chmod(0o600)
    (output/'SHA256SUMS').write_text(hashlib.sha256(path.read_bytes()).hexdigest()+'  comparison.json\n')
    return value


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--on-result',type=Path,required=True);p.add_argument('--off-result',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    print(json.dumps(compare(a.on_result,a.off_result,a.output),indent=2))


if __name__=='__main__':main()
