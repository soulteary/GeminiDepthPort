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
    def __init__(self, root, run, capacities, phase, disks=(), factory=None, interval=1., max_bytes=128*1024**2):
        self.root=Path(root);self.root.mkdir(mode=0o700)
        self.run=run;self.capacities=capacities;self.phase=phase;self.disks=disks
        self.factory=factory or (lambda c:probe.Target(spec=c['container']+':/state/status.json'))
        rt.require(0.2<=interval<=10 and max_bytes>0,"LIFECYCLE_LIMITS")
        self.interval=interval;self.max_bytes=max_bytes;self.used=0;self.samples=0;self.errors=[];self.error_count=0
        self.resource_seen={c['name']:0 for c in run['cameras']}
        self.seen={c['name']:set() for c in run['cameras']};self.lock=threading.Lock();self.done=threading.Event()
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
                validate_status(status,self.capacities[name])
                self.seen[name].add((status.get('raw_recording') or {}).get('state'))

    def _error(self, error):
        self.error_count+=1
        if len(self.errors)<256:self.errors.append(error)

    def _run(self):
        targets={};start_cpu=time.thread_time_ns()
        try:
            while not self.done.is_set():
                now=time.monotonic_ns();phase=self.phase();rows=[]
                for camera in self.run['cameras']:
                    name=camera['name']
                    try:
                        if name not in targets:targets[name]=self.factory(camera)
                        result,events=targets[name].sample(now)
                        rows.append({'name':name,**result,'checkpoint_events':events})
                        self.resource_seen[name]+=1
                        if target_loss:=getattr(getattr(targets[name],'trace',None),'lost',0):self._error(name+':CHECKPOINT_EVENTS_LOST:'+str(target_loss))
                    except Exception as error:
                        rows.append({'name':name,'error':type(error).__name__+':'+str(error)})
                        if self.phase() in ('sample','wait-complete'):self._error(name+':'+str(error))
                with self.lock:
                    disk_values=probe.read_disks(self.disks) if self.disks else {}
                    if set(disk_values)!=set(self.disks):self._error('MISSING_LIFECYCLE_DISK')
                    if self.last_sample_ns is not None:self.max_gap_ns=max(self.max_gap_ns,now-self.last_sample_ns)
                    self.last_sample_ns=now
                    self._emit({'kind':'resource-sample','monotonic_ns':now,'unix_ns':time.time_ns(),'phase':phase,
                                'cameras':rows,'disks':disk_values,'iteration_wall_ns':time.monotonic_ns()-now})
                    self.samples+=1
                    rt.require(self.samples<=100000,"LIFECYCLE_SAMPLE_LIMIT")
                self.done.wait(self.interval)
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
                       'observer_thread_cpu_ns':self.cpu_ns,'errors':self.errors,'error_count':self.error_count,'resource_samples':self.resource_seen,
                       'sampling_ok':not self.errors and all(self.resource_seen.values()),
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
