"""Host runtime identity regressions. No Docker or camera is invoked."""
import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

HERE=Path(__file__).resolve().parents[1]/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('sdk_runtime_identity_hardware',HERE/'hardware.py')
hw=importlib.util.module_from_spec(spec);spec.loader.exec_module(hw)
import run_hardware as controller
import sdk_lifecycle


class RuntimeIdentityTests(unittest.TestCase):
    def case(self,root):
        (root/'a/config').mkdir(parents=True)
        binding={'run_id':'b'*32,'instance_id':'d'*64,'capture_sha256':'e'*64,'image_id':'sha256:'+'c'*64}
        status={'generation':'b'*32,'config_sha256':'e'*64,'instance_id':'f'*64,'session_id':'a'*32,
                'pid':17,'restart_attempt':0,'updated_unix_ms':time.time_ns()//10**6,'state':'ready'}
        camera={'name':'a','container':'exp-a'}
        run={'run_id':binding['run_id'],'image_id':binding['image_id'],'cameras':[camera]}
        hw.write(root/'run.json',run);hw.write(root/'a/config/experiment.json',binding)
        info={'Id':'owned-container','Image':binding['image_id'],
              'State':{'Running':True,'StartedAt':'2026-10-31T00:00:00Z','Pid':100}}
        process={'container_init_pid':100,'pid_namespace':'pid:[42]','worker_host_pid':117,'worker_start_ticks':300,
                 'supervisor_host_pid':107,'supervisor_namespace_pid':7,'supervisor_start_ticks':200}
        return argparse.Namespace(run_dir=root),camera,status,binding,info,process

    def pin(self,case):
        a,c,s,b,i,p=case
        with patch.object(hw,'own_container',return_value=i),patch.object(hw,'observe_runtime_process',return_value=p):
            return hw.validate_runtime_identity(a,c,s,b)

    def test_live_supervisor_instance_differs_from_launch_and_is_pinned_once(self):
        with tempfile.TemporaryDirectory() as d:
            case=self.case(Path(d));a,c,s,b,i,p=case
            result=self.pin(case)
            self.assertNotEqual(result['instance_id'],result['launch_instance_id'])
            path=Path(d)/'a/runtime-identity.json';before=path.read_bytes()
            self.assertEqual(path.stat().st_mode&0o777,0o600)
            self.assertEqual(self.pin(case),result);self.assertEqual(path.read_bytes(),before)
            stopped=copy.deepcopy(i);stopped['State']['Running']=False;stopped['State']['Pid']=0
            with patch.object(hw,'own_container',return_value=stopped),patch.object(hw,'observe_runtime_process') as probe:
                self.assertEqual(hw.validate_runtime_identity(a,c,dict(s,state='stopped'),b),result)
                probe.assert_not_called()

    def test_no_pin_cannot_be_learned_from_stopped_container(self):
        with tempfile.TemporaryDirectory() as d:
            a,c,s,b,i,p=self.case(Path(d));i['State']['Running']=False;i['State']['Pid']=0
            with patch.object(hw,'own_container',return_value=i),patch.object(hw,'observe_runtime_process') as probe:
                with self.assertRaisesRegex(ValueError,'NOT_PINNED'):hw.validate_runtime_identity(a,c,s,b)
                probe.assert_not_called()
            self.assertFalse((Path(d)/'a/runtime-identity.json').exists())

    def test_status_identity_changes_and_restarts_rejected_without_repinning(self):
        with tempfile.TemporaryDirectory() as d:
            case=self.case(Path(d));a,c,s,b,i,p=case;self.pin(case)
            before=(Path(d)/'a/runtime-identity.json').read_bytes()
            for key,value in [('instance_id','1'*64),('session_id','2'*32),('pid',18),('restart_attempt',1),
                              ('generation','3'*32),('config_sha256','4'*64),('instance_id','bad')]:
                with self.subTest(key=key),patch.object(hw,'own_container',return_value=i),self.assertRaises(ValueError):
                    hw.validate_runtime_identity(a,c,dict(s,**{key:value}),b)
            self.assertEqual((Path(d)/'a/runtime-identity.json').read_bytes(),before)

    def test_container_replacement_restart_and_pid_reuse_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            case=self.case(Path(d));a,c,s,b,i,p=case;self.pin(case)
            for changed in [dict(i,Id='other'),dict(i,Image='sha256:'+'1'*64),dict(i,State=dict(i['State'],StartedAt='later'))]:
                with patch.object(hw,'own_container',return_value=changed),self.assertRaisesRegex(ValueError,'IDENTITY_CHANGED'):
                    hw.validate_runtime_identity(a,c,s,b)
            with patch.object(hw,'own_container',return_value=i),patch.object(hw,'observe_runtime_process',return_value=dict(p,worker_start_ticks=301)),self.assertRaisesRegex(ValueError,'IDENTITY_CHANGED'):
                hw.validate_runtime_identity(a,c,s,b)

    def test_first_pin_requires_fresh_live_status_and_stable_container(self):
        with tempfile.TemporaryDirectory() as d:
            a,c,s,b,i,p=self.case(Path(d))
            with patch.object(hw,'own_container',return_value=i),self.assertRaisesRegex(ValueError,'NOT_LIVE'):
                hw.validate_runtime_identity(a,c,dict(s,updated_unix_ms=0),b)
            changed=dict(i,State=dict(i['State'],StartedAt='later'))
            with patch.object(hw,'own_container',side_effect=[i,changed]),patch.object(hw,'observe_runtime_process',return_value=p),self.assertRaisesRegex(ValueError,'CONTAINER_CHANGED'):
                hw.validate_runtime_identity(a,c,s,b)
            self.assertFalse((Path(d)/'a/runtime-identity.json').exists())

    def test_private_pin_not_symlink_or_public_and_concurrent_pin_cannot_replace_it(self):
        with tempfile.TemporaryDirectory() as d:
            case=self.case(Path(d));self.pin(case);path=Path(d)/'a/runtime-identity.json'
            original=hw._read_runtime_identity(path)
            self.assertEqual(hw._pin_runtime_identity(path,{'other':'identity'}),original)
            path.chmod(0o644)
            with self.assertRaisesRegex(ValueError,'PRIVATE_RUNTIME_IDENTITY'):hw._read_runtime_identity(path)
            path.chmod(0o600);link=Path(d)/'link';link.symlink_to(path)
            with self.assertRaises(OSError):hw._read_runtime_identity(link)

    def proc_fixture(self,root):
        binary=root/'binary';binary.write_bytes(b'fixture')
        def make(pid,namespace,parent,start,args,token,children):
            p=root/str(pid);p.mkdir();(p/'ns').mkdir();(p/'ns/pid').symlink_to('pid:[42]')
            fields=['0']*45;fields[0]='S';fields[1]=str(parent);fields[19]=str(start)
            (p/'stat').write_text(f'{pid} (capture-service) '+' '.join(fields))
            (p/'status').write_text(f'NSpid:\t{pid}\t{namespace}\n')
            (p/'cmdline').write_bytes(b'\0'.join(x.encode() for x in args)+b'\0')
            (p/'environ').write_bytes(b'PRIVATE_UNRELATED=not-returned\0GDP_CAPTURE_INSTANCE_ID='+token.encode()+b'\0')
            (p/'exe').symlink_to(binary);expected=p/'root'/hw.rt.SERVICE.lstrip('/');expected.parent.mkdir(parents=True);expected.symlink_to(binary)
            (p/'task'/str(pid)).mkdir(parents=True);(p/'task'/str(pid)/'children').write_text(' '.join(map(str,children)))
        make(100,1,0,100,['tini'],'d'*64,[107])
        make(107,7,100,200,[hw.rt.SERVICE,'--config','/config/capture.json'],'d'*64,[117])
        make(117,17,107,300,[hw.rt.SERVICE,'--worker','--config','/config/capture.json',
             '--expected-config-sha','e'*64,'--restart-attempt','0','--supervisor-pid','7'],'f'*64,[])
        return {'State':{'Pid':100}},{'pid':17,'instance_id':'f'*64},{'instance_id':'d'*64,'capture_sha256':'e'*64}

    def test_proc_observer_finds_exact_worker_and_validates_both_environment_tokens(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);info,s,b=self.proc_fixture(proc)
            result=hw.observe_runtime_process(info,s,b,proc=proc)
            self.assertEqual(result['worker_host_pid'],117);self.assertEqual(result['supervisor_host_pid'],107)
            self.assertNotIn('not-returned',json.dumps(result))
            self.assertEqual(hw.observe_runtime_process(info,s,b,previous=result,proc=proc),result)
            for pid in (107,117):
                path=proc/str(pid)/'environ';before=path.read_bytes();path.write_bytes(b'GDP_CAPTURE_INSTANCE_ID=wrong\0')
                with self.subTest(pid=pid),self.assertRaisesRegex(ValueError,'PROCESS_INSTANCE'):
                    hw.observe_runtime_process(info,s,b,proc=proc)
                path.write_bytes(before)

    def test_proc_observer_rejects_zombie_duplicate_token_and_worker_argument_changes(self):
        faults = [('stat', b') S ', b') Z '),
                  ('environ', b'PRIVATE_UNRELATED=not-returned', b'GDP_CAPTURE_INSTANCE_ID=duplicate'),
                  ('cmdline', b'--restart-attempt\0' + b'0', b'--restart-attempt\0' + b'1'),
                  ('cmdline', b'--supervisor-pid\0' + b'7', b'--supervisor-pid\0' + b'8'),
                  ('cmdline', b'e' * 64, b'a' * 64)]
        for filename, before, after in faults:
            with self.subTest(filename=filename, after=after), tempfile.TemporaryDirectory() as d:
                proc=Path(d);info,s,b=self.proc_fixture(proc)
                path=proc/'117'/filename
                original=path.read_bytes();self.assertIn(before,original)
                path.write_bytes(original.replace(before,after))
                with self.assertRaises(ValueError):hw.observe_runtime_process(info,s,b,proc=proc)

    def test_process_replaced_during_observation_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);info,s,b=self.proc_fixture(proc)
            original=hw._process_record
            calls=0
            def read(pid, root):
                nonlocal calls
                record=original(pid,root);calls+=1
                if calls==3:record['start_ticks']+=1
                return record
            with patch.object(hw,'_process_record',side_effect=read),self.assertRaisesRegex(ValueError,'PROCESS_REPLACED'):
                hw.observe_runtime_process(info,s,b,proc=proc)

    def test_proc_observer_rejects_foreign_namespace_wrong_config_and_wrong_executable(self):
        with tempfile.TemporaryDirectory() as d:
            proc=Path(d);info,s,b=self.proc_fixture(proc)
            path=proc/'117/ns/pid';path.unlink();path.symlink_to('pid:[foreign]')
            with self.assertRaisesRegex(ValueError,'WORKER_NOT_FOUND'):hw.observe_runtime_process(info,s,b,proc=proc)
            path.unlink();path.symlink_to('pid:[42]')
            args=proc/'117/cmdline';before=args.read_bytes();args.write_bytes(before.replace(b'/config/capture.json',b'/other.json'))
            with self.assertRaisesRegex(ValueError,'PROCESS_CONFIG'):hw.observe_runtime_process(info,s,b,proc=proc)
            args.write_bytes(before)
            binary=proc/'other-binary';binary.write_bytes(b'wrong');exe=proc/'117/exe';exe.unlink();exe.symlink_to(binary)
            with self.assertRaisesRegex(ValueError,'PROCESS_EXECUTABLE'):hw.observe_runtime_process(info,s,b,proc=proc)


class TerminalStatusTests(unittest.TestCase):
    """Real Rust status fields through the real controller/identity/procfs chain.

    Docker inspect is the only substituted boundary. The process files, private
    pins, config binding and their disappearance/replacement are actual files.
    """
    def case(self,root):
        fixture=json.loads((HERE.parents[1]/'tests/fixtures/sdk_terminal_status.json').read_text())
        self.assertEqual(fixture['capture']['raw_recording']['reason'],'RAW_QUEUE_OVERFLOW')
        self.assertEqual(fixture['capture']['imu']['error'],'IMU_QUEUE_OVERFLOW')
        base=RuntimeIdentityTests();a,_,identity,binding,info,_=base.case(root)
        proc=root/'proc';proc.mkdir();base.proc_fixture(proc)
        a.pairs=31680
        cameras=[{'name':name,'container':'exp-'+name} for name in 'abc']
        run={'run_id':binding['run_id'],'image_id':binding['image_id'],'cameras':cameras}
        hw.write(root/'run.json',run)
        infos={};rows=[]
        for camera in cameras:
            name=camera['name'];config=root/name/'config';config.mkdir(parents=True,exist_ok=True)
            hw.write(config/'experiment.json',binding)
            current=copy.deepcopy(info);current['Id']='owned-'+name
            current.update(Config={'Labels':{controller.hw.LABEL:hw.rt.EXPERIMENT,controller.hw.RUN:run['run_id']}},
                           HostConfig={'RestartPolicy':{'Name':'no'}})
            infos[camera['container']]=current
            status=copy.deepcopy(fixture['capture'])
            for key in ('generation','config_sha256','instance_id','session_id','pid','restart_attempt','updated_unix_ms'):
                status[key]=identity[key]
            status['raw_recording'].update(session_id=identity['session_id'],
                job_path='/recordings/capture-'+identity['session_id'],
                partial_path='/recordings/capture-'+identity['session_id']+'.partial')
            rows.append({'name':name,'running':True,'health':'healthy','capture':status})
        return a,run,{'cameras':rows},proc,infos

    def docker(self,infos):
        def command(argv,**kwargs):
            self.assertEqual(argv[:2],['docker','inspect'])
            return json.dumps([infos[argv[2]]])
        return patch.object(controller.hw,'command',side_effect=command)

    def pin(self,a,run,failed,proc):
        healthy=copy.deepcopy(failed)
        for row in healthy['cameras']:
            status=row['capture'];status.update(state='ready',ready=True)
            status['imu'].update(error=None,ready=True)
            status['raw_recording'].update(state='recording',reason=None,whole_pair_queue_overflows=0)
        self.assertEqual(controller.check_rows(a,run,healthy,proc=proc),(True,False))
        return healthy

    def test_real_rust_failure_survives_worker_disappearance_and_keeps_all_cameras(self):
        with tempfile.TemporaryDirectory() as d:
            a,run,failed,proc,infos=self.case(Path(d))
            with self.docker(infos):
                self.pin(a,run,failed,proc)
                pins={name:(Path(d)/name/'runtime-identity.json').read_bytes() for name in 'abc'}
                (proc/'117/stat').unlink()
                with self.assertRaises(controller.CaptureStatusFailure) as caught:
                    controller.check_rows(a,run,failed,sampling=True,proc=proc)
            details=caught.exception.details['cameras']
            self.assertEqual([value['camera'] for value in details],list('abc'))
            for value in details:
                self.assertEqual(value['classifications'],['RECORDING_FAILED','IMU_FAILED'])
                self.assertEqual(value['raw_reason'],'RAW_QUEUE_OVERFLOW')
                self.assertEqual(value['imu_error'],'IMU_QUEUE_OVERFLOW')
                self.assertIn('FileNotFoundError',value['liveness']['process_error'])
                self.assertTrue(value['status_identity_pinned'])
            self.assertEqual(pins,{name:(Path(d)/name/'runtime-identity.json').read_bytes() for name in 'abc'})

    def test_stopped_container_failure_remains_bound_but_unpinned_failure_cannot_pin(self):
        for pinned in (False,True):
            with self.subTest(pinned=pinned),tempfile.TemporaryDirectory() as d:
                a,run,failed,proc,infos=self.case(Path(d))
                with self.docker(infos):
                    if pinned:self.pin(a,run,failed,proc)
                    for value in infos.values():value['State']['Running']=False;value['State']['Pid']=0
                    for row in failed['cameras']:row['running']=False
                    if pinned:
                        with self.assertRaises(controller.CaptureStatusFailure) as caught:
                            controller.check_rows(a,run,failed,proc=proc)
                        self.assertTrue(all(v['liveness']['process_error']=='RUNTIME_CONTAINER_EXITED' for v in caught.exception.details['cameras']))
                    else:
                        with self.assertRaisesRegex(ValueError,'NOT_PINNED'):controller.check_rows(a,run,failed,proc=proc)
                        self.assertFalse((Path(d)/'a/runtime-identity.json').exists())

    def test_failure_cannot_bypass_status_container_or_process_identity_changes(self):
        for fault in ('no-pin','session','config','pid','container','container-start','pid-reuse'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                a,run,failed,proc,infos=self.case(Path(d))
                with self.docker(infos):
                    if fault!='no-pin':self.pin(a,run,failed,proc)
                    status=failed['cameras'][0]['capture']
                    if fault=='session':status['session_id']='2'*32
                    elif fault=='config':status['config_sha256']='2'*64
                    elif fault=='pid':status['pid']=18
                    elif fault=='container':infos['exp-a']['Id']='replacement'
                    elif fault=='container-start':infos['exp-a']['State']['StartedAt']='later'
                    elif fault=='pid-reuse':
                        path=proc/'117/stat';path.write_text(path.read_text().replace(' 300 ',' 301 '))
                    with self.assertRaises(ValueError) as caught:controller.check_rows(a,run,failed,proc=proc)
                    self.assertNotIsInstance(caught.exception,controller.CaptureStatusFailure)

    def test_healthy_status_still_requires_live_worker_and_rejects_pid_reuse(self):
        for fault in ('disappeared','pid-reuse','container-stopped'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as d:
                a,run,failed,proc,infos=self.case(Path(d))
                with self.docker(infos):
                    healthy=self.pin(a,run,failed,proc)
                    path=proc/'117/stat'
                    if fault=='disappeared':path.unlink()
                    elif fault=='pid-reuse':path.write_text(path.read_text().replace(' 300 ',' 301 '))
                    else:infos['exp-a']['State']['Running']=False;infos['exp-a']['State']['Pid']=0
                    with self.assertRaises((OSError,ValueError)) as caught:
                        controller.check_rows(a,run,healthy,sampling=True,proc=proc)
                    self.assertNotIsInstance(caught.exception,controller.CaptureStatusFailure)

    def test_failure_only_api_cannot_be_used_as_a_healthy_path(self):
        with tempfile.TemporaryDirectory() as d:
            a,run,failed,proc,infos=self.case(Path(d))
            with self.docker(infos):
                healthy=self.pin(a,run,failed,proc)
                with self.assertRaisesRegex(ValueError,'TERMINAL_FAILURE_STATUS_REQUIRED'):
                    controller.hw.failed_recording_status(a,run['cameras'][0],healthy['cameras'][0]['capture'],proc=proc)

    def test_snapshot_observer_cannot_mask_bound_storage_failure_or_authenticate_unpinned_status(self):
        class ResourceTarget:
            def sample(self,now):return {'status':{'updated_unix_ms':time.time_ns()//1_000_000}},[]
        for pinned in (True,False):
            with self.subTest(pinned=pinned),tempfile.TemporaryDirectory() as d:
                root=Path(d);a,run,failed,proc,infos=self.case(root)
                a.root=root
                with self.docker(infos):
                    if pinned:self.pin(a,run,failed,proc)
                for camera,row in zip(run['cameras'],failed['cameras']):
                    status=row['capture'];status.update(state='ready',ready=True)
                    status['imu'].update(error=None,ready=True,failure_observation=None,drain_interval={'samples':1})
                    status['status_persistence']={'errors':0,'submission_errors':0,'queue_wait':{},'critical_wait':{},
                        'critical_event_capacity':16,'critical_events':[]}
                    status['cleanup']={'history_capacity':8,'completed':[]}
                    status['raw_recording'].update(reason='RAW_STORAGE_FAILED',disk_stages={'failed_calls':{'fsync':1}})
                    camera['status']=str(root/camera['name']/'state/status.json')
                    Path(camera['status']).parent.mkdir();hw.write(Path(camera['status']),status)
                hw.write(root/'run.json',run)
                with self.docker(infos):
                    (proc/'117/stat').unlink()
                    runner=controller.Runner(a);runner.run=run;runner.stage='sample'
                    runner.lifecycle=sdk_lifecycle.Lifecycle(root/'lifecycle',run,dict.fromkeys('abc',128),
                        lambda:runner.stage,factory=lambda _:ResourceTarget())
                    try:
                        snapshot=runner.snapshot('latest-status')
                        with self.assertRaises(ValueError) as caught:
                            controller.check_rows(a,run,snapshot,sampling=True,proc=proc)
                        if pinned:
                            self.assertIsInstance(caught.exception,controller.CaptureStatusFailure)
                            for camera in caught.exception.details['cameras']:
                                self.assertEqual(camera['raw_reason'],'RAW_STORAGE_FAILED')
                                self.assertEqual(camera['disk_failed_calls'],{'fsync':1})
                                self.assertIn('FileNotFoundError',camera['liveness']['process_error'])
                        else:
                            self.assertNotIsInstance(caught.exception,controller.CaptureStatusFailure)
                            self.assertIn('NOT_PINNED',str(caught.exception))
                    finally:summary=runner.lifecycle.stop()
                self.assertFalse(summary['sampling_ok'])
                self.assertTrue(any('LIFECYCLE_DISK_FAILURE' in error for error in summary['errors']))
                self.assertTrue((root/'latest-status.json').exists())
                self.assertEqual((root/'a/runtime-identity.json').exists(),pinned)


if __name__=='__main__':unittest.main()
