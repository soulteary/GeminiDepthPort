"""Bounded read-only lifecycle trace, separate from the fixed A/B window."""
import importlib.util
import json
import os
from pathlib import Path
import threading
import time

import experiment_runtime as rt

_spec=importlib.util.spec_from_file_location('gdp_checkpoint_probe',Path(__file__).resolve().parents[2]/'scripts/validation/checkpoint_probe.py')
probe=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(probe)

PHASES=frozenset(('preflight','build','prepare','start','wait-healthy','sample','wait-complete','stop','audit','verify-dds','cleanup','complete','failed',
                 'crash-prepare','crash-wait','crash-inject','crash-audit','crash-cleanup'))
STRICT_READ_PHASES=frozenset(('sample','wait-complete'))


def phase_name(value):
    return value if value in PHASES else 'other'


def validate_status(status, capacity):
    """Validate c063450 observation contracts; never certify payload integrity."""
    imu=status.get('imu') or {}; raw=status.get('raw_recording') or {}
    persistence=status.get('status_persistence') or {}; cleanup=status.get('cleanup') or {}
    rt.require(imu.get('error') is None or status.get('ready') is False,'IMU_FAILURE_STILL_READY')
    if not status.get('ready'):return
    rt.require(type(imu.get('queue_high_water')) is int and 0<=imu['queue_high_water']<=capacity,'IMU_QUEUE_CAPACITY')
    rt.require(imu.get('failure_observation') is None and imu.get('error') is None,'IMU_TERMINAL_ERROR')
    rt.require(persistence.get('errors',0)==0 and persistence.get('submission_errors',0)==0,'STATUS_PERSISTENCE_FAILURE')
    rt.require(type(imu.get('drain_interval')) is dict and type(imu['drain_interval'].get('samples')) is int,'IMU_DRAIN_OBSERVATION')
    rt.require(type(persistence.get('queue_wait')) is dict and type(persistence.get('critical_wait')) is dict,'PERSISTENCE_OBSERVATION')
    events=persistence.get('critical_events')
    rt.require(persistence.get('critical_event_capacity')==16 and type(events) is list and len(events)<=16,'CRITICAL_HISTORY')
    for event in events:
        rt.require(type(event.get('sequence')) is int and type(event.get('completed_sequence')) is int
                   and event['completed_sequence']>=event['sequence'] and event.get('ok') is True
                   and all(type(event.get(k)) is int and event[k]>=0 for k in ('entered_unix_ms','returned_unix_ms','wall_ns')),'CRITICAL_EVENT')
    rt.require(cleanup.get('history_capacity')==8 and type(cleanup.get('completed')) is list and len(cleanup['completed'])<=8,'CLEANUP_OBSERVATION')
    failures=(raw.get('disk_stages') or {}).get('failed_calls')
    rt.require(type(failures) is dict and not failures,'LIFECYCLE_DISK_FAILURE')


class Lifecycle:
    def __init__(self, root, run, capacities, phase, disks=(), factory=None, interval=1., event_interval=.2, max_bytes=128*1024**2):
        self.root=Path(root);self.root.mkdir(mode=0o700)
        self.run=run;self.capacities=capacities;self.phase=phase;self.disks=disks
        self.factory=factory or (lambda c:probe.Target(spec=c['container']+':/state/status.json'))
        rt.require(0.2<=interval<=10 and .05<=event_interval<=.2 and event_interval<=interval and max_bytes>0,"LIFECYCLE_LIMITS")
        self.interval=interval;self.event_interval=event_interval;self.max_bytes=max_bytes;self.used=0;self.samples=0;self.errors=[];self.error_count=0
        self.resource_seen={c['name']:0 for c in run['cameras']}
        self.read_errors={'count':0,'fatal_count':0,'by_started_phase':{},'by_finished_phase':{},'by_transition':{},'by_operation':{}}
        self.event_polls=0
        self.polls={c['name']:{'reads':0,'successful_reads':0,'failed_reads':0,'maximum_read_gap_ns':0,'last_read_monotonic_ns':None,
                    'last_status_age_ms':None,'maximum_status_age_ms':None,'lost_events_after_start':0,
                    'history_events_before_start':0,'epoch':None,'loss_reports':0,'initial_history_reports':0} for c in run['cameras']}
        self.seen={c['name']:set() for c in run['cameras']};self.lock=threading.Lock();self.error_lock=threading.Lock();self.done=threading.Event()
        self.started=time.monotonic_ns();self.started_unix_ns=time.time_ns();self.max_gap_ns=0;self.last_sample_ns=None;self.cpu_ns=0;self.finished=None
        self.out=(self.root/'samples.jsonl').open('x')
        os.chmod(self.root/'samples.jsonl',0o600)
        self._emit({'kind':'lifecycle-start','monotonic_ns':self.started,'unix_ns':self.started_unix_ns,'phase':self.phase()})
        self.thread=threading.Thread(target=self._run,name='lifecycle-observer',daemon=True)
        self.thread.start()

    def _emit(self, value):
        data=json.dumps(value,separators=(',',':'))+'\n'
        size=len(data.encode())
        if self.used+size>self.max_bytes:raise ValueError('LIFECYCLE_OUTPUT_LIMIT')
        self.out.write(data);self.out.flush();self.used+=size

    def observe(self, rows):
        if self.finished is not None:return
        with self.lock:
            self._emit({'kind':'controller-status','monotonic_ns':time.monotonic_ns(),'unix_ns':time.time_ns(),'phase':self.phase(),'cameras':rows['cameras']})
            for row in rows['cameras']:
                status=row.get('capture') or {};name=row['name']
                try:validate_status(status,self.capacities[name])
                except ValueError as error:
                    # The observer cannot authenticate terminal ownership. Keep
                    # its contract failure sticky, then let check_rows validate
                    # the existing pin and retain the raw/IMU failure reasons.
                    self._error(name+':CONTROLLER_STATUS_CONTRACT:'+str(error))
                    raw=status.get('raw_recording') or {};imu=status.get('imu') or {}
                    terminal=(raw.get('state')=='failed' or raw.get('whole_pair_queue_overflows',0)!=0
                              or imu.get('error') is not None or status.get('state') in ('failed','stopped'))
                    if not terminal:raise
                self.seen[name].add((status.get('raw_recording') or {}).get('state'))

    def _error(self, error):
        with self.error_lock:
            self.error_count+=1
            if len(self.errors)<256:self.errors.append(str(error)[:4096])

    def _read_error(self,name,error,started_phase,operation):
        finished_phase=phase_name(self.phase());started_phase=phase_name(started_phase)
        fatal=bool({started_phase,finished_phase}&STRICT_READ_PHASES)
        stats=self.read_errors;stats['count']+=1;stats['fatal_count']+=int(fatal)
        for field,key in (('by_started_phase',started_phase),('by_finished_phase',finished_phase),
                          ('by_transition',started_phase+'->'+finished_phase),('by_operation',operation)):
            stats[field][key]=stats[field].get(key,0)+1
        error_text=type(error).__name__+':'+str(error)[:4096]
        if fatal:self._error(name+':'+error_text)
        return {'error':error_text,'read_started_phase':started_phase,'read_finished_phase':finished_phase,
                'read_operation':operation,'counted_as_sampling_error':fatal}

    def _poll_meta(self, name, target, now, updated=None, succeeded=True):
        stats=self.polls[name]
        last=stats['last_read_monotonic_ns']
        gap=None if last is None else now-last
        stats['reads']+=1;stats['last_read_monotonic_ns']=now
        stats['successful_reads' if succeeded else 'failed_reads']+=1
        if gap is not None:stats['maximum_read_gap_ns']=max(stats['maximum_read_gap_ns'],gap)
        age=time.time_ns()//1_000_000-updated if type(updated) is int else None
        stats['last_status_age_ms']=age
        if age is not None:stats['maximum_status_age_ms']=age if stats['maximum_status_age_ms'] is None else max(age,stats['maximum_status_age_ms'])
        trace=getattr(target,'trace',None)
        lost=getattr(trace,'lost',0)
        if lost<stats['lost_events_after_start']:raise ValueError('TRACE_LOSS_COUNTER_RESET')
        if lost>stats['lost_events_after_start']:
            delta=lost-stats['lost_events_after_start']
            self._error(name+':CHECKPOINT_EVENTS_LOST:'+str(lost))
            stats['loss_reports']+=1
        else:delta=0
        stats['lost_events_after_start']=lost
        before=getattr(trace,'before_start',0)
        if before<stats['history_events_before_start']:raise ValueError('TRACE_INITIAL_HISTORY_COUNTER_RESET')
        initial_delta=before-stats['history_events_before_start']
        if initial_delta:
            self._error(name+':CHECKPOINT_HISTORY_BEFORE_FIRST_READ:'+str(before))
            stats['initial_history_reports']+=1
        stats['history_events_before_start']=before
        stats['epoch']=getattr(trace,'epoch',None)
        return {'read_monotonic_ns':now,'read_gap_ns':gap,'status_age_ms':age,
                'lost_events_after_start':lost,'new_lost_events':delta,
                'history_events_before_start':stats['history_events_before_start'],'new_history_events_before_start':initial_delta,'epoch':stats['epoch']}

    def _run(self):
        targets={};start_cpu=time.thread_time_ns();next_resource=0
        try:
            while not self.done.is_set():
                now=time.monotonic_ns();phase=self.phase();rows=[]
                resource_due=now>=next_resource
                for camera in self.run['cameras']:
                    name=camera['name'];read_at=time.monotonic_ns();read_phase=phase_name(self.phase())
                    operation='resource' if resource_due else 'events'
                    try:
                        if name not in targets:targets[name]=self.factory(camera)
                        target=targets[name];read_at=time.monotonic_ns()
                        if resource_due:
                            result,events=target.sample(read_at)
                            if getattr(target,'pid',None):
                                try:result['thread_cpu']=probe.thread_cpu(target.pid)
                                except Exception as error:
                                    # Keep events already extracted from status;
                                    # an exiting thread tree cannot erase them.
                                    result['thread_cpu_read_error']=self._read_error(name,error,read_phase,'thread-cpu')
                            updated=(result.get('status') or {}).get('updated_unix_ms')
                            self.resource_seen[name]+=1
                        elif hasattr(target,'events'):
                            result,events=target.events(read_at)
                            updated=result.get('source_updated_unix_ms')
                        else:
                            # Compatibility for synthetic test factories without
                            # a fast event API; production Target always has it.
                            continue
                        meta=self._poll_meta(name,target,read_at,updated)
                        rows.append({'name':name,**result,'poll':meta,'checkpoint_events':events})
                    except Exception as error:
                        meta=self._poll_meta(name,targets.get(name),read_at,succeeded=False)
                        rows.append({'name':name,'poll':meta,**self._read_error(name,error,read_phase,operation)})
                with self.lock:
                    row={'kind':'resource-sample' if resource_due else 'event-sample',
                         'monotonic_ns':now,'unix_ns':time.time_ns(),'phase':phase,
                         'cameras':rows,'iteration_wall_ns':time.monotonic_ns()-now}
                    if resource_due:
                        disk_values=probe.read_disks(self.disks) if self.disks else {}
                        if set(disk_values)!=set(self.disks):self._error('MISSING_LIFECYCLE_DISK')
                        row['disks']=disk_values
                        if self.last_sample_ns is not None:self.max_gap_ns=max(self.max_gap_ns,now-self.last_sample_ns)
                        self.last_sample_ns=now
                        self.samples+=1
                        finished=time.monotonic_ns();period=int(self.interval*1e9)
                        next_resource=(finished if finished-now>=period else now)+period
                        rt.require(self.samples<=100000,"LIFECYCLE_SAMPLE_LIMIT")
                    self._emit(row)
                    self.event_polls+=1
                    rt.require(self.event_polls<=500000,"LIFECYCLE_EVENT_POLL_LIMIT")
                remaining=self.event_interval-(time.monotonic_ns()-now)/1e9
                self.done.wait(max(0,remaining))
        except Exception as error:self._error(type(error).__name__+':'+str(error))
        finally:self.cpu_ns=time.thread_time_ns()-start_cpu

    def stop(self, successful=False):
        if self.finished is not None:return self.finished
        self.done.set();self.thread.join(timeout=35)
        rt.require(not self.thread.is_alive(),'LIFECYCLE_STOP_TIMEOUT')
        self.out.flush();os.fsync(self.out.fileno());self.out.close()
        self.finished={'version':1,'samples':self.samples,'bytes':self.used,'max_bytes':self.max_bytes,
                       'started_monotonic_ns':self.started,'finished_monotonic_ns':time.monotonic_ns(),
                       'started_unix_ns':self.started_unix_ns,'finished_unix_ns':time.time_ns(),'maximum_sample_gap_ns':self.max_gap_ns,
                       'resource_interval_seconds':self.interval,'event_interval_seconds':self.event_interval,
                       'event_polls':self.event_polls,'event_targets':self.polls,
                       'observer_thread_cpu_ns':self.cpu_ns,'errors':self.errors,'error_count':self.error_count,'resource_samples':self.resource_seen,
                       'read_errors':self.read_errors,'all_target_reads_succeeded':self.read_errors['count']==0,
                       'sampling_ok':not self.errors and all(self.resource_seen.values()),
                       'sampling_ok_scope':'no fatal observer errors and at least one resource sample per target; read errors touching sample/wait-complete are fatal; other phase read errors are counted separately; not hardware or payload acceptance',
                       'states':{k:sorted(s for s in v if s is not None) for k,v in self.seen.items()},
                       'complete_observed':all('complete' in v for v in self.seen.values()),
                       'scope':'full lifecycle sampling; independent of fixed A/B window; not payload certification'}
        with (self.root/'summary.json').open('x') as out:
            os.fchmod(out.fileno(),0o600)
            json.dump(self.finished,out,indent=2);out.flush();os.fsync(out.fileno())
        fd=os.open(self.root,os.O_RDONLY|os.O_DIRECTORY)
        try:os.fsync(fd)
        finally:os.close(fd)
        if successful:
            rt.require(self.finished['sampling_ok'] and self.finished['complete_observed'] and self.samples>0,'LIFECYCLE_INCOMPLETE')
        return self.finished
