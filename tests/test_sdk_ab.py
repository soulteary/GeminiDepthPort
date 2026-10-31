"""Synthetic measurement arithmetic and batch orchestration. No camera access."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
HERE=ROOT/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
import ab_metrics as metrics
import run_ab as batch
import hardware as hw
import run_hardware as one
import experiment_runtime as rt


def emit(path,values):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(''.join(json.dumps(v)+'\n' for v in values))


def fixture(root,mode='on'):
    cameras=[{'name':n} for n in 'abc'];source={}
    image='sha256:'+'a'*64;digest='b'*64
    result={'ok':True,'hardware_acceptance_verified':True,'cleanup_errors':[],
            'image_id':image,'runtime_sha256':digest,'uid':os.getuid(),'gid':os.getgid(),
            'command_timings_ns':{'audit':1000},'source_gap_free':False,'source_sequence_gaps':{'a':{'depth':1}}}
    one.atomic(root/'result.json',result)
    (root/'capture').mkdir();one.atomic(root/'capture/run.json',{'cameras':cameras,'mode':mode,'image_id':image,'runtime_sha256':digest})
    disk=[]
    for n in 'abc':
        cp=root/'capture'/n/'config';cp.mkdir(parents=True)
        one.atomic(cp/'capture.json',{'service':{'generation':root.name},'serial':n})
        one.atomic(cp/'health.json',{'name':n})
        source[n]='c'*64
        one.atomic(cp/'experiment.json',{'capture_sha256':rt.sha(cp/'capture.json'),'health_sha256':rt.sha(cp/'health.json'),
                                       'source_config_sha256':source[n],'diagnostics':mode=='on'})
        disk.append({'name':n,'capture':{'raw_recording':{'state':'complete','disk_stages':{'failed_calls':0,
                     'append':{'samples':20,'p99_ns':{'lower_bound_ns':8,'upper_bound_ns':9}},
                     'verification':{'samples':2,'total_ns':'100'},'checkpoint_observation':{'stages':{}}}}}})
    one.atomic(root/'final-status.json',{'cameras':disk})
    samples=[]
    for j,stamp in enumerate((10,12,15)):
        targets=[]
        for i in range(3):
            targets.append({'index':i,'epoch':0,'process':{'host_pid':i+10,'start_ticks':99,
                            'memory':{'VmRSS':100+j*10,'VmHWM':200+j*10}},
                            'cgroup':{'memory.peak':1000},'rates':None if j==0 else {'cpu_cores':1 if j==1 else 2}})
        samples.append({'kind':'sample','monotonic_ns':stamp*10**9,'targets':targets})
    emit(root/'capture/checkpoint-samples/samples.jsonl',samples)
    one.atomic(root/'capture/checkpoint-samples/summary.json',{'observer_cpu_seconds':1})
    one.atomic(root/'capture/wire.sqlite3.summary.json',{'observer_cpu_seconds':2})
    sdk=[]
    def hist(count):return {'count':count,'total_ns':count*4,'log2_ns_buckets':[0,0,count]+[0]*61,'max_ns':4}
    for n in 'abc':
        for j in (1,2):
            sdk.append({'source':n,'file':'sdk-diag-1.json','snapshot':{'sdk_base':rt.BASE,'pid':10,'instance':1,
                       'sink_errors':0,'snapshot_monotonic_ns':j*5*10**9,'counters':{'input_frames':j*20,'candidate_outside_window':j*2},
                       'locks':[{'site':'pipeline_callback','wait':hist(j*10),'hold':hist(j*10)}],
                       'streams':[{'type':2,'input':j*20,'aggregate_queue_peak':3}]}})
    emit(root/'capture/sdk-samples/samples.jsonl',sdk)
    one.atomic(root/'capture/sdk-samples/summary.json',{'collector_cpu_seconds':.1})
    manifest={'image_id':image,'runtime_sha256':digest,'uid':os.getuid(),'gid':os.getgid(),'source_configs':source}
    return manifest,cameras,samples,sdk


class MeasurementTests(unittest.TestCase):
    def test_weighted_cpu_concurrent_memory_lifetime_p99_and_sdk_deltas(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest,_,_,_=fixture(root)
            report=metrics.summarize(root,manifest,'on')
            self.assertAlmostEqual(report['metrics']['worker_cpu_cores'],4.8) # (1*2+2*3)/5 per camera
            self.assertEqual(report['metrics']['worker_rss_peak_bytes'],360)
            self.assertEqual(report['metrics']['sum_hwm_lifetime_bytes'],660)
            self.assertEqual(report['metrics']['writer_verification_total_ns'],300)
            self.assertEqual(report['metrics']['worst_camera_append_p99_upper_ns'],9)
            instance=report['sdk']['cameras'][0]['instances'][0]
            self.assertEqual(instance['counter_deltas']['candidate_outside_window'],2)
            self.assertEqual(instance['streams_lifetime_peaks'][0]['aggregate_queue_peak'],3)
            self.assertEqual(instance['locks'][0]['wait']['p99_log2_bounds_ns'],{'lower_bound_ns':4,'upper_bound_ns':7})

    def test_off_is_unavailable_not_zero_and_observer_overhead_separate(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest,_,_,_=fixture(root,'off')
            report=metrics.summarize(root,manifest,'off')
            self.assertFalse(report['sdk']['available']);self.assertNotIn('cameras',report['sdk'])
            self.assertEqual(report['observers']['dds']['observer_cpu_seconds'],2)

    def test_cpu_epoch_clock_and_missing_data_fail_closed(self):
        for fault in ('epoch','clock','missing','nan'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root=Path(d);_,cameras,samples,_=fixture(root)
                if fault=='epoch':samples[-1]['targets'][0]['epoch']=1
                elif fault=='clock':samples[-1]['monotonic_ns']=10*10**9
                elif fault=='missing':samples[-1]['targets'].pop()
                else:samples[-1]['targets'][0]['rates']['cpu_cores']=float('nan')
                emit(root/'capture/checkpoint-samples/samples.jsonl',samples)
                with self.assertRaises(ValueError):metrics.cpu_memory(root,cameras)

    def test_sdk_counter_reset_and_missing_camera_rejected(self):
        for fault in ('reset','missing'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root=Path(d);_,cameras,_,sdk=fixture(root)
                if fault=='reset':sdk[1]['snapshot']['counters']['input_frames']=0;sdk[1]['snapshot']['counters']['candidate_outside_window']=0
                else:sdk=[r for r in sdk if r['source']!='c']
                emit(root/'capture/sdk-samples/samples.jsonl',sdk)
                with self.assertRaises(ValueError):metrics.sdk_metrics(root,'on',cameras)

    def test_failed_unbound_or_mutated_runs_never_enter_comparison(self):
        for fault in ('failed','image','config'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root=Path(d);manifest,_,_,_=fixture(root)
                if fault=='failed':r=rt.read_json(root/'result.json');r['ok']=False;one.atomic(root/'result.json',r)
                elif fault=='image':manifest['image_id']='other'
                else:(root/'capture/a/config/health.json').write_text('{}')
                with self.assertRaises(ValueError):metrics.summarize(root,manifest,'on')

    def test_three_round_pairs_partial_exclusion_and_zero_denominator(self):
        groups=[]
        for item in batch.schedule(3,'on'):
            summary={'ok':True,'normalized_config_hashes':{'a':'same'},'metrics':{'cpu':2 if item['mode']=='on' else 1,'zero':0},'sdk':{'available':False}}
            groups.append(dict(item,summary=summary))
        result=metrics.aggregate(groups,3)
        self.assertTrue(result['complete']);self.assertEqual(result['comparison']['cpu']['paired_deltas'][0]['percent'],100)
        self.assertIsNone(result['comparison']['zero']['paired_deltas'][0]['percent'])
        groups.pop();self.assertIsNone(metrics.aggregate(groups,3)['comparison'])
        groups.append({'name':'off-03','mode':'off','round':3,'error':'failed'})
        self.assertFalse(metrics.aggregate(groups,3)['complete'])


class BatchTests(unittest.TestCase):
    def test_minimum_three_alternating_rounds(self):
        self.assertEqual([v['name'] for v in batch.schedule(3,'on')],['on-01','off-01','on-02','off-02','on-03','off-03'])
        self.assertEqual(batch.schedule(3,'off')[0]['mode'],'off')
        with self.assertRaises(ValueError):batch.schedule(2,'on')

    def test_source_copy_immutable_and_original_edits_do_not_change_frozen(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);source=root/'source';source.mkdir();out=root/'out';out.mkdir()
            manifest,_,_,_=fixture(source)
            plan={'cameras':[{'name':n,'capture_config':str(source/'capture'/n/'config/capture.json'),
                              'health_config':str(source/'capture'/n/'config/health.json')} for n in 'abc']}
            one.atomic(root/'plan.json',plan)
            with patch.object(rt,'validate_health'):files,configs=batch.freeze(root/'plan.json',out)
            expected={'frozen_files':files};batch.check_frozen(out,expected)
            (source/'capture/a/config/capture.json').write_text('edited original')
            batch.check_frozen(out,expected);self.assertEqual(configs['a'],rt.sha(out/'frozen/a/capture.json'))
            (out/'frozen/a/capture.json').write_text('{}')
            with self.assertRaisesRegex(ValueError,'FROZEN'):batch.check_frozen(out,expected)

class BatchWorkflowTests(unittest.TestCase):
    def setup_case(self,base,build=False):
        import argparse
        source=base/'inputs';source.mkdir()
        cameras=[]
        for n in 'abc':
            cp=source/(n+'.json');hp=source/(n+'-health.json')
            one.atomic(cp,{'color':{'fps':30},'depth':{'fps':30}});one.atomic(hp,{})
            cameras.append({'name':n,'capture_config':str(cp),'health_config':str(hp)})
        plan=source/'plan.json';one.atomic(plan,{'cameras':cameras})
        return argparse.Namespace(root=base/'batch',plan=plan,image='mutable:tag',rounds=3,first='on',seconds=600,
                                  max_bytes=128*1024**3,ros_domain_id=78,healthy_timeout=300,audit_timeout=60,
                                  poll_seconds=1,trim_seconds=5,pairs=None,completion_timeout=None,build=build,
                                  build_timeout=60,platform='linux/arm64',cooldown_seconds=0,perf=False,disk=[],execute=True)

    def run_case(self,a,fault=None):
        calls=[];image='sha256:'+'a'*64
        def command(controller,argv,name,timeout):
            calls.append((name,argv))
            if name=='build':return
            root=a.root/name;root.mkdir()
            one.atomic(root/'result.json',{'ok':name!=fault,'cleanup_errors':[],'uid':os.getuid(),'gid':os.getgid()})
            if name==fault:raise ValueError('synthetic group failure')
        def summarize(root,manifest,mode):
            return {'ok':True,'normalized_config_hashes':{'a':'fixed'},'metrics':{'cpu':2 if mode=='on' else 1},'sdk':{'available':False}}
        with patch.object(rt,'validate_health'),patch.object(one.Runner,'command',command), \
             patch.object(hw,'image_manifest',return_value=(image,{'fixed':'inventory'})) as resolve, \
             patch.object(metrics,'summarize',side_effect=summarize),patch.object(hw,'load_run',return_value={'cameras':[{'name':n} for n in 'abc']}), \
             patch.object(hw,'own_container',return_value={'State':{'Running':False}}), \
             patch.object(one,'rollback',return_value={'ok':True}) as rollback:
            code=batch.execute(a)
            self.assertEqual(resolve.call_count,1)
        return code,calls,rollback.call_count

    def test_six_groups_build_once_pin_image_and_same_limits(self):
        with tempfile.TemporaryDirectory() as d:
            a=self.setup_case(Path(d),build=True);code,calls,rolled=self.run_case(a)
            self.assertEqual(code,0);self.assertEqual(rolled,0)
            self.assertEqual([n for n,_ in calls],['build','on-01','off-01','on-02','off-02','on-03','off-03'])
            args=[one.parser().parse_args(argv[2:]) for name,argv in calls if name!='build']
            self.assertEqual({v.image for v in args},{'sha256:'+'a'*64})
            self.assertEqual({v.plan for v in args},{a.root/'frozen/plan.json'})
            self.assertEqual({v.pairs for v in args},{31680})
            self.assertTrue(rt.read_json(a.root/'summary.json')['complete'])
            self.assertTrue(rt.read_json(a.root/'result.json')['ok'])

    def test_failure_stops_sequence_keeps_partial_report_and_rolls_back_failed_group(self):
        with tempfile.TemporaryDirectory() as d:
            a=self.setup_case(Path(d));code,calls,rolled=self.run_case(a,fault='off-01')
            self.assertEqual(code,1);self.assertEqual([n for n,_ in calls],['on-01','off-01']);self.assertEqual(rolled,1)
            summary=rt.read_json(a.root/'summary.json')
            self.assertFalse(summary['complete']);self.assertIsNone(summary['comparison']);self.assertEqual(summary['accepted_groups'],1)
            self.assertTrue((a.root/'on-01/result.json').exists());self.assertTrue((a.root/'off-01/result.json').exists())
            self.assertTrue((a.root/'ABORT.json').exists())

    def test_batch_rollback_rejects_uid_and_untrusted_group_paths(self):
        import argparse
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);one.atomic(root/'result.json',{'uid':os.getuid()+1,'gid':os.getgid()})
            with self.assertRaisesRegex(ValueError,'UID_GID'):batch.rollback(argparse.Namespace(root=root))
            one.atomic(root/'result.json',{'uid':os.getuid(),'gid':os.getgid(),'run_token':'token','groups':[{'name':'../production'}]})
            with patch.object(one,'stop_saved_children',return_value=[]),self.assertRaisesRegex(ValueError,'GROUP_PATH'):
                batch.rollback(argparse.Namespace(root=root))

class SdkAggregateTests(unittest.TestCase):
    def test_sdk_on_round_summary_retains_queue_peaks_and_independent_lock_sites(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest,_,_,_=fixture(root);summary=metrics.summarize(root,manifest,'on')
            groups=[{'round':r,'mode':'on','summary':summary} for r in range(1,4)]
            result=metrics.sdk_round_summary(groups)
            self.assertIsNone(result['off'])
            self.assertEqual(result['on']['a']['counter_delta_medians']['candidate_outside_window'],2)
            self.assertEqual(result['on']['a']['rounds'][0]['stream_lifetime_queue_peaks']['2'],3)
            self.assertEqual(result['on']['a']['lock_medians']['pipeline_callback/wait']['total_ns'],40)

    def test_nontransactional_histogram_count_mismatch_is_explicit_not_fake_p99(self):
        a={'count':1,'total_ns':4,'log2_ns_buckets':[0,0,1]+[0]*61}
        b={'count':3,'total_ns':8,'log2_ns_buckets':[0,0,2]+[0]*61}
        result=metrics.histogram_delta(a,b)
        self.assertFalse(result['histogram_count_consistent']);self.assertIsNone(result['p99_log2_bounds_ns'])
        b['log2_ns_buckets'].append(0)
        with self.assertRaisesRegex(ValueError,'SIZE'):metrics.histogram_delta(a,b)


class ControllerProcessTests(unittest.TestCase):
    def test_reaped_round_pids_are_removed_but_live_bindings_retained(self):
        import argparse
        class Child:
            def __init__(self,pid,code):self.pid=pid;self.code=code
            def poll(self):return self.code
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);controller=one.Runner(argparse.Namespace(root=root))
            controller.children=[(Child(123,0),root/'done.log'),(Child(456,None),root/'live.log')]
            controller.child_bindings=[{'pid':123},{'pid':456}]
            with patch.object(controller,'command'):batch.controller_command(controller,[],'round',1)
            self.assertEqual(rt.read_json(root/'children.json')['processes'],[{'pid':456}])
