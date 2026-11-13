"""Fault selector scans bounded status files, including chunk boundaries."""
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest
ROOT=Path(__file__).resolve().parents[1]

@unittest.skipUnless(sys.platform=='linux','Linux preload fault fixture')
class ContentFaultTests(unittest.TestCase):
    def test_real_fsync_selector_finds_late_and_split_tokens_without_unbounded_reads(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);lib=root/'fault.so'
            subprocess.run(['cc','-shared','-fPIC','-std=c11','-Wall','-Wextra','-Werror',str(ROOT/'tests/support/io_faults.c'),'-ldl','-o',str(lib)],check=True,capture_output=True)
            for offset,expected in ((70000,True),(8192-5,True),(4*1024**2+1,False)):
                with self.subTest(offset=offset):
                    path=root/(str(offset)+'.tmp');marker=root/(str(offset)+'.marker')
                    path.write_bytes(b'x'*offset+b'"state":"draining"')
                    env=dict(os.environ,LD_PRELOAD=str(lib),GDP_IO_FAIL_OP='fsync',GDP_IO_FAIL_SUFFIX='.tmp',GDP_IO_REQUIRE_CONTENT='"state":"draining"',GDP_IO_FAULT_MARKER=str(marker))
                    result=subprocess.run([sys.executable,'-c',"import os,sys;f=open(sys.argv[1],'r+b');os.fsync(f.fileno())",str(path)],env=env,capture_output=True)
                    self.assertEqual(marker.exists(),expected)
                    self.assertEqual(result.returncode!=0,expected,result.stderr.decode())
if __name__=='__main__':unittest.main()
