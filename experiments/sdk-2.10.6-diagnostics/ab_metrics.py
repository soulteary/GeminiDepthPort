"""Streaming summaries of accepted SDK experiments; scopes are never conflated."""
import math
from pathlib import Path
import statistics
import experiment_runtime as rt
import hardware as hw


def rows(path, limit=4*1024**2):
    with Path(path).open('rb') as source:
        while True:
            line=source.readline(limit+1)
            if not line:break
            rt.require(len(line)<=limit,'METRIC_LINE_LIMIT')
            import json
            yield json.loads(line,object_pairs_hook=rt.pairs,parse_constant=lambda _:rt.require(False,'NONFINITE_METRIC'))


def number(value):
    rt.require(type(value) in (int,float) and math.isfinite(value) and value>=0,'INVALID_METRIC')
    return value


def cpu_memory(root, cameras):
    previous=None;identities=None;count=0;elapsed=0
    output=[{'camera':c['name'],'cpu_seconds':0.,'rss_peak_bytes':0,'hwm_lifetime_bytes':0,'cgroup_memory_peak_bytes':None} for c in cameras]
    total_rss_peak=0
    for row in rows(root/'capture/checkpoint-samples/samples.jsonl'):
        if row['kind']!='sample':continue
        stamp=number(row['monotonic_ns']);targets=row['targets']
        rt.require(len(targets)==3 and {t['index'] for t in targets}=={0,1,2},'THREE_METRIC_TARGETS')
        targets=sorted(targets,key=lambda v:v['index'])
        current=[(t['process']['host_pid'],t['process']['start_ticks'],t['epoch']) for t in targets]
        if identities is None:identities=current
        rt.require(identities==current,'METRIC_WORKER_CHANGED')
        dt=0 if previous is None else (stamp-previous)/1e9
        rt.require(previous is None or dt>0,'METRIC_CLOCK_REGRESSION')
        concurrent=0
        for i,t in enumerate(targets):
            memory=t['process']['memory'];rss=number(memory['VmRSS']);hwm=number(memory['VmHWM'])
            output[i]['rss_peak_bytes']=max(output[i]['rss_peak_bytes'],rss)
            output[i]['hwm_lifetime_bytes']=max(output[i]['hwm_lifetime_bytes'],hwm)
            group=t.get('cgroup') or {};peak=group.get('memory.peak')
            if peak is not None:output[i]['cgroup_memory_peak_bytes']=max(output[i]['cgroup_memory_peak_bytes'] or 0,number(peak))
            else:output[i]['cgroup_memory_peak_unavailable']=True
            concurrent+=rss
            if previous is not None:output[i]['cpu_seconds']+=number(t['rates']['cpu_cores'])*dt
        total_rss_peak=max(total_rss_peak,concurrent);previous=stamp;elapsed+=dt;count+=1
    rt.require(count>=2 and elapsed>0,'INSUFFICIENT_CPU_SAMPLES')
    for item in output:item['cpu_cores']=item['cpu_seconds']/elapsed
    return {'observed_seconds':elapsed,'samples':count,'cameras':output,
            'worker_cpu_cores':sum(v['cpu_cores'] for v in output),'concurrent_worker_rss_peak_bytes':total_rss_peak,
            'sum_worker_hwm_lifetime_bytes':sum(v['hwm_lifetime_bytes'] for v in output),
            'scope':'worker process including SDK threads; excludes supervisor/health/observers; RSS is sampled, HWM lifetime; cgroup includes file cache'}


def histogram_delta(first,last):
    rt.require(len(first['log2_ns_buckets'])==len(last['log2_ns_buckets'])==64,'SDK_HISTOGRAM_SIZE')
    buckets=[b-a for a,b in zip(first['log2_ns_buckets'],last['log2_ns_buckets'])]
    rt.require(len(buckets)==64 and all(n>=0 for n in buckets),'SDK_HISTOGRAM_REGRESSION')
    count=last['count']-first['count'];total=last['total_ns']-first['total_ns']
    rt.require(count>=0 and total>=0,'SDK_LOCK_REGRESSION')
    consistent=sum(buckets)==count;p99=None
    if count and consistent:
        rank=(count*99+99)//100;cumulative=0
        for i,n in enumerate(buckets):
            cumulative+=n
            if cumulative>=rank:p99={'lower_bound_ns':0 if i==0 else 2**i,'upper_bound_ns':2**(i+1)-1};break
    return {'count_delta':count,'total_ns_delta':total,'p99_log2_bounds_ns':p99,
            'histogram_count_consistent':consistent,'scope':'SDK nontransactional cumulative snapshot delta; nested lock sites not additive'}


def sdk_metrics(root,mode,cameras):
    if mode=='off':return {'available':False,'reason':'diagnostics-disabled; not zero measurements'}
    snapshots={}
    for row in rows(root/'capture/sdk-samples/samples.jsonl'):
        s=row['snapshot'];rt.require(s['sdk_base']==rt.BASE and s.get('sink_errors')==0,'SDK_METRIC_SOURCE_OR_SINK')
        key=(row['source'],row['file'],s['pid'],s['instance'])
        old=snapshots.get(key)
        if old:
            rt.require(s['snapshot_monotonic_ns']>=old[1]['snapshot_monotonic_ns'],'SDK_CLOCK_REGRESSION')
            snapshots[key]=(old[0],s)
        else:snapshots[key]=(s,s)
    output=[]
    for camera in cameras:
        instances=[]
        for key,(first,last) in snapshots.items():
            if key[0]!=camera['name'] or last['counters']['input_frames']==0:continue
            duration=(last['snapshot_monotonic_ns']-first['snapshot_monotonic_ns'])/1e9
            rt.require(duration>0,'SDK_WINDOW_TOO_SHORT')
            counters={k:last['counters'][k]-v for k,v in first['counters'].items()}
            rt.require(all(v>=0 for v in counters.values()),'SDK_COUNTER_REGRESSION')
            locks=[]
            rt.require([v['site'] for v in first['locks']]==[v['site'] for v in last['locks']],'SDK_LOCK_SITES_CHANGED')
            for a,b in zip(first['locks'],last['locks']):
                locks.append({'site':b['site'],**{name:histogram_delta(a[name],b[name]) for name in ('wait','hold')}})
            instances.append({'file':key[1],'pid':key[2],'instance':key[3],'observed_seconds':duration,'counter_deltas':counters,'counter_rates_per_second':{k:v/duration for k,v in counters.items()},
                              'streams_lifetime_peaks':[v for v in last['streams'] if v['input'] or v['aggregate_queue_peak']], 'locks':locks})
        rt.require(instances,'MISSING_SDK_CAMERA_METRICS:'+camera['name'])
        output.append({'camera':camera['name'],'instances':instances})
    return {'available':True,'cameras':output,'scope':'first/last observed SDK snapshots, typically 5-second cadence; queue peaks are lifetime, not deltas; candidate counts are not unique lost frames'}


def summarize(root, manifest, mode):
    result=rt.read_json(root/'result.json');run=rt.read_json(root/'capture/run.json')
    rt.require(result.get('ok') is True and result.get('hardware_acceptance_verified') is True
               and not result.get('cleanup_errors'),'UNACCEPTED_RUN')
    rt.require(result['image_id']==run['image_id']==manifest['image_id']
               and result['runtime_sha256']==run['runtime_sha256']==manifest['runtime_sha256']
               and run['mode']==mode and result['uid']==manifest['uid'] and result['gid']==manifest['gid'],'RUN_BINDING_CHANGED')
    config_hashes={}
    for camera in run['cameras']:
        cp=root/'capture'/camera['name']/'config';config=rt.read_json(cp/'capture.json')
        binding=rt.read_json(cp/'experiment.json')
        rt.require(rt.sha(cp/'capture.json')==binding['capture_sha256'] and rt.sha(cp/'health.json')==binding['health_sha256'],'CONFIG_BYTES_CHANGED')
        rt.require(binding['diagnostics']==(mode=='on') and binding['source_config_sha256']==manifest['source_configs'][camera['name']], 'SOURCE_OR_MODE_CHANGED')
        del config['service']['generation']
        config_hashes[camera['name']]=hw.digest({'capture':config,'health_sha256':binding['health_sha256']})
    final=rt.read_json(root/'final-status.json');disk=[]
    for camera in final['cameras']:
        raw=camera['capture']['raw_recording'];stages=raw['disk_stages']
        rt.require(raw['state']=='complete' and stages['failed_calls']==0,'FINAL_DISK_STATUS')
        append=stages['append'];verify=stages['verification']
        rt.require(append['samples']>0 and verify['samples']>0 and append['p99_ns'] is not None,'MISSING_DISK_TIMINGS')
        disk.append({'camera':camera['name'],'append_p99_bounds_ns':append['p99_ns'],
                     'verification_wall_total_ns':int(verify['total_ns']),
                     'checkpoint_readback':stages['checkpoint_observation']['stages'].get('readback'),
                     'final_readback':stages['checkpoint_observation']['stages'].get('final_readback')})
    rt.require(len(disk)==3 and {v['camera'] for v in disk}==set(config_hashes),'THREE_DISK_METRICS')
    cpu=cpu_memory(root,run['cameras'])
    values={'worker_cpu_cores':cpu['worker_cpu_cores'],'worker_rss_peak_bytes':cpu['concurrent_worker_rss_peak_bytes'],
            'sum_hwm_lifetime_bytes':cpu['sum_worker_hwm_lifetime_bytes'],
            'worst_camera_append_p99_upper_ns':max(v['append_p99_bounds_ns']['upper_bound_ns'] for v in disk),
            'writer_verification_total_ns':sum(v['verification_wall_total_ns'] for v in disk),
            'external_audit_wall_ns':number(result['command_timings_ns']['audit'])}
    overhead={'checkpoint':rt.read_json(root/'capture/checkpoint-samples/summary.json'),
              'dds':rt.read_json(root/'capture/wire.sqlite3.summary.json')}
    if mode=='on':overhead['sdk']=rt.read_json(root/'capture/sdk-samples/summary.json')
    return {'ok':True,'mode':mode,'root':str(root),'normalized_config_hashes':config_hashes,'metrics':values,
            'cpu_memory':cpu,'disk_lifetime':disk,'sdk':sdk_metrics(root,mode,run['cameras']),
            'source_gap_free':result['source_gap_free'],'source_sequence_gaps':result['source_sequence_gaps'],
            'observers':overhead,'scope':'CPU/RSS use sampling window; disk P99/verification cover whole native recording; external audit includes Docker and three sequential audits; quantile bounds are not exact/global P99'}


def sdk_round_summary(groups):
    by_camera={}
    for group in groups:
        sdk=group['summary']['sdk']
        if not sdk['available']:continue
        for camera in sdk['cameras']:
            counters={};queues={};locks={};intervals=[]
            for instance in camera['instances']:
                intervals.append(instance['observed_seconds'])
                for name,value in instance['counter_deltas'].items():counters[name]=counters.get(name,0)+value
                for stream in instance['streams_lifetime_peaks']:
                    key=str(stream['type']);queues[key]=max(queues.get(key,0),stream['aggregate_queue_peak'])
                for lock in instance['locks']:
                    for phase in ('wait','hold'):
                        key=lock['site']+'/'+phase;value=lock[phase]
                        item=locks.setdefault(key,{'count':0,'total_ns':0,'worst_instance_p99_upper_ns':None})
                        item['count']+=value['count_delta'];item['total_ns']+=value['total_ns_delta']
                        if value['p99_log2_bounds_ns'] is not None:
                            item['worst_instance_p99_upper_ns']=max(item['worst_instance_p99_upper_ns'] or 0,value['p99_log2_bounds_ns']['upper_bound_ns'])
            by_camera.setdefault(camera['camera'],[]).append({'round':group['round'],'counter_deltas':counters,
                'stream_lifetime_queue_peaks':queues,'locks':locks,'observed_instance_intervals_seconds':intervals})
    output={}
    for name,values in by_camera.items():
        counters=set().union(*(v['counter_deltas'] for v in values));sites=set().union(*(v['locks'] for v in values))
        output[name]={'rounds':values,'counter_delta_medians':{k:statistics.median(v['counter_deltas'][k] for v in values) for k in counters},
            'lock_medians':{k:{m:statistics.median(v['locks'][k][m] for v in values) for m in ('count','total_ns')} for k in sites}}
    return {'on':output,'off':None,'off_reason':'diagnostics-disabled',
            'scope':'on-only per-round snapshot deltas and lifetime queue peaks; lock sites overlap; P99 bounds retained per round, never pooled; inspect observed durations'}


def aggregate(groups,rounds):
    accepted=[g for g in groups if g.get('summary',{}).get('ok') is True]
    complete=len(accepted)==2*rounds and len(groups)==2*rounds
    report={'complete':complete,'required_rounds':rounds,'accepted_groups':len(accepted),'groups':groups,
            'comparison':None,'scope':'no pooling or averaging P99; descriptive medians across rounds, no significance or hardware benefit claim'}
    if not complete:return report
    hashes=accepted[0]['summary']['normalized_config_hashes']
    rt.require(all(g['summary']['normalized_config_hashes']==hashes for g in accepted),'CROSS_RUN_CONFIG_CHANGED')
    keys=accepted[0]['summary']['metrics'].keys();comparison={}
    for key in keys:
        on=[g['summary']['metrics'][key] for g in accepted if g['mode']=='on']
        off=[g['summary']['metrics'][key] for g in accepted if g['mode']=='off']
        rt.require(len(on)==len(off)==rounds,'UNBALANCED_ROUNDS')
        pairs=[]
        for r in range(1,rounds+1):
            a=next(g['summary']['metrics'][key] for g in accepted if g['round']==r and g['mode']=='on')
            b=next(g['summary']['metrics'][key] for g in accepted if g['round']==r and g['mode']=='off')
            pairs.append({'round':r,'on_minus_off':a-b,'percent':None if b==0 else (a-b)*100/b})
        comparison[key]={'on_median':statistics.median(on),'off_median':statistics.median(off),'paired_deltas':pairs}
    report['comparison']=comparison;report['sdk_on_rounds']=sdk_round_summary(accepted);return report
