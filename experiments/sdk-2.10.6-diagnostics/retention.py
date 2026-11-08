"""Release only new, stopped, accepted V3 payloads after independent audits.

Receipts, indices, audits, configuration and failures remain. This is an opt-in
acceptance evidence policy, never production recording rotation or old-data GC.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time

import experiment_runtime as rt
import run_hardware as one

POLICIES = ('all', 'verified-window')
GIB = 1024**3


def capacity(native, metadata, groups, wire, policy):
    rt.require(policy in POLICIES, 'RETENTION_POLICY')
    # Retained indices/IMU are bounded by the same 64KiB/pair estimate. Keep
    # three bounded 2GiB audits and 1GiB diagnostics per group, plus crash data.
    retained = metadata + 7*GIB
    if policy == 'all':
        working = (groups+1)*native + groups*2*wire
    else:
        working = native + 2*wire + groups*retained
    return {'estimated_required_bytes':working*5//4+GIB,
            'retention_policy':policy, 'retained_metadata_per_group_budget':retained,
            'successful_raw_payloads_retained':policy == 'all',
            'normal_groups':groups, 'native_bytes_per_group':native,
            'dds_bytes_per_group':wire, 'scope':'retained evidence plus full active/crash recording, 25% + 1GiB; estimate, not quota'}


def _stamp(info):
    return [info.st_dev,info.st_ino,info.st_size,info.st_mtime_ns,info.st_ctime_ns]


def _directory(parent, name):
    rt.require(re.fullmatch(r'[A-Za-z0-9_.-]+',name) is not None and name not in ('.','..'), 'RETENTION_PATH')
    return os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,dir_fd=parent)


def _hash(parent, name, max_bytes=4*GIB):
    fd=os.open(name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK,dir_fd=parent)
    try:
        before=os.fstat(fd)
        rt.require(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_uid==os.getuid() and before.st_size<=max_bytes, 'RETENTION_FILE_IDENTITY')
        digest=hashlib.sha256()
        while True:
            block=os.read(fd,1024*1024)
            if not block:break
            digest.update(block)
        rt.require(_stamp(os.fstat(fd))==_stamp(before),'RETENTION_FILE_CHANGED')
        return {'size':before.st_size,'sha256':digest.hexdigest(),'identity':_stamp(before)}
    finally:os.close(fd)


def release(group_root, expected_pairs, stopped):
    """Caller just checked every pinned container is stopped; refuse stale runs.

    Call only from the fresh-root batch loop, after summarize() and ownership
    checks. No recursive deletion, shell commands, symlink following or resume.
    """
    group_root=Path(group_root)
    rt.require(stopped is True,'RETENTION_REQUIRES_STOPPED')
    result=rt.read_json(group_root/'result.json')
    rt.require(result.get('ok') is True and result.get('hardware_acceptance_verified') is True
               and not result.get('cleanup_errors')
               and result.get('uid')==os.getuid() and result.get('gid')==os.getgid(), 'RETENTION_UNACCEPTED_GROUP')
    report=rt.read_json(group_root/'capture/dds-native-comparison.json')
    cameras=report.get('cameras',[])
    rt.require(report.get('ok') is True and len(cameras)==3 and len({v['camera'] for v in cameras})==3,'RETENTION_THREE_AUDITS')
    for camera in cameras:
        audit=camera['summary']
        rt.require(audit.get('ok') is True and audit.get('audit_version')==2
                   and audit.get('independent_audit_passes')==2 and audit.get('native_hashes_verified') is True
                   and audit.get('recovered') is False and audit.get('pairs')==expected_pairs,
                   'RETENTION_TWO_FULL_READS_REQUIRED')
    receipt=group_root/'payload-retention.json'
    rt.require(not receipt.exists(),'RETENTION_NO_RESUME')
    root=os.open(group_root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
    opened=[];files=[]
    try:
        capture=_directory(root,'capture');opened.append(capture)
        for camera in cameras:
            name=camera['camera'];cam=_directory(capture,name);opened.append(cam)
            recordings=_directory(cam,'recordings');opened.append(recordings)
            jobs=os.listdir(recordings)
            rt.require(len(jobs)==1,'RETENTION_ONE_COMPLETED_JOB')
            job=jobs[0];rt.require(not job.endswith('.partial'),'RETENTION_NO_PARTIAL')
            fd=_directory(recordings,job);opened.append(fd)
            manifest_fd=os.open('manifest.json',os.O_RDONLY|os.O_NOFOLLOW,dir_fd=fd)
            try:
                rt.require(stat.S_ISREG(os.fstat(manifest_fd).st_mode) and os.fstat(manifest_fd).st_size<=1024*1024,'RETENTION_MANIFEST_LIMIT')
                manifest=json.loads(os.read(manifest_fd,1024*1024))
            finally:os.close(manifest_fd)
            rt.require(manifest.get('version')==3 and manifest.get('record_count')==expected_pairs,'RETENTION_V3_COMPLETE_TARGET')
            _hash(fd,'COMPLETE',1024*1024)
            names=os.listdir(fd);rt.require(len(names)<=100000,'RETENTION_ENTRY_LIMIT')
            payloads=[n for n in names if re.fullmatch(r'payload-[0-9]{20}\.bin',n)]
            rt.require(payloads,'RETENTION_PAYLOAD_REQUIRED')
            for n in sorted(payloads):files.append((fd,n,{'path':f'capture/{name}/recordings/{job}/{n}',**_hash(fd,n,128*1024**2+65536)}))
        # The compared DDS database contains hashes, not images, and can also be
        # released after the accepted comparison. Preserve its hash and summary.
        files.append((capture,'wire.sqlite3',{'path':'capture/wire.sqlite3',**_hash(capture,'wire.sqlite3')}))
        rt.require(not any(n in os.listdir(capture) for n in ('wire.sqlite3-journal','wire.sqlite3-wal','wire.sqlite3-shm')),'RETENTION_DATABASE_STILL_ACTIVE')
        value={'version':1,'policy':'verified-window','state':'prepared','prepared_unix_ns':time.time_ns(),
               'audit_sha256':rt.sha(group_root/'capture/dds-native-comparison.json'),
               'files':[entry for _,_,entry in files], 'scope':'successful pixels/DDS DB intentionally released; metadata retained; raw replay unavailable'}
        one.atomic(receipt,value)
        # Recheck ALL files before the first removal, then each exact directory
        # entry again. An interrupted release remains explicitly incomplete.
        for fd,n,entry in files:rt.require(_stamp(os.stat(n,dir_fd=fd,follow_symlinks=False))==entry['identity'],'RETENTION_FILE_CHANGED')
        for fd,n,entry in files:
            rt.require(_stamp(os.stat(n,dir_fd=fd,follow_symlinks=False))==entry['identity'],'RETENTION_FILE_CHANGED')
            os.unlink(n,dir_fd=fd)
        for fd in {fd for fd,_,_ in files}:os.fsync(fd)
        value.update(state='released',finished_unix_ns=time.time_ns(),released_bytes=sum(e['size'] for _,_,e in files))
        one.atomic(receipt,value)
        return {'policy':'verified-window','receipt':'payload-retention.json','receipt_sha256':rt.sha(receipt),
                'released_bytes':value['released_bytes'],'raw_replay_available':False}
    finally:
        for fd in reversed(opened):os.close(fd)
        os.close(root)
