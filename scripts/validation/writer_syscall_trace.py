#!/usr/bin/env python3
"""Bounded, PID-scoped syscall/scheduler trace in a private tracefs instance.

No mounts, package installation, global trace changes, or target signals. Start
after owned writers are ready and before releasing their common workload barrier.
Only normalized scalar fields are retained: no writev pointers, buffers, paths,
registers, or unrelated task names. This is not block-request/device attribution.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import sys
import threading
import time
import uuid

REQUIRED_EVENTS = tuple('syscalls:' + prefix + name
    for name in ('fsync', 'fdatasync', 'writev') for prefix in ('sys_enter_', 'sys_exit_')) + (
    'sched:sched_switch', 'sched:sched_wakeup', 'sched:sched_process_fork', 'sched:sched_process_exit')
REQUIRED_EVENTS += ('task:task_rename',)
READ_EVENTS = tuple('syscalls:' + prefix + name
    for name in ('read', 'readv', 'pread64', 'lseek') for prefix in ('sys_enter_', 'sys_exit_'))
MAX_LINE = 8192
MAX_THREADS = 1024
METADATA_RESERVE = 512 * 1024
LOSS_COUNTERS = ('overrun', 'commit overrun', 'dropped events')
PREFIX = re.compile(r'^\s*.+-(\d+)\s+(?:\(\s*(?:\d+|-----)\)\s+)?\[(\d+)\]\s+.*?(\d+)\.(\d+):\s*(.*)$')
SYSCALL_NAMES = ('fsync','fdatasync','writev','read','readv','pread64','lseek')
SCOPE = ('Per-instance set_event_pid + event-fork follows initial writer TIDs and their descendants. '
    'Caller must hold the workload startup barrier until ready.json; initial numeric TID registration is not an atomic attach to already-running short-lived threads. '
    'Kernel scheduler PID filtering includes switch prev/next and wakeup target. '
    'Incidental peer TIDs may be present; unrelated comm, pointers and syscall buffers are not exported. '
    'Text timestamps retain their printed resolution, not assumed nanosecond precision. '
    'Syscall wall and off-CPU intervals overlap and must not be summed; fd numbers can be reused. '
    'No unique filesystem, SSD or block-request attribution and no hardware/power-loss certification.')


def require(value, code):
    if not value:raise ValueError(code)


def bounded_text(path, limit=65536):
    with Path(path).open('rb') as source:
        raw=source.read(limit+1)
    require(len(raw)<=limit, 'TRACE_INPUT_LIMIT:'+str(path.name))
    return raw.decode('utf-8', 'strict')


def proc_identity(pid, proc=Path('/proc')):
    raw=bounded_text(proc/str(pid)/'stat',16384);fields=raw[raw.rfind(')')+2:].split()
    require(len(fields)>19 and fields[0] not in ('Z','X'), 'TRACE_TARGET_NOT_LIVE')
    return {'pid':pid,'start_ticks':int(fields[19])}


def sha(path):
    digest=hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda:source.read(1024**2),b''):digest.update(block)
    return digest.hexdigest()


def tracefs_mount(path, mountinfo=Path('/proc/self/mountinfo')):
    target=path.resolve(strict=True);matches=[]
    for line in bounded_text(mountinfo,2*1024**2).splitlines():
        left,right=line.split(' - ',1);fields=left.split();kind=right.split()[0]
        mount=Path(fields[4].replace('\\040',' ').replace('\\134','\\'))
        if target==mount or mount in target.parents:matches.append((len(str(mount)),kind))
    require(matches and max(matches)[1] in ('tracefs','debugfs'), 'TRACEFS_NOT_MOUNTED')


def zero_monotonic_offset(proc=Path('/proc')):
    values=[]
    for row in bounded_text(proc/'self/timens_offsets',4096).splitlines():
        fields=row.split()
        if fields and fields[0]=='monotonic':
            require(len(fields)==3,'TRACE_TIMENS_OFFSET_SCHEMA')
            values.append((int(fields[1]),int(fields[2])))
    require(values==[(0,0)],'TRACE_MONOTONIC_NAMESPACE_OFFSET')
    return {'seconds':0,'nanoseconds':0,'scope':'kernel trace mono clock is initial-namespace time; observer and targets must have zero monotonic offset'}


def capability(tracefs=Path('/sys/kernel/tracing'), include_reads=False):
    try:
        require(sys.platform=='linux','LINUX_REQUIRED')
        require(os.uname().machine in ('x86_64','aarch64','arm64'),'TRACE_KERNEL_ABI_UNSUPPORTED')
        offsets=zero_monotonic_offset()
        tracefs_mount(tracefs)
        available=set(bounded_text(tracefs/'available_events',4*1024**2).split())
        missing=sorted(set(REQUIRED_EVENTS)-available)
        require(not missing,'TRACEPOINTS_MISSING:'+','.join(missing))
        require((tracefs/'instances').is_dir(),'TRACEFS_INSTANCES_UNAVAILABLE')
        selected=list(REQUIRED_EVENTS)
        read_available=[event for event in READ_EVENTS if event in available]
        if include_reads:
            require(len(read_available)==len(READ_EVENTS),'READ_TRACEPOINTS_MISSING')
            selected+=read_available
        return {'available':True,'events':selected,'read_events_available':read_available,
            'read_events_enabled':include_reads,'tracefs':str(tracefs.resolve()),'monotonic_timens_offset':offsets,'scope':SCOPE}
    except (OSError,ValueError,UnicodeError) as error:
        return {'available':False,'reason':type(error).__name__+':'+str(error),'scope':SCOPE}


def scalar(fields,name):
    match=re.search(r'\b'+re.escape(name)+r'\s*(?:=|:)\s*(-?(?:0x[0-9a-fA-F]+|\d+))\b',fields)
    require(match is not None,'TRACE_FIELD_MISSING:'+name)
    value=match.group(1)
    return int(value,16 if '0x' in value.lower() else 10)


def event_text(body):
    # trace_syscalls.c has a custom print routine, not the generic event-name
    # printer: sys_fsync(fd: 3) / sys_fsync -> 0x0. Generic named variants also
    # occur in normalized exports; support both without retaining pointers.
    named=re.fullmatch(r'([\w:]+):\s*(.*)',body)
    if named:return named.group(1).split(':')[-1],named.group(2)
    entered=re.fullmatch(r'sys_([a-z0-9_]+)\((.*)\)',body)
    if entered and entered.group(1) in SYSCALL_NAMES:return 'sys_enter_'+entered.group(1),entered.group(2)
    exited=re.fullmatch(r'sys_([a-z0-9_]+)\s*->\s*(-?(?:0x[0-9a-fA-F]+|\d+))',body)
    if exited and exited.group(1) in SYSCALL_NAMES:return 'sys_exit_'+exited.group(1),exited.group(2)
    raise ValueError('TRACE_EVENT_TEXT_SCHEMA')


def comm_text(fields,name,next_field):
    match=re.search(r'\b'+name+r'=(.*?)\s+'+next_field+r'=',fields)
    require(match is not None and len(match.group(1))<=64,'TRACE_COMM_SCHEMA')
    return match.group(1)


def marker_pid_proof(text,token,expected):
    matches=[]
    for raw in text.splitlines():
        match=PREFIX.match(raw)
        if match and match.group(5).strip()=='tracing_mark_write: '+token:
            matches.append(int(match.group(1)))
    require(len(matches)==1,'TRACE_PID_CALIBRATION_NOT_PROVABLE')
    require(matches[0]==expected,'TRACE_PID_NAMESPACE_MISMATCH')
    return {'observer_local_pid':expected,'trace_header_pid':matches[0],'verified_equal':True,
        'scope':'private-instance self marker with all events disabled before target PID filters; observer must run in its process main thread'}


class Decoder:
    """Bounded active calls/off-CPU state, streamed normalized event output."""
    def __init__(self,tids,emit,max_threads=MAX_THREADS,comm=None,owners=None):
        self.tids={tid:1 for tid in tids};self.initial=set(tids);self.emit=emit
        self.comm={tid:name for tid,name in (comm or {}).items() if tid in self.tids}
        self.owners={tid:(owners or {}).get(tid,tid) for tid in tids}
        self.max_threads=max_threads;self.next_generation=1;self.calls={};self.off_cpu={};self.wake={}
        self.seen_syscall=set();self.events=0;self.intervals=0;self.switches=0
        self.left_censored=0;self.exited_open=0;self.discarded_unrelated=0
        self.loss_records=0;self.minimum_resolution_ns=None
        self.paired_by_name={name:0 for name in SYSCALL_NAMES}

    def line(self,line):
        if not line.strip() or line.lstrip().startswith('#'):return
        lost=re.search(r'\bLOST\s+(\d+)\s+EVENTS\b',line)
        if lost:
            self.loss_records+=int(lost.group(1));raise ValueError('TRACE_STREAM_LOST_EVENTS')
        match=PREFIX.match(line);require(match is not None,'TRACE_LINE_SCHEMA')
        tid,cpu,seconds,fraction,body=match.groups();tid=int(tid);cpu=int(cpu)
        event,fields=event_text(body)
        require(1<=len(fraction)<=9,'TRACE_TIMESTAMP_RESOLUTION')
        resolution=10**(9-len(fraction));at=int(seconds)*10**9+int(fraction)*resolution
        self.minimum_resolution_ns=max(self.minimum_resolution_ns or 0,resolution)
        row={'kind':event,'monotonic_ns':at,'printed_resolution_ns':resolution,'cpu':cpu,'context_tid':tid}
        if tid in self.comm:row['context_comm']=self.comm[tid]
        if tid in self.owners:row['context_root_pid']=self.owners[tid]
        if event=='sched_process_fork':
            parent=scalar(fields,'parent_pid' if re.search(r'\bparent_pid=',fields) else 'pid');child=scalar(fields,'child_pid')
            require(parent in self.tids,'TRACE_UNBOUND_FORK_PARENT')
            require(child not in self.tids,'TRACE_DUPLICATE_LIVE_TID')
            require(len(self.tids)<self.max_threads,'TRACE_THREAD_LIMIT')
            self.next_generation+=1;generation=self.next_generation
            self.tids[child]=generation;row.update(parent_tid=parent,child_tid=child,child_generation=generation)
            self.owners[child]=self.owners[parent];row['child_root_pid']=self.owners[child]
            if re.search(r'\bchild_comm=',fields):
                self.comm[child]=comm_text(fields,'child_comm','child_pid');row['child_comm']=self.comm[child]
        elif event=='sched_process_exit':
            exited=scalar(fields,'pid')
            require(exited in self.tids,'TRACE_UNBOUND_EXIT_TID')
            row.update(tid=exited,tid_generation=self.tids.pop(exited))
            row['root_pid']=self.owners.pop(exited)
            if exited in self.comm:row['comm']=self.comm.pop(exited)
            if self.calls.pop(exited,None) is not None:self.exited_open+=1
            self.off_cpu.pop(exited,None);self.wake.pop(exited,None);self.seen_syscall.discard(exited);self.initial.discard(exited)
        elif event=='sched_switch':
            previous=scalar(fields,'prev_pid');following=scalar(fields,'next_pid')
            if previous not in self.tids and following not in self.tids:
                self.discarded_unrelated+=1;return
            row.update(prev_tid=previous,next_tid=following);self.switches+=1
            if previous in self.owners:row['prev_root_pid']=self.owners[previous]
            if following in self.owners:row['next_root_pid']=self.owners[following]
            if previous in self.comm:row['prev_comm']=self.comm[previous]
            if following in self.comm:row['next_comm']=self.comm[following]
            if following in self.off_cpu:
                start,state=self.off_cpu.pop(following);require(at>=start,'TRACE_CLOCK_REGRESSION')
                wake=self.wake.pop(following,None)
                resumed={'tid':following,'off_cpu_ns':at-start,'prev_state':state}
                if wake is not None:
                    require(start<=wake<=at,'TRACE_WAKE_CLOCK');resumed['wakeup_to_run_ns']=at-wake
                row['resumed']=resumed
            if previous in self.tids and previous!=following:
                state=re.search(r'\bprev_state=(\S+)',fields);require(state is not None,'TRACE_SWITCH_STATE')
                require(previous not in self.off_cpu,'TRACE_SWITCH_WITHOUT_RESUME')
                self.off_cpu[previous]=(at,state.group(1));self.wake.pop(previous,None)
                row['prev_state']=state.group(1)
        elif event=='sched_wakeup':
            woken=scalar(fields,'pid')
            if woken not in self.tids:
                self.discarded_unrelated+=1;return
            row['woken_tid']=woken
            row['woken_root_pid']=self.owners[woken]
            if woken in self.comm:row['woken_comm']=self.comm[woken]
            if woken in self.off_cpu:self.wake.setdefault(woken,at)
        elif event=='task_rename':
            renamed=scalar(fields,'pid');require(renamed in self.tids,'TRACE_UNBOUND_RENAME_TID')
            self.comm[renamed]=comm_text(fields,'newcomm','oom_score_adj')
            row.update(tid=renamed,comm=self.comm[renamed])
            row['root_pid']=self.owners[renamed]
        elif event.startswith(('sys_enter_','sys_exit_')):
            require(tid in self.tids,'TRACE_UNBOUND_SYSCALL_TID')
            name=event.partition('sys_enter_')[2] if event.startswith('sys_enter_') else event.partition('sys_exit_')[2]
            require(name in SYSCALL_NAMES,'TRACE_UNEXPECTED_SYSCALL')
            row.update(tid=tid,tid_generation=self.tids[tid],syscall=name)
            row['root_pid']=self.owners[tid]
            if tid in self.comm:row['comm']=self.comm[tid]
            if event.startswith('sys_enter_'):
                require(tid not in self.calls,'TRACE_SYSCALL_ENTRY_WITHOUT_EXIT')
                args={'fd':scalar(fields,'fd')}
                if name in ('writev','readv'):
                    # Kernel names this argument vlen, never export vec/ptr.
                    args['vlen']=scalar(fields,'vlen')
                row.update(args);self.calls[tid]=(name,at,args)
            else:
                match_ret=re.search(r'\bret\s*(?:=|:)\s*(-?(?:0x[0-9a-fA-F]+|\d+))',fields)
                if match_ret:
                    raw=match_ret.group(1)
                else:
                    raw=fields.strip();require(re.fullmatch(r'-?(?:0x[0-9a-fA-F]+|\d+)',raw) is not None,'TRACE_RETURN_SCHEMA')
                result=int(raw,16 if '0x' in raw.lower() else 10)
                # syscall tracepoints can print negative long results unsigned.
                require(-(2**63)<=result<2**64,'TRACE_RETURN_WIDTH')
                if result>=2**63:result-=2**64
                row.update(return_value=result,kernel_signed_long_bits=64);entry=self.calls.pop(tid,None)
                if entry is None:
                    require(tid in self.initial and tid not in self.seen_syscall,'TRACE_SYSCALL_EXIT_WITHOUT_ENTRY')
                    self.left_censored+=1;row['left_censored']=True
                else:
                    require(entry[0]==name and at>=entry[1],'TRACE_SYSCALL_PAIR_MISMATCH')
                    row.update(entry[2]);row.update(entry_monotonic_ns=entry[1],duration_ns=at-entry[1]);self.intervals+=1
                    self.paired_by_name[name]+=1
            self.seen_syscall.add(tid)
        else:raise ValueError('TRACE_UNEXPECTED_EVENT:'+event)
        self.emit(row);self.events+=1

    def report(self):
        return {'events':self.events,'paired_syscalls':self.intervals,'scheduler_switches':self.switches,
            'left_censored_initial_calls':self.left_censored,'open_calls_at_thread_exit':self.exited_open,
            'right_censored_calls':len(self.calls),'right_censored_off_cpu':len(self.off_cpu),
            'stream_lost_events':self.loss_records,'discarded_unrelated_events':self.discarded_unrelated,
            'coarsest_printed_resolution_ns':self.minimum_resolution_ns,'live_tid_contexts':len(self.tids),
            'paired_syscalls_by_name':self.paired_by_name}


TRACE_OUTPUT_BUFFER_BYTES = 256 * 1024


class TraceSession:
    def __init__(self,output,max_bytes,max_seconds,buffer_kib=256,tracefs=Path('/sys/kernel/tracing'),
                 required=True,max_memory_bytes=64*1024**2,include_reads=False,proc=Path('/proc')):
        require(type(max_bytes) is int and 1024**2<=max_bytes<=8*1024**3,'TRACE_OUTPUT_BOUND')
        require(math.isfinite(max_seconds) and 0<max_seconds<=86400,'TRACE_WALL_BOUND')
        require(type(buffer_kib) is int and 4<=buffer_kib<=65536,'TRACE_BUFFER_BOUND')
        require(type(max_memory_bytes) is int and 1024**2<=max_memory_bytes<=256*1024**2,'TRACE_MEMORY_BOUND')
        self.output=Path(output).absolute();self.max_bytes=max_bytes;self.max_seconds=max_seconds
        self.buffer_kib=buffer_kib;self.tracefs=Path(tracefs);self.required=required
        self.max_memory_bytes=max_memory_bytes;self.include_reads=include_reads;self.proc=proc
        self.instance=None;self.fd=None;self.stream=None;self.decoder=None;self.failure=None
        self.state='not-started';self.started_ns=None;self.data_bytes=0;self.fragment=b'';self.targets=[]
        self.before={};self.capabilities=None;self.finished=None;self.active=False

    def _capability(self):return capability(self.tracefs,self.include_reads)

    def _target_abi(self,pid):
        with (self.proc/str(pid)/'exe').open('rb') as source:header=source.read(64)
        machine={'x86_64':62,'aarch64':183,'arm64':183}.get(os.uname().machine)
        require(len(header)>=20 and header[:6]==b'\x7fELF\x02\x01' and
                int.from_bytes(header[18:20],'little')==machine,'TRACE_TARGET_ABI_UNSUPPORTED')
        return {'format':'ELF64-little','machine':machine,'kernel_signed_long_bits':64}

    def _make_instance(self):
        self.instance=self.tracefs/'instances'/('gdp-writer-'+str(os.getpid())+'-'+uuid.uuid4().hex)
        self.instance.mkdir(mode=0o700)

    def _calibrate_pid_namespace(self):
        require(threading.get_native_id()==os.getpid(),'TRACE_SETUP_REQUIRES_MAIN_THREAD')
        token='gdp-pid-proof-'+uuid.uuid4().hex
        self._write('events/enable','0');self._write('options/markers','1')
        self._write('trace','')
        self._write('tracing_on','1')
        try:self._write('trace_marker',token)
        finally:self._write('tracing_on','0')
        proof=marker_pid_proof(bounded_text(self.instance/'trace',65536),token,os.getpid())
        self._write('trace','')
        return proof

    def _write(self,relative,value):
        # All mutations are constrained to the unique instance we created.
        require(self.instance is not None and not Path(relative).is_absolute() and '..' not in Path(relative).parts,'TRACE_CONTROL_PATH')
        path=self.instance/relative;require(not path.is_symlink(),'TRACE_CONTROL_SYMLINK')
        with path.open('w') as out:out.write(value+'\n')

    def _stats(self):
        stats={};cpus=sorted((self.instance/'per_cpu').glob('cpu[0-9]*'))
        require(0<len(cpus)<=256,'TRACE_CPU_COUNT')
        for cpu in cpus:
            values={}
            for line in bounded_text(cpu/'stats',16384).splitlines():
                key,separator,value=line.partition(':')
                if separator and key.strip() in LOSS_COUNTERS:
                    raw=value.strip();require(raw.isdigit(),'TRACE_COUNTER_SCHEMA');values[key.strip()]=int(raw)
            require(set(values)==set(LOSS_COUNTERS),'TRACE_LOSS_COUNTERS_UNAVAILABLE')
            stats[cpu.name]=values
        return stats

    def _emit(self,row):
        data=(json.dumps(row,separators=(',',':'))+'\n').encode()
        require(self.data_bytes+len(data)<=self.max_bytes-METADATA_RESERVE,'TRACE_OUTPUT_LIMIT')
        self.stream.write(data);self.data_bytes+=len(data)

    def start(self,processes):
        require(self.state=='not-started','TRACE_ALREADY_STARTED')
        require(1<=len(processes)<=3,'TRACE_TARGET_COUNT')
        require(not self.output.exists() and not self.output.is_symlink(),'TRACE_FRESH_OUTPUT')
        self.output.mkdir(mode=0o700,parents=True,exist_ok=False)
        self.started_ns=time.monotonic_ns();self.capabilities=self._capability()
        if not self.capabilities['available']:
            self.state='unavailable'
            if self.required:self.failure='TRACE_UNAVAILABLE:'+self.capabilities['reason']
            self._json('ready.json',{'ok':False,'available':False,'state':self.state,**self.capabilities})
            return {'ok':False,'available':False,'state':self.state,'reason':self.capabilities['reason']}
        try:
            tids=set();comm={};owners={};own_time=os.readlink(self.proc/'self/ns/time')
            own_pid_namespace=os.readlink(self.proc/'self/ns/pid')
            offsets=zero_monotonic_offset(self.proc)
            for process in processes:
                require(process.poll() is None,'TRACE_TARGET_EXITED_BEFORE_START')
                identity=proc_identity(process.pid,self.proc)
                abi=self._target_abi(process.pid)
                require(os.readlink(self.proc/str(process.pid)/'ns/time')==own_time,'TRACE_TIME_NAMESPACE_MISMATCH')
                require(os.readlink(self.proc/str(process.pid)/'ns/pid')==own_pid_namespace,'TRACE_TARGET_PID_NAMESPACE_MISMATCH')
                current=[int(p.name) for p in (self.proc/str(process.pid)/'task').iterdir() if p.name.isdigit()]
                require(1<=len(current)<=MAX_THREADS,'TRACE_TARGET_THREAD_LIMIT');tids.update(current)
                for tid in current:
                    owners[tid]=process.pid
                    try:comm[tid]=bounded_text(self.proc/str(process.pid)/'task'/str(tid)/'comm',64).rstrip('\n')
                    except FileNotFoundError:pass
                self.targets.append({'process':process,'identity':identity,'abi':abi})
            require(len(tids)<=MAX_THREADS,'TRACE_THREAD_LIMIT')
            self._make_instance();self._write('tracing_on','0');self._write('current_tracer','nop')
            self._write('trace_clock','mono');self._write('options/overwrite','0');self._write('options/event-fork','1')
            self._write('options/context-info','1');self._write('options/irq-info','1')
            for option in ('raw','hex','bin','verbose','latency-format'):
                if (self.instance/'options'/option).exists():self._write('options/'+option,'0')
            pid_namespace_proof=self._calibrate_pid_namespace()
            cpus=list((self.instance/'per_cpu').glob('cpu[0-9]*'))
            require(0<len(cpus)<=256 and self.buffer_kib*1024*len(cpus)<=self.max_memory_bytes,'TRACE_TOTAL_RING_MEMORY_LIMIT')
            self._write('buffer_size_kb',str(self.buffer_kib))
            total=bounded_text(self.instance/'buffer_total_size_kb',1024).strip()
            require(total.isdigit() and int(total)*1024<=self.max_memory_bytes,'TRACE_TOTAL_RING_MEMORY_LIMIT')
            self._write('set_event_pid',' '.join(map(str,sorted(tids))))
            for target in self.targets:
                require(proc_identity(target['process'].pid,self.proc)==target['identity'],'TRACE_TARGET_IDENTITY_CHANGED')
                # event-fork now covers new descendants; seed any thread born
                # between the first enumeration and installing the filter.
                current={int(p.name) for p in (self.proc/str(target['process'].pid)/'task').iterdir() if p.name.isdigit()}
                tids.update(current)
                for tid in current:
                    owners[tid]=target['process'].pid
                    if tid not in comm:
                        try:comm[tid]=bounded_text(self.proc/str(target['process'].pid)/'task'/str(tid)/'comm',64).rstrip('\n')
                        except FileNotFoundError:pass
            require(len(tids)<=MAX_THREADS,'TRACE_THREAD_LIMIT')
            self._write('set_event_pid',' '.join(map(str,sorted(tids))))
            for event in self.capabilities['events']:
                category,name=event.split(':');self._write(f'events/{category}/{name}/enable','1')
            self.before=self._stats()
            require(all(value==0 for counters in self.before.values() for value in counters.values()),'TRACE_DIRTY_INITIAL_COUNTERS')
            self.fd=os.open(self.instance/'trace_pipe',os.O_RDONLY|os.O_NONBLOCK)
            self.stream=(self.output/'trace-events.jsonl').open('xb',buffering=TRACE_OUTPUT_BUFFER_BYTES);os.chmod(self.output/'trace-events.jsonl',0o600)
            self.decoder=Decoder(tids,self._emit,comm=comm,owners=owners);self._write('tracing_on','1');self.active=True;self.state='running'
            ready={'ok':True,'available':True,'state':self.state,'started_monotonic_ns':time.monotonic_ns(),
                'clock':'mono','initial_tids':sorted(tids),'targets':[{**target['identity'],'abi':target['abi']} for target in self.targets],
                'monotonic_timens_offset':offsets,
                'pid_namespace_calibration':pid_namespace_proof,
                'initial_owned_thread_comm':comm,
                'initial_tid_root_pid':owners,
                'events':self.capabilities['events'],'kernel_ring_buffer_bytes':int(total)*1024,
                'output_buffer_bytes':TRACE_OUTPUT_BUFFER_BYTES,
                'max_output_bytes':self.max_bytes,'metadata_reserve_bytes':METADATA_RESERVE,
                'max_seconds':self.max_seconds,'scope':SCOPE}
            self._json('ready.json',ready);return ready
        except (OSError,ValueError,UnicodeError) as error:
            self.failure='TRACE_SETUP_ERROR:'+type(error).__name__+':'+str(error);self.state='setup-error'
            self._stop();return {'ok':False,'available':False,'state':self.state,'reason':self.failure}

    def _json(self,name,value):
        with (self.output/name).open('x') as out:
            os.fchmod(out.fileno(),0o600);json.dump(value,out,indent=2);out.write('\n')

    def _read(self,budget=1024**2):
        total=0;began=time.monotonic_ns()
        while total<budget and time.monotonic_ns()-began<20_000_000:
            try:chunk=os.read(self.fd,min(65536,budget-total))
            except BlockingIOError:return total,True
            if not chunk:return total,True
            total+=len(chunk);self.fragment+=chunk
            while b'\n' in self.fragment:
                raw,self.fragment=self.fragment.split(b'\n',1);require(len(raw)<=MAX_LINE,'TRACE_LINE_LIMIT')
                self.decoder.line(raw.decode('utf-8','strict'))
            require(len(self.fragment)<=MAX_LINE,'TRACE_LINE_LIMIT')
        return total,False

    def poll(self):
        if not self.active:return
        try:
            require((time.monotonic_ns()-self.started_ns)/1e9<=self.max_seconds,'TRACE_WALL_LIMIT')
            for target in self.targets:
                # Never read a recycled numeric PID after Popen has reaped it.
                if target['process'].poll() is None:
                    try:current=proc_identity(target['process'].pid,self.proc)
                    except (FileNotFoundError,ValueError) as error:
                        exit_adjacent=isinstance(error,FileNotFoundError) or str(error)=='TRACE_TARGET_NOT_LIVE'
                        if not exit_adjacent or target['process'].poll() is None:raise
                    else:require(current==target['identity'],'TRACE_TARGET_IDENTITY_CHANGED')
            self._read()
        except (OSError,ValueError,UnicodeError) as error:
            self.failure='TRACE_RUNTIME_ERROR:'+type(error).__name__+':'+str(error);self.state='incomplete';self._stop()

    def _stop(self):
        if self.instance is not None:
            try:self._write('tracing_on','0')
            except (OSError,ValueError) as error:
                self.failure=self.failure or 'TRACE_STOP_ERROR:'+type(error).__name__
        self.active=False

    def _remove_instance(self):
        if self.instance is not None:self.instance.rmdir()

    def finish(self):
        if self.finished is not None:return self.finished
        require(self.state!='not-started','TRACE_NOT_STARTED')
        self._stop();after={};stats_error=None;drain_error=None
        if self.fd is not None:
            try:
                # Bounded final drain after tracing is disabled. An exhausted
                # time budget or leftover fragment is an explicit truncation.
                limit=time.monotonic()+2
                while time.monotonic()<limit:
                    consumed,drained=self._read()
                    if drained:break
                require(time.monotonic()<limit,'TRACE_FINAL_DRAIN_LIMIT')
                require(not self.fragment,'TRACE_TRUNCATED_FINAL_LINE')
            except (OSError,ValueError,UnicodeError) as error:
                drain_error=type(error).__name__+':'+str(error);self.failure=self.failure or 'TRACE_FINISH_ERROR:'+drain_error
            finally:
                # Loss counters are independent evidence. A decode/drain error
                # must not suppress them or turn missing evidence into zero loss.
                try:after=self._stats()
                except (OSError,ValueError) as error:
                    stats_error=type(error).__name__+':'+str(error)
                    self.failure=self.failure or 'TRACE_STATS_ERROR:'+stats_error
                os.close(self.fd);self.fd=None
        if self.stream is not None:self.stream.close();self.stream=None
        lost=0
        counters_complete=bool(self.before) and stats_error is None and set(after)==set(self.before)
        if set(after)!=set(self.before):self.failure=self.failure or 'TRACE_CPU_SET_CHANGED'
        for cpu,values in after.items():
            if cpu not in self.before:self.failure=self.failure or 'TRACE_CPU_SET_CHANGED';continue
            for key,value in values.items():
                delta=value-self.before[cpu][key]
                if delta<0:
                    self.failure=self.failure or 'TRACE_COUNTER_REGRESSION';counters_complete=False
                else:lost+=delta
        if lost:self.failure=self.failure or 'TRACE_KERNEL_LOSS'
        decoded=self.decoder.report() if self.decoder else {}
        if decoded.get('stream_lost_events',0):self.failure=self.failure or 'TRACE_STREAM_LOSS'
        if decoded.get('right_censored_calls',0) or decoded.get('open_calls_at_thread_exit',0):
            self.failure=self.failure or 'TRACE_UNPAIRED_SYSCALLS'
        if self.decoder and not decoded['paired_syscalls']:self.failure=self.failure or 'TRACE_NO_PAIRED_SYSCALLS'
        if self.decoder and not decoded['scheduler_switches']:self.failure=self.failure or 'TRACE_NO_SCHED_SWITCHES'
        terminal={}
        for target in self.targets:
            try:terminal[str(target['identity']['pid'])]=target['process'].poll() is not None
            except (OSError,ValueError) as error:
                terminal[str(target['identity']['pid'])]=None
                self.failure=self.failure or 'TRACE_FINISH_TARGET_IDENTITY:'+type(error).__name__
        if terminal and all(value is True for value in terminal.values()) and decoded.get('live_tid_contexts',0):
            self.failure=self.failure or 'TRACE_MISSING_THREAD_EXIT'
        unavailable=self.state=='unavailable'
        if self.decoder is not None and not counters_complete:
            self.failure=self.failure or 'TRACE_LOSS_COUNTERS_UNAVAILABLE'
        ok=not unavailable and self.failure is None and self.decoder is not None and counters_complete
        try:self._remove_instance()
        except OSError as error:
            self.failure=self.failure or 'TRACE_CLEANUP_ERROR:'+type(error).__name__;ok=False
        self.finished={'schema_version':2,'ok':ok,'available':not unavailable and self.decoder is not None,
            'state':'complete' if ok else self.state if unavailable else 'incomplete','error':self.failure,
            'capabilities':self.capabilities,'lost_events':lost+decoded.get('stream_lost_events',0) if counters_complete else None,
            'loss_counters_complete':counters_complete,'final_drain_error':drain_error,
            'kernel_loss_counters_before':self.before,'kernel_loss_counters_after':after,'stats_error':stats_error,
            'loss_count_scope':'sum of complete exposed counter deltas and stream annotations; null when counters unavailable/regressed; categories may overlap, not a unique event count; any loss rejects completeness',
            'decoded':decoded,'event_bytes':self.data_bytes,'max_output_bytes':self.max_bytes,
            'output_buffer_bytes':TRACE_OUTPUT_BUFFER_BYTES,
            'started_monotonic_ns':self.started_ns,'finished_monotonic_ns':time.monotonic_ns(),
            'scope':SCOPE,'hardware_acceptance_verified':False}
        self.finished['targets_terminal_at_finish']=terminal
        # Metadata is bounded by <=256 CPUs and <=1024 initial TIDs. Do not
        # silently write past the aggregate cap if a future schema grows.
        current=sum(p.stat().st_size for p in self.output.iterdir() if p.is_file())
        self.finished['output_bytes_before_summary_and_sha256']=current
        names=[p.name for p in self.output.iterdir() if p.is_file()]+['summary.json']
        manifest_size=sum(67+len(name.encode()) for name in names)
        self.finished['output_bytes']=0
        for _ in range(8):
            metadata=json.dumps(self.finished,indent=2).encode()+b'\n'
            size=current+len(metadata)+manifest_size
            if self.finished['output_bytes']==size:break
            self.finished['output_bytes']=size
        require(self.finished['output_bytes']==size and size<=self.max_bytes,'TRACE_METADATA_OUTPUT_LIMIT')
        self._json('summary.json',self.finished)
        with (self.output/'SHA256SUMS').open('x') as out:
            for path in sorted(self.output.iterdir()):
                if path.is_file() and path.name!='SHA256SUMS':out.write(sha(path)+'  '+path.name+'\n')
        require(sum(p.stat().st_size for p in self.output.iterdir() if p.is_file())==size,'TRACE_OUTPUT_SIZE_CHANGED')
        return self.finished


class AttachedProcess:
    """Read-only CLI target; never signals or waits/reaps an unrelated process."""
    def __init__(self,pid,start_ticks,proc=Path('/proc')):
        self.pid=pid;self.expected={'pid':pid,'start_ticks':start_ticks};self.proc=proc;self.exited=False
        require(proc_identity(pid,proc)==self.expected,'TRACE_TARGET_IDENTITY_CHANGED')

    def poll(self):
        if self.exited:return 0
        try:current=proc_identity(self.pid,self.proc)
        except FileNotFoundError:self.exited=True;return 0
        except ValueError as error:
            if str(error)=='TRACE_TARGET_NOT_LIVE':self.exited=True;return 0
            raise
        require(current==self.expected,'TRACE_TARGET_IDENTITY_CHANGED');return None


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe',action='store_true',help='read-only capability probe; does not create an instance')
    parser.add_argument('--tracefs',type=Path,default=Path('/sys/kernel/tracing'))
    parser.add_argument('--pid',action='append',default=[],metavar='PID:START_TICKS',help='targets must remain at their startup barrier until ready.json; read-only PID/start-time pin, never signals targets')
    parser.add_argument('--output',type=Path)
    parser.add_argument('--seconds',type=float,default=900,help='maximum lifetime; reaching this bound is incomplete, not a successful shortened trace')
    parser.add_argument('--max-output-mib',type=int,default=4096)
    parser.add_argument('--buffer-kib',type=int,default=256)
    parser.add_argument('--max-kernel-memory-mib',type=int,default=64)
    parser.add_argument('--include-read-syscalls',action='store_true')
    args=parser.parse_args()
    if args.probe:
        result=capability(args.tracefs,args.include_read_syscalls);print(json.dumps(result,indent=2));return 0 if result['available'] else 2
    require(args.output is not None and args.pid,'TRACE_OUTPUT_AND_PINNED_PIDS_REQUIRED')
    processes=[]
    for value in args.pid:
        require(re.fullmatch(r'[1-9]\d*:[1-9]\d*',value) is not None,'TRACE_PID_START_FORMAT')
        pid,start=map(int,value.split(':'));processes.append(AttachedProcess(pid,start))
    session=TraceSession(args.output,args.max_output_mib*1024**2,args.seconds,args.buffer_kib,args.tracefs,
        max_memory_bytes=args.max_kernel_memory_mib*1024**2,include_reads=args.include_read_syscalls)
    interrupted=False
    def stop(signum,frame):
        nonlocal interrupted
        interrupted=True
    old={number:signal.signal(number,stop) for number in (signal.SIGINT,signal.SIGTERM)}
    try:
        session.start(processes)
        while session.active and not interrupted:
            session.poll()
            if all(process.poll() is not None for process in processes):break
            time.sleep(.05)
        if interrupted:session.failure=session.failure or 'TRACE_INTERRUPTED'
    finally:
        result=session.finish()
        for number,handler in old.items():signal.signal(number,handler)
    print(json.dumps(result,indent=2));return 0 if result['ok'] else 1


if __name__=='__main__':
    try:sys.exit(main())
    except (OSError,ValueError) as error:raise SystemExit(type(error).__name__+':'+str(error))
