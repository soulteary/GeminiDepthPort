#!/usr/bin/env python3
"""Opt-in, device-scoped ext4/jbd2/block evidence, including kernel threads.

Only private trace instances are changed. Device timelines do NOT uniquely
attribute block requests or SSD firmware time to an inode or writer.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import sys
import time

_spec=importlib.util.spec_from_file_location('gdp_storage_syscalls',Path(__file__).with_name('writer_syscall_trace.py'))
syscalls=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(syscalls)
require=syscalls.require
EVENTS=('ext4:ext4_sync_file_enter','ext4:ext4_sync_file_exit',
        'jbd2:jbd2_start_commit','jbd2:jbd2_commit_locking','jbd2:jbd2_commit_flushing',
        'jbd2:jbd2_commit_logging','jbd2:jbd2_end_commit',
        'block:block_rq_insert','block:block_rq_issue','block:block_rq_complete','block:block_rq_requeue')
SCOPE=('Device-scoped events with EMPTY set_event_pid: includes jbd2/kworker and unrelated IO on selected devices. '
       'ext4 uses filesystem device/inode; jbd2 uses filesystem device/transaction. '
       'Block sectors are whole-device addresses, not inode offsets. RWBS F and FUA flags retained; '
       'tracepoints expose no stable request ID, so sector tuples/overlap never prove unique request or SSD firmware latency. '
       'No filesystem tuning, mounts, global trace changes or hardware certification.')


def kernel_dev(pair):
    major,minor=pair;require(0<=major<4096 and 0<=minor<2**20,'STORAGE_DEVICE_RANGE')
    return (major<<20)|minor


def device_binding(path,disk,sys_block=Path('/sys/class/block'),mountinfo=Path('/proc/self/mountinfo')):
    require(re.fullmatch(r'[A-Za-z0-9_.-]{1,64}',disk or '') is not None,'STORAGE_WHOLE_DISK_REQUIRED')
    path=Path(path).resolve(strict=True);info=path.stat();fs=(os.major(info.st_dev),os.minor(info.st_dev))
    matches=[]
    for row in syscalls.bounded_text(mountinfo,2*1024**2).splitlines():
        left,right=row.split(' - ',1);fields=left.split();mount=Path(fields[4].replace('\\040',' ').replace('\\134','\\'))
        if path==mount or mount in path.parents:matches.append((len(str(mount)),fields[2],right.split()[0]))
    require(matches and max(matches)[2]=='ext4','STORAGE_EXT4_REQUIRED')
    require(max(matches)[1]==f'{fs[0]}:{fs[1]}','STORAGE_MOUNT_DEVICE_MISMATCH')
    entry=sys_block/disk;require(not (entry/'partition').exists(),'STORAGE_WHOLE_DISK_REQUIRED')
    whole=tuple(map(int,syscalls.bounded_text(entry/'dev',128).strip().split(':')))
    require(len(whole)==2,'STORAGE_DEVICE_SCHEMA')
    target=(sys_block.parent.parent/'dev/block'/f'{fs[0]}:{fs[1]}').resolve(strict=True)
    selected=entry.resolve(strict=True)
    require(target==selected or (target.parent==selected and (target/'partition').is_file()),'STORAGE_DIRECT_DEVICE_REQUIRED')
    return {'filesystem_dev':list(fs),'block_dev':list(whole),'disk':disk,'filesystem':'ext4',
            'filesystem_kernel_dev':kernel_dev(fs),'block_kernel_dev':kernel_dev(whole),
            'stacked_device_supported':False,'external_journal_verified':False,
            'scope':'recording filesystem st_dev and mountinfo matched to direct whole sysfs disk/partition; no physical SSD identity certification'}


def capability(tracefs, binding):
    result=syscalls.capability(tracefs)
    if not result['available']:return result
    try:
        available=set(syscalls.bounded_text(Path(tracefs)/'available_events',4*1024**2).split())
        require(set(EVENTS)<=available,'STORAGE_TRACEPOINTS_MISSING:'+','.join(sorted(set(EVENTS)-available)))
        formats={}
        for event in EVENTS:
            category,name=event.split(':');raw=syscalls.bounded_text(Path(tracefs)/'events'/category/name/'format')
            require(re.search(r'field:[^;]*\bdev;\s*offset:\d+;\s*size:4;\s*signed:0;',raw) is not None,'STORAGE_DEV_FIELD_SCHEMA:'+event)
            formats[event]=syscalls.hashlib.sha256(raw.encode()).hexdigest()
        return {**result,'events':list(EVENTS),'event_format_sha256':formats,'device_binding':binding,'scope':SCOPE}
    except (OSError,ValueError) as failure:return {'available':False,'reason':str(failure),'scope':SCOPE}


def number(fields,key):
    match=re.search(r'\b'+re.escape(key)+r'(?:=|\s+)\s*(-?\d+)\b',fields)
    require(match is not None,'STORAGE_FIELD_MISSING:'+key);return int(match.group(1))


def rwbs_flags(raw):
    # Linux blk_fill_rwbs: optional PREFLUSH F, operation, optional FUA F,
    # then A/S/M/U. Pure flush FF is ambiguous; retain null, never guess.
    match=re.fullmatch(r'(F?)(WZ|W|R|DE|D|N)(F?)(A?S?M?U?)',raw)
    if match:return {'operation':match[2],'preflush':bool(match[1]),'fua':bool(match[3]),'flush':bool(match[1])}
    match=re.fullmatch(r'(F{1,3})(A?S?M?U?)',raw)
    if match:return {'operation':'F','preflush':False if len(match[1])==1 else None,'fua':False if len(match[1])==1 else None,'flush':True}
    require(re.fullmatch(r'F?Z(?:RA|A|R|F|O|C)F?A?S?M?U?',raw) is not None,'STORAGE_RWBS_SCHEMA')
    return {'operation':'zone','preflush':None,'fua':None,'flush':None}


class StorageDecoder:
    def __init__(self,binding,emit,max_pending=4096):
        self.binding=binding;self.emit=emit;self.max_pending=max_pending
        self.syncs={};self.transactions={};self.events=0;self.pairs=0;self.censored=0;self.lost=0;self.flush=0;self.requeues=0;self.blocks=0;self.journal_pairs=0
    def line(self,line):
        if not line.strip() or line.lstrip().startswith('#'):return
        lost=re.search(r'\bLOST\s+(\d+)\s+EVENTS',line)
        if lost:self.lost+=int(lost.group(1));raise ValueError('STORAGE_STREAM_LOSS')
        match=syscalls.PREFIX.match(line);require(match is not None,'STORAGE_LINE_SCHEMA')
        tid,cpu,seconds,fraction,body=match.groups();require(1<=len(fraction)<=9,'STORAGE_CLOCK_SCHEMA')
        event,fields=syscalls.event_text(body);require(event in {e.split(':')[1] for e in EVENTS},'STORAGE_UNEXPECTED_EVENT')
        dev=re.search(r'^(?:dev\s+)?(\d+),(\d+)\b',fields);require(dev is not None,'STORAGE_DEV_SCHEMA')
        device=list(map(int,dev.groups()));is_block=event.startswith('block_')
        require(device==self.binding['block_dev' if is_block else 'filesystem_dev'],'STORAGE_DEVICE_FILTER_ESCAPE')
        at=int(seconds)*10**9+int(fraction)*10**(9-len(fraction))
        row={'kind':event,'monotonic_ns':at,'printed_resolution_ns':10**(9-len(fraction)),
             'context_tid':int(tid),'cpu':int(cpu),'dev':device,'scope':'selected-device, all tasks including kernel threads'}
        if event.startswith('ext4_'):
            row['inode']=number(fields,'ino');key=(int(tid),row['inode'])
            if event.endswith('_enter'):
                require(key not in self.syncs,'STORAGE_DUPLICATE_SYNC_ENTRY');require(len(self.syncs)<self.max_pending,'STORAGE_PENDING_LIMIT')
                self.syncs[key]=at;row['datasync']=number(fields,'datasync')
            else:
                row['return_value']=number(fields,'ret');entry=self.syncs.pop(key,None)
                if entry is not None:
                    require(at>=entry,'STORAGE_CLOCK_REGRESSION');row.update(entry_monotonic_ns=entry,duration_ns=at-entry);self.pairs+=1
                else:row['left_censored']=True;self.censored+=1
        elif event.startswith('jbd2_'):
            tx=number(fields,'transaction');row['transaction']=tx;row['sync']=number(fields,'sync')
            if event=='jbd2_start_commit':
                require(tx not in self.transactions,'STORAGE_DUPLICATE_TRANSACTION');require(len(self.transactions)<self.max_pending,'STORAGE_PENDING_LIMIT');self.transactions[tx]=at
            elif event=='jbd2_end_commit':
                entry=self.transactions.pop(tx,None)
                if entry is not None:
                    require(at>=entry,'STORAGE_CLOCK_REGRESSION');row.update(entry_monotonic_ns=entry,duration_ns=at-entry);self.journal_pairs+=1
                else:row['left_censored']=True;self.censored+=1
        else:
            # Handles old/new kernel printers: issue/insert have a byte count,
            # optional command/ioprio text; complete has only sectors/error.
            rwbs=re.match(r'\s*([A-Z]+)\b',fields[dev.end():]);require(rwbs is not None,'STORAGE_RWBS_SCHEMA')
            span=re.search(r'(\d+)\s*\+\s*(\d+)',fields);require(span is not None,'STORAGE_SECTOR_SCHEMA')
            row.update(rwbs=rwbs.group(1),sector=int(span.group(1)),nr_sectors=int(span.group(2)),
                       unique_request_identity=False,**rwbs_flags(rwbs.group(1)))
            if event=='block_rq_complete':
                error=re.search(r'\[(-?\d+)\]\s*$',fields);require(error is not None,'STORAGE_BLOCK_ERROR_SCHEMA');row['error']=int(error.group(1))
            self.blocks+=1
            if row['flush']:self.flush+=1
            if event=='block_rq_requeue':self.requeues+=1
        self.emit(row);self.events+=1
    def report(self):
        return {'events':self.events,'paired_ext4_syncs':self.pairs,'left_censored_intervals':self.censored,
                'right_censored_ext4_syncs':len(self.syncs),'right_censored_journal_transactions':len(self.transactions),
                'stream_lost_events':self.lost,'flush_events':self.flush,'requeue_events':self.requeues,
                'unique_block_request_identity':False,'block_events':self.blocks,'paired_journal_transactions':self.journal_pairs}


class StorageSession(syscalls.TraceSession):
    scope=SCOPE
    requires_owned_syscalls=False
    def __init__(self,*args,binding,**kwargs):self.binding=binding;super().__init__(*args,**kwargs)
    def _capability(self):return capability(self.tracefs,self.binding)
    def _configure_events(self,tids):
        # No PID filter: journal and block completions commonly run in kernel
        # threads, not in the writer's process ancestry.
        self.installed_filters={}
        self._write('set_event_pid','')
        require(not syscalls.bounded_text(self.instance/'set_event_pid').strip(),'STORAGE_PID_FILTER_NOT_EMPTY')
        for event in self.capabilities['events']:
            category,name=event.split(':');dev=self.binding['block_kernel_dev' if category=='block' else 'filesystem_kernel_dev']
            self._write(f'events/{category}/{name}/filter',f'dev == {dev}')
            installed=syscalls.bounded_text(self.instance/f'events/{category}/{name}/filter')
            require(re.fullmatch(r'\s*dev\s*==\s*'+str(dev)+r'\s*',installed) is not None,'STORAGE_DEVICE_FILTER_REJECTED')
            self.installed_filters[event]=installed.strip()
            self._write(f'events/{category}/{name}/enable','1')
    def _ready_details(self):
        return {'set_event_pid':'','device_binding':self.binding,'installed_device_filters':self.installed_filters,'event_format_sha256':self.capabilities['event_format_sha256'] if 'event_format_sha256' in self.capabilities else {}}
    def _new_decoder(self,tids,comm,owners):return StorageDecoder(self.binding,self._emit)
    def _decoder_problem(self,decoded):
        if not decoded.get('paired_ext4_syncs'):return 'STORAGE_NO_PAIRED_EXT4_SYNCS'
        if not decoded.get('block_events'):return 'STORAGE_NO_BLOCK_EVENTS'
        if not decoded.get('paired_journal_transactions'):return 'STORAGE_NO_PAIRED_JOURNAL_COMMITS'
        return None


class FileSession(syscalls.TraceSession):
    """Best-effort FD role samples, never interpreted as an entry-time binding."""
    def __init__(self,*args,**kwargs):super().__init__(*args,**kwargs);self.fd_samples={};self.next_fd_sample_ns=0
    def _emit(self,row):
        if row.get('kind') in ('sys_enter_fsync','sys_enter_fdatasync'):
            key=(row['tid'],row['tid_generation']);sample={'available':False,'observed_monotonic_ns':time.monotonic_ns(),
                'scope':'FD sampled while draining trace, NOT syscall-entry identity; match dev/inode to nested ext4 event before attributing'}
            try:
                # Best-effort lookup is capped globally at 100 attempts/s. Late
                # and skipped samples remain unknown, never guessed bindings.
                now=time.monotonic_ns()
                require(now>=self.next_fd_sample_ns,'STORAGE_FD_SAMPLE_RATE_LIMIT')
                self.next_fd_sample_ns=now+10_000_000
                root=row['root_pid'];target=next(t for t in self.targets if t['identity']['pid']==root)
                require(target['process'].poll() is None and syscalls.proc_identity(root,self.proc)==target['identity'],'STORAGE_FD_PROCESS_EXITED')
                task=syscalls.proc_identity(row['tid'],self.proc)
                path=self.proc/str(row['tid'])/'fd'/str(row['fd']);before=path.stat();link=os.readlink(path);after=path.stat()
                require(syscalls.proc_identity(row['tid'],self.proc)==task,'STORAGE_FD_TID_REUSED')
                require((before.st_dev,before.st_ino)==(after.st_dev,after.st_ino),'STORAGE_FD_REUSED')
                name=Path(link.removesuffix(' (deleted)')).name
                role='payload' if re.fullmatch(r'payload-\d{20}\.bin',name) else 'index' if name=='records.jsonl' else 'checkpoint' if name in ('.checkpoint.tmp','checkpoint.json') else 'directory' if stat.S_ISDIR(after.st_mode) else 'other'
                sample.update(available=True,role=role,dev=[os.major(after.st_dev),os.minor(after.st_dev)],inode=after.st_ino,
                              sampled_tid_start_ticks=task['start_ticks'],sample_finished_monotonic_ns=time.monotonic_ns())
            except (OSError,ValueError,StopIteration) as error:sample['reason']=str(error) if str(error)=='STORAGE_FD_SAMPLE_RATE_LIMIT' else type(error).__name__
            require(len(self.fd_samples)<syscalls.MAX_THREADS or key in self.fd_samples,'STORAGE_FD_STATE_LIMIT')
            self.fd_samples[key]=sample;row={**row,'fd_sample':sample}
        elif row.get('kind') in ('sys_exit_fsync','sys_exit_fdatasync'):
            sample=self.fd_samples.pop((row['tid'],row['tid_generation']),None)
            if sample is not None:row={**row,'fd_sample':sample}
        elif row.get('kind')=='sched_process_exit':
            tid=row.get('tid',row['context_tid'])
            for key in [k for k in self.fd_samples if k[0]==tid]:self.fd_samples.pop(key)
        super()._emit(row)


class CombinedSession:
    """Two independent bounded instances; either incomplete rejects attribution."""
    def __init__(self,output,max_bytes,max_seconds,buffer_kib=256,tracefs=Path('/sys/kernel/tracing'),required=True,
                 storage_max_bytes=4*1024**3,recording_path=None,disk=None):
        self.binding=device_binding(recording_path,disk)
        self.file=FileSession(output,max_bytes,max_seconds,buffer_kib,tracefs,required,max_memory_bytes=32*1024**2)
        self.storage=StorageSession(Path(output)/'storage',storage_max_bytes,max_seconds,buffer_kib,tracefs,required,
                                    max_memory_bytes=32*1024**2,binding=self.binding)
        self.failure=None;self.finished=None
    def start(self,processes):
        first=self.file.start(processes)
        if first.get('ok') is not True:self.failure=self.file.failure or 'STORAGE_PARENT_TRACE_UNAVAILABLE';return first
        second=self.storage.start(processes)
        if second.get('ok') is not True:self.failure=self.storage.failure or 'STORAGE_TRACE_UNAVAILABLE'
        return {**first,'ok':first.get('ok') is True and second.get('ok') is True,'storage':second}
    def start_draining(self):
        self.file.start_draining()
        try:self.storage.start_draining()
        except Exception:
            self.stop_capture();raise
    def stop_capture(self):
        self.file.stop_capture();self.storage.stop_capture()
    def poll(self):
        self.file.poll();self.storage.poll();self.failure=self.failure or self.file.failure or self.storage.failure
    def finish(self):
        if self.finished is not None:return self.finished
        child=None;error=None
        try:
            if self.storage.state!='not-started':child=self.storage.finish()
        except (OSError,ValueError) as failure:error=type(failure).__name__+':'+str(failure)
        finally:parent=self.file.finish()
        self.failure=self.failure or self.file.failure or self.storage.failure or error
        complete=parent.get('loss_counters_complete') is True and (child or {}).get('loss_counters_complete') is True
        result={**parent,'storage_trace':child,'storage_finish_error':error,'storage_device_binding':self.binding,
                'ok':parent.get('ok') is True and (child or {}).get('ok') is True,
                'loss_counters_complete':complete,
                'lost_events':parent['lost_events']+child['lost_events'] if complete else None,
                'aggregate_event_bytes':parent.get('event_bytes',0)+(child or {}).get('event_bytes',0)}
        if not result['ok']:result.update(state='incomplete',error=self.failure or 'STORAGE_TRACE_INCOMPLETE')
        # Keep the immutable child/parent hash inventories intact. The caller's
        # outer manifest covers this bounded sibling receipt as well.
        data=(json.dumps(result,indent=2)+'\n').encode();require(len(data)<=1024**2,'STORAGE_COMBINED_METADATA_LIMIT')
        path=self.file.output.with_name(self.file.output.name+'-combined.json')
        with path.open('xb') as out:os.fchmod(out.fileno(),0o600);out.write(data)
        self.finished=result;return result


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--probe',action='store_true',required=True)
    p.add_argument('--recording-path',type=Path,required=True);p.add_argument('--disk',required=True)
    p.add_argument('--tracefs',type=Path,default=Path('/sys/kernel/tracing'));a=p.parse_args()
    binding=device_binding(a.recording_path,a.disk);result=capability(a.tracefs,binding)
    print(json.dumps(result,indent=2));return 0 if result['available'] else 2

if __name__=='__main__':
    try:sys.exit(main())
    except (OSError,ValueError) as error:raise SystemExit(str(error))
