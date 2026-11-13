#!/usr/bin/env python3
"""Compare identical-target three-writer groups, fixed 8 versus 6 MiB, sync_all."""
import argparse
from bisect import bisect_left, bisect_right
from collections import Counter
import json
from pathlib import Path
import sys
import first_checkpoint_phase_run as common

GAP_NS=33_333_334
CREDIT_REASONS={'global_payload_credit','global_pair_credit','global_payload_and_pair_credit'}


def observe_child(child,target):
    obs=child['experiment_phase'];queue=child['experiment_queue']
    if obs.get('target_mib')!=target or obs.get('clock')!='CLOCK_MONOTONIC' or queue.get('clock')!='CLOCK_MONOTONIC':
        raise ValueError('FIXED_TARGET_OR_CLOCK_CONTRACT')
    if obs['dropped'] or queue['dropped']: raise ValueError('OBSERVATION_DROPPED')
    events=obs['events']; samples=queue['samples']; stamps=[s['monotonic_ns'] for s in samples]
    if stamps!=sorted(stamps): raise ValueError('QUEUE_CLOCK_ORDER')
    if [s['sequence'] for s in samples]!=list(range(1,len(samples)+1)): raise ValueError('QUEUE_SAMPLE_SEQUENCE')
    if any(not 0<=s['queued_pairs']<=16 or s['oldest_age_ns']<0 for s in samples): raise ValueError('QUEUE_OBSERVATION_RANGE')
    def near(stamp,after=False):
        index=bisect_left(stamps,stamp) if after else bisect_right(stamps,stamp)-1
        return samples[index] if 0<=index<len(samples) else None
    dispatch=[e for e in events if e['phase']=='dispatch'];previous=0;batches=[]
    for e in dispatch:
        count=e['pairs']-previous
        if count<=0 or e['bytes']<=0: raise ValueError('BATCH_PROGRESS')
        batches.append(dict(checkpoint=e['checkpoint'],pairs=count,bytes=e['bytes'],monotonic_ns=e['monotonic_ns']))
        previous=e['pairs']
    starts={};waits=[]
    for e in events:
        if e['phase']=='credit_start':
            if e['checkpoint'] in starts: raise ValueError('DUPLICATE_WAIT_START')
            c=e['credit']
            if c['payload_limit_bytes']!=16*1024**2 or c['pair_limit']!=32: raise ValueError('CREDIT_LIMIT_CHANGED')
            if c['pending_payload_bytes']+c['active_payload_bytes']>16*1024**2 or c['pending_pairs']+c['active_pairs']>32:
                raise ValueError('CREDIT_LIMIT_EXCEEDED')
            starts[e['checkpoint']]=e
        if e['phase']=='credit_end':
            start=starts.pop(e['checkpoint'],None)
            if start is None or e['monotonic_ns']<start['monotonic_ns']: raise ValueError('WAIT_PAIR_OR_CLOCK')
            if start['reason'] in CREDIT_REASONS:
                waits.append(dict(start_ns=start['monotonic_ns'],end_ns=e['monotonic_ns'],
                    wall_ns=e['monotonic_ns']-start['monotonic_ns'],reason=start['reason'],credit=start['credit'],
                    queue_before=near(start['monotonic_ns']),queue_after=near(e['monotonic_ns'],True)))
    waits.sort(key=lambda w:w['start_ns']);episodes=[]
    for wait in waits:
        if not episodes or wait['start_ns']-episodes[-1]['end_ns']>GAP_NS:
            episodes.append(dict(start_ns=wait['start_ns'],end_ns=wait['end_ns'],wait_ns=wait['wall_ns'],waits=1))
        else:
            episode=episodes[-1];episode.update(end_ns=wait['end_ns'],wait_ns=episode['wait_ns']+wait['wall_ns'],waits=episode['waits']+1)
    committed={e['checkpoint'] for e in events if e['phase']=='commit_end'}
    complete=not starts and {e['checkpoint'] for e in dispatch}==committed
    if child['ok']:
        if not complete or len(samples)!=child['pairs']: raise ValueError('NORMAL_OBSERVATION_INCOMPLETE')
        if not batches or batches[0]['pairs']!={8:7,6:5}[target]: raise ValueError('FIRST_BATCH_NOT_EXPECTED')
    seconds=child['capture_elapsed_ns']/1e9
    return dict(target_mib=target,complete=complete,actual_batches=batches,
        batch_pair_histogram=dict(Counter(b['pairs'] for b in batches)),
        dispatches_in_capture=sum(b['monotonic_ns']<=child['capture_finished_monotonic_ns'] for b in batches),
        capture_seconds=seconds,checkpoint_dispatches_per_capture_second=sum(b['monotonic_ns']<=child['capture_finished_monotonic_ns'] for b in batches)/seconds,
        lifecycle_fsync_calls=child['diagnostics']['fsync']['samples'],credit_waits=waits,
        unclosed_waits=list(starts.values()),continuous_wait_gap_limit_ns=GAP_NS,continuous_wait_episodes=episodes,
        oldest_queued_age_ns_max=max((s['oldest_age_ns'] for s in samples),default=0),
        queue_depth_at_push_max=max((s['queued_pairs'] for s in samples),default=0),
        scope='before-push queue samples are not atomic with writer spans; lifecycle fsync count includes setup/finalization')


def observe(row):
    children=row['writers'];target={'mib8':8,'mib6':6}[row['variant']]
    if len(children)!=3: raise ValueError('THREE_WRITERS_REQUIRED')
    barriers={c['barrier_target_monotonic_ns'] for c in children}
    if len(barriers)!=1: raise ValueError('COMMON_BARRIER_REQUIRED')
    return dict(target_mib=target,barrier_monotonic_ns=next(iter(barriers)),
        actual_start_offsets_ns=[c['actual_start_monotonic_ns']-c['barrier_target_monotonic_ns'] for c in children],
        writers=[observe_child(c,target) for c in children])


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--build',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    group=p.add_mutually_exclusive_group();group.add_argument('--faults',action='store_true');group.add_argument('--recovery-only',action='store_true')
    p.add_argument('--seconds',type=int,default=600);p.add_argument('--repeats',type=int,default=3)
    p.add_argument('--max-output-bytes',type=int,default=768*1024**3);p.add_argument('--max-wall-seconds',type=int,default=24000)
    args=p.parse_args();build=args.build.resolve();modes=('mib8','mib6')
    manifest=json.loads((build/'build-manifest.json').read_text())
    if manifest.get('experiment')!='fixed-checkpoint-target-v1' or manifest.get('target_mib')!={'mib8':8,'mib6':6}:
        raise ValueError('FIXED_TARGET_BUILD_REQUIRED')
    if args.faults:
        rows=common.faults.run(build,args.output.absolute(),tuple((m,'fsync') for m in modes),delay_ms=400)
        for row in rows:
            child=row['result'];target=int(row['variant'][3:]);observed=observe_child(child,target)
            if row['fault']=='delay' and (not observed['credit_waits'] or observed['oldest_queued_age_ns_max']<=0):
                raise ValueError('CREDIT_STALL_NOT_OBSERVED')
        common.ab.write_json(args.output.absolute()/'fixed-target-fault-verdict.json',dict(ok=True,cases=len(rows),delay_ms=400,target_mib=[8,6]))
    elif args.recovery_only:
        common.recovery_only(build,args.output.absolute(),modes)
    else:
        result=common.run(args,variants=modes,observer=observe,recovery_modes=modes)
        print(json.dumps({k:result[k] for k in ('ok','error','completed_groups','attempted_groups','planned_groups')}))
        sys.exit(0 if result['ok'] else 1)


if __name__=='__main__':main()
