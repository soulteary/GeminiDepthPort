#!/usr/bin/env python3
"""Read-only association of checkpoint tails with sampled queues and device IO.

Produces a new derived directory. It never edits a run, audits payloads, changes
acceptance, or infers a unique cause from temporal overlap.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import stat
import sys

MAX_FILE = 512 * 1024**2
MAX_LINE = 2 * 1024**2
MAX_ROWS = 200000
EVENT_FIELDS = ('event_id', 'info', 'ok', 'phase', 'pid', 'span_id',
                'start_monotonic_ns', 'thread_cpu_ns', 'tid', 'wall_ns')
WAIT_REASONS = frozenset(('finished_reap', 'global_payload_credit', 'global_pair_credit',
                          'global_payload_and_pair_credit', 'chunk_rotation', 'explicit_checkpoint'))


def require(condition, code):
    if not condition:raise ValueError(code)


def pairs(items):
    value = {}
    for key, item in items:
        require(key not in value, 'DUPLICATE_JSON_KEY')
        value[key] = item
    return value


def invalid_constant(value):
    raise ValueError('NONFINITE_JSON_NUMBER:'+value)


def digest(path):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_FILE, 'INPUT_FILE_LIMIT_OR_TYPE')
    result = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024**2), b''):result.update(block)
    return result.hexdigest()


def records(path, lines=False):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_FILE, 'INPUT_FILE_LIMIT_OR_TYPE')
    with path.open('rb') as source:
        if not lines:
            require(info.st_size <= MAX_LINE, 'INPUT_JSON_LIMIT')
            return json.loads(source.read(), object_pairs_hook=pairs, parse_constant=invalid_constant)
        result = []
        for raw in source:
            require(len(raw) <= MAX_LINE and len(result) < MAX_ROWS, 'INPUT_JSONL_LIMIT')
            result.append(json.loads(raw, object_pairs_hook=pairs, parse_constant=invalid_constant))
        return result


def integer(value, code):
    require(type(value) is int and value >= 0, code)
    return value


def point(row, entry, source, name):
    at = integer(row['monotonic_ns'], 'SAMPLE_CLOCK')
    process = entry.get('process') or {}
    identity = None
    if process:
        identity = (integer(process['host_pid'], 'WORKER_PID'), integer(process['start_ticks'], 'WORKER_START'))
    status = entry.get('status') or (entry if 'raw' in entry else {})
    raw = status.get('raw') or status.get('raw_recording') or {}
    result = {'source': source, 'sample_kind': row.get('kind'), 'target': name, 'source_epoch': entry.get('epoch'),
            'monotonic_ns': at, 'unix_ns': row.get('unix_ns'), 'phase': row.get('phase'),
            'identity': identity, 'process': process, 'rates': entry.get('rates'),
            'raw': raw, 'status_updated_unix_ms':status.get('updated_unix_ms'), 'writer_queue': status.get('writer_queue') or raw.get('writer_queue'),
            'active': (status.get('observation') or {}).get('active', []),
            'disks': row.get('disks', {}), 'error': entry.get('error')}
    observation = status.get('observation') or {}
    if 'active_append' in observation:result['active_append'] = observation['active_append']
    return result


def phase_stats(events):
    values = {}
    for event in events:
        phase = event['phase']; stage = values.setdefault(phase, {'events': 0, 'wall_total_ns': 0,
            'wall_max_ns': 0, 'thread_cpu_available': 0, 'thread_cpu_total_ns': 0, 'thread_cpu_max_ns': None})
        stage['events'] += 1; stage['wall_total_ns'] += event['wall_ns']
        stage['wall_max_ns'] = max(stage['wall_max_ns'], event['wall_ns'])
        cpu = event['thread_cpu_ns']
        if cpu is not None:
            stage['thread_cpu_available'] += 1; stage['thread_cpu_total_ns'] += cpu
            stage['thread_cpu_max_ns'] = max(stage['thread_cpu_max_ns'] or 0, cpu)
    return values


def bracket(samples, start, end):
    result = {}
    for source in ('lifecycle', 'checkpoint'):
        selected = [v for v in samples if v['source'] == source and v.get('process')]
        before = [v for v in selected if v['monotonic_ns'] <= start]
        after = [v for v in selected if v['monotonic_ns'] >= end]
        during = [v for v in selected if start <= v['monotonic_ns'] <= end]
        result[source] = {'before': before[-1]['sample_id'] if before else None,
                          'during': [v['sample_id'] for v in during],
                          'after': after[0]['sample_id'] if after else None}
    return result


def disk_window(samples, start, end):
    output = {}
    for source in ('lifecycle', 'checkpoint'):
        rows = [v for v in samples if v['source'] == source and v.get('disks')]
        before = [v for v in rows if v['monotonic_ns'] <= start]
        after = [v for v in rows if v['monotonic_ns'] >= end]
        if not before or not after:
            output[source] = {'available': False, 'reason': 'unbracketed sampled interval'}; continue
        first, last = before[-1], after[0]
        devices = {}
        for name in sorted(set(first['disks']) & set(last['disks'])):
            a, b = first['disks'][name], last['disks'][name]
            keys = ('read_ops', 'read_sectors', 'read_ms', 'write_ops', 'write_sectors',
                    'write_ms', 'io_ms', 'weighted_ms')
            delta = {key: b[key]-a[key] for key in keys if key in a and key in b}
            require(all(type(v) is int and v >= 0 for v in delta.values()), 'DISK_COUNTER_REGRESSION')
            devices[name] = {'deltas': delta,
                'write_bytes': delta.get('write_sectors', 0)*512,
                'read_bytes': delta.get('read_sectors', 0)*512,
                'first_inflight': a.get('inflight'), 'last_inflight': b.get('inflight')}
        output[source] = {'available': True, 'first_sample_id': first['sample_id'],
            'last_sample_id': last['sample_id'], 'sampled_span_ns': last['monotonic_ns']-first['monotonic_ns'],
            'devices': devices, 'scope': 'whole device cumulative counters over enclosing samples; not per-checkpoint physical latency'}
    return output


def credit_snapshot(value):
    require(type(value) is dict, 'CREDIT_SCHEMA')
    fields = ('pending_payload_bytes', 'pending_pairs', 'active_payload_bytes',
              'active_pairs', 'payload_limit_bytes', 'pair_limit')
    require(set(value) == set(fields), 'CREDIT_SCHEMA')
    return {key: integer(value[key], 'CREDIT_SCHEMA') for key in fields}


def internal_queue_history(samples, start, end):
    histories = {}
    for sample in samples:
        history = (sample.get('writer_queue') or {}).get('history') or {}
        if history.get('enabled') is not True:continue
        key = (sample['target'],sample['epoch'])
        group = histories.setdefault(key, {'target':key[0], 'epoch':key[1], 'events':{}, 'retained_ranges':[], 'before_window':None, 'after_window':None})
        rows = history.get('events',[])
        require(type(rows) is list and len(rows)<=512, 'QUEUE_HISTORY_BOUND')
        ids=[]
        for row in rows:
            if type(row) is list:
                columns=history.get('columns')
                require(type(columns) is list and len(columns)==len(row) and len(set(columns))==len(columns),'QUEUE_EVENT_COLUMNS')
                row=dict(zip(columns,row))
            require(type(row) is dict and row.get('operation') in ('enqueue','dequeue','written','overflow','close','abandon'), 'QUEUE_EVENT_SCHEMA')
            event_id=integer(row.get('event_id'),'QUEUE_EVENT_ID'); at=row.get('monotonic_ns')
            if at is None:continue
            integer(at,'QUEUE_EVENT_CLOCK');ids.append((event_id,at))
            if at<=start and (group['before_window'] is None or at>group['before_window']['monotonic_ns']):group['before_window']=row
            if at>=end and (group['after_window'] is None or at<group['after_window']['monotonic_ns']):group['after_window']=row
            if start<=at<=end:
                old=group['events'].setdefault(event_id,row)
                require(old==row,'CONFLICTING_QUEUE_EVENT')
        if ids and ids[-1][1]>=start and ids[0][1]<=end:group['retained_ranges'].append({'first_id':ids[0][0],'last_id':ids[-1][0],
            'first_monotonic_ns':ids[0][1],'last_monotonic_ns':ids[-1][1], 'sample_id':sample['sample_id']})
    output=[]
    for group in histories.values():
        rows=[group['events'][i] for i in sorted(group['events'])]
        group['events']=rows
        ids=sorted({r['event_id'] for r in rows+([group['before_window']] if group['before_window'] else [])+([group['after_window']] if group['after_window'] else [])})
        group['missing_between_selected_events']=sum(max(0,b-a-1) for a,b in zip(ids,ids[1:]))
        ranges=group['retained_ranges']
        group['window_bracketed']=group['before_window'] is not None and group['after_window'] is not None
        group['scope']='internal transition clocks; separate from sampled publication age; bounded retention and explicit missing events'
        output.append(group)
    return output


def wait_chain(samples, groups, failure):
    # Prefer the once-locked overflow timestamp to the later polling clock.
    frozen = failure['raw'].get('overflow_observation') or {}
    detected = frozen.get('detected_monotonic_ns')
    exact = detected is not None
    at = integer(detected, 'OVERFLOW_CLOCK') if exact else failure['monotonic_ns']
    require(at <= failure['monotonic_ns'], 'OVERFLOW_AFTER_OBSERVATION')
    start, end = max(0, at-1_000_000_000), at+100_000_000
    timeline, links = [], []
    for group in groups:
        for event in group['events']:
            first = event['start_monotonic_ns']; last = first+event['wall_ns']
            if last < start or first > end:continue
            row = {'target':group['target'], 'epoch':group['epoch'],
                   'checkpoint_id':group['checkpoint_id'], **event,
                   'relative_start_ns':first-at, 'relative_end_ns':last-at}
            timeline.append(row)
            if event['phase'] == 'verify_wait':
                dependencies = [other['event_id'] for other in group['events']
                    if other['phase'] != 'verify_wait' and other['start_monotonic_ns'] <= last
                    and other['start_monotonic_ns']+other['wall_ns'] >= first]
                links.append({'target':group['target'], 'epoch':group['epoch'],
                    'wait_event_id':event['event_id'], 'checkpoint_id':group['checkpoint_id'],
                    'wait_reason':event.get('wait_reason'), 'credit_at_wait':event.get('credit_at_wait'),
                    'overlapping_task_event_ids':dependencies,
                    'join_return_monotonic_ns':last, 'ok':event['ok']})
    queues = [{'sample_id':v['sample_id'], 'target':v['target'], 'epoch':v['epoch'],
               'source':v['source'], 'monotonic_ns':v['monotonic_ns'],
               'relative_ns':v['monotonic_ns']-at, 'writer_queue':v['writer_queue'],
               'active':v['active'], 'active_append':v.get('active_append')}
              for v in samples if start <= v['monotonic_ns'] <= end]
    return {'anchor_monotonic_ns':at, 'anchor_basis':'overflow_detection' if exact else 'first_failure_sample',
        'window_start_monotonic_ns':start, 'window_end_monotonic_ns':end,
        'queue_at_detection':frozen.get('queue_at_detection'),
        'writer_context_at_detection':frozen.get('writer_context'),
        'events':sorted(timeline,key=lambda v:(v['start_monotonic_ns'],v['target'],v['event_id'])),
        'wait_links':links, 'queue_samples':queues,
        'internal_queue_history':internal_queue_history(samples,start,end),
        'scope':'Credits belong to one writer, not a cross-process pool. Join completion is observed, not a credit transfer between writers. Same-checkpoint overlapping phases are task context, not proven exclusive causes. Sample times are not atomic with queue detection. Missing events/CPU/kernel traces remain missing; no unique storage or scheduling attribution.'}


def analyze(run_dir, before_seconds=10., after_seconds=2., failure_camera=None):
    require(math.isfinite(before_seconds) and 0 < before_seconds <= 600
            and math.isfinite(after_seconds) and 0 <= after_seconds <= 60, 'WINDOW_LIMIT')
    root = Path(run_dir).resolve()
    paths = {'run': root/'capture/run.json', 'lifecycle': root/'lifecycle/samples.jsonl',
             'checkpoint': root/'capture/checkpoint-samples/samples.jsonl',
             'lifecycle_summary': root/'lifecycle/summary.json',
             'checkpoint_summary': root/'capture/checkpoint-samples/summary.json'}
    hashes = {key: digest(path) for key, path in paths.items()}
    run = records(paths['run']); names = [c['name'] for c in run['cameras']]
    require(len(names) == len(set(names)) and names, 'TARGET_NAMES')
    summaries = {key: records(paths[key+'_summary']) for key in ('lifecycle', 'checkpoint')}
    samples = []; pending_events = []; identities = {}; duplicate_count = {'within_observer': 0, 'across_observers': 0}
    for source in ('lifecycle', 'checkpoint'):
        rows = records(paths[source], lines=True)
        for row in rows:
            if source == 'lifecycle' and row.get('kind') in ('resource-sample', 'event-sample'):
                for entry in row['cameras']:
                    require(entry['name'] in names, 'UNKNOWN_TARGET')
                    sample = point(row, entry, source, entry['name']); samples.append(sample)
                    for event in entry.get('checkpoint_events', []):
                        pending_events.append((source, entry['name'], entry['epoch'], event))
            elif source == 'checkpoint' and row.get('kind') == 'sample':
                for entry in row['targets']:
                    index = integer(entry['index'], 'TARGET_INDEX'); require(index < len(names), 'TARGET_INDEX')
                    samples.append(point(row, entry, source, names[index]))
            elif source == 'checkpoint' and row.get('kind') == 'checkpoint_event':
                index = integer(row['target_index'], 'TARGET_INDEX'); require(index < len(names), 'TARGET_INDEX')
                pending_events.append((source, names[index], row['epoch'], row))
    # Epoch counters are observer-local. Bind them to PID/start_ticks before merging.
    for sample in samples:
        if sample['identity'] is None or sample['source_epoch'] is None:continue
        key = (sample['source'], sample['target'], integer(sample['source_epoch'], 'SOURCE_EPOCH'))
        if key in identities:require(identities[key] == sample['identity'], 'OBSERVER_EPOCH_IDENTITY_CHANGED')
        identities[key] = sample['identity']
    canonical = {}
    for name in names:
        instances = sorted({identity for (_, target, _), identity in identities.items() if target == name}, key=lambda value: (value[1], value[0]))
        for epoch, identity in enumerate(instances):canonical[(name, identity)] = epoch
    for sample in samples:
        if sample['identity'] is None and sample['source_epoch'] is not None:
            sample['identity'] = identities.get((sample['source'],sample['target'],sample['source_epoch']))
        sample['epoch'] = canonical.get((sample['target'], sample['identity']))
    events = {}
    for source, name, epoch, raw in pending_events:
        key = (source, name, integer(epoch, 'SOURCE_EPOCH')); require(key in identities, 'EVENT_WITHOUT_BOUND_WORKER')
        value = {field: raw.get(field) for field in EVENT_FIELDS}
        if 'wait_reason' in raw:
            reason = raw['wait_reason']
            require(value['phase'] == 'verify_wait' and type(reason) is str
                    and reason in WAIT_REASONS, 'CHECKPOINT_WAIT_REASON')
            value['wait_reason'] = reason
        if 'credit_at_wait' in raw:
            require(value['phase'] == 'verify_wait', 'CREDIT_PHASE')
            value['credit_at_wait'] = credit_snapshot(raw['credit_at_wait'])
        integer(value['event_id'], 'EVENT_ID'); integer(value['start_monotonic_ns'], 'EVENT_CLOCK'); integer(value['wall_ns'], 'EVENT_WALL')
        if value['thread_cpu_ns'] is not None:integer(value['thread_cpu_ns'], 'EVENT_CPU')
        require(type(value['phase']) is str and type(value['info']) is dict, 'EVENT_SCHEMA')
        checkpoint = integer(value['info']['checkpoint_id'], 'CHECKPOINT_ID')
        identity = identities[key]; canonical_epoch = canonical[(name, identity)]
        event_key = (name, canonical_epoch, value['event_id'])
        if event_key in events:
            previous = events[event_key]
            require(previous['event'] == value, 'CONFLICTING_DUPLICATE_EVENT')
            duplicate_count['within_observer' if source in previous['sources'] else 'across_observers'] += 1
            previous['sources'].add(source)
        else:events[event_key] = {'event': value, 'sources': {source}, 'target': name, 'epoch': canonical_epoch, 'checkpoint_id': checkpoint}
    samples.sort(key=lambda value: (value['monotonic_ns'], value['source'], value['target']))
    for index, sample in enumerate(samples):sample['sample_id'] = index
    failures = [v for v in samples if v['raw'].get('state') == 'failed' and (failure_camera is None or v['target'] == failure_camera)]
    require(failures, 'NO_OBSERVED_RAW_FAILURE')
    failure = failures[0]; start = max(0, failure['monotonic_ns']-round(before_seconds*1e9)); end = failure['monotonic_ns']+round(after_seconds*1e9)
    detected = (failure['raw'].get('overflow_observation') or {}).get('detected_monotonic_ns')
    if detected is not None:
        detected = integer(detected, 'OVERFLOW_CLOCK')
        require(detected <= failure['monotonic_ns'], 'OVERFLOW_AFTER_OBSERVATION')
        start = min(start, max(0, detected-1_000_000_000))
        end = max(end, detected+100_000_000)
    context = []
    for name in names:
        selected = [v for v in samples if v['target'] == name]
        for source in ('lifecycle', 'checkpoint'):
            rows = [v for v in selected if v['source'] == source]
            prior = [v for v in rows if v['monotonic_ns'] < start]
            later = [v for v in rows if v['monotonic_ns'] > end]
            context.extend(([prior[-1]] if prior else [])+[v for v in rows if start <= v['monotonic_ns'] <= end]+([later[0]] if later else []))
    context.sort(key=lambda value: value['sample_id'])
    groups = {}
    selected_keys = {(item['target'],item['epoch'],item['checkpoint_id']) for item in events.values()
        if item['event']['start_monotonic_ns']+item['event']['wall_ns']>=start and item['event']['start_monotonic_ns']<=end}
    for item in events.values():
        event = item['event']; at = event['start_monotonic_ns']; until = at+event['wall_ns']
        key = (item['target'], item['epoch'], item['checkpoint_id'])
        if key not in selected_keys:continue
        group = groups.setdefault(key, {'target': key[0], 'epoch': key[1], 'checkpoint_id': key[2], 'events': []})
        group['events'].append({**event, 'sources': sorted(item['sources'])})
    for group in groups.values():
        group['events'].sort(key=lambda value: (value['start_monotonic_ns'], value['event_id']))
        group['start_monotonic_ns'] = min(v['start_monotonic_ns'] for v in group['events'])
        group['end_monotonic_ns'] = max(v['start_monotonic_ns']+v['wall_ns'] for v in group['events'])
        group['phase_stats'] = phase_stats(group['events'])
        peers = [v for v in samples if v['target'] == group['target'] and v['epoch'] == group['epoch']]
        group['resource_brackets'] = bracket(peers, group['start_monotonic_ns'], group['end_monotonic_ns'])
        group['disk_brackets'] = disk_window(peers, group['start_monotonic_ns'], group['end_monotonic_ns'])
    coverage = []
    for name, epoch in sorted({(key[0],key[1]) for key in events}):
        ids = sorted(key[2] for key in events if key[:2] == (name,epoch))
        gaps = []; previous = 0
        for event_id in ids:
            if event_id > previous+1:gaps.append({'first_missing_event_id':previous+1,'last_missing_event_id':event_id-1})
            previous = event_id
        coverage.append({'target':name,'epoch':epoch,'first_event_id':ids[0],'last_event_id':ids[-1],
            'unique_events':len(ids),'missing_through_last_observed':sum(v['last_missing_event_id']-v['first_missing_event_id']+1 for v in gaps),
            'missing_ranges':gaps,'scope':'union observed prefix only; does not prove events after last observed or payload completeness'})
    selected_events = [v for group in groups.values() for v in group['events']]
    healthy = [v for v in samples if v['target'] == failure['target'] and v['epoch'] == failure['epoch']
               and v['raw'].get('state') == 'recording' and v['monotonic_ns'] < failure['monotonic_ns']]
    summary = {'version': 1, 'analysis_only': True, 'hardware_acceptance_verified': False,
        'failure_first_observed': failure, 'failure_previous_recording_sample': healthy[-1] if healthy else None,
        'window': {'start_monotonic_ns': start, 'end_monotonic_ns': end, 'before_seconds': before_seconds, 'after_seconds': after_seconds},
        'three_writer_wait_chain':wait_chain(samples, list(groups.values()), failure),
        'source_summaries': summaries, 'input_sha256': hashes, 'deduplication': {**duplicate_count, 'unique_events_all_windows': len(events)},
        'worker_epochs': [{'source': key[0], 'target': key[1], 'source_epoch': key[2], 'host_pid': value[0], 'start_ticks': value[1], 'epoch': canonical[(key[1], value)]} for key, value in sorted(identities.items())],
        'event_union_coverage':coverage, 'checkpoint_groups': len(groups), 'phase_stats': phase_stats(selected_events),
        'limitations': ['Selected checkpoints have at least one event overlapping the window; all available phases of those groups are included, not a completeness guarantee.',
            'First observed failure brackets the fault, not its exact occurrence; lookup uses resource/event samples, not controller-status rows.',
            'Both observer event streams are deduplicated by target, bound worker epoch and event_id; missing histories remain missing.',
            'Checkpoint phases overlap and wall totals must not be added as end-to-end latency.',
            'Thread CPU may slightly exceed wall for short events due to clock resolution; missing CPU is not zero.',
            'Queue samples and whole-device disk counters establish temporal association, not a unique SSD/SDK/lock root cause.',
            'No payload reads, hashes or recovery acceptance are performed. Existing official results remain unchanged.']}
    referenced = {sample['sample_id'] for sample in context}
    for group in groups.values():
        for value in group['resource_brackets'].values():
            referenced.update(value['during'])
            referenced.update(v for v in (value['before'],value['after']) if v is not None)
        for value in group['disk_brackets'].values():
            if value.get('available'):referenced.update((value['first_sample_id'],value['last_sample_id']))
    context = [sample for sample in samples if sample['sample_id'] in referenced]
    require(hashes == {key: digest(path) for key, path in paths.items()}, 'INPUT_CHANGED_DURING_ANALYSIS')
    return summary, sorted(groups.values(), key=lambda value: (value['target'], value['epoch'], value['checkpoint_id'])), context, paths


def write_analysis(run_dir, output, **kwargs):
    root = Path(run_dir).resolve(); destination = Path(output).absolute()
    require(not destination.resolve().is_relative_to(root), 'OUTPUT_MUST_BE_OUTSIDE_EVIDENCE')
    require(not destination.exists(), 'FRESH_OUTPUT_REQUIRED')
    summary, groups, samples, paths = analyze(root, **kwargs)
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    for name, value in (('summary.json', summary), ('checkpoints.json', groups), ('resources.json', samples)):
        path = destination/name
        with path.open('x') as out:
            os.fchmod(out.fileno(), 0o600); json.dump(value, out, indent=2); out.write('\n'); out.flush(); os.fsync(out.fileno())
    sums = ''.join(digest(destination/name)+'  '+name+'\n' for name in ('summary.json', 'checkpoints.json', 'resources.json'))
    with (destination/'SHA256SUMS').open('x') as out:
        os.fchmod(out.fileno(), 0o600); out.write(sums); out.flush(); os.fsync(out.fileno())
    fd = os.open(destination, os.O_RDONLY|os.O_DIRECTORY)
    try:os.fsync(fd)
    finally:os.close(fd)
    require(summary['input_sha256'] == {key: digest(path) for key, path in paths.items()}, 'INPUT_CHANGED_DURING_OUTPUT')
    return {'ok': True, 'analysis_only': True, 'output': str(destination), 'checkpoint_groups': len(groups), 'failure_target': summary['failure_first_observed']['target']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-dir', type=Path, required=True); parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--before-seconds', type=float, default=10); parser.add_argument('--after-seconds', type=float, default=2)
    parser.add_argument('--failure-camera')
    args = parser.parse_args()
    try:
        value = write_analysis(args.run_dir, args.output, before_seconds=args.before_seconds, after_seconds=args.after_seconds, failure_camera=args.failure_camera)
        print(json.dumps(value)); return 0
    except Exception as error:print(json.dumps({'ok': False, 'error': str(error)}), file=sys.stderr); return 1


if __name__ == '__main__':raise SystemExit(main())
