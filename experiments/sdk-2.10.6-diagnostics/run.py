#!/usr/bin/env python3
"""Reproducible SDK-only synthetic diagnostics tests and alternating overhead runs.

No device enumeration, USB, ROS or production recording. Results cannot certify
hardware throughput, absence of source loss, or native recording durability.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import statistics
import subprocess
import time

BASE = '575347ca742d219ef61b1e214e97043f17f48d23'


def require(ok, message):
    if not ok:
        raise ValueError(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def environment(root, enabled):
    env = os.environ.copy()
    env.pop('OB_GDP_DIAGNOSTICS', None)
    env.pop('OB_GDP_DIAGNOSTICS_DIR', None)
    if enabled:
        env.update(OB_GDP_DIAGNOSTICS='1', OB_GDP_DIAGNOSTICS_DIR=str(root))
    return env


def snapshots(root):
    files = list(root.glob('sdk-diag-*.json'))
    require(len(files) <= 16, 'diagnostic instance bound')
    rows = []
    for path in files:
        require(path.stat().st_size <= 65536, 'snapshot size bound')
        require(path.stat().st_mode & 0o777 == 0o600, 'private snapshot')
        row = json.loads(path.read_text())
        require(row['sdk_base'] == BASE and len(row['events']) <= 64, 'schema and event bound')
        rows.append(row)
    return rows


def run_probe(binary, root, scenario, loops, pace, enabled):
    root.mkdir(mode=0o700)
    result = subprocess.run([str(binary), scenario, str(loops), str(pace)], env=environment(root,enabled),
                            capture_output=True,text=True,timeout=pace*loops/1e6+90)
    (root/'stderr.log').write_text(result.stderr)
    require(result.returncode == 0, 'SDK probe failed: '+result.stderr[-2000:])
    values = [json.loads(v) for v in result.stdout.splitlines() if v.startswith('{')]
    require(len(values) == 1, 'one driver result required')
    row = values[0]
    docs = snapshots(root)
    require(len(docs) == (1 if enabled else 0), 'enabled/disabled sink creation')
    if enabled:
        require(docs[0]['sink_errors'] == 0, 'diagnostic sink errors')
        (root/'snapshot-summary.json').write_text(json.dumps(docs[0],indent=2)+'\n')
    return row, docs


def signature(row):
    return tuple(row[k] for k in ('output_sets','output_frames','behavior_digest'))


def compare_abi(before, after):
    def exports(path):
        text = subprocess.check_output(['nm','-D','--defined-only',str(path)],text=True)
        return sorted(line.split()[-1] for line in text.splitlines() if line.split() and line.split()[-1].startswith('ob_'))
    a,b = exports(before),exports(after)
    require(a and a == b,'public C exports changed')
    versions = []
    for libpath in (before,after):
        lib=ctypes.CDLL(str(libpath))
        versions.append(lib.ob_get_version())
    require(versions[0] == versions[1] == 21006,'SDK version changed')
    return {'public_c_exports':len(a),'public_c_exports_identical':True,'versions':versions,
            'libraries_sha256':{'before':digest(before),'after':digest(after)}}


def unit_tests(binary, root):
    result = []
    for mode in ('disabled','bounds','concurrent','limit'):
        out=root/mode;out.mkdir(mode=0o700)
        run=subprocess.run([str(binary),mode],env=environment(out,mode!='disabled'),capture_output=True,text=True,timeout=30)
        require(run.returncode==0,'diagnostics helper '+mode+': '+run.stderr)
        docs=snapshots(out)
        if mode=='disabled':require(not docs,'disabled emitted files')
        elif mode=='bounds':
            require(len(docs)==1,'one instance')
            v=docs[0]
            require(v['counters']['input_frames']==200 and v['counters']['policy_dropped_frames']==200,'exact counters')
            require(v['events_total']==400 and v['events_overwritten']==336 and len(v['events'])==64,'bounded overwrite accounting')
            lock=v['locks'][0]
            require(lock['wait']['max_ns']>=1_000_000 and lock['hold']['max_ns']>=1_000_000,'separate wait and hold clocks')
        elif mode=='concurrent':
            v=docs[0]
            require(v['counters']['input_frames']==200000,'concurrent counters')
            require(v['events_total']+v['event_skipped_contention']==200000,'event sampling contention accounting')
        else:require(len(docs)==16,'global live-instance limit')
        result.append(mode)
    for bad in ('world-readable','symlink','missing','relative'):
        out=root/bad;out.mkdir(mode=0o700)
        destination=out
        if bad=='world-readable':out.chmod(0o755)
        elif bad=='symlink':
            destination=root/'link';destination.symlink_to(out,target_is_directory=True)
        elif bad=='missing':destination=out/'absent'
        else:destination=Path('relative-diagnostic-directory')
        run=subprocess.run([str(binary),'disabled'],env=environment(destination,True),capture_output=True,text=True,timeout=30)
        require(run.returncode==0 and not snapshots(out),'unsafe sink must disable diagnostics without exception')
        result.append(bad)
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--before',type=Path,required=True);p.add_argument('--after',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True);p.add_argument('--repeats',type=int,default=5)
    p.add_argument('--paced-seconds',type=int,default=8);p.add_argument('--pipelines',type=int,default=3)
    p.add_argument('--tests-only',action='store_true')
    p.add_argument('--load',choices=('all','unpaced','paced'),default='all')
    p.add_argument('--before-source',type=Path);p.add_argument('--after-source',type=Path)
    a=p.parse_args();os.umask(0o077)
    require(1<=a.repeats<=10 and 1<=a.paced_seconds<=60 and 1<=a.pipelines<=3,'bounded benchmark inputs')
    a.output.mkdir(mode=0o700,parents=True,exist_ok=False)
    before=a.before/'bin/gdp_sdk_probe';after=a.after/'bin/gdp_sdk_probe'
    library=lambda root:next((root/'lib').glob('libOrbbecSDK.so.2.10.6'))
    result={'sdk_base':BASE,'compiler':subprocess.check_output(['c++','--version'],text=True).splitlines()[0],
            'abi':compare_abi(library(a.before),library(a.after)), 'probe_sha256':{'before':digest(before),'after':digest(after)},
            'unit_tests':unit_tests(a.after/'bin/gdp_diagnostic_unit',a.output), 'behavior_tests':[], 'rows':[]}
    for scenario in ('paired','skew','partial','starved','overflow','pipeline-lock','aggregator-lock'):
        expected=None
        for variant,binary,enabled in (('before',before,False),('off',after,False),('on',after,True)):
            row,docs=run_probe(binary,a.output/(scenario+'-'+variant),scenario,120,0,enabled)
            if expected is None:expected=signature(row)
            require(signature(row)==expected,'diagnostics changed SDK output: '+scenario)
            if enabled:
                v=docs[0];c=v['counters']
                require(c['emitted_sets']==row['output_sets']+c['output_queue_dropped_sets'],'output queue accounting')
                require(c['input_frames']==sum(s['input'] for s in v['streams']),'stream accounting')
                if scenario=='skew':require(c['candidate_outside_window']>0 and c['policy_dropped_frames']>0,'pair exclusion events')
                if scenario=='starved':require(c['aggregate_queue_limit_hits']>0 and c['policy_dropped_frames']>0,'aggregate limits are distinct from policy drops')
                if scenario=='partial':require(c['partial_sets']>0 and c['policy_dropped_frames']==0,'partial sets emitted, not discarded')
                if scenario=='overflow':require(c['output_queue_dropped_sets']>0,'actual dequeue overflow instrumentation')
                if scenario.endswith('-lock'):
                    site=0 if scenario=='pipeline-lock' else 1
                    require(v['locks'][site]['wait']['max_ns']>=1_000_000,'real SDK lock contention')
        result['behavior_tests'].append(scenario)
    # A removed sink must not turn a healthy pipeline into a capture failure.
    sink=a.output/'removed-sink';sink.mkdir(mode=0o700)
    child=subprocess.Popen([str(after),'paired','60','33333'],env=environment(sink,True),stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:
        deadline=time.monotonic()+5
        while not list(sink.glob('*.json')):
            require(child.poll() is None and time.monotonic()<deadline,'sink did not initialize')
            time.sleep(.01)
        shutil.rmtree(sink)
        stdout,stderr=child.communicate(timeout=15)
        require(child.returncode==0 and json.loads([v for v in stdout.splitlines() if v.startswith('{')][-1])['output_sets']==60,'sink failure changed capture')
    finally:
        if child.poll() is None:child.kill();child.communicate()
    result['removed_sink_does_not_fail_capture']=True
    if not a.tests_only:
        for scenario,loops,pace in (('unpaced',100000,0),('paced',a.paced_seconds*30,33333)):
            if a.load!='all' and a.load!=scenario:continue
            for repeat in range(a.repeats):
                variants=[('before',before,False),('off',after,False),('on',after,True)]
                if repeat%2:variants.reverse()
                for variant,binary,enabled in variants:
                    def one(camera):
                        row,_=run_probe(binary,a.output/(scenario+'-'+str(repeat)+'-'+variant+'-'+str(camera)),'paired',loops,pace,enabled)
                        row.update(load=scenario,repeat=repeat,variant=variant,camera=camera)
                        require(row['output_sets']==loops and row['output_frames']==loops*2,'benchmark output count')
                        return row
                    with ThreadPoolExecutor(max_workers=a.pipelines) as executor:
                        result['rows'].extend(executor.map(one,range(a.pipelines)))
        summaries=[]
        for load in ('unpaced','paced'):
            if a.load!='all' and a.load!=load:continue
            for variant in ('before','off','on'):
                rows=[r for r in result['rows'] if r['load']==load and r['variant']==variant]
                summaries.append({'load':load,'variant':variant,**{k:statistics.median(r[k] for r in rows) for k in ('cpu_seconds','cpu_cores','wall_seconds','peak_rss_kib','callback_p99_ns')}})
        result['medians_per_process']=summaries
    if a.before_source and a.after_source:
        def headers(root):
            return {str(p.relative_to(root/'include')):digest(p) for p in sorted((root/'include').rglob('*')) if p.is_file()}
        before_headers,after_headers=headers(a.before_source),headers(a.after_source)
        require(before_headers and before_headers==after_headers,'installed public headers changed')
        result['abi']['public_header_files_identical']=len(before_headers)
        result['abi']['public_headers_manifest_sha256']=hashlib.sha256(json.dumps(before_headers,sort_keys=True).encode()).hexdigest()
    result.update(ok=True,driver_peak_rss_kib=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                  scope='Real SDK C++ Pipeline/FrameAggregator with in-memory device, synthetic 2x2 frames; three separate paced processes or throughput saturation. No USB/IMU/DDS/recording. Driver calls are timed; CPU includes construction/destruction/report threads. Not a camera performance or durability certification.')
    (a.output/'summary.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({k:v for k,v in result.items() if k in ('ok','medians_per_process','behavior_tests','abi')}))

if __name__=='__main__':main()
