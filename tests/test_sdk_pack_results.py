"""Evidence packaging tests: bounded streaming, integrity and no raw payloads."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0, str(HERE))
import pack_results as pack


def write(root, name, value):
    p = root/name; p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(value)); return p


def fixture(root):
    source = root/'run'; source.mkdir(mode=0o700)
    write(source, 'result.json', {'uid': os.getuid(), 'gid': os.getgid(), 'run_token': 'a'*32,
          'ok': False, 'finished_unix_ns': 1, 'image_id': 'sha256:image', 'runtime_sha256': 'runtime'})
    for path in ('manifest.json', 'frozen/a/capture.json', 'ab/on-01/capture/a/config/runtime.json',
                 'ab/off-01/capture/checkpoint-samples/samples.jsonl', 'crash/b/recovered-audit.jsonl',
                 'crash-injection.json', 'controller/build.log', 'crash/b/crash.json',
                 'ab/on-01/lifecycle/samples.jsonl','ab/on-01/lifecycle/summary.json',
                 'ab/on-01/writer-tails/samples.jsonl','ab/on-01/writer-tails/summary.json',
                 'ab/on-01/writer-tails/manifest.json',
                 'crash-lifecycle/samples.jsonl','crash-lifecycle/summary.json'):
        write(source, path, {'evidence': path})
    write(source, 'frozen/recordings/capture.json', {'camera_named_recordings': True})
    write(source, 'frozen/raw/capture.json', {'camera_named_raw': True})
    (source/'ab/on-01/writer-tails/SHA256SUMS').write_text('original writer checksum manifest\n')
    write(source, 'crash/b/recordings/capture.partial/checkpoint.json', {'record_count': 32})
    write(source, 'crash/b/recordings/capture.partial/snapshot.json', {'snapshot': 'hash'})
    for path in ('crash/b/recordings/capture.partial/payload-0001.bin',
                 'crash/b/recordings/capture.partial/records.jsonl', 'ab/on-01/capture/wire.sqlite3',
                 'ab/on-01/capture/wire.sqlite3-wal', 'ab/on-01/capture/checkpoint-samples/perf.data',
                 'ab/on-01/writer-tails/perf.data'):
        p = source/path; p.parent.mkdir(parents=True, exist_ok=True); p.write_bytes(b'RAW-MUST-NOT-APPEAR')
    a = argparse.Namespace(root=source, output=root/'evidence.tar.gz', max_file_mib=1, max_total_mib=4, max_files=100)
    shm = root/'shm'; shm.mkdir(); sdk = shm/('gdp-retest-'+'a'*32); sdk.mkdir(mode=0o700)
    write(sdk, 'b/sdk-snapshot.json', {'sink_errors': 0})
    return a, shm


def archive_files(path):
    with tarfile.open(path, 'r:gz') as archive:
        files = {}
        for info in archive:
            assert info.isfile() and info.name.startswith(pack.PREFIX+'/')
            assert '..' not in Path(info.name).parts
            files[info.name[len(pack.PREFIX)+1:]] = archive.extractfile(info).read()
        return files


class PackTests(unittest.TestCase):
    def test_bundle_contains_config_image_metrics_recovery_and_logs_with_valid_hashes(self):
        with tempfile.TemporaryDirectory() as d:
            a, shm = fixture(Path(d)); before = (a.root/'result.json').read_bytes()
            with patch.object(pack, 'SDK_PARENT', shm): result = pack.pack(a)
            files = archive_files(a.output)
            for name in ('retest/frozen/a/capture.json', 'retest/frozen/recordings/capture.json', 'retest/frozen/raw/capture.json', 'retest/manifest.json',
                         'retest/ab/on-01/capture/a/config/runtime.json', 'retest/crash/b/recovered-audit.jsonl',
                         'retest/controller/build.log', 'sdk-recovery/b/sdk-snapshot.json',
                         'retest/ab/on-01/lifecycle/samples.jsonl','retest/ab/on-01/lifecycle/summary.json',
                         'retest/ab/on-01/writer-tails/samples.jsonl','retest/ab/on-01/writer-tails/summary.json',
                         'retest/ab/on-01/writer-tails/manifest.json',
                         'retest/ab/on-01/writer-tails/SHA256SUMS',
                         'retest/crash-lifecycle/samples.jsonl','retest/crash-lifecycle/summary.json',
                         'retest/crash/b/recordings/capture.partial/checkpoint.json'):
                self.assertIn(name, files)
            self.assertFalse(any(b'RAW-MUST-NOT-APPEAR' in raw for raw in files.values()))
            self.assertEqual(files['retest/ab/on-01/writer-tails/SHA256SUMS'],
                             b'original writer checksum manifest\n')
            for line in files['SHA256SUMS'].decode().splitlines():
                digest, name = line.split('  ', 1)
                self.assertEqual(hashlib.sha256(files[name]).hexdigest(), digest)
            self.assertEqual(set(files)-{'SHA256SUMS'}, {line.split('  ',1)[1] for line in files['SHA256SUMS'].decode().splitlines()})
            self.assertEqual(result['sha256'], hashlib.sha256(a.output.read_bytes()).hexdigest())
            self.assertEqual(a.output.stat().st_mode & 0o777, 0o600)
            self.assertEqual((a.root/'result.json').read_bytes(), before)
            self.assertFalse(json.loads(files['PACKAGE.json'])['run_ok'])
            exclusions = json.loads(files['EXCLUDED.json'])
            self.assertTrue(any('recordings' in r['path'] for r in exclusions))
            self.assertEqual(result['warnings'], [])

    def test_symlinks_and_fifo_never_followed_even_for_checkpoint_metadata(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a, shm = fixture(root)
            outside = root/'secret.json'; outside.write_text('DO-NOT-COLLECT')
            (a.root/'alias.json').symlink_to(outside)
            (a.root/'alias-dir').symlink_to(root, target_is_directory=True)
            cp = a.root/'crash/b/recordings/capture.partial/checkpoint.json'; cp.unlink(); cp.symlink_to(outside)
            os.mkfifo(a.root/'blocked.log')
            with patch.object(pack, 'SDK_PARENT', shm): result = pack.pack(a)
            self.assertFalse(any(b'DO-NOT-COLLECT' in v for v in archive_files(a.output).values()))
            self.assertTrue(any('symlink' in warning for warning in result['warnings']))

    def test_missing_tmpfs_and_oversized_evidence_are_explicit_not_silent(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a, _ = fixture(root)
            with (a.root/'large.log').open('wb') as f: f.truncate(2*1024**2)
            with patch.object(pack, 'SDK_PARENT', root/'missing'): result = pack.pack(a)
            self.assertTrue(any('tmpfs' in warning for warning in result['warnings']))
            self.assertTrue(any('oversized-evidence' in warning for warning in result['warnings']))
            self.assertNotIn('retest/large.log', archive_files(a.output))

    def test_output_inside_source_existing_output_or_sidecar_is_rejected(self):
        for fault in ('inside', 'archive', 'sidecar'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as d:
                a, shm = fixture(Path(d))
                if fault == 'inside': a.output = a.root/'new.tar.gz'
                else: Path(str(a.output)+('.sha256' if fault == 'sidecar' else '')).write_text('existing')
                with patch.object(pack, 'SDK_PARENT', shm), self.assertRaises(ValueError): pack.pack(a)
                if fault != 'inside': self.assertEqual(Path(str(a.output)+('.sha256' if fault == 'sidecar' else '')).read_text(), 'existing')

    def test_budget_failure_keeps_no_partial_archive(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a, shm = fixture(root); a.max_files = 1
            with patch.object(pack, 'SDK_PARENT', shm), self.assertRaisesRegex(ValueError, 'BUDGET'): pack.pack(a)
            self.assertFalse(a.output.exists()); self.assertFalse(list(root.glob('.gdp-evidence-*')))

    def test_mutation_between_reads_rejects_snapshot_and_removes_temporary_archive(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a, shm = fixture(root); original = pack.Bundle.add
            def changing(bundle, fd, source, relative):
                original(bundle, fd, source, relative)
                if source == 'retest' and relative == 'result.json':
                    (a.root/'result.json').write_text('{}')
            with patch.object(pack, 'SDK_PARENT', shm), patch.object(pack.Bundle, 'add', changing), self.assertRaisesRegex(ValueError, 'SOURCE_CHANGED'):
                pack.pack(a)
            self.assertFalse(a.output.exists()); self.assertFalse(list(root.glob('.gdp-evidence-*')))

    def test_wrong_owner_metadata_public_root_or_symlink_root_rejected(self):
        for fault in ('uid', 'public', 'symlink'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as d:
                root = Path(d); a, shm = fixture(root)
                if fault == 'uid': write(a.root, 'result.json', {'uid': os.getuid()+1, 'gid': os.getgid(), 'run_token': 'a'*32})
                elif fault == 'public': a.root.chmod(0o755)
                else:
                    link = root/'alias'; link.symlink_to(a.root, target_is_directory=True); a.root = link
                with patch.object(pack, 'SDK_PARENT', shm), self.assertRaises((ValueError, OSError)): pack.pack(a)
                self.assertFalse(a.output.exists())

    def test_real_cli_emits_archive_digest_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as d:
            a, shm = fixture(Path(d))
            code = """import sys
from pathlib import Path
sys.path.insert(0, sys.argv.pop(1))
import pack_results as p
p.SDK_PARENT = Path(sys.argv.pop(1))
raise SystemExit(p.main())
"""
            argv = [sys.executable, '-c', code, str(HERE), str(shm), '--root', str(a.root), '--output', str(a.output)]
            child = subprocess.run(argv, capture_output=True, text=True, timeout=15)
            self.assertEqual(child.returncode, 0, child.stderr+child.stdout)
            report = json.loads(child.stdout)
            self.assertEqual(report['sha256'], hashlib.sha256(a.output.read_bytes()).hexdigest())
            before = a.output.read_bytes()
            second = subprocess.run(argv, capture_output=True, text=True, timeout=15)
            self.assertEqual(second.returncode, 1)
            self.assertIn('FRESH_OUTPUT_REQUIRED', second.stdout)
            self.assertEqual(a.output.read_bytes(), before)

    def test_publish_collision_preserves_existing_archive(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); a, shm = fixture(root); original = pack.os.link
            def collision(source, target):
                Path(target).write_text('other writer'); original(source, target)
            with patch.object(pack, 'SDK_PARENT', shm), patch.object(pack.os, 'link', collision), self.assertRaises(FileExistsError): pack.pack(a)
            self.assertEqual(a.output.read_text(), 'other writer')
            self.assertFalse(list(root.glob('.gdp-evidence-*')))


if __name__ == '__main__': unittest.main()
