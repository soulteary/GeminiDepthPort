#!/usr/bin/env python3
"""Run an unchanged retest command with optional bounded host observation.

Trace failures stop only observation; the original driver keeps its own writer
failure/cleanup rules. Outputs must be new and private. No hardware acceptance
is inferred from process exit, tracing, or file hashes.
"""
import argparse
import re
import hashlib
import json
import os
from pathlib import Path
import signal
import resource
import subprocess
import time
from host_kernel_trace import HostKernelTrace, stream_sha, document


def source_identity(source,revision):
    if re.fullmatch('[a-f0-9]{40}',revision) is None:raise ValueError('EXPLICIT_SOURCE_REVISION_REQUIRED')
    git=['git','--no-optional-locks','-c','safe.directory='+str(Path(source).resolve()),'-C',str(source)]
    head=subprocess.check_output(git+['rev-parse','HEAD'],text=True,timeout=30).strip()
    clean=subprocess.check_output(git+['status','--porcelain','--untracked-files=all'],text=True,timeout=30)
    if head!=revision or clean:raise ValueError('FIXED_CLEAN_SOURCE_REQUIRED')
    return head



def workload_receipt(args):
    """Conservative, read-only equivalence receipt for the official retest command."""
    try:
        command=list(args.command)
        script=Path(args.source)/'experiments/sdk-2.10.6-diagnostics/run_retest.py'
        if len(command)<3 or Path(command[1]).resolve()!=script.resolve() or command[2]!='run':
            return {'supported':False,'reason':'not-the-official-retest-command'}
        normalized=[Path(command[0]).name,script.name,'run'];tokens=command[3:]
        if tokens.count('--plan')!=1 or tokens.count('--root')!=1:
            return {'supported':False,'reason':'plan-or-root-not-unique'}
        plan_path=Path(tokens[tokens.index('--plan')+1])
        root=Path(tokens[tokens.index('--root')+1]).resolve()
        if not root.is_relative_to(Path(args.recording_path).resolve()):
            return {'supported':False,'reason':'retest-root-outside-recording-device-path'}
        plan=document(plan_path)
        if {c['name'] for c in plan['cameras']}!={'a','b','c'} or len(plan['cameras'])!=3:
            return {'supported':False,'reason':'three-camera-plan-required'}
        files=[]
        for camera in sorted(plan['cameras'],key=lambda c:c['name']):
            row={'name':camera['name'],'additional_plan_fields':{k:v for k,v in camera.items() if k not in ('name','capture_config','health_config')}}
            for field in ('capture_config','health_config'):
                path=Path(camera[field])
                if path.is_symlink() or not path.is_file() or not 0<path.stat().st_size<=4*1024**2:
                    return {'supported':False,'reason':'frozen-config-file-unavailable-or-unbounded'}
                with path.open('rb') as stream:row[field+'_sha256']=stream_sha(stream)
            files.append(row)
        for i,token in enumerate(tokens):
            normalized.append('<fresh-root>' if i and tokens[i-1]=='--root' else
                              '<frozen-plan>' if i and tokens[i-1]=='--plan' else token)
        profile={'source_revision':args.revision,'disk':args.disk,'filesystem_uuid':args.filesystem_uuid,
                 'recording_st_dev':Path(args.recording_path).stat().st_dev,'command':normalized,'camera_inputs':files,
                 'additional_plan_fields':{k:v for k,v in plan.items() if k!='cameras'}}
        fingerprint=hashlib.sha256(json.dumps(profile,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        return {'supported':True,'fingerprint':fingerprint,'profile':profile,
                'scope':'Exact command/config bytes with only fresh root/plan paths normalized; no external-load or cache-state proof.'}
    except (OSError,ValueError,KeyError,IndexError,TypeError) as error:
        return {'supported':False,'reason':'workload-receipt-unavailable','error':type(error).__name__+':'+str(error)}


def run(args):
    if not args.execute or not args.command:raise ValueError('EXPLICIT_EXECUTE_AND_COMMAND_REQUIRED')
    output=args.output.absolute()
    if output.exists() or output.is_symlink():raise ValueError('FRESH_OUTPUT_REQUIRED')
    if not 600<=args.max_seconds<=86400:raise ValueError('DRIVER_DEADLINE_BOUND')
    revision=source_identity(args.source,args.revision)
    output.mkdir(parents=True,mode=0o700)
    observer=None;child=None;trace=None;began=time.monotonic();failure=None
    usage_before=resource.getrusage(resource.RUSAGE_SELF)
    receipt_before=workload_receipt(args)
    try:
        if args.observer=='on':
            observer=HostKernelTrace(args.source,output/'trace',args.recording_path,args.retest_root,
                disk=args.disk,expected_uuid=args.filesystem_uuid,expected_revision=args.revision,
                max_bytes=args.max_bytes,max_seconds=args.max_seconds)
            observer.start()
        stop_file=output/'stop-after-current-group.json'
        child=subprocess.Popen(args.command,cwd=args.source,env=dict(os.environ,GDP_OBSERVER_STOP_FILE=str(stop_file)))
        while child.poll() is None:
            if observer:
                reason=observer.poll()
                if reason and not stop_file.exists():
                    fd=os.open(stop_file,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
                    with os.fdopen(fd,'w') as stream:json.dump({'reason':reason,'stop_after_current_group':True},stream)
                # The batch checks this only before/after a group: never interrupt writer cleanup.
            if time.monotonic()-began>args.max_seconds:
                failure='DRIVER_DEADLINE';child.send_signal(signal.SIGTERM)
                try:child.wait(timeout=120)
                except subprocess.TimeoutExpired:child.kill();child.wait()
                break
            time.sleep(.2)
    finally:
        if child and child.poll() is None:
            child.send_signal(signal.SIGTERM)
            try:child.wait(timeout=120)
            except subprocess.TimeoutExpired:child.kill();child.wait()
        if observer:
            try:trace=observer.finish()
            except Exception as error:
                trace={'capture_complete':False,'finalization_error':type(error).__name__+':'+str(error)}
        receipt_after=workload_receipt(args)
        usage_after=resource.getrusage(resource.RUSAGE_SELF)
        result={'workload_receipt':receipt_before,
            'workload_unchanged':receipt_before==receipt_after if receipt_before.get('supported') else None,
            'observer_user_cpu_seconds':usage_after.ru_utime-usage_before.ru_utime,
            'observer_system_cpu_seconds':usage_after.ru_stime-usage_before.ru_stime,
            'observer_maxrss_native_units':usage_after.ru_maxrss,
            'cpu_scope':'wrapper plus its observer threads only; excludes driver, Docker workers and kernel threads',
            'version':1,'source_revision':revision,'command_sha256':hashlib.sha256(json.dumps(args.command).encode()).hexdigest(),
            'observer_mode':args.observer,'driver_returncode':child.returncode if child else None,
            'driver_failure':failure,'observation':trace,'hardware_acceptance_verified':False,
            'wall_seconds':time.monotonic()-began,
            'scope':'driver and observer conclusions remain separate; original acceptance reports are authoritative'}
        cpu=result['observer_user_cpu_seconds']+result['observer_system_cpu_seconds']
        result['observer_cpu_cores']=cpu/result['wall_seconds']
        rows=trace.get('instances',[]) if isinstance(trace,dict) else []
        result['raw_trace_bytes']=(sum(r['raw_bytes'] for r in rows) if len(rows)==2 and all(type(r.get('raw_bytes')) is int and r['raw_bytes']>=0 for r in rows) else None) if observer else 0
        path=output/'result.json'
        fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as stream:json.dump(result,stream,indent=2);stream.write('\n')
        with path.open('rb') as stream:digest=stream_sha(stream)
        fd=os.open(output/'SHA256SUMS',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as stream:
            stream.write(digest+'  result.json\n')
            for name in ('stop-after-current-group.json','trace/SHA256SUMS'):
                path=output/name
                if path.is_file():
                    with path.open('rb') as source:stream.write(stream_sha(source)+'  '+name+'\n')
    return 0 if child and child.returncode==0 and not failure and (not observer or trace['capture_complete']) else 2


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--execute',action='store_true');p.add_argument('--observer',choices=('on','off'),required=True)
    for name in ('source','output','recording-path','retest-root'):p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--revision',required=True);p.add_argument('--filesystem-uuid',required=True);p.add_argument('--disk',required=True)
    p.add_argument('--max-seconds',type=int,default=21600);p.add_argument('--max-bytes',type=int,default=1024**3)
    p.add_argument('command',nargs=argparse.REMAINDER)
    a=p.parse_args()
    if a.command[:1]==['--']:a.command=a.command[1:]
    return run(a)


if __name__=='__main__':raise SystemExit(main())
