import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('checkpoint_probe',ROOT/'scripts/validation/checkpoint_probe.py')
probe=importlib.util.module_from_spec(spec);spec.loader.exec_module(probe)


class CheckpointProbeTests(unittest.TestCase):
    def test_proc_and_disk_counters_are_units_not_latency_claims(self):
        fields=['S']+['0']*48
        for n,v in ((7,4),(9,2),(11,100),(12,20),(17,3),(19,999),(39,7)):fields[n]=str(v)
        value=probe.proc_stat('123 (a tricky ) name) '+' '.join(fields))
        self.assertEqual(value['cpu_ticks'],120);self.assertEqual(value['start_ticks'],999)
        a=dict(read_ops=1,read_sectors=2,read_ms=3,write_ops=4,write_sectors=5,write_ms=6,inflight=0,io_ms=7,weighted_ms=8)
        b={k:v+2 for k,v in a.items()}
        rates=probe.disk_delta(a,b,1.)
        self.assertEqual(rates['write_bytes_per_second'],1024);self.assertEqual(rates['average_request_ms'],1.)
        self.assertEqual(probe.disk_delta(b,a,1.)['error'],'DISK_COUNTER_RESET')
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);(root/'diskstats').write_text('8 0 sda 1 0 2 3 4 0 5 6 0 7 8\n')
            self.assertEqual(probe.read_disks({'sda'},root)['sda'],a)

    def test_trace_cursor_gaps_and_epochs_do_not_copy_config_or_paths(self):
        t=probe.Trace();process={'host_pid':1,'start_ticks':2}
        def status(ids,session='one'):
            return {'session_id':session,'config':'PRIVATE','raw_recording':{'job_path':'PRIVATE','disk_stages':{'checkpoint_observation':{'version':1,'enabled':True,'latest_event_id':max(ids,default=0),'events':[{'event_id':i,'phase':'payload_sync'} for i in ids]}}}}
        row,events=t.extract(status([3,4]),process)
        self.assertEqual(t.before_start,2);self.assertEqual(t.lost,0)
        self.assertNotIn('PRIVATE',json.dumps(row));self.assertEqual(len(events),2)
        self.assertEqual(t.extract(status([3,4]),process)[1],[])
        t.extract(status([7,8]),process);self.assertEqual(t.lost,2)
        t.extract(status([1],'two'),process);self.assertEqual(t.epoch,1)
        with self.assertRaises(ValueError):t.extract(status([],'two'),process)

    def test_output_limit_and_bounded_status(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);old=os.umask(0o077)
            try:
                out=probe.Output(root/'probe',10)
                with self.assertRaises(ValueError):out.emit({'larger':'than ten'})
                out.file.close();self.assertEqual((root/'probe').stat().st_mode&0o777,0o700)
                p=root/'status';p.write_bytes(b'x'*(probe.LIMIT+1))
                with self.assertRaises(ValueError):probe.bounded_json(p)
            finally:os.umask(old)

    def test_failed_queue_observation_survives_event_dedup_and_later_drain(self):
        trace=probe.Trace(); process={'host_pid':17,'start_ticks':91}
        value={'session_id':'capture','raw_recording':{'session_id':'raw','state':'recording',
            'job_path':'PRIVATE','disk_stages':{'checkpoint_observation':{
                'version':1,'enabled':True,'latest_event_id':1,'events':[{'event_id':1}],
                'active_append':{'pair_sequence':8}}}}}
        healthy,_=trace.extract(value,process)
        self.assertIsNone(healthy['raw']['overflow_observation'])
        frozen={'version':1,'detected_monotonic_ns':100,'queue_at_detection':{'queued_pairs':16,
            'oldest_queued_age_ns':500_000_000},'writer_context':{
                'active_append':{'pair_sequence':8},'active_checkpoint_spans':[{'phase':'verify_wait',
                    'wait_reason':'chunk_rotation','info':{'checkpoint_id':1}}]}}
        raw=value['raw_recording'];raw.update(state='failed',reason='RAW_QUEUE_OVERFLOW',
            overflow_observation=frozen,writer_queue={'queued_pairs':16})
        before=json.dumps(value,sort_keys=True)
        failed,events=trace.extract(value,process)
        self.assertEqual(events,[])
        self.assertEqual(failed['raw']['overflow_observation'],frozen)
        self.assertEqual(failed['observation']['active_append'],{'pair_sequence':8})
        self.assertEqual(json.dumps(value,sort_keys=True),before)
        raw['writer_queue']['queued_pairs']=0
        raw['disk_stages']['checkpoint_observation']['active_append']=None
        drained,_=trace.extract(value,process)
        self.assertEqual(drained['writer_queue']['queued_pairs'],0)
        self.assertEqual(drained['raw']['overflow_observation'],frozen)
        self.assertIsNone(drained['observation']['active_append'])
        self.assertNotIn('PRIVATE',json.dumps(drained))

    @unittest.skipUnless(sys.platform=='linux','actual procfs integration')
    def test_cli_is_read_only_and_profiles_only_owned_child_group(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            worker=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
            try:
                status=root/'status.json';status.write_text(json.dumps({'version':1,'kind':'capture-service-status','pid':worker.pid,'session_id':'synthetic','state':'ready','ready':True,'raw_recording':{'disk_stages':{'checkpoint_observation':{'version':1,'enabled':True,'latest_event_id':1,'events':[{'event_id':1,'phase':'payload_sync'}]}}}}))
                fake=root/'perf';fake.write_text('#!'+sys.executable+'\nimport sys,time,signal\nfrom pathlib import Path\nPath(sys.argv[sys.argv.index("-o")+1]).write_bytes(b"SYNTHETIC-PERF-NOT-REAL")\nsignal.signal(signal.SIGINT,lambda s,f:sys.exit(0))\ntime.sleep(30)\n');fake.chmod(0o700)
                for mode in ('off','fake','missing','disk'):
                    out=root/mode
                    args=[sys.executable,str(ROOT/'scripts/validation/checkpoint_probe.py'),'--host-status',str(status),'--pid',str(worker.pid),'--seconds','.35','--interval','.05','--thread-interval','.1','--output',str(out)]
                    if mode in ('fake','missing'):args+=['--perf','--perf-bin',str(fake if mode=='fake' else root/'absent')]
                    if mode=='disk':args+=['--disk','gdp-definitely-not-a-device']
                    result=subprocess.run(args,capture_output=True,text=True,timeout=15)
                    self.assertEqual(result.returncode,1 if mode in ('missing','disk') else 0,result.stderr+result.stdout)
                    summary=json.loads((out/'summary.json').read_text())
                    self.assertGreaterEqual(summary['samples'],3);self.assertEqual(summary['targets'][0]['lost_events_after_start'],0)
                    self.assertTrue(summary['targets'][0]['observation_supported'])
                    self.assertIsNone(worker.poll())
                    self.assertEqual((out/'samples.jsonl').stat().st_mode&0o777,0o600)
                    if mode=='fake':self.assertEqual(summary['perf']['state'],'recorded_not_validated')
            finally:
                worker.terminate();worker.wait(timeout=5)


class EventCadenceTests(unittest.TestCase):
    @staticmethod
    def published(t):
        # 30 FPS / 7 pairs per checkpoint, 11 completion events/checkpoint.
        # Each 500 ms publication can expose a burst, rather than uniform events.
        published=int(t/.5)*.5
        latest=int((published+1e-8)/(7/30))*11
        return {'session_id':'session','raw_recording':{'session_id':'raw','disk_stages':{
            'checkpoint_observation':{'version':1,'enabled':True,'latest_event_id':latest,
                'events':[{'event_id':i,'phase':'payload_sync'} for i in range(max(1,latest-63),latest+1)]}}}}

    def test_fast_polls_cover_64_ring_with_publication_phase_and_completion_bursts(self):
        process={'host_pid':1,'start_ticks':2}
        slow=probe.Trace();fast=probe.Trace();seen=[]
        for i in range(121):
            _,events=fast.extract(self.published(i*.2),process);seen += [v['event_id'] for v in events]
        for i in range(24):slow.extract(self.published(i*1.045),process)
        self.assertEqual(fast.lost,0)
        self.assertEqual(fast.before_start,0)
        self.assertEqual(len(seen),len(set(seen)))
        self.assertGreater(slow.lost,0)

    def test_internal_gaps_duplicates_conflicts_and_counter_reset(self):
        process={'host_pid':1,'start_ticks':2};trace=probe.Trace()
        def source(ids):
            value=self.published(0)
            value['raw_recording']['disk_stages']['checkpoint_observation'].update(
                latest_event_id=max(ids,default=0),events=[{'event_id':i,'phase':'payload_sync'} for i in ids])
            return value
        _,events=trace.extract(source([1,1,3]),process)
        self.assertEqual([v['event_id'] for v in events],[1,3]);self.assertEqual(trace.lost,1)
        self.assertEqual(trace.extract(source([1,3]),process)[1],[])
        with self.assertRaisesRegex(ValueError,'COUNTER_RESET'):trace.extract(source([1]),process)
        bad=source([4,4]);bad['raw_recording']['disk_stages']['checkpoint_observation']['events'][1]['phase']='readback'
        with self.assertRaisesRegex(ValueError,'CONFLICT'):trace.extract(bad,process)

    def test_fast_path_does_not_read_resource_counters_and_refuses_changed_identity(self):
        from unittest.mock import patch
        target=probe.Target.__new__(probe.Target)
        target.status=Path('/private/synthetic-status');target.trace=probe.Trace()
        target.last_process={'host_pid':123,'start_ticks':456}
        target.ns_pid=17;target.event_identity=(17,'session','raw')
        value=self.published(1);value.update(pid=17,updated_unix_ms=1)
        with patch.object(probe,'bounded_json',return_value=value),patch.object(probe,'read_process',side_effect=AssertionError('resource read')):
            result,events=target.events(2)
            self.assertGreater(len(events),0);self.assertEqual(result['epoch'],0)
            value['session_id']='changed'
            with self.assertRaisesRegex(ValueError,'IDENTITY_REFRESH'):target.events(3)
            value['session_id']='session';value['pid']=18
            with self.assertRaisesRegex(ValueError,'IDENTITY_REFRESH'):target.events(4)

    def test_fast_path_caches_only_the_same_published_file_version(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'status.json';value=self.published(1)
            value.update(version=1,kind='capture-service-status',pid=17,updated_unix_ms=1)
            path.write_text(json.dumps(value))
            target=probe.Target.__new__(probe.Target);target.status=path;target.trace=probe.Trace()
            target.last_process={'host_pid':123,'start_ticks':456};target.event_identity=(17,'session','raw')
            with patch.object(probe,'bounded_json',wraps=probe.bounded_json) as read:
                self.assertTrue(target.events(1)[1]);self.assertEqual(target.events(2)[1],[])
                self.assertEqual(read.call_count,1)
                value['pid']=18
                replacement=path.with_suffix('.tmp');replacement.write_text(json.dumps(value));replacement.replace(path)
                with self.assertRaisesRegex(ValueError,'IDENTITY_REFRESH'):target.events(3)
                with self.assertRaisesRegex(ValueError,'IDENTITY_REFRESH'):target.events(4)
                self.assertEqual(read.call_count,3)

if __name__=='__main__':unittest.main()
