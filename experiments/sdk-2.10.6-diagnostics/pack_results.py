#!/usr/bin/env python3
"""Stream a bounded, SHA256-listed retest evidence bundle without raw recordings."""
import argparse
from contextlib import contextmanager
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile
import tempfile
import time

import experiment_runtime as rt

SDK_PARENT = Path('/dev/shm')
TEXT = {'.json', '.jsonl', '.md', '.log', '.stderr', '.stdout', '.txt', '.csv', '.yaml', '.yml', '.sha256', '.ready'}
RAW_TREE = re.compile(r'^(?:ab/(?:on|off)-[0-9]{2}/capture/[^/]+|crash/[^/]+)/recordings$')
RECORDING_METADATA = ('checkpoint.json', 'snapshot.json', 'manifest.json', 'ros-snapshot.json', 'COMPLETE', 'INTERRUPTED.json')
PREFIX = 'gdp-retest-evidence'


def signature(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def component(name):
    rt.require(name not in ('', '.', '..') and not any(ord(c) < 32 or c == '\\' for c in name), 'UNSAFE_FILENAME')
    name.encode('utf-8')


@contextmanager
def relative_open(root, relative, directory=False):
    parts = PurePosixPath(relative).parts
    rt.require(not PurePosixPath(relative).is_absolute() and '..' not in parts, 'RELATIVE_PATH_REQUIRED')
    fd = os.dup(root)
    try:
        for i, part in enumerate(parts):
            component(part)
            flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            if directory or i < len(parts)-1: flags |= os.O_DIRECTORY
            child = os.open(part, flags, dir_fd=fd); os.close(fd); fd = child
        yield fd
    finally:
        os.close(fd)


def private_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        rt.require(info.st_uid == os.getuid() and not info.st_mode & 0o077, 'PRIVATE_SOURCE_DIRECTORY_REQUIRED')
    except BaseException:
        os.close(fd); raise
    return fd


def metadata(root, name):
    with relative_open(root, name) as fd:
        info = os.fstat(fd)
        rt.require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_size <= 16*1024**2,
                   'RESULT_METADATA_FILE')
        with os.fdopen(os.dup(fd), 'rb') as reader: raw = reader.read(16*1024**2+1)
        rt.require(signature(os.fstat(fd)) == signature(info), 'SOURCE_CHANGED')
        return json.loads(raw, object_pairs_hook=rt.pairs), signature(info)


class Bundle:
    def __init__(self, archive, args):
        self.archive = archive; self.args = args; self.files = []; self.excluded = []; self.directories = []
        self.used = 0; self.visited = 0; self.warnings = []

    def exclude(self, source, path, reason):
        self.excluded.append({'source': source, 'path': path, 'reason': reason})
        if reason in ('oversized-evidence', 'symlink', 'special-file'):
            self.warnings.append(source+'/'+path+':'+reason)

    def add(self, root, source, relative):
        name = str(PurePosixPath(relative)); member = source+'/'+name
        with relative_open(root, name) as fd:
            before = os.fstat(fd)
            if not stat.S_ISREG(before.st_mode): self.exclude(source, name, 'special-file'); return
            rt.require(before.st_uid == os.getuid(), 'SOURCE_FILE_OWNER')
            if before.st_size > self.args.max_file_mib*1024**2:
                self.exclude(source, name, 'oversized-evidence'); return
            rt.require(len(self.files) < self.args.max_files and self.used+before.st_size <= self.args.max_total_mib*1024**2,
                       'BUNDLE_EVIDENCE_BUDGET_EXCEEDED')
            digest = hashlib.sha256()
            class Reader:
                def read(self, size):
                    data = reader.read(size); digest.update(data); return data
            info = tarfile.TarInfo(PREFIX+'/'+member); info.size = before.st_size; info.mode = 0o600
            with os.fdopen(os.dup(fd), 'rb') as reader: self.archive.addfile(info, Reader())
            rt.require(signature(os.fstat(fd)) == signature(before), 'SOURCE_CHANGED:'+member)
            self.used += before.st_size
            self.files.append({'source': source, 'path': name, 'member': member, 'size': before.st_size,
                               'sha256': digest.hexdigest(), 'signature': signature(before)})

    def walk(self, root, source, relative='', depth=0):
        rt.require(depth <= 32, 'DIRECTORY_DEPTH_LIMIT')
        with relative_open(root, relative, directory=True) as fd:
            before = os.fstat(fd)
            rt.require(before.st_uid == os.getuid(), 'SOURCE_DIRECTORY_OWNER')
            names = []
            with os.scandir(fd) as entries:
                for entry in entries:
                    self.visited += 1
                    rt.require(self.visited <= 100000, 'ENTRY_LIMIT')
                    component(entry.name); names.append(entry.name)
            for name in sorted(names):
                path = str(PurePosixPath(relative)/name)
                info = os.stat(name, dir_fd=fd, follow_symlinks=False)
                if stat.S_ISLNK(info.st_mode): self.exclude(source, path, 'symlink'); continue
                if stat.S_ISDIR(info.st_mode):
                    if source == 'retest' and RAW_TREE.fullmatch(path):
                        self.exclude(source, path, 'raw-tree; only selected checkpoint metadata retained')
                        self.recording_metadata(root, source, path)
                    else: self.walk(root, source, path, depth+1)
                elif stat.S_ISREG(info.st_mode):
                    if name != 'records.jsonl' and (Path(name).suffix.lower() in TEXT or name in ('COMPLETE', 'SHA256SUMS')): self.add(root, source, path)
                    else: self.exclude(source, path, 'raw-or-nontext; includes DDS databases, perf binaries and images')
                else: self.exclude(source, path, 'special-file')
            rt.require(signature(os.fstat(fd)) == signature(before), 'SOURCE_DIRECTORY_CHANGED')
            self.directories.append((source, relative, signature(before)))

    def recording_metadata(self, root, source, path):
        # Never enumerate payload files or V2's potentially millions of frames.
        with relative_open(root, path, directory=True) as fd:
            before = os.fstat(fd); jobs = []; count = 0
            with os.scandir(fd) as entries:
                for entry in entries:
                    count += 1
                    rt.require(count <= 1000, 'RECORDING_JOB_LIMIT'); component(entry.name)
                    if entry.is_symlink(): self.exclude(source, path+'/'+entry.name, 'symlink')
                    elif entry.is_dir(follow_symlinks=False): jobs.append(entry.name)
            for job in sorted(jobs):
                relative = path+'/'+job
                with relative_open(root, relative, directory=True) as directory:
                    stamp = signature(os.fstat(directory))
                    for name in RECORDING_METADATA:
                        try: info = os.stat(name, dir_fd=directory, follow_symlinks=False)
                        except FileNotFoundError: continue
                        if stat.S_ISLNK(info.st_mode): self.exclude(source, relative+'/'+name, 'symlink')
                        elif stat.S_ISREG(info.st_mode): self.add(root, source, relative+'/'+name)
                        else: self.exclude(source, relative+'/'+name, 'special-file')
                    self.directories.append((source, relative, stamp))
            rt.require(signature(os.fstat(fd)) == signature(before), 'SOURCE_DIRECTORY_CHANGED')
            self.directories.append((source, path, signature(before)))

    def generated(self, name, value):
        raw = (json.dumps(value, sort_keys=True, indent=2)+'\n').encode()
        info = tarfile.TarInfo(PREFIX+'/'+name); info.size = len(raw); info.mode = 0o600
        self.archive.addfile(info, io.BytesIO(raw))
        return hashlib.sha256(raw).hexdigest()+'  '+name+'\n'

    def verify_stable(self, sources):
        for row in self.files:
            with relative_open(sources[row['source']], row['path']) as fd:
                rt.require(signature(os.fstat(fd)) == row['signature'], 'SOURCE_CHANGED:'+row['member'])
        for source, path, expected in self.directories:
            with relative_open(sources[source], path, directory=True) as fd:
                rt.require(signature(os.fstat(fd)) == expected, 'SOURCE_DIRECTORY_CHANGED:'+path)


def pack(args):
    root = args.root.absolute(); output = args.output.absolute(); component(output.name); sidecar = Path(str(output)+'.sha256')
    rt.require(output.name.endswith('.tar.gz'), 'TAR_GZ_OUTPUT_REQUIRED')
    rt.require(not output.exists() and not output.is_symlink() and not sidecar.exists() and not sidecar.is_symlink(), 'FRESH_OUTPUT_REQUIRED')
    rt.require(not output.parent.resolve().is_relative_to(root.resolve()), 'OUTPUT_MUST_BE_OUTSIDE_SOURCE')
    rt.require(1 <= args.max_file_mib <= args.max_total_mib <= 16384 and 1 <= args.max_files <= 100000, 'BUNDLE_LIMITS')
    sources = {}; temporary = None; published = False; sidecar_inode = None
    try:
        sources['retest'] = private_directory(root)
        result, result_signature = metadata(sources['retest'], 'result.json')
        rt.require(result.get('uid') == os.getuid() and result.get('gid') == os.getgid(), 'RUN_UID_GID_MISMATCH')
        token = result.get('run_token', '')
        rt.require(re.fullmatch('[a-f0-9]{32}', token) is not None, 'RUN_TOKEN_REQUIRED')
        sdk = SDK_PARENT/('gdp-retest-'+token)
        sdk_warning = None
        try: sources['sdk-recovery'] = private_directory(sdk)
        except FileNotFoundError: sdk_warning = 'recovery SDK tmpfs directory absent; may not have started or was lost after reboot'
        handle, temporary = tempfile.mkstemp(prefix='.gdp-evidence-', suffix='.tmp', dir=output.parent)
        with os.fdopen(handle, 'wb') as dest:
            with gzip.GzipFile(filename='', fileobj=dest, mode='wb', mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode='w|', format=tarfile.PAX_FORMAT) as archive:
                    bundle = Bundle(archive, args)
                    if sdk_warning: bundle.warnings.append(sdk_warning)
                    if result.get('retention_policy')=='verified-window':bundle.warnings.append('verified-window: accepted pixels/DDS databases intentionally released after audits; raw replay unavailable; consult per-group payload-retention receipts')
                    if not result.get('finished_unix_ns'): bundle.warnings.append('run has no finished marker; rollback/stop before packaging')
                    for source, fd in sources.items(): bundle.walk(fd, source)
                    rt.require(any(r['source'] == 'retest' and r['path'] == 'result.json' and r['signature'] == result_signature for r in bundle.files), 'RESULT_METADATA_CHANGED_OR_EXCLUDED')
                    bundle.verify_stable(sources)
                    sums = ''.join(r['sha256']+'  '+r['member']+'\n' for r in bundle.files)
                    inventory = [{k: v for k, v in row.items() if k != 'signature'} for row in bundle.files]
                    sums += bundle.generated('SOURCE_MANIFEST.json', inventory)
                    sums += bundle.generated('EXCLUDED.json', bundle.excluded)
                    sums += bundle.generated('PACKAGE.json', {'version': 1, 'kind': 'gdp-retest-evidence', 'created_unix_ns': time.time_ns(),
                        'source_root': str(root), 'run_token': token, 'run_ok': result.get('ok'),
                        'image_id': result.get('image_id'), 'runtime_sha256': result.get('runtime_sha256'),
                        'retention_policy':result.get('retention_policy','all'),
                        'source_files': len(bundle.files), 'source_bytes': bundle.used, 'warnings': bundle.warnings,
                        'limits': {'max_file_mib': args.max_file_mib, 'max_total_mib': args.max_total_mib, 'max_files': args.max_files},
                        'scope': 'evidence snapshot, not hardware acceptance; raw recordings and wire databases excluded; SHA256SUMS excludes itself; outer archive hash is a sidecar'})
                    info = tarfile.TarInfo(PREFIX+'/SHA256SUMS'); raw = sums.encode(); info.size = len(raw); info.mode = 0o600
                    archive.addfile(info, io.BytesIO(raw))
            dest.flush(); os.fsync(dest.fileno())
        digest = rt.sha(temporary)
        os.link(temporary, output); published = True
        sidecar_fd = os.open(sidecar, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        sidecar_inode = os.fstat(sidecar_fd).st_ino
        with os.fdopen(sidecar_fd, 'w') as dest:
            dest.write(digest+'  '+output.name+'\n'); dest.flush(); os.fsync(dest.fileno())
        directory = os.open(output.parent, os.O_RDONLY | os.O_DIRECTORY)
        try: os.fsync(directory)
        finally: os.close(directory)
        return {'ok': True, 'archive': str(output), 'sha256': digest, 'sha256_file': str(sidecar),
                'source_files': len(bundle.files), 'source_bytes': bundle.used, 'warnings': bundle.warnings}
    except BaseException:
        if published:
            # Remove only the archive inode created by this invocation.
            if temporary and output.lstat().st_ino == os.stat(temporary).st_ino: output.unlink()
        if sidecar_inode is not None and sidecar.lstat().st_ino == sidecar_inode: sidecar.unlink()
        raise
    finally:
        for fd in sources.values(): os.close(fd)
        if temporary: Path(temporary).unlink(missing_ok=True)


def main():
    os.umask(0o077); p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root', type=Path, required=True); p.add_argument('--output', type=Path, required=True)
    p.add_argument('--max-file-mib', type=int, default=256); p.add_argument('--max-total-mib', type=int, default=2048)
    p.add_argument('--max-files', type=int, default=20000); a = p.parse_args()
    try: print(json.dumps(pack(a), indent=2)); return 0
    except Exception as error: print(json.dumps({'ok': False, 'error': str(error)})); return 1


if __name__ == '__main__': raise SystemExit(main())
