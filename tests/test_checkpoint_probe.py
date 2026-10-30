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

if __name__=='__main__':unittest.main()
