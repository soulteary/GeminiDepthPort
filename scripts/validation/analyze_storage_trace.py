#!/usr/bin/env python3
"""Bounded, read-only syscall/ext4/journal/block window association.

No merging of block requests by sector, no guessed request IDs or unique SSD
causality. Kernel event overlaps and file evidence are reported separately.
"""
import argparse
import hashlib
import json
import os
import re
import stat
from pathlib import Path
import time

MAX_SOURCE=8*1024**3
MAX_EVENTS=20000
MAX_LINE=32768


def require(value,code):
    if not value:raise ValueError(code)


def bounded(path,limit):
    require(not path.is_symlink(),'STORAGE_SOURCE_SYMLINK')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    try:
        require(stat.S_ISREG(os.fstat(fd).st_mode),'STORAGE_REGULAR_FILE_REQUIRED')
        require(os.fstat(fd).st_size<=limit,'STORAGE_METADATA_BOUND')
        data=os.read(fd,limit+1);require(len(data)<=limit,'STORAGE_METADATA_BOUND');return data
    finally:os.close(fd)


def window_rows(path,start,end,deadline,expected):
    used=0;seen=0;kept_bytes=0;digest=hashlib.sha256()
    require(not path.is_symlink() and path.is_file(),'STORAGE_REGULAR_FILE_REQUIRED')
    with path.open('rb') as stream:
        while True:
            line=stream.readline(MAX_LINE+1)
            if not line:break
            used+=len(line);digest.update(line);require(len(line)<=MAX_LINE and used<=MAX_SOURCE,'STORAGE_ANALYSIS_SOURCE_BOUND')
            require(time.monotonic()<deadline,'STORAGE_ANALYSIS_WALL_BOUND')
            row=json.loads(line);at=row['monotonic_ns']
            require(type(at) is int,'STORAGE_ANALYSIS_CLOCK')
            # Include completed intervals intersecting the window, even when
            # their entry precedes it. Never assume globally sorted CPU streams.
            if row.get('entry_monotonic_ns',at)<=end and at>=start:
                seen+=1;kept_bytes+=len(line)
                require(seen<=MAX_EVENTS and kept_bytes<=16*1024**2,'STORAGE_ANALYSIS_EVENT_BOUND');yield row

    require(digest.hexdigest()==expected,'STORAGE_SOURCE_HASH_MISMATCH')


def verified_summary(directory,deadline):
    raw=bounded(directory/'summary.json',1024**2)
    summary=json.loads(raw)
    require(summary.get('ok') is True and summary.get('loss_counters_complete') is True
            and type(summary.get('lost_events')) is int and summary.get('lost_events')==0,'STORAGE_ANALYSIS_REQUIRES_COMPLETE_TRACE')
    sums=bounded(directory/'SHA256SUMS',1024**2).decode()
    expected={}
    for line in sums.splitlines():
        digest,name=line.split(maxsplit=1)
        require('/' not in name and name not in ('.','..') and name not in expected and re.fullmatch('[a-f0-9]{64}',digest) is not None,'STORAGE_MANIFEST_SCHEMA');expected[name]=digest
    hashes={}
    for name in ('summary.json','ready.json','trace-events.jsonl'):
        require(name in expected,'STORAGE_MANIFEST_REQUIRED_FILE')
        if name=='trace-events.jsonl':hashes[name]=expected[name];continue
        h=hashlib.sha256();used=0;path=directory/name;require(not path.is_symlink(),'STORAGE_SOURCE_SYMLINK')
        before=path.stat()
        with path.open('rb') as stream:
            while True:
                block=stream.read(1024**2)
                if not block:break
                used+=len(block);require(used<=MAX_SOURCE and time.monotonic()<deadline,'STORAGE_VERIFY_BOUND');h.update(block)
        after=path.stat();require((before.st_ino,before.st_size,before.st_mtime_ns)==(after.st_ino,after.st_size,after.st_mtime_ns),'STORAGE_SOURCE_CHANGED')
        require(h.hexdigest()==expected[name],'STORAGE_SOURCE_HASH_MISMATCH');hashes[name]=expected[name]
    return hashes


def correlate(syscalls,events,start,end):
    syncs=[r for r in events if r['kind']=='ext4_sync_file_exit' and 'entry_monotonic_ns' in r]
    journals=[r for r in events if r['kind']=='jbd2_end_commit' and 'entry_monotonic_ns' in r]
    blocks=[r for r in events if r['kind'].startswith('block_')]
    require(len(syncs)<=4096 and len(journals)<=4096,'STORAGE_CORRELATION_STATE_BOUND')
    output=[];deadline=time.monotonic()+10
    for call in syscalls:
        if call.get('syscall') not in ('fsync','fdatasync') or 'duration_ns' not in call:continue
        require(time.monotonic()<deadline,'STORAGE_CORRELATION_WALL_BOUND')
        require(len(output)<4096,'STORAGE_CORRELATION_CALL_BOUND')
        a,b=call['entry_monotonic_ns'],call['monotonic_ns']
        nested=[e for e in syncs if e['context_tid']==call['tid'] and a<=e['entry_monotonic_ns']<=e['monotonic_ns']<=b]
        sample=call.get('fd_sample') or {};matching=[e for e in nested if e['dev']==sample.get('dev') and e['inode']==sample.get('inode')]
        observed=sample.get('observed_monotonic_ns');finished=sample.get('sample_finished_monotonic_ns')
        verified=(len(matching)==1 and sample.get('available') is True and type(observed) is int and type(finished) is int and a<=observed<=finished<=b)
        overlaps=[j for j in journals if j['entry_monotonic_ns']<=b and j['monotonic_ns']>=a]
        output.append({'tid':call['tid'],'entry_monotonic_ns':a,'exit_monotonic_ns':b,'syscall':call['syscall'],
                       'fd':call['fd'],'return_value':call.get('return_value'),'duration_ns':call['duration_ns'],'ext4_syncs':nested,
                       'file_role':sample.get('role') if verified else None,'file_role_kernel_inode_matched':verified,
                       'journal_overlap_count':len(overlaps),'journal_overlap_transactions':[j['transaction'] for j in overlaps[:32]],
                       'journal_association':'temporal/device overlap only; not an fsync-to-transaction causal ID'})
    counts={}
    for row in blocks:counts[row['kind']]=counts.get(row['kind'],0)+1
    return {'version':1,'window_start_ns':start,'window_end_ns':end,'fsync_correlations':output,
            'journal_transactions':journals,'block_event_counts':counts,
            'flush_events':[r for r in blocks if r.get('flush') is True],
            'fua_events':[r for r in blocks if r.get('fua') is True],
            'block_events':blocks,'unique_block_request_identity':False,'hardware_cause_verified':False,
            'scope':'block events retain partial completions/requeues/duplicate sector tuples; no synthetic request latency. File role requires nested ext4 dev/inode AND FD observation inside syscall interval.'}


def analyze(root,start,end):
    require(type(start) is int and type(end) is int and 0<=start<end and end-start<=30*10**9,'STORAGE_ANALYSIS_WINDOW_BOUND')
    deadline=time.monotonic()+120;root=Path(root)
    proof={'syscalls':verified_summary(root,deadline),'storage':verified_summary(root/'storage',deadline)}
    first=json.loads(bounded(root/'ready.json',1024**2));second=json.loads(bounded(root/'storage/ready.json',1024**2))
    require(first.get('clock')==second.get('clock')=='mono' and first.get('targets') and first.get('targets')==second.get('targets'),'STORAGE_TRACE_TARGET_OR_CLOCK_MISMATCH')
    for directory in (root,root/'storage'):
        summary=json.loads(bounded(directory/'summary.json',1024**2))
        require(summary['started_monotonic_ns']<=start<end<=summary['finished_monotonic_ns'],'STORAGE_WINDOW_OUTSIDE_CAPTURE')
    calls=list(window_rows(root/'trace-events.jsonl',start,end,deadline,proof['syscalls']['trace-events.jsonl']))
    events=list(window_rows(root/'storage/trace-events.jsonl',start,end,deadline,proof['storage']['trace-events.jsonl']))
    result=correlate(calls,events,start,end);result['source_hashes']=proof;return result


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--trace',type=Path,required=True)
    p.add_argument('--start-ns',type=int,required=True);p.add_argument('--end-ns',type=int,required=True)
    p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    require(not a.output.exists(),'STORAGE_ANALYSIS_FRESH_OUTPUT')
    result=analyze(a.trace,a.start_ns,a.end_ns);raw=(json.dumps(result,indent=2)+'\n').encode()
    require(len(raw)<=32*1024**2,'STORAGE_ANALYSIS_OUTPUT_BOUND')
    with a.output.open('xb') as out:__import__('os').fchmod(out.fileno(),0o600);out.write(raw)
    return 0

if __name__=='__main__':main()
