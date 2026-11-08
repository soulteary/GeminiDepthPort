"""Long-run policy plus real owned-process control; no camera performance claim."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('writer_long_run', ROOT / 'scripts/validation/writer_long_run.py')
long = importlib.util.module_from_spec(spec);spec.loader.exec_module(long)
oldspec = importlib.util.spec_from_file_location('writer_ab_fixture', ROOT / 'tests/test_writer_contention_ab.py')
old = importlib.util.module_from_spec(oldspec);oldspec.loader.exec_module(old)


def status_proof(periodic=2):
    return {'errors':0,'submission_errors':0,'pump_errors':0,'submitted_sequence':periodic+2,
            'completed_sequence':periodic+2,'pending_snapshots':0,'protected_sequence':None,
            'start_critical_ack':True,'final_critical_ack':True,'checkpoint_critical_ack':True,'heartbeat_count':periodic,
            'periodic_submissions':periodic,'heartbeat_max_gap_ns':500_000_000}


def args(root,binary):
    return long.parser().parse_args(['--binary',str(binary),'--output',str(root/'output'),
        '--seconds','1','--repeats','1','--max-output-bytes',str(2*long.GIB),
        '--max-wall-seconds','30','--trace-max-bytes',str(long.MIB),'--smoke'])


def fake_binary(path,behavior='ok'):
    # Only the controller, identity/barrier, SIGKILL and cleanup are exercised.
    # Durability claims require the independent real Rust adapter smoke/long run.
    body='''#!SHEBANG
import json,os,pathlib,time
root=pathlib.Path(os.environ['GDP_CONTENTION_ROOT']);pairs=int(os.environ['GDP_CONTENTION_PAIRS'])
mode=os.environ['GDP_CONTENTION_MODE'];on=os.environ['GDP_CONTENTION_STATUS_LOAD']=='1';behavior=BEHAVIOR
hashes=HASHES
def write(name,value): (root/name).write_text(json.dumps(value))
def proof(periodic):
    return {'errors':0,'submission_errors':0,'pump_errors':0,'submitted_sequence':periodic+2,'completed_sequence':periodic+2,
        'pending_snapshots':0,'protected_sequence':None,'start_critical_ack':True,'final_critical_ack':True,'checkpoint_critical_ack':True,
        'heartbeat_count':periodic,'periodic_submissions':periodic,'heartbeat_max_gap_ns':500000000}
if mode=='crash':
    if on:(root/'state').mkdir()
    partial=root/'capture.partial';partial.mkdir();(partial/'checkpoint.json').write_text('confirmed32')
    write('recovery-ready.json',dict(pairs=32,pid=os.getpid(),status_background_load=on,status_persistence=proof(0) if on else None,**hashes))
    while True: time.sleep(.01)
elif mode=='recover':
    if behavior=='mutate-recovery': (root/'capture.partial/checkpoint.json').write_text('mutated')
    write('recovery-result.json',dict(passes=2,pairs_per_pass=[32,32],ordinary_reader_rejected=True,per_frame_color_depth_evidence_hashes=True,status_background_load=on,**hashes))
else:
    if on:
        (root/'state').mkdir()
        write('state/status.json',{'version':1,'kind':'capture-service-status','pid':os.getpid(),
            'session_id':'a'*32,'heartbeat_sequence':1,'updated_unix_ms':time.time_ns()//1000000-(10001 if behavior=='stale-status' else 0)})
    write('ready.json',{'pid':os.getpid()+(1 if behavior=='wrong-pid' else 0)})
    barrier=pathlib.Path(os.environ['GDP_CONTENTION_BARRIER'])
    while not barrier.exists(): time.sleep(.002)
    target=json.loads(barrier.read_text())['start_monotonic_ns']
    while time.monotonic_ns()<target: time.sleep(.0001)
    if behavior=='hang' or (behavior=='stale-status' and on) or (behavior=='early-failed-one' and root.name!='writer-0') or (behavior=='on-overflow' and on and root.name!='writer-0'):
        while True:time.sleep(.01)
    if behavior=='stagger-finish' and on and root.name!='writer-0':
        end=time.monotonic()+10.3;sequence=1
        while time.monotonic()<end:
            sequence+=1
            temp=root/'state/status.tmp'
            temp.write_text(json.dumps({'version':1,'kind':'capture-service-status','pid':os.getpid(),
                'session_id':'a'*32,'heartbeat_sequence':sequence,'updated_unix_ms':time.time_ns()//1000000}))
            temp.replace(root/'state/status.json')
            time.sleep(.1)
    value=RESULT
    value.update(pairs=pairs,written_pairs=pairs,status_background_load=on,status_persistence=proof(max(2,pairs//30)) if on else None,
                 capture_elapsed_ns=pairs//30*1000000000,capture_cpu_seconds=.01,capture_cpu_cores=.01,checkpoint_pipeline=True,credit_pairs=32,credit_bytes=16777216,
                 actual_start_monotonic_ns=time.monotonic_ns(),actual_start_unix_ns=time.time_ns(),**hashes)
    value['diagnostics'].update(durable_pairs=pairs,validation_image_bytes=2*PAIR_BYTES*pairs,checkpoint_batching=os.environ.get('GDP_CONTENTION_BATCH_POLICY','full-credit'))
    value['independent_readback']['pairs_per_pass']=[pairs,pairs]
    if behavior in ('overflow','early-failed-one') or (behavior=='on-overflow' and on):value.update(ok=False,reason='RAW_QUEUE_OVERFLOW',overflow=1,queue_peak=16)
    if behavior=='different-on' and on:value['input_sha256']='c'*64
    if behavior=='short':value['capture_elapsed_ns']=1
    (root/'capture').mkdir()
    samples=(json.dumps({'monotonic_ns':1,'synthetic_fixture':True})+'\\n').encode()*2
    (root/'status-load-samples.jsonl').write_bytes(samples)
    value['background_sampling']={'ok':True,'rows':2,'bytes':len(samples),'max_rows':20000,'max_bytes':67108864,'relative_path':'status-load-samples.jsonl'}
    events=json.dumps({'event_id':1}).encode()+bytes([10])
    (root/'checkpoint-events.jsonl').write_bytes(events)
    value['background_sampling']['checkpoint_events']={'ok':True,'rows':1,'bytes':len(events),'max_bytes':16777216,'relative_path':'checkpoint-events.jsonl'}
    if on:
        value['status_persistence']['state_directory']=str(root/'state')
    write('result.json',value)
'''
    body=body.replace('SHEBANG',sys.executable).replace('BEHAVIOR',repr(behavior))
    body=body.replace('HASHES',repr(long.source_hashes())).replace('RESULT',repr(old.result())).replace('PAIR_BYTES',str(long.ab.PAIR_BYTES))
    path.write_text(body);path.chmod(0o700);return path


class CompleteTrace:
    failure=None
    starts=[]
    def __init__(self,output,**kwargs):
        self.output=output;self.output.mkdir();self.kwargs=kwargs
    def start(self,processes):
        self.starts.append(len(processes))
        if len(processes)!=3:raise AssertionError('three owned targets required')
        for index,p in enumerate(processes):
            self.assert_live(p)
            if not (self.output.parent/('writer-'+str(index))/'ready.json').exists():raise AssertionError('trace before ready')
        if (self.output.parent/'start.json').exists():raise AssertionError('trace after barrier')
        return {'ok':True,'state':'running'}
    @staticmethod
    def assert_live(p):
        if p.poll() is not None:raise AssertionError('not a live owned Popen')
    def poll(self):pass
    def finish(self):
        result={'ok':True,'state':'complete','lost_events':0,'bytes':0}
        long.ab.write_json(self.output/'summary.json',result);return result


class UnavailableTrace(CompleteTrace):
    def start(self,processes):
        super().start(processes)
        return {'ok':False,'state':'unavailable','reason':'synthetic missing tracefs capability'}
    def finish(self):
        result={'ok':False,'state':'unavailable','reason':'synthetic missing tracefs capability'}
        long.ab.write_json(self.output/'summary.json',result);return result


class LostTrace(CompleteTrace):
    def finish(self):
        result={'ok':False,'state':'incomplete','lost_events':1,'reason':'controlled loss'}
        long.ab.write_json(self.output/'summary.json',result);return result


class LongPolicyTests(unittest.TestCase):
    def test_runtime_free_space_reserve_includes_visible_backing_without_deleting(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,root/'binary');a.output.mkdir();a.backing_path=root
            (a.output/'preserved').write_bytes(b'old')
            enough=type('FS',(),{'f_bavail':long.FREE_SPACE_RESERVE,'f_frsize':1})()
            low=type('FS',(),{'f_bavail':1,'f_frsize':1})()
            with patch.object(long.os,'statvfs',side_effect=[enough,low]),self.assertRaisesRegex(RuntimeError,'FREE_SPACE_RESERVE'):
                long.storage_guard(a,time.monotonic()+10)
            self.assertEqual((a.output/'preserved').read_bytes(),b'old')

    def test_checkpoint_archive_requires_contiguous_bounded_real_file(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);path=root/'checkpoint-events.jsonl'
            for raw,rows,error in [(b'{"event_id":1}\n',1,None),
                                   (b'{"event_id":2}\n',1,'GAP'),
                                   (b'{"event_id":1}\n{"event_id":1}\n',2,'GAP'),
                                   (b'{"event_id":1}\n',2,'TAIL'),
                                   (b'x'*65537,1,'LINE_LIMIT')]:
                path.write_bytes(raw)
                proof={'ok':True,'rows':rows,'bytes':len(raw),'max_bytes':16*long.MIB,'relative_path':path.name}
                if error:
                    with self.assertRaisesRegex(ValueError,error):long.validate_event_archive(root,proof)
                else:long.validate_event_archive(root,proof)
            path.unlink();path.symlink_to(root/'missing')
            with self.assertRaisesRegex(ValueError,'FILE'):long.validate_event_archive(root,proof)

    def test_defaults_are_six_three_writer_groups_at_least_600_seconds(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=long.parser().parse_args(['--binary',str(root/'binary'),'--output',str(root/'new')])
            self.assertEqual((a.seconds,a.repeats),(600,3))
            self.assertEqual(a.trace_max_bytes,4*long.GIB)
            if sys.platform=='linux':
                planned=long.plan(a);self.assertEqual(planned['writers_per_group'],3)
                self.assertEqual(planned['planned_groups'],6)
                self.assertEqual(planned['pairs_per_writer'],18000)
                self.assertGreater(planned['planned_upper_bound_bytes'],6*3*18000*long.ab.PAIR_BYTES+6*a.trace_max_bytes)

    @unittest.skipUnless(sys.platform=='linux','Linux planning')
    def test_short_mode_budgets_and_output_reuse_are_rejected_before_writing(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,root/'absent')
            a.smoke=False
            with self.assertRaisesRegex(ValueError,'600'):long.plan(a)
            self.assertFalse(a.output.exists())
            a.smoke=True;a.max_output_bytes=128*long.MIB
            with self.assertRaisesRegex(ValueError,'CAPACITY'):long.plan(a)
            a.max_output_bytes=2*long.GIB;a.max_wall_seconds=2
            with self.assertRaisesRegex(ValueError,'READBACK'):long.plan(a)
            a.max_wall_seconds=30;a.trace_buffer_kib=0
            with self.assertRaisesRegex(ValueError,'TRACE'):long.plan(a)
            a.trace_buffer_kib=256;a.trace_max_bytes=8*long.GIB+1
            with self.assertRaisesRegex(ValueError,'TRACE'):long.plan(a)

    def test_requested_whole_device_is_proved_by_sysfs_and_diskstats_not_guessed(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);block=root/'class/block';block.mkdir(parents=True)
            actual=root/'devices/pci/nvme0n1';actual.mkdir(parents=True);(actual/'dev').write_text('259:0\n')
            (block/'nvme0n1').symlink_to(actual,target_is_directory=True)
            disks={'nvme0n1':{'major':259,'minor':0}}
            identity=long.disk_identity('nvme0n1',block,disks)
            self.assertTrue(identity['whole_block_device_verified']);self.assertFalse(identity['physical_device_verified'])
            (actual/'partition').write_text('2\n')
            with self.assertRaisesRegex(ValueError,'NOT_PARTITION'):long.disk_identity('nvme0n1',block,disks)
            (actual/'partition').unlink();(actual/'dev').write_text('259:1\n')
            with self.assertRaisesRegex(ValueError,'MISMATCH'):long.disk_identity('nvme0n1',block,disks)
            (actual/'dev').write_bytes(b'x'*129)
            with self.assertRaisesRegex(ValueError,'BYTE_LIMIT'):long.disk_identity('nvme0n1',block,disks)
            with self.assertRaisesRegex(ValueError,'MISSING'):long.disk_identity('missing',block,disks)
            with tempfile.TemporaryDirectory() as external:
                (block/'nvme0n1').unlink();(block/'nvme0n1').symlink_to(Path(external),target_is_directory=True)
                with self.assertRaisesRegex(ValueError,'PATH_ESCAPE'):long.disk_identity('nvme0n1',block,disks)
        self.assertFalse(long.disk_identity(None)['whole_block_device_verified'])

    def test_result_input_is_bounded_and_duplicate_or_nonfinite_json_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'result.json'
            for raw,error in ((b'x'*(4*long.MIB+1),'BYTE_LIMIT'),(b'{"ok":true,"ok":false}','DUPLICATE'),
                              (b'{"cpu_seconds":NaN}','NONFINITE'),(b'[]','OBJECT')):
                path.write_bytes(raw)
                with self.subTest(error=error),self.assertRaisesRegex(ValueError,error):long.read_result(path)

    def test_actual_status_heartbeat_deadline_identity_and_clock_checks(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);(root/'state').mkdir();path=root/'state/status.json'
            value={'version':1,'kind':'capture-service-status','pid':42,'session_id':'a'*32,
                   'heartbeat_sequence':1,'updated_unix_ms':1000}
            def write():path.write_text(json.dumps(value))
            write();monitor=long.StatusMonitor(root,42)
            self.assertEqual(monitor.sample(11000)['status_age_ms'],10000)
            with self.assertRaisesRegex(ValueError,'STALE'):monitor.sample(11001)
            with self.assertRaisesRegex(ValueError,'FUTURE'):long.StatusMonitor(root,42).sample(999)
            for key,bad,error in (('pid',43,'IDENTITY'),('version',True,'IDENTITY'),
                                  ('heartbeat_sequence',True,'SCHEMA'),('session_id','b'*32,'CHANGED')):
                previous=value[key];value[key]=bad;write()
                with self.subTest(key=key),self.assertRaisesRegex(ValueError,error):monitor.sample(11000)
                value[key]=previous
            value['heartbeat_sequence']=2;value['updated_unix_ms']=2000;write();monitor.sample(12000)
            value['heartbeat_sequence']=1;write()
            with self.assertRaisesRegex(ValueError,'REGRESSION'):monitor.sample(12000)
            path.write_bytes(b'x'*(2*long.MIB+1))
            with self.assertRaisesRegex(ValueError,'BYTE_LIMIT'):monitor.sample(12000)
        ready={'status_background_load':True,'status_persistence':status_proof()}
        ready['status_persistence']['heartbeat_max_gap_ns']=10_000_000_001
        with self.assertRaisesRegex(ValueError,'DEADLINE'):long.status_contract(ready,True)

    def test_live_background_files_are_bounded_and_status_off_does_not_write_state(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);(root/'capture').mkdir();data=b'{"monotonic_ns":1}\n'
            sample=root/'status-load-samples.jsonl';sample.write_bytes(data)
            row={'background_sampling':{'ok':True,'rows':1,'bytes':len(data),'max_rows':20000,
                'max_bytes':64*long.MIB,'relative_path':sample.name},'status_persistence':None}
            events=b'{"event_id":1}\n'
            (root/'checkpoint-events.jsonl').write_bytes(events)
            row['background_sampling']['checkpoint_events']={'ok':True,'rows':1,'bytes':len(events),'max_bytes':16*long.MIB,'relative_path':'checkpoint-events.jsonl'}
            long.validate_child_files(root,row,False,root.stat().st_dev)
            bad=copy.deepcopy(row);bad['background_sampling']['rows']=2
            with self.assertRaisesRegex(ValueError,'ROW_COUNT'):long.validate_child_files(root,bad,False,root.stat().st_dev)
            (root/'state').mkdir()
            with self.assertRaisesRegex(ValueError,'STATUS_OFF'):long.validate_child_files(root,row,False,root.stat().st_dev)
            row['status_persistence']={'state_directory':str(root/'state')}
            long.validate_child_files(root,row,True,root.stat().st_dev)
            with self.assertRaisesRegex(ValueError,'FILESYSTEM'):long.validate_child_files(root,row,True,root.stat().st_dev+1)

    def test_crash_fence_is_not_a_fabricated_final_cleanup_ack(self):
        ready={'status_background_load':True,'status_persistence':status_proof(0)}
        ready['status_persistence'].pop('final_critical_ack')
        long.checkpoint_status_contract(ready,True)
        ready['status_persistence']['checkpoint_critical_ack']=False
        with self.assertRaisesRegex(ValueError,'CHECKPOINT_CRITICAL'):long.checkpoint_status_contract(ready,True)
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'ready.json';path.write_bytes(b' '*65537)
            with self.assertRaisesRegex(ValueError,'BYTE_LIMIT'):long.recovery_ready(path)
            path.write_bytes(b'{');self.assertEqual(long.recovery_ready(path),(False,None))

    def test_real_persistence_contract_is_strict_and_input_report_is_immutable(self):
        row=old.result();row.update(status_background_load=True,status_persistence=status_proof(),
            capture_elapsed_ns=1_000_000_000,capture_cpu_seconds=.01,capture_cpu_cores=.01,checkpoint_pipeline=True,credit_pairs=32,credit_bytes=16*long.MIB,
            common_driver_sha256='a'*64,status_driver_sha256='b'*64)
        hashes={key:row[key] for key in ('common_driver_sha256','status_driver_sha256')}
        before=json.dumps(row,sort_keys=True)
        long.validate_result(row,30,True,hashes,1)
        self.assertEqual(json.dumps(row,sort_keys=True),before)
        for key,value in (('errors',False),('pending_snapshots',False),('final_critical_ack',1),
                          ('completed_sequence',1),('periodic_submissions',0),('heartbeat_max_gap_ns',float('nan'))):
            bad=copy.deepcopy(row);bad['status_persistence'][key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):long.validate_result(bad,30,True,hashes,1)
        bad=copy.deepcopy(row);bad['capture_elapsed_ns']=999_999_999
        with self.assertRaisesRegex(ValueError,'DURATION'):long.validate_result(bad,30,True,hashes,1)
        bad=copy.deepcopy(row);bad['status_driver_sha256']='c'*64
        with self.assertRaisesRegex(ValueError,'SOURCE_IDENTITY'):long.validate_result(bad,30,True,hashes,1)
        with self.assertRaisesRegex(ValueError,'STATUS_LOAD_MODE'):long.status_contract(row,False)
        row['status_background_load']=False
        with self.assertRaisesRegex(ValueError,'STATUS_OFF'):long.status_contract(row,False)


@unittest.skipUnless(sys.platform=='linux','actual owned subprocess/SIGKILL control')
class LongProcessTests(unittest.TestCase):
    def test_successful_first_trace_does_not_certify_an_omitted_failed_group(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,fake_binary(root/'driver','on-overflow'))
            # Only the on group's first writer fails; peers wait for controller
            # cleanup. Simultaneous exits can legitimately finish the whole trace
            # before data rejection, which is a different scenario from omission.
            result=long.run(a,trace_factory=CompleteTrace)
            failed=a.output/'round-00-status-on'
            self.assertFalse((a.output/'round-00-status-on.json').exists())
            self.assertEqual(json.loads((failed/'writer-0/result.json').read_text())['reason'],'RAW_QUEUE_OVERFLOW')
            self.assertFalse((failed/'writer-1/result.json').exists())
            self.assertFalse((failed/'writer-2/result.json').exists())
            self.assertFalse(result['ok']);self.assertEqual(result['completed_groups'],1)
            self.assertTrue(result['completed_group_traces_complete'])
            self.assertFalse(result['trace_complete'])
            self.assertFalse(result['long_comparison_complete'])
            self.assertEqual(result['recovery'],{})
            self.assertIn('RAW_QUEUE_OVERFLOW',result['error'])

    def test_known_finished_writer_is_not_aged_while_slower_healthy_peers_read_back(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,fake_binary(root/'driver','stagger-finish'))
            result=long.run(a,trace_factory=CompleteTrace)
            self.assertTrue(result['ok'],result)
            final=result['groups'][1]['final_status_heartbeats']
            self.assertEqual(len(final),3)
            self.assertGreater(final[1]['updated_unix_ms']-final[0]['updated_unix_ms'],10000)
            self.assertTrue(all(row['alive_deadline_applied'] is False for row in final))
            self.assertTrue(all(row['status_age_ms']>=0 for row in final))

    def test_first_failed_writer_stops_healthy_hung_peers_and_preserves_native_cause(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,fake_binary(root/'driver','early-failed-one'))
            started=time.monotonic();result=long.run(a,trace_factory=LostTrace)
            self.assertLess(time.monotonic()-started,8)
            self.assertFalse(result['ok']);self.assertIn('RECORDING_FAILED:RAW_QUEUE_OVERFLOW',result['error'])
            failed=a.output/'round-00-status-off'
            sidecar=json.loads((failed/'batch-failure.json').read_text())
            self.assertEqual(sidecar['first_child_failure']['reason'],'RAW_QUEUE_OVERFLOW')
            self.assertEqual(sidecar['trace']['lost_events'],1)
            self.assertEqual(result['completed_groups'],0)
            self.assertFalse((a.output/'round-00-status-on').exists())
            for marker in failed.glob('writer-*/ready.json'):
                pid=json.loads(marker.read_text())['pid']
                with self.assertRaises(ProcessLookupError):os.kill(pid,0)

    def test_real_three_children_trace_before_barrier_hashes_and_both_recoveries(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,fake_binary(root/'driver'))
            result=long.run(a,trace_factory=CompleteTrace)
            self.assertTrue(result['ok'],result)
            self.assertEqual(result['completed_groups'],2)
            self.assertFalse(result['long_duration_verified']);self.assertFalse(result['long_comparison_complete'])
            self.assertTrue(result['trace_complete']);self.assertFalse(result['hardware_acceptance_verified'])
            self.assertEqual([row['status_background_load'] for row in result['groups']],[False,True])
            self.assertEqual(set(result['recovery']),{'off','on'})
            self.assertEqual(len(result['paired_comparisons']),1)
            paired=result['paired_comparisons'][0]
            self.assertEqual(paired['append_p99_ns_by_position']['off'],[10]*3)
            self.assertEqual(paired['on_minus_off']['capture_cpu_seconds_sum'],0)
            self.assertGreaterEqual(paired['on']['observer']['cpu_seconds'],0)
            for line in (a.output/'SHA256SUMS').read_text().splitlines():
                digest,name=line.split('  ');self.assertEqual(hashlib.sha256((a.output/name).read_bytes()).hexdigest(),digest)
            before={str(p):p.read_bytes() for p in a.output.rglob('*') if p.is_file()}
            with self.assertRaisesRegex(ValueError,'FRESH_OUTPUT'):long.run(a,trace_factory=CompleteTrace)
            self.assertEqual({str(p):p.read_bytes() for p in a.output.rglob('*') if p.is_file()},before)

    def test_unavailable_requires_explicit_opt_in_and_still_never_counts_as_complete(self):
        for allow in (False,True):
            with self.subTest(allow=allow),tempfile.TemporaryDirectory() as d:
                root=Path(d);a=args(root,fake_binary(root/'driver'));a.allow_unavailable_trace=allow
                result=long.run(a,trace_factory=UnavailableTrace)
                self.assertEqual(result['ok'],allow,result)
                self.assertFalse(result['trace_complete']);self.assertFalse(result['long_comparison_complete'])
                if not allow:
                    self.assertIn('TRACE_NOT_READY',result['error'])
                    self.assertFalse((a.output/'round-00-status-off/start.json').exists())
                else:self.assertEqual(len(result['recovery']),2)

    def test_runtime_loss_cannot_use_unavailable_escape_and_failed_results_are_retained(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a=args(root,fake_binary(root/'driver'));a.allow_unavailable_trace=True
            result=long.run(a,trace_factory=LostTrace)
            self.assertFalse(result['ok']);self.assertIn('TRACE_INCOMPLETE',result['error'])
            self.assertEqual(result['recovery'],{})
            self.assertTrue((a.output/'round-00-status-off/writer-0/result.json').exists())
            self.assertTrue((a.output/'SHA256SUMS').exists())

    def test_failed_overflow_changed_input_short_capture_and_mutated_recovery_stop_the_run(self):
        for behavior,error in (('overflow','RECORDING_FAILED'),('different-on','HASH_MISMATCH'),
                               ('short','DURATION'),('mutate-recovery','MUTATED_INPUT'),('stale-status','HEARTBEAT_STALE')):
            with self.subTest(behavior=behavior),tempfile.TemporaryDirectory() as d:
                root=Path(d);a=args(root,fake_binary(root/'driver',behavior))
                result=long.run(a,trace_factory=CompleteTrace)
                self.assertFalse(result['ok']);self.assertIn(error,result['error'])
                self.assertTrue((a.output/'summary.json').exists())
                self.assertTrue((a.output/'round-00-status-off/writer-0/result.json').exists())
                self.assertFalse(result['long_comparison_complete'])

    def test_owned_hung_children_are_killed_and_no_barrier_released_for_wrong_identity(self):
        for behavior,error in (('hang','WALL_TIME_LIMIT'),('wrong-pid','PID_MISMATCH')):
            with self.subTest(behavior=behavior),tempfile.TemporaryDirectory() as d:
                root=Path(d);a=args(root,fake_binary(root/'driver',behavior))
                a.max_wall_seconds=3
                result=long.run(a,trace_factory=CompleteTrace)
                self.assertFalse(result['ok']);self.assertIn(error,result['error'])
                for ready in a.output.rglob('ready.json'):
                    pid=json.loads(ready.read_text())['pid']-(1 if behavior=='wrong-pid' else 0)
                    with self.assertRaises(ProcessLookupError):os.kill(pid,0)
                if behavior=='wrong-pid':self.assertFalse((a.output/'round-00-status-off/start.json').exists())


class ResourceLifecycleTests(unittest.TestCase):
    def test_sampling_continues_across_cleanup_and_trace_finish_with_final_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);a=args(root,root/'binary')
            children=[object(),object(),object()]
            process={'pid':123,'rss_bytes':4096}
            with patch.object(long.ab,'owned_process_sample',return_value=process),patch.object(long.ab,'process_sample',return_value=process),patch.object(long.ab,'disk_sample',return_value={'synthetic':{}}):
                sampler=long.ResourceSampler(root,children,a,.005);sampler.thread.start()
                def observed(phase):
                    deadline=time.monotonic()+2
                    while True:
                        path=root/'resources.jsonl'
                        if path.exists() and any(json.loads(line)['phase']==phase for line in path.read_text().splitlines()):return
                        self.assertLess(time.monotonic(),deadline,'resource phase not observed: '+phase)
                        time.sleep(.005)
                observed('recording');sampler.phase='cleanup';observed('cleanup')
                sampler.phase='trace-drain';observed('trace-drain');end=time.monotonic_ns()
                summary=sampler.finish()
            rows=[json.loads(line) for line in (root/'resources.jsonl').read_text().splitlines()]
            self.assertTrue(summary['resource_diagnostic_complete'])
            self.assertTrue(rows[-1]['final']);self.assertGreaterEqual(rows[-1]['monotonic_ns'],end)
            self.assertEqual({r['phase'] for r in rows},{'recording','cleanup','trace-drain'})
            self.assertEqual(summary['simultaneous_rss_peak_bytes'],12288)
            self.assertEqual(summary['resource_samples'],len(rows))
            self.assertEqual(summary['resource_log_bytes'],(root/'resources.jsonl').stat().st_size)

    def test_sampler_byte_limit_is_terminal_and_does_not_write_over_budget(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);a=args(root,root/'binary')
            with patch.object(long.ab,'owned_process_sample',return_value={'terminal':True,'error':'EXITED'}),patch.object(long.ab,'process_sample',return_value={'rss_bytes':1}),patch.object(long.ab,'disk_sample',return_value={'synthetic':{}}):
                sampler=long.ResourceSampler(root,[object()]*3,a,.005);sampler.limit=1
                sampler.thread.start();sampler.thread.join(timeout=2)
                with self.assertRaisesRegex(RuntimeError,'BYTE_LIMIT_BEFORE_WRITE'):sampler.finish()
            self.assertEqual((root/'resources.jsonl').stat().st_size,0)


@unittest.skipUnless(sys.platform=='linux','real owned subprocess cleanup and /proc sampling')
class ResourceFailureLifecycleTests(unittest.TestCase):
    def test_native_failure_keeps_sampling_through_slow_cleanup_trace_and_terminal_row(self):
        class SlowFinishTrace(CompleteTrace):
            finished_ns=0
            def finish(self):
                deadline=time.monotonic()+2
                while not any(json.loads(line)['phase']=='trace-drain' for line in (self.output.parent/'resources.jsonl').read_text().splitlines()):
                    if time.monotonic()>=deadline:raise AssertionError('cleanup was not sampled')
                    time.sleep(.005)
                time.sleep(.05)
                result=super().finish();type(self).finished_ns=time.monotonic_ns();return result
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);a=args(root,fake_binary(root/'driver','early-failed-one'));a.output.mkdir()
            with self.assertRaisesRegex(ValueError,'RAW_QUEUE_OVERFLOW'):
                long.run_batch(a.binary,a.output/'failed',30,False,a,time.monotonic()+10,SlowFinishTrace,.01)
            failure=json.loads((a.output/'failed/batch-failure.json').read_text())
            self.assertIn('RAW_QUEUE_OVERFLOW',failure['first_failure'])
            self.assertEqual(failure['cleanup_errors'],[])
            rows=[json.loads(line) for line in (a.output/'failed/resources.jsonl').read_text().splitlines()]
            self.assertTrue(rows[-1]['final'])
            self.assertGreaterEqual(rows[-1]['monotonic_ns'],SlowFinishTrace.finished_ns)
            self.assertIn('trace-drain',{row['phase'] for row in rows})
            self.assertTrue(all(p['terminal'] for p in rows[-1]['processes']))
            self.assertEqual(failure['resources']['resource_finished_monotonic_ns'],rows[-1]['monotonic_ns'])
