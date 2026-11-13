"""Private SDK trace ABI: real C++11 shared library, no camera or ROS."""
import ctypes
import hashlib
import fnmatch
import re
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
HERE=ROOT/'experiments/sdk-2.10.6-diagnostics'

class UvcObservationTests(unittest.TestCase):
    def test_patch_identity_and_header_match_the_compiled_contract(self):
        spec=importlib.util.spec_from_file_location('uvc_runtime',HERE/'experiment_runtime.py')
        runtime=importlib.util.module_from_spec(spec);spec.loader.exec_module(runtime)
        patch=(HERE/'uvc-observations.patch').read_text()
        self.assertEqual(hashlib.sha256(patch.encode()).hexdigest(),runtime.UVC_PATCH)
        tail=patch.split('+++ b/src/shared/utils/UvcPropertyObservations.hpp\n',1)[1]
        header=''.join(line[1:] for line in tail.splitlines(True) if line.startswith('+') and not line.startswith('+++'))
        self.assertEqual(header,(HERE/'uvc_observations.hpp').read_text())
        self.assertIn('gdpResult = xioctl(fd, VIDIOC_G_CTRL',patch)
        self.assertIn('gdpResult = uvc_get_ctrl',patch)
        self.assertIn('gdpResult = xioctl(fd, VIDIOC_QUERYCTRL',patch)
        self.assertNotIn('setPu(',patch)
        self.assertIn('gdp_uvc::record_xu(2, 4',patch)
        self.assertIn('gdp_uvc::record_xu(1, 5',patch)

    def test_diagnostic_copy_sources_survive_the_actual_dockerignore(self):
        rules=(HERE/'capture.Dockerfile.dockerignore').read_text().splitlines()
        dockerfile=(HERE/'capture.Dockerfile').read_text()
        sources=re.findall(r'^COPY (experiments/\S+) ',dockerfile,re.M)
        self.assertIn('experiments/sdk-2.10.6-diagnostics/uvc-observations.patch',sources)
        for source in sources:
            self.assertTrue((ROOT/source).is_file())
            for path in [source]+[str(p) for p in Path(source).parents if str(p)!='.']:
                allowed=True
                for rule in rules:
                    if rule and not rule.startswith('#') and fnmatch.fnmatch(path,rule.lstrip('!').rstrip('/')):
                        allowed=rule.startswith('!')
                self.assertTrue(allowed,path+' excluded from build context')

    def test_real_cpp_tls_cross_translation_units_capacity_errno_and_backend_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            (root/'api.cpp').write_text('''#include "uvc_observations.hpp"
#include <thread>
#include <cassert>
extern "C" __attribute__((visibility("default"))) void begin(){gdp_uvc::begin();}
extern "C" __attribute__((visibility("default"))) unsigned snapshot(gdp_uvc::Event *p,unsigned n){return gdp_uvc::snapshot(p,n);}
extern "C" void emit(int,int);
extern "C" __attribute__((visibility("default"))) void isolation(){
 std::thread other([]{gdp_uvc::begin();emit(2,-7);gdp_uvc::Event e{};assert(gdp_uvc::snapshot(&e,1)==1);assert(e.backend==2&&e.result==-7);});other.join();
}
''')
            (root/'backend.cpp').write_text('''#include "uvc_observations.hpp"
#include <cassert>
extern "C" __attribute__((visibility("default"))) void emit(int backend,int result){
 const int before=errno;const auto stamp=gdp_uvc::stamp();
 gdp_uvc::record({static_cast<unsigned>(backend),1,2002,1,result,result<0?5:0,256,result>=0?1U:0U,1,255,2,101,stamp,0});
 assert(errno==before);
}
extern "C" __attribute__((visibility("default"))) void emitxu(int backend,int result,int valid){
 uint8_t bytes[4]={0x10,0x27,0,0};
 gdp_uvc::record_xu(backend,backend==2?4:5,0x0310,129,result, result<0?5:0,
     valid?bytes:nullptr,valid?4:1000000,valid!=0,gdp_uvc::stamp());
}
''')
            library=root/'trace.so'
            result=subprocess.run(['c++','-std=c++11','-Wall','-Wextra','-Werror','-fvisibility=hidden',
                                   '-dynamiclib' if sys.platform=='darwin' else '-shared','-fPIC','-pthread',
                                   '-I',str(HERE),str(root/'api.cpp'),str(root/'backend.cpp'),'-o',str(library)],capture_output=True,text=True)
            self.assertEqual(result.returncode,0,result.stderr)
            class Event(ctypes.Structure):
                _fields_=[(name,kind) for name,kind in [
                    ('backend',ctypes.c_uint32),('operation',ctypes.c_uint32),('selector',ctypes.c_uint32),('request',ctypes.c_uint32),
                    ('result',ctypes.c_int32),('error_number',ctypes.c_int32),('value',ctypes.c_int32),('value_valid',ctypes.c_uint32),
                    ('minimum',ctypes.c_int32),('maximum',ctypes.c_int32),('step',ctypes.c_int32),('default_value',ctypes.c_int32),
                    ('started_ns',ctypes.c_uint64),('finished_ns',ctypes.c_uint64)]]
            self.assertEqual(ctypes.sizeof(Event),64)
            lib=ctypes.CDLL(str(library),use_errno=True);lib.snapshot.argtypes=[ctypes.POINTER(Event),ctypes.c_uint32];lib.snapshot.restype=ctypes.c_uint32
            out=(Event*9)();lib.begin();ctypes.set_errno(73);lib.emit(1,0);self.assertEqual(ctypes.get_errno(),73)
            lib.isolation();self.assertEqual(lib.snapshot(out,8),1);self.assertEqual(out[0].backend,1);self.assertEqual(out[0].value,256)
            self.assertGreaterEqual(out[0].finished_ns,out[0].started_ns)
            lib.begin();self.assertEqual(lib.snapshot(out,8),0)
            lib.begin()
            for _ in range(20):lib.emit(1,0)
            out[1].value=77
            self.assertEqual(lib.snapshot(out,1),9);self.assertEqual(out[1].value,77)
            for backend in (1,2):
                lib.begin();ctypes.set_errno(73);lib.emitxu(backend,64 if backend==2 else 0,1)
                self.assertEqual(ctypes.get_errno(),73);self.assertEqual(lib.snapshot(out,8),1)
                self.assertEqual(out[0].backend,backend);self.assertEqual(out[0].operation,4 if backend==2 else 5)
                self.assertEqual(out[0].value,10000);self.assertEqual(out[0].value_valid,1)
                lib.begin();lib.emitxu(backend,-7,0);self.assertEqual(lib.snapshot(out,8),1)
                self.assertEqual(out[0].value,0);self.assertEqual(out[0].value_valid,0)
                self.assertEqual(out[0].result,-7)
            lib.begin();lib.emit(2,-7);self.assertEqual(lib.snapshot(out,8),1)
            self.assertEqual(out[0].value_valid,0);self.assertEqual(out[0].error_number,5);self.assertEqual(out[0].result,-7)

if __name__=='__main__':unittest.main()
