#!/usr/bin/env python3
"""IMU long-run acceptance: explicit expected commit, three isolated Linux containers.

No launch/config editing. Only `crash --execute` sends a signal, to one pinned
container with restart disabled. Normal capture must finish its configured pair target.
"""
import argparse
import hashlib
import json
import math
import os
import re
import struct
import resource
from pathlib import Path
import sqlite3
import subprocess
import time

import checkpoint_probe as probe

COMMIT = '45992a4c0e81dd7a23b0b0f17fafd14a24d926b7'


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def load_plan(path):
    plan = json.loads(Path(path).read_text())
    cameras = plan['cameras']
    require(isinstance(plan['commit'],str) and re.fullmatch('[0-9a-f]{40}',plan['commit']) is not None and len(cameras) == 3, 'PINNED_COMMIT_AND_THREE_CAMERAS_REQUIRED')
    for key in ('name', 'container', 'namespace', 'status_host', 'recording_host'):
        require(len({c[key] for c in cameras}) == 3, 'DUPLICATE_' + key)
    for c in cameras:
        require(c['namespace'].startswith('/') and not c['namespace'].endswith('/'), 'NAMESPACE')
        for key in ('status_host', 'config_host', 'recording_host'):
            require(Path(c[key]).is_absolute(), 'ABSOLUTE_HOST_PATH_REQUIRED')
    return plan


def command(args):
    return subprocess.check_output(args, text=True, timeout=20)


def bound_host_path(info, container_path):
    path = Path(container_path)
    require(path.is_absolute() and '..' not in path.parts, 'CONTAINER_PATH')
    mounts = sorted(info['Mounts'], key=lambda m:len(m['Destination']), reverse=True)
    for mount in mounts:
        try:relative = path.relative_to(mount['Destination'])
        except ValueError:continue
        require(mount['Type']=='bind', 'HOST_BIND_MOUNT_REQUIRED')
        return (Path(mount['Source'])/relative).resolve()
    raise ValueError('HOST_BIND_MOUNT_NOT_FOUND')


def preflight(c, expected_commit):
    config_bytes = Path(c['config_host']).read_bytes()
    config = json.loads(config_bytes)
    status = probe.bounded_json(c['status_host'])
    require(status['config_sha256'] == hashlib.sha256(config_bytes).hexdigest(), 'CONFIG_HASH')
    require(status['ready'] is True and status['message_timestamp'] == 'global', 'NOT_READY_GLOBAL')
    require(status['imu']['shared_ros_context'] is True and status['imu']['ready'] is True, 'IMU_SHARED_CONTEXT')
    require(config['imu']['enabled'] is True and config['pairing']['clock'] == 'device', 'IMU_DEVICE_PAIRING_REQUIRED')
    require(config['ros']['namespace'] == c['namespace'], 'NAMESPACE_BINDING')
    require(config['service']['max_restarts'] == 0, 'TEST_RESTARTS_MUST_BE_ZERO')
    raw = config['raw_recording']
    require(raw['storage_format'] == 'chunked-v3' and raw.get('checkpoint_observation') is True, 'OBSERVED_V3_REQUIRED')
    info = json.loads(command(['docker', 'inspect', c['container']]))[0]
    require(info['State']['Running'] and info['HostConfig']['RestartPolicy']['Name'] == 'no', 'ISOLATED_NO_RESTART_CONTAINER_REQUIRED')
    require(Path(c['status_host']).resolve()==bound_host_path(info,config['service']['status_path']), 'STATUS_MOUNT_BINDING')
    raw_key = 'partial_path' if Path(c['recording_host']).name.endswith('.partial') else 'job_path'
    require(Path(c['recording_host']).resolve()==bound_host_path(info,status['raw_recording'][raw_key]), 'RECORDING_MOUNT_BINDING')
    image = json.loads(command(['docker', 'image', 'inspect', info['Image']]))[0]
    require((image['Config'].get('Labels') or {}).get('org.opencontainers.image.revision') == expected_commit, 'IMAGE_REVISION_LABEL')
    executable = command(['docker', 'exec', info['Id'], 'sha256sum', '/proc/' + str(status['pid']) + '/exe']).split()[0]
    require(executable == c['binary_sha256'], 'WORKER_BINARY_DIGEST')
    require(0 <= time.time_ns() // 10**6 - status['updated_unix_ms'] <= 5000, 'STALE_STATUS')
    return {'container_id': info['Id'], 'image_id': info['Image'], 'binary_sha256': executable,
            'config_sha256': status['config_sha256'], 'session_id': status['session_id'], 'config': config}, status


def health(status):
    require(status['message_timestamp'] == 'global' and status['imu']['shared_ros_context'] is True, 'CLOCK_OR_CONTEXT_CHANGED')
    require(status['imu']['error'] is None, 'IMU_ERROR')
    for key in ('publish_failures', 'stamp_failures', 'monotonic_rejections'):
        require(status.get(key) == 0, key)
    require(status['restart_attempt'] == 0, 'UNEXPECTED_RESTART')
    require((status.get('status_persistence') or {}).get('errors',0) == 0, 'STATUS_PERSISTENCE_ERROR')
    raw = status['raw_recording']
    require(raw['state'] != 'failed' and raw.get('whole_pair_queue_overflows') == 0, 'RECORDING_FAILURE_OR_OVERFLOW')


def sample(a, plan):
    require(1 <= a.seconds <= 86400 and .2 <= a.interval <= 10 and 1 <= a.max_mib <= 8192, 'SAMPLING_LIMITS')
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    bindings, targets = [], []
    for c in plan['cameras']:
        binding, status = preflight(c, plan['commit'])
        health(status)
        bindings.append(binding)
        targets.append(probe.Target(spec=c['container'] + ':' + binding['config']['service']['status_path']))
    require(len({b['config']['serial'] for b in bindings})==3, 'THREE_DISTINCT_SERIALS_REQUIRED')
    (a.output/'identity.json').write_text(json.dumps({'commit': plan['commit'], 'baseline_commit':COMMIT, 'cameras': bindings}, indent=2))
    started, cpu0, used = time.monotonic(), time.process_time(), 0
    errors, totals = [], [{'worker_cpu_seconds': 0., 'supervisor_cpu_seconds': 0., 'worker_peak_rss_bytes': 0, 'worker_peak_hwm_bytes': 0} for _ in targets]
    previous_supervisor = [None] * 3
    last_now = None
    with (a.output/'samples.jsonl').open('x') as out:
        try:
            while time.monotonic()-started < a.seconds:
                now = time.monotonic_ns()
                for i, (c, target) in enumerate(zip(plan['cameras'], targets)):
                    row, events = target.sample(now)
                    status = probe.bounded_json(c['status_host'])
                    require(status['session_id'] == bindings[i]['session_id'] and status['config_sha256'] == bindings[i]['config_sha256'], 'SESSION_OR_CONFIG_CHANGED')
                    require(0 <= time.time_ns()//10**6-status['updated_unix_ms'] <= 5000, 'STALE_STATUS')
                    health(status)
                    require(status['ready'] is True, 'LOST_READINESS')
                    require(status['raw_recording']['state'] != 'complete', 'RECORDING_COMPLETED_BEFORE_MEASUREMENT_END')
                    supervisor = probe.read_process(probe.resolve_worker(target.init, status['supervisor_pid']))
                    if previous_supervisor[i]:
                        require(supervisor['start_ticks'] == previous_supervisor[i]['start_ticks'], 'SUPERVISOR_CHANGED')
                        totals[i]['supervisor_cpu_seconds'] += (supervisor['cpu_ticks']-previous_supervisor[i]['cpu_ticks'])/os.sysconf('SC_CLK_TCK')
                    previous_supervisor[i] = supervisor
                    if row['rates'] and last_now is not None:
                        totals[i]['worker_cpu_seconds'] += row['rates']['cpu_cores'] * (now-last_now)/1e9
                    for source, dest in [('VmRSS', 'worker_peak_rss_bytes'), ('VmHWM', 'worker_peak_hwm_bytes')]:
                        totals[i][dest] = max(totals[i][dest], row['process']['memory'].get(source, 0))
                    raw = status['raw_recording']
                    row.update(kind='sample', camera=c['name'], monotonic_ns=now, supervisor=supervisor,
                               acquisition={k:status.get(k) for k in ('pairs_published','pairs_rejected','missing_pairs','imu','status_persistence')},
                               recording={k:raw.get(k) for k in ('state','written_pairs','submitted_pairs','imu_samples_submitted','payload_bytes_submitted','whole_pair_queue_overflows')},
                               append=raw['disk_stages']['append'], events=events)
                    text = json.dumps(row, separators=(',', ':'))+'\n'
                    used += len(text.encode())
                    require(used <= a.max_mib*1024**2, 'SAMPLER_OUTPUT_LIMIT')
                    out.write(text)
                out.flush()
                last_now = now
                time.sleep(a.interval)
        except (Exception, KeyboardInterrupt) as e:
            errors.append(type(e).__name__+':'+str(e))
    elapsed = time.monotonic()-started
    for total in totals:
        total['worker_cpu_cores'] = total['worker_cpu_seconds']/elapsed
        total['supervisor_cpu_cores'] = total['supervisor_cpu_seconds']/elapsed
    result = {'ok': not errors, 'elapsed_seconds': elapsed, 'errors': errors, 'processes': totals,
              'observer_cpu_seconds': time.process_time()-cpu0, 'observer_peak_rss_kib': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss, 'jsonl_bytes': used,
              'trace': [{'lost_events_after_start': t.trace.lost, 'history_events_before_start': t.trace.before_start} for t in targets],
              'scope': 'RSS/HWM are process memory, cgroup memory includes file cache; append P99 is a histogram interval, not exact latency'}
    require_trace = all(t.trace.lost == 0 for t in targets)
    result['ok'] &= require_trace
    (a.output/'summary.json').write_text(json.dumps(result, indent=2)+'\n')
    return result


def compare_native(db, ns, path, trim):
    windows = {}
    for stream in ('color', 'depth', 'accel', 'gyro'):
        bounds = db.execute('SELECT min(stamp),max(stamp) FROM messages WHERE ns=? AND stream=?', (ns, stream)).fetchone()
        require(bounds[0] is not None and bounds[1]-bounds[0] > 2*trim*10**9, 'WIRE_WINDOW_TOO_SHORT')
        windows[stream] = list(bounds)
    db.execute('CREATE TEMP TABLE IF NOT EXISTS matched(stream TEXT,stamp INTEGER,PRIMARY KEY(stream,stamp))')
    db.execute('DELETE FROM matched')
    progress, gaps, repeats, matched, native_bounds = {}, {}, {}, {}, {}
    summary = None
    stream_counts, max_interval = {}, {}
    def match(stream, stamp, data):
        lo, hi = windows[stream]
        if not lo+trim*10**9 <= stamp <= hi-trim*10**9:
            return
        row = db.execute('SELECT data FROM messages WHERE ns=? AND stream=? AND stamp=?', (ns, stream, stamp)).fetchone()
        require(row is not None, 'NATIVE_SAMPLE_MISSING_FROM_DDS:'+stream)
        wire = json.loads(row[0])
        if stream in ('color','depth'):
            require(isinstance(data.get('sha256'),str) and re.fullmatch('[0-9a-f]{64}',data['sha256']) is not None, 'NATIVE_HASH_REQUIRED')
        for key in ('sha256', 'frame_id', 'width', 'height', 'step', 'xyz'):
            if key in data:
                expected = data[key]
                if stream == 'depth' and key == 'sha256':expected = data['ros_sha256']
                if stream == 'depth' and key == 'step':expected = data['ros_step']
                require(wire.get(key) == expected, 'DDS_NATIVE_MISMATCH:'+stream+':'+key)
        if stream == 'depth':
            require(wire['encoding']=='16UC1' and wire['is_bigendian']==0, 'ROS_DEPTH_LAYOUT')
        if stream in ('color','depth'):
            meta = db.execute('SELECT data FROM messages WHERE ns=? AND stream=? AND stamp=?', (ns,stream+'_metadata',stamp)).fetchone()
            require(meta is not None, 'MISSING_IMAGE_METADATA')
            meta = json.loads(meta[0])
            require(meta['global_timestamp_us']*1000 == stamp and meta['stamp_mapping'] == 'sdk-global-unix', 'GLOBAL_MAPPING')
            for key, expected in [('sequence',data['sequence']),('device_timestamp_us',data['device_us']),('system_timestamp_us',data['system_us'])]:
                require(meta[key] == expected, 'IMAGE_METADATA_SOURCE_MISMATCH')
            if stream == 'depth':
                scale = data['source_depth_scale_mm']
                require(type(scale) in (int,float) and math.isfinite(scale) and 0 < scale <= 3.4028234663852886e38, 'NATIVE_DEPTH_SCALE')
                # C++ metadata prints enough digits for f32 round-trip, not f64 equality.
                reported = meta['source_depth_scale_mm']
                require(type(reported) in (int,float) and math.isfinite(reported) and 0 < reported <= 3.4028234663852886e38, 'ROS_DEPTH_SCALE')
                require(struct.pack('<f',reported)==struct.pack('<f',scale), 'DEPTH_SOURCE_SCALE_MISMATCH')
                require(type(data['ros_normalized']) is bool and meta['normalized'] is data['ros_normalized'] and meta['output_depth_scale_mm']==1., 'ROS_DEPTH_REPRESENTATION')
        db.execute('INSERT INTO matched VALUES(?,?)', (stream,stamp))
        matched[stream] = matched.get(stream,0)+1
    with Path(path).open() as source:
        for line in source:
            require(len(line) <= 4*1024**2, 'AUDIT_LINE_LIMIT')
            require(summary is None, 'DATA_AFTER_AUDIT_SUMMARY')
            item = json.loads(line)
            if item['kind'] == 'summary':
                summary = item
                continue
            items = [(s,item[s]) for s in ('color','depth')] if item['kind']=='pair' else [(item['sample']['kind'],item['sample'])]
            for stream, data in items:
                imu = stream in ('accel','gyro')
                seq, device, system, stamp = (data['sequence'],data['device_timestamp_us'],data['system_timestamp_us'],data['global_timestamp_us']*1000) if imu else (data['sequence'],data['device_us'],data['system_us'],data['stamp_ns'])
                require(all(type(v) is int and v > 0 for v in (device,system,stamp)) and type(seq) is int and seq >= 0, 'INVALID_SOURCE_CLOCK')
                if stream in progress:
                    prev = progress[stream]
                    require(seq > prev[0] and device > prev[1] and system >= prev[2] and stamp > prev[3], 'SOURCE_REGRESSION')
                    gaps[stream] = gaps.get(stream,0) + seq-prev[0]-1
                    max_interval[stream] = max(max_interval.get(stream,0),stamp-prev[3])
                    repeats[stream] = repeats.get(stream,0) + int(system == prev[2])
                stream_counts[stream] = stream_counts.get(stream,0)+1
                progress[stream] = (seq,device,system,stamp)
                native_bounds.setdefault(stream,[stamp,stamp])[1] = stamp
                if imu:
                    require(all(math.isfinite(v) for v in data['xyz']), 'NONFINITE_IMU')
                match(stream,stamp,data)
    require(summary and summary.get('audit_version')==2 and summary.get('native_hashes_verified') is True and summary['ok'] is True and summary['independent_audit_passes'] == 2, 'AUDIT_NOT_FINISHED')
    for stream, bounds in native_bounds.items():
        lo = max(bounds[0],windows[stream][0])+int(trim*10**9)
        hi = min(bounds[1],windows[stream][1])-int(trim*10**9)
        require(hi > lo, 'NO_COMMON_INTERIOR_WINDOW')
        # Boundary startup/tail data are excluded explicitly; every interior DDS
        # sample must also occur in native recording, including unpaired-image loss.
        missing = db.execute('SELECT count(*) FROM messages m WHERE ns=? AND stream=? AND stamp BETWEEN ? AND ? AND NOT EXISTS(SELECT 1 FROM matched x WHERE x.stream=m.stream AND x.stamp=m.stamp)',(ns,stream,lo,hi)).fetchone()[0]
        require(missing == 0 and matched.get(stream,0)>0, 'DDS_SAMPLE_MISSING_FROM_NATIVE:'+stream)
    require(len(native_bounds)==4, 'FOUR_NATIVE_STREAMS_REQUIRED')
    return {'summary':summary,'sequence_gaps':gaps,'equal_system_samples':repeats,'matched_interior_samples':matched,
            'zero_source_gaps':not any(gaps.values()),'boundary_trim_seconds':trim, 'max_interval_ns':max_interval,
            'stream_counts':stream_counts,'stream_rates_hz':{s:(stream_counts[s]-1)*1e9/(b[1]-b[0]) for s,b in native_bounds.items() if b[1]>b[0]}}


def check(a, plan):
    sampling = json.loads((a.sample_dir/'summary.json').read_text())
    require(sampling['ok'] is True, 'SAMPLING_FAILED')
    db = sqlite3.connect('file:'+str(a.wire.resolve())+'?mode=ro', uri=True)
    db.execute('PRAGMA temp_store=FILE')
    try:
        wire_result = db.execute('SELECT data FROM result').fetchall()
        require(len(wire_result)==1 and json.loads(wire_result[0][0])['ok'] is True, 'DDS_OBSERVER_FAILED')
        audits = dict(v.split('=',1) for v in a.audit)
        require(set(audits)=={c['name'] for c in plan['cameras']}, 'THREE_NAMED_AUDITS_REQUIRED')
        rows = []
        identity_document = json.loads((a.sample_dir/'identity.json').read_text())
        require(identity_document['commit']==plan['commit'], 'EXPECTED_COMMIT_CHANGED')
        identities = identity_document['cameras']
        require(len(identities)==3, 'THREE_SAMPLED_IDENTITIES_REQUIRED')
        for c, identity in zip(plan['cameras'],identities):
            final = probe.bounded_json(c['status_host'])
            require(final['config_sha256']==identity['config_sha256'] and final['session_id']==identity['session_id'], 'FINAL_IDENTITY_CHANGED')
            health(final)
            require(final['state']=='stopped' and final['ready'] is False, 'FINAL_STOPPED_STATE_REQUIRED')
            raw = final['raw_recording']
            require(raw['state']=='complete', 'NORMAL_RECORDING_NOT_COMPLETE')
            result = compare_native(db,c['namespace'],audits[c['name']],a.trim_seconds)
            require(result['summary']['recovered'] is False, 'RECOVERED_PREFIX_IS_NOT_COMPLETE')
            pairs, imu = result['summary']['pairs'],sum(result['summary']['imu'])
            require(raw['written_pairs']==pairs and raw['imu_samples_submitted']==imu, 'NATIVE_COUNTS')
            disk = raw['disk_stages']
            require(disk['validation_imu_samples']==imu*2 and disk['validation_image_bytes']==raw['payload_bytes_submitted']*2, 'PRODUCTION_TWO_READBACKS')
            require(disk['append']['samples']==pairs, 'APPEND_COUNT')
            result.update(camera=c['name'],append_p99_ns=disk['append']['p99_ns'],final_status=final)
            rows.append(result)
        zero_gaps=all(r['zero_source_gaps'] for r in rows)
        return {'ok':zero_gaps,'commit':plan['commit'],'baseline_commit':COMMIT,'integrity_ok':True,'zero_source_gaps':zero_gaps,'cameras':rows,'sampling':sampling,
                'scope':'DDS/native comparison covers trimmed common window; independent reader hashes cover entire completed native recording; no pre-SDK completeness or power-loss certification'}
    finally:
        db.close()


def crash(a, plan):
    require(a.execute, 'EXPLICIT_EXECUTE_REQUIRED')
    c = next(c for c in plan['cameras'] if c['name']==a.camera)
    binding, before = preflight(c, plan['commit'])
    health(before)
    require(before['raw_recording']['state']!='complete', 'CRASH_REQUIRES_ACTIVE_PARTIAL_RECORDING')
    partial = Path(c['recording_host'])
    require(partial.name.endswith('.partial') and not (partial/'COMPLETE').exists(), 'EXPLICIT_PARTIAL_PATH_REQUIRED')
    checkpoint = (partial/'checkpoint.json').read_bytes()
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    (a.output/'before.json').write_text(json.dumps({'identity':binding,'status':before,'checkpoint':json.loads(checkpoint)},indent=2))
    # Recheck immutable container identity, preventing name reuse from changing target.
    current = json.loads(command(['docker','inspect',binding['container_id']]))[0]
    require(current['Id']==binding['container_id'] and current['State']['Running'], 'CONTAINER_CHANGED')
    command(['docker','kill','--signal','KILL',binding['container_id']])
    (a.output/'injection.json').write_text(json.dumps({'container_id':binding['container_id'],'signal':'SIGKILL','checkpoint_sha256':hashlib.sha256(checkpoint).hexdigest(),'time_ns':time.time_ns()}))
    return {'ok':True,'injected':True,'recovery_verified':False,'next':'Run read-only audit_imu_long_run PARTIAL --recovered twice as documented; never create COMPLETE'}


def recover(a, plan):
    c = next(c for c in plan['cameras'] if c['name']==a.camera)
    before = json.loads((a.injection/'before.json').read_text())
    require(before['identity']['config_sha256']==hashlib.sha256(Path(c['config_host']).read_bytes()).hexdigest(), 'CRASH_CONFIG_CHANGED')
    info = json.loads(command(['docker','inspect',before['identity']['container_id']]))[0]
    require(not info['State']['Running'], 'CONTAINER_MUST_REMAIN_STOPPED')
    partial = Path(c['recording_host'])
    require(partial.name.endswith('.partial') and not (partial/'COMPLETE').exists(), 'PARTIAL_WITHOUT_COMPLETE_REQUIRED')
    checkpoint = (partial/'checkpoint.json').read_bytes()
    cp = json.loads(checkpoint)
    require(cp['record_count'] >= before['checkpoint']['record_count'], 'ACKNOWLEDGED_PREFIX_REGRESSED')
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    summary = None
    used = 0
    with (a.output/'audit.jsonl').open('x') as out, (a.output/'stderr.log').open('x') as err:
        child = subprocess.Popen([str(a.auditor.resolve()), str(partial), '--recovered'], stdout=subprocess.PIPE, stderr=err, text=True)
        try:
            for line in child.stdout:
                used += len(line.encode())
                require(used <= 2*1024**3 and len(line)<=4*1024**2, 'RECOVERY_AUDIT_OUTPUT_LIMIT')
                item = json.loads(line)
                require(summary is None, 'DATA_AFTER_AUDIT_SUMMARY')
                if item['kind']=='summary':summary=item
                out.write(line)
            require(child.wait()==0, 'RECOVERED_READER_FAILED')
        finally:
            if child.poll() is None:child.kill();child.wait()
            child.stdout.close()
    require(summary and summary.get('audit_version')==2 and summary.get('native_hashes_verified') is True and summary['ok'] and summary['recovered'] and summary['independent_audit_passes']==2, 'RECOVERY_AUDIT_INCOMPLETE')
    require(summary['pairs']==cp['record_count'], 'RECOVERY_PREFIX_COUNT')
    with (a.output/'normal-open.log').open('x') as err:
        normal = subprocess.run([str(a.auditor.resolve()),str(partial)],stdout=subprocess.DEVNULL,stderr=err,timeout=60)
    require(normal.returncode != 0, 'PARTIAL_WRONGLY_ACCEPTED_AS_COMPLETE')
    require(checkpoint==(partial/'checkpoint.json').read_bytes() and not (partial/'COMPLETE').exists(), 'RECOVERY_MUTATED_STATE')
    result = {'ok':True,'checkpoint_before_kill':before['checkpoint']['record_count'],'checkpoint_after_kill':cp['record_count'],
              'audit':summary,'durable_prefix_preserved':True,'complete_recording':False,
              'scope':'SIGKILL process crash only; no kernel/power-loss or unacknowledged-tail durability claim'}
    (a.output/'summary.json').write_text(json.dumps(result,indent=2)+'\n')
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--plan',type=Path,required=True)
    sub = p.add_subparsers(dest='action',required=True)
    s = sub.add_parser('sample');s.add_argument('--seconds',type=float,default=3600);s.add_argument('--interval',type=float,default=1);s.add_argument('--max-mib',type=int,default=512);s.add_argument('--output',type=Path,required=True)
    s = sub.add_parser('check');s.add_argument('--sample-dir',type=Path,required=True);s.add_argument('--wire',type=Path,required=True);s.add_argument('--audit',action='append',required=True);s.add_argument('--trim-seconds',type=float,default=30);s.add_argument('--output',type=Path,required=True)
    s = sub.add_parser('crash');s.add_argument('--camera',required=True);s.add_argument('--execute',action='store_true');s.add_argument('--output',type=Path,required=True)
    s = sub.add_parser('recover');s.add_argument('--camera',required=True);s.add_argument('--injection',type=Path,required=True);s.add_argument('--auditor',type=Path,required=True);s.add_argument('--output',type=Path,required=True)
    a = p.parse_args();os.umask(0o077)
    try:
        plan = load_plan(a.plan)
        if a.action=='sample':result = sample(a,plan)
        elif a.action=='check':
            require(0 <= a.trim_seconds <= 300,'TRIM_LIMIT')
            result = check(a,plan)
            with a.output.open('x') as f:json.dump(result,f,indent=2)
        elif a.action=='crash':result = crash(a,plan)
        else:result = recover(a,plan)
        print(json.dumps({'ok':result['ok'],'zero_source_gaps':result.get('zero_source_gaps')}))
        return 0 if result['ok'] else 1
    except Exception as e:
        failure = {'ok':False,'error':type(e).__name__+':'+str(e)}
        if a.action == 'check' and not a.output.exists():
            try:
                with a.output.open('x') as f:json.dump(failure,f,indent=2)
            except OSError:pass
        print(json.dumps(failure))
        return 1

if __name__=='__main__':
    raise SystemExit(main())
