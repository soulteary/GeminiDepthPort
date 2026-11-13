#!/usr/bin/env python3
"""Three simultaneous producers: aligned 7/7/7 versus first batches 3/5/7."""
import argparse
import json
import os
from pathlib import Path
import sys
import time
import writer_contention_ab as ab
import index_sync_faults as faults


def phase_report(row, targets):
    if len(row['writers']) != 3 or len(targets) != 3: raise ValueError('THREE_WRITERS_REQUIRED')
    starts=[]; intervals=[]; first=[]
    barrier=None
    for child,target in zip(row['writers'],targets):
        if barrier is None: barrier=child['barrier_target_monotonic_ns']
        if child['barrier_target_monotonic_ns'] != barrier: raise ValueError('PRODUCERS_NOT_SAME_BARRIER')
        obs=child['experiment_phase']
        if obs['clock']!='CLOCK_MONOTONIC' or obs['dropped']!=0: raise ValueError('PHASE_OBSERVATION_INCOMPLETE')
        events=obs['events']; dispatch=[e for e in events if e['phase']=='dispatch']
        if not dispatch or dispatch[0]['pairs']!=target: raise ValueError('FIRST_BATCH_CONTRACT')
        first.append(dispatch[0]['pairs'])
        starts.append([e['monotonic_ns']-barrier for e in dispatch])
        index={e['checkpoint']:e['monotonic_ns'] for e in events if e['phase']=='index_start'}
        ends={e['checkpoint']:e['monotonic_ns'] for e in events if e['phase']=='index_end'}
        commits={e['checkpoint'] for e in events if e['phase']=='commit_end'}
        if set(index)!=set(ends) or {e['checkpoint'] for e in dispatch}!=commits:
            raise ValueError('CHECKPOINT_PHASE_INCOMPLETE')
        for checkpoint,start in index.items():
            end=ends[checkpoint]
            if end<start: raise ValueError('PHASE_CLOCK_ORDER')
            intervals.extend(((start,1),(end,-1)))
    occupied={1:0,2:0,3:0}; active=0; previous=None
    for stamp,change in sorted(intervals):
        if previous is not None and active: occupied[active]+=stamp-previous
        active+=change;previous=stamp
    return dict(first_pairs=first, dispatch_offsets_ns=starts,
                index_sync_concurrency_wall_ns=occupied,
                scope='application spans across same-host CLOCK_MONOTONIC; not block-device latency')


def run(args):
    if sys.platform!='linux': raise ValueError('LINUX_REQUIRED')
    if not 1<=args.seconds<=3600 or not 3<=args.repeats<=10: raise ValueError('BOUNDED_SECONDS_REPEATS_REQUIRED')
    if not 1<=args.max_wall_seconds<=86400: raise ValueError('BOUNDED_WALL_REQUIRED')
    if not 128*1024**2<=args.max_output_bytes<=1024**4: raise ValueError('BOUNDED_CAPACITY_REQUIRED')
    build=args.build.resolve(); manifest=json.loads((build/'build-manifest.json').read_text())
    for v in manifest['variants'].values():
        if ab.sha256(v['binary'])!=v['binary_sha256'] or ab.sha256(v['payload'])!=v['payload_sha256']:
            raise ValueError('BUILD_HASH_MISMATCH')
    pairs=args.seconds*30; estimate=ab.estimate_bytes(pairs,args.repeats,2)
    output=args.output.absolute()
    if output.exists() or output.is_symlink(): raise ValueError('FRESH_OUTPUT_REQUIRED')
    if estimate>args.max_output_bytes: raise ValueError('PLANNED_CAPACITY_EXCEEDS_LIMIT:'+str(estimate))
    output.parent.mkdir(parents=True,exist_ok=True)
    fs=os.statvfs(output.parent)
    if fs.f_bavail*fs.f_frsize<estimate: raise ValueError('INSUFFICIENT_FREE_SPACE')
    output.mkdir();deadline=time.monotonic()+args.max_wall_seconds
    ab.write_json(output/'manifest.json',dict(build=manifest,seconds=args.seconds,repeats=args.repeats,
        planned_bytes=estimate,filesystem=ab.mount_evidence(output),status_background_load=False,
        queue_capacity=16,credit_bytes=16*1024**2,credit_pairs=32,common_barrier=True,
        limits='Synthetic storage-only experiment, no SDK/ROS/heartbeats or production status load; identical bounded collector in both; no hardware acceptance'))
    rows=[]; recovery={}; error=None; expected=None; completed=0
    try:
        for repeat in range(args.repeats):
            for variant in (('aligned','staggered') if repeat%2==0 else ('staggered','aligned')):
                binary=Path(manifest['variants'][variant]['binary'])
                name=f'round-{repeat:02d}-{variant}'
                row=ab.run_batch(binary,output/name,3,pairs,output,args.max_output_bytes,deadline)
                row.update(variant=variant,writer_count=3,repeat=repeat,directory=name)
                rows.append(row);ab.write_json(output/(name+'.json'),row)
                for child in row['writers']:
                    fingerprints=ab.validate_result(child,pairs)
                    if child['common_driver_sha256']!=manifest['common_driver_sha256']: raise ValueError('COMMON_DRIVER_HASH')
                    # Layout changes deliberately; each file hash and every frame
                    # are validated independently, not equated across layouts.
                    fingerprint=(fingerprints[0],fingerprints[1]['snapshot.json'])
                    if expected is None: expected=fingerprint
                    if expected!=fingerprint: raise ValueError('SAME_INPUT_OR_SNAPSHOT_HASH_MISMATCH')
                if not row['resource_diagnostic_complete']: raise ValueError('RESOURCE_DIAGNOSTIC_INCOMPLETE')
                phase=phase_report(row,[3,5,7] if variant=='staggered' else [7,7,7])
                ab.write_json(output/(name+'-phase.json'),phase)
                completed+=1
        for mode in ('first3','first5','first7'):
            recovery[mode]=ab.run_recovery(Path(manifest['variants'][mode]['binary']),output/('recovery-'+mode),output,args.max_output_bytes,deadline)
    except Exception as e:
        error=type(e).__name__+':'+str(e)
    summary=dict(ok=error is None,error=error,completed_groups=completed,attempted_groups=len(rows),planned_groups=args.repeats*2,
                 groups=ab.summarize_groups(rows,{'aligned':None,'staggered':None}),recovery=recovery,hardware_acceptance=False)
    ab.write_json(output/'summary.json',summary)
    with (output/'SHA256SUMS').open('x') as f:
        for path in sorted(output.rglob('*')):
            if path.is_file() and path.name!='SHA256SUMS': f.write(ab.sha256(path)+'  '+str(path.relative_to(output))+'\n')
    return summary


def recovery_only(build, output):
    if sys.platform!='linux': raise ValueError('LINUX_REQUIRED')
    manifest=json.loads((build/'build-manifest.json').read_text())
    output.mkdir(parents=True,exist_ok=False)
    results={}; error=None
    try:
        for mode in ('first3','first5','first7'):
            v=manifest['variants'][mode]
            if ab.sha256(v['binary'])!=v['binary_sha256'] or ab.sha256(v['payload'])!=v['payload_sha256']:
                raise ValueError('BUILD_HASH_MISMATCH')
            results[mode]=ab.run_recovery(Path(v['binary']),output/mode,output,2*1024**3,time.monotonic()+60)
    except Exception as e:
        error=type(e).__name__+':'+str(e)
    ab.write_json(output/'recovery-summary.json',dict(ok=error is None,error=error,results=results,
        scope='independent synthetic recovery regression; does not accept a failed performance group'))
    if error is not None: raise ValueError(error)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--build',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--faults',action='store_true');p.add_argument('--recovery-only',action='store_true');p.add_argument('--seconds',type=int,default=600)
    p.add_argument('--repeats',type=int,default=3);p.add_argument('--max-output-bytes',type=int,default=768*1024**3)
    p.add_argument('--max-wall-seconds',type=int,default=24000)
    a=p.parse_args()
    if a.faults and a.recovery_only: p.error('choose faults or recovery-only')
    if a.recovery_only:
        recovery_only(a.build.resolve(),a.output.absolute())
    elif a.faults:
        rows=faults.run(a.build.resolve(),a.output.absolute(),tuple((f'first{n}','fsync') for n in (3,5,7)))
        for row in rows:
            obs=row['result']['experiment_phase']
            events=[e for e in obs['events'] if e['phase']=='dispatch']
            if obs['dropped']!=0 or not events or events[0]['pairs']!=int(row['variant'][5:]):
                raise ValueError('FAULT_FIRST_BATCH_OR_OBSERVATION_CONTRACT')
        ab.write_json(a.output.absolute()/'phase-fault-verdict.json',dict(ok=True,cases=len(rows),first_pairs=[3,5,7]))
    else:
        result=run(a);print(json.dumps({k:result[k] for k in ('ok','error','completed_groups','planned_groups')}));sys.exit(0 if result['ok'] else 1)


if __name__=='__main__':main()
