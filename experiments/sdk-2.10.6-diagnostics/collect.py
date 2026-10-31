#!/usr/bin/env python3
"""Bounded read-only collection of explicitly enabled experimental SDK snapshots."""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import time

BASE = '575347ca742d219ef61b1e214e97043f17f48d23'
LIMIT = 65536


def read_snapshot(path):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    try:
        info=os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_size>LIMIT or info.st_mode&0o077:
            raise ValueError('PRIVATE_BOUNDED_REGULAR_SNAPSHOT_REQUIRED')
        with os.fdopen(fd,'rb',closefd=False) as f:raw=f.read(LIMIT+1)
        if not raw:return None  # Initial reserved file before first background output.
        if len(raw)>LIMIT:raise ValueError('SNAPSHOT_LIMIT')
        value=json.loads(raw)
        if value.get('version')!=1 or value.get('sdk_base')!=BASE or len(value.get('events',[]))>64:
            raise ValueError('SDK_DIAGNOSTIC_SCHEMA')
        return value
    finally:os.close(fd)


class Cursor:
    def __init__(self):self.latest=None;self.event=0;self.history_missing=0;self.events_missing=0
    def accept(self,value):
        stamp=value['snapshot_monotonic_ns']
        if self.latest is not None and stamp<self.latest:raise ValueError('SNAPSHOT_CLOCK_REGRESSION')
        if stamp==self.latest:return None
        self.latest=stamp
        events=sorted((v for v in value['events'] if v['id']>self.event),key=lambda v:v['id'])
        if events:
            missing=max(0,events[0]['id']-self.event-1)
            if self.event:self.events_missing+=missing
            else:self.history_missing+=missing
            self.event=events[-1]['id']
        return events


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source',action='append',required=True,metavar='NAME=/ABS/PRIVATE/DIRECTORY')
    p.add_argument('--seconds',type=float,default=600);p.add_argument('--interval',type=float,default=1)
    p.add_argument('--max-mib',type=int,default=64);p.add_argument('--output',type=Path,required=True)
    a=p.parse_args();os.umask(0o077)
    if not (0<a.seconds<=86400 and .1<=a.interval<=10 and 1<=a.max_mib<=8192 and len(a.source)<=8):p.error('bounded inputs required')
    sources={}
    for spec in a.source:
        name,sep,path=spec.partition('=')
        if not sep or not re.fullmatch(r'[A-Za-z0-9_-]{1,32}',name) or not Path(path).is_absolute() or name in sources:p.error('unique name and absolute directory required')
        root=Path(path)
        if root.is_symlink() or not root.is_dir() or root.stat().st_mode&0o077:p.error('private non-symlink directory required')
        sources[name]=root
    a.output.mkdir(mode=0o700,parents=True,exist_ok=False)
    cursors,errors,observed={},[],set()
    started,cpu0,used=time.monotonic(),time.process_time(),0
    with (a.output/'samples.jsonl').open('x') as out:
        try:
            while time.monotonic()-started<a.seconds:
                for name,root in sources.items():
                    files=sorted(root.glob('sdk-diag-*.json'))
                    if len(files)>256:raise ValueError('TOO_MANY_INSTANCE_FILES_USE_FRESH_RUN_DIRECTORY')
                    for file in files:
                        value=read_snapshot(file)
                        if value is None:continue
                        key=name+'/'+file.name
                        cursor=cursors.setdefault(key,Cursor())
                        events=cursor.accept(value)
                        if events is None:continue
                        observed.add(name)
                        row={'source':name,'file':file.name,'observed_unix_ns':time.time_ns(),'observed_monotonic_ns':time.monotonic_ns(),'snapshot':value,'new_sampled_events':events}
                        text=json.dumps(row,separators=(',',':'))+'\n';used+=len(text.encode())
                        if used>a.max_mib*1024**2:raise ValueError('OUTPUT_LIMIT')
                        out.write(text)
                        if value.get('sink_errors'):raise ValueError('SDK_DIAGNOSTIC_SINK_ERRORS')
                out.flush();time.sleep(a.interval)
        except (Exception,KeyboardInterrupt) as error:errors.append(type(error).__name__+':'+str(error))
    if observed!=set(sources):errors.append('MISSING_SOURCE_SNAPSHOTS')
    summary={'ok':not errors,'errors':errors,'elapsed_seconds':time.monotonic()-started,'collector_cpu_seconds':time.process_time()-cpu0,'jsonl_bytes':used,
             'sources_observed':sorted(observed),'cursors':{k:{'initial_unobserved_events':v.history_missing,'later_unobserved_events':v.events_missing} for k,v in cursors.items()},
             'scope':'Counters and histograms are cumulative nontransactional snapshots; events are bounded samples, not a lossless frame log; unobserved diagnostic events are not recording loss. No capture mutation.'}
    (a.output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
    print(json.dumps({'ok':summary['ok'],'errors':errors}))
    return 0 if summary['ok'] else 1

if __name__=='__main__':raise SystemExit(main())
