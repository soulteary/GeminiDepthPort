"""Read-only tail correlation; observer identities and event dedup are real code."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('checkpoint_tail_analysis',ROOT/'experiments/sdk-2.10.6-diagnostics/analyze_checkpoint_tails.py')
analysis=importlib.util.module_from_spec(spec);spec.loader.exec_module(analysis)
probe_spec=importlib.util.spec_from_file_location('tail_checkpoint_probe',ROOT/'scripts/validation/checkpoint_probe.py')
probe=importlib.util.module_from_spec(probe_spec);probe_spec.loader.exec_module(probe)


def write(path,value,lines=False):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(''.join(json.dumps(v)+'\n' for v in value) if lines else json.dumps(value))


def event(event_id=1,checkpoint_id=7,start=1_000_000_000,wall=400_000_000,cpu=100_000,phase='verify_wait'):
    return {'event_id':event_id,'span_id':event_id,'info':{'checkpoint_id':checkpoint_id,'prefix_pairs':49},
            'start_monotonic_ns':start,'wall_ns':wall,'thread_cpu_ns':cpu,'phase':phase,'pid':17,'tid':19,'ok':True}


def sample(at,state='recording',epoch=0,pid=20,start=2):
    entry={'name':'c','epoch':epoch,'process':{'host_pid':pid,'start_ticks':start,'cpu_ticks':at//1000000},
           'rates':{'cpu_cores':.25},'status':{'raw':{'state':state,'reason':'RAW_QUEUE_OVERFLOW' if state=='failed' else None},
            'writer_queue':{'queued_pairs':15 if state=='failed' else 0,'inflight_pairs':1,'capacity_pairs':16}},'checkpoint_events':[]}
    return {'kind':'resource-sample','monotonic_ns':at,'unix_ns':at+100000000000,'phase':'sample','cameras':[entry],
            'disks':{'nvme0n1':{'write_ops':at//1000000,'write_sectors':at//1000000,'read_sectors':0,'io_ms':at//1000000}}}


def fixture(root):
    write(root/'capture/run.json',{'cameras':[{'name':'c'}]})
    e=event();a=sample(500_000_000);b=sample(1_500_000_000,'failed');b['cameras'][0]['checkpoint_events']=[e]
    write(root/'lifecycle/samples.jsonl',[a,b],True)
    write(root/'lifecycle/summary.json',{'sampling_ok':False,'errors':['c:CHECKPOINT_EVENTS_LOST:2']})
    fixed=[]
    for row in (a,b):
        value=copy.deepcopy(row);value['kind']='sample';entry=value.pop('cameras')[0];entry['index']=0;entry.pop('checkpoint_events');value['targets']=[entry];fixed.append(value)
    fixed.insert(1,{'kind':'checkpoint_event','target_index':0,'epoch':0,**e})
    write(root/'capture/checkpoint-samples/samples.jsonl',fixed,True)
    write(root/'capture/checkpoint-samples/summary.json',{'interrupted':True,'errors':{},'targets':[{'index':0,'lost_events_after_start':0}]})
    return root


class TailTests(unittest.TestCase):
    def test_real_trace_to_analysis_preserves_overflow_append_and_wait_reason(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d)/'evidence');wait=event();wait['wait_reason']='chunk_rotation'
            frozen={'version':1,'detected_monotonic_ns':1_350_000_000,
                'rejected_pair':{'pair_sequence':67,'color_sequence':71,'depth_sequence':72},
                'queue_at_detection':{'queued_pairs':16,'oldest_queued_age_ns':500_000_000}}
            append={'pair_sequence':50,'start_monotonic_ns':1_000_000_000}
            source={'session_id':'capture','raw_recording':{'session_id':'raw','state':'failed',
                'reason':'RAW_QUEUE_OVERFLOW','overflow_observation':frozen,
                'disk_stages':{'checkpoint_observation':{'version':1,'enabled':True,
                    'latest_event_id':1,'events':[wait],'active_append':append,'active':[]}}}}
            compact,events=probe.Trace().extract(source,{'host_pid':20,'start_ticks':2})
            path=root/'lifecycle/samples.jsonl';rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1]['cameras'][0].update(status=compact,checkpoint_events=events);write(path,rows,True)
            path=root/'capture/checkpoint-samples/samples.jsonl';rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1].update(wait);rows[2]['targets'][0]['status']=compact;write(path,rows,True)
            before={str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()}
            out=Path(d)/'derived';analysis.write_analysis(root,out,before_seconds=1,after_seconds=0)
            groups=json.loads((out/'checkpoints.json').read_text())
            summary=json.loads((out/'summary.json').read_text())
            resources=json.loads((out/'resources.json').read_text())
            self.assertEqual(groups[0]['events'][0]['wait_reason'],'chunk_rotation')
            self.assertEqual(groups[0]['events'][0]['sources'],['checkpoint','lifecycle'])
            self.assertEqual(summary['failure_first_observed']['raw']['overflow_observation'],frozen)
            self.assertTrue(any(row.get('active_append')==append for row in resources))
            self.assertEqual({str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()},before)

    def test_unknown_or_non_wait_reason_is_rejected_instead_of_silently_dropped(self):
        for reason,phase in (('unknown','verify_wait'),(False,'verify_wait'),('chunk_rotation','payload_sync')):
            with self.subTest(reason=reason,phase=phase),tempfile.TemporaryDirectory() as d:
                root=fixture(Path(d));path=root/'lifecycle/samples.jsonl'
                rows=[json.loads(v) for v in path.read_text().splitlines()]
                rows[1]['cameras'][0]['checkpoint_events'][0].update(wait_reason=reason,phase=phase)
                write(path,rows,True)
                with self.assertRaisesRegex(ValueError,'CHECKPOINT_WAIT_REASON'):analysis.analyze(root)

    def test_dedup_cpu_wall_queue_and_device_association_do_not_rewrite_inputs(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d)/'evidence');before={str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()}
            destination=Path(d)/'derived';result=analysis.write_analysis(root,destination,before_seconds=1,after_seconds=0)
            summary=json.loads((destination/'summary.json').read_text());groups=json.loads((destination/'checkpoints.json').read_text())
            self.assertTrue(result['analysis_only']);self.assertFalse(summary['hardware_acceptance_verified'])
            self.assertEqual(summary['deduplication']['unique_events_all_windows'],1)
            self.assertEqual(summary['deduplication']['across_observers'],1)
            self.assertEqual(groups[0]['phase_stats']['verify_wait']['wall_max_ns'],400_000_000)
            self.assertEqual(groups[0]['phase_stats']['verify_wait']['thread_cpu_max_ns'],100_000)
            self.assertEqual(groups[0]['events'][0]['sources'],['checkpoint','lifecycle'])
            self.assertEqual(groups[0]['disk_brackets']['lifecycle']['devices']['nvme0n1']['write_bytes'],512000)
            self.assertFalse(summary['source_summaries']['lifecycle']['sampling_ok'])
            self.assertTrue(summary['source_summaries']['checkpoint']['interrupted'])
            self.assertEqual({str(p):p.read_bytes() for p in root.rglob('*') if p.is_file()},before)
            for line in (destination/'SHA256SUMS').read_text().splitlines():
                digest,name=line.split('  ');self.assertEqual(hashlib.sha256((destination/name).read_bytes()).hexdigest(),digest)
            second=Path(d)/'derived-again';analysis.write_analysis(root,second,before_seconds=1,after_seconds=0)
            for name in ('summary.json','checkpoints.json','resources.json','SHA256SUMS'):
                self.assertEqual((destination/name).read_bytes(),(second/name).read_bytes())
            with self.assertRaisesRegex(ValueError,'FRESH_OUTPUT'):analysis.write_analysis(root,destination)
            with self.assertRaisesRegex(ValueError,'OUTSIDE_EVIDENCE'):analysis.write_analysis(root,root/'derived')

    def test_same_observer_repeats_are_counted_once(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d));path=root/'lifecycle/samples.jsonl'
            rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1]['cameras'][0]['checkpoint_events']*=2
            write(path,rows,True)
            summary,groups,_,_=analysis.analyze(root)
            self.assertEqual(summary['deduplication']['within_observer'],1)
            self.assertEqual(summary['deduplication']['across_observers'],1)
            self.assertEqual(groups[0]['phase_stats']['verify_wait']['events'],1)

    def test_conflicting_duplicates_are_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d));path=root/'capture/checkpoint-samples/samples.jsonl'
            rows=[json.loads(v) for v in path.read_text().splitlines()];rows[1]['wall_ns']+=1;write(path,rows,True)
            with self.assertRaisesRegex(ValueError,'CONFLICTING_DUPLICATE'):analysis.analyze(root)

    def test_conflicting_known_wait_reasons_are_not_deduplicated(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d));path=root/'lifecycle/samples.jsonl'
            rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1]['cameras'][0]['checkpoint_events'][0]['wait_reason']='chunk_rotation'
            write(path,rows,True)
            path=root/'capture/checkpoint-samples/samples.jsonl'
            rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1]['wait_reason']='explicit_checkpoint';write(path,rows,True)
            with self.assertRaisesRegex(ValueError,'CONFLICTING_DUPLICATE'):analysis.analyze(root)

    def test_observer_local_epoch_numbers_are_bound_to_worker_identity(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d));path=root/'capture/checkpoint-samples/samples.jsonl'
            rows=[json.loads(v) for v in path.read_text().splitlines()]
            for row in rows:
                if row['kind']=='checkpoint_event':row['epoch']=0
                else:row['targets'][0]['process']['start_ticks']=3;row['targets'][0]['process']['host_pid']=21
            write(path,rows,True)
            summary,groups,_,_=analysis.analyze(root)
            self.assertEqual(summary['deduplication']['unique_events_all_windows'],2)
            self.assertEqual({v['epoch'] for v in groups},{0,1})

    def test_new_fast_event_rows_and_missing_cpu_are_supported(self):
        with tempfile.TemporaryDirectory() as d:
            root=fixture(Path(d));path=root/'lifecycle/samples.jsonl';rows=[json.loads(v) for v in path.read_text().splitlines()]
            rows[1]['cameras'][0]['checkpoint_events']=[]
            rows.insert(1,{'kind':'event-sample','monotonic_ns':1_300_000_000,'cameras':[{'name':'c','epoch':0,'raw':{'state':'recording'},'checkpoint_events':[event(event_id=2,cpu=None,phase='payload_sync')]}]})
            write(path,rows,True)
            summary,groups,_,_=analysis.analyze(root)
            self.assertEqual(summary['deduplication']['unique_events_all_windows'],2)
            self.assertEqual(groups[0]['phase_stats']['payload_sync']['thread_cpu_available'],0)
            self.assertIsNone(groups[0]['phase_stats']['payload_sync']['thread_cpu_max_ns'])

    def test_no_failure_unbound_events_and_counter_regression_are_rejected(self):
        for fault in ('no-failure','unbound','disk'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                root=fixture(Path(d))
                if fault=='unbound':
                    path=root/'capture/checkpoint-samples/samples.jsonl';rows=[json.loads(v) for v in path.read_text().splitlines()];rows[1]['epoch']=9;write(path,rows,True)
                else:
                    for name in ('lifecycle/samples.jsonl','capture/checkpoint-samples/samples.jsonl'):
                        path=root/name;rows=[json.loads(v) for v in path.read_text().splitlines()]
                        for row in rows:
                            entries=row.get('cameras',row.get('targets',[]))
                            if fault=='no-failure':
                                for entry in entries:entry['status']['raw']['state']='recording'
                            elif row.get('monotonic_ns')==1_500_000_000:row['disks']['nvme0n1']['write_sectors']=0
                        write(path,rows,True)
                with self.assertRaises(ValueError):analysis.analyze(root,before_seconds=1,after_seconds=0)
