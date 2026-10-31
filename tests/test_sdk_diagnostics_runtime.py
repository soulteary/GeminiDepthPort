import argparse
import copy
import importlib.util
import json
import io
import hashlib
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
HERE=ROOT/'experiments/sdk-2.10.6-diagnostics'
sys.path.insert(0,str(HERE))
import experiment_runtime as rt
spec=importlib.util.spec_from_file_location('sdk_experiment_hardware',HERE/'hardware.py')
hw=importlib.util.module_from_spec(spec);spec.loader.exec_module(hw)
spec=importlib.util.spec_from_file_location('sdk_experiment_health',HERE/'health.py')
health=importlib.util.module_from_spec(spec);spec.loader.exec_module(health)


def fixture(root):
    def file(path,data=b'fixture'):
        target=rt.physical(root,path);target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(data)
        return target
    for path in (rt.LIBRARY,rt.SDK+'/lib/libOrbbecSDK.so.2.10.6',rt.SERVICE,rt.BRIDGE,rt.AUDITOR):file(path)
    for name in rt.REQUIRED_LICENSES:file(rt.SDK+'/'+name)
    for name in ('experiment_runtime.py','entrypoint.sh','health.py'):file(rt.HOME+'/'+name)
    file(rt.HOME+'/sdk-diagnostics.patch',(HERE/'sdk-diagnostics.patch').read_bytes())
    file(rt.HOME+'/public-c-exports.txt',(HERE/'public-c-exports.txt').read_bytes())
    headers={}
    for n in range(33):
        p=file(rt.SDK+'/include/header-'+str(n)+'.h')
        headers[p.name]=rt.sha(p)
    generated=file(rt.SDK+'/include/libobsensor/h/Export.h')
    installed=dict(headers,**{'libobsensor/h/Export.h':rt.sha(generated)})
    value={'version':1,'experiment':rt.EXPERIMENT,'repository':'https://github.com/orbbec/OrbbecSDK_v2',
           'commit':rt.BASE,'sdk_version':'2.10.6','patch_sha256':rt.PATCH,'sdk_sha256':rt.sha(rt.physical(root,rt.LIBRARY)),
           'public_c_exports':406,'public_header_count':33,'installed_public_header_count':34,
           'installed_public_headers_sha256':hashlib.sha256(json.dumps(installed,sort_keys=True).encode()).hexdigest(),'public_headers_sha256':hashlib.sha256(json.dumps(headers,sort_keys=True).encode()).hexdigest(),
           'public_exports_sha256':rt.sha(rt.physical(root,rt.HOME+'/public-c-exports.txt'))}
    file(rt.HOME+'/sdk-build.json',json.dumps(value).encode())
    return rt.make_manifest(root,'a'*40)


def source(name='a'):
    return {'version':1,'sdk_library':'/opt/orbbec-sdk/lib/libOrbbecSDK.so','serial':'SERIAL-'+name,
            'imu':{'enabled':True,'accel_frame_id':name+'_accel','gyro_frame_id':name+'_gyro'},'pairing':{'clock':'device','max_delta_us':20000},
            'ros':{'bridge':'/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so','namespace':'/'+name,'stamp_source':'sdk-global-unix'},
            'color':{'width':640,'height':480,'fps':30,'frame_id':name+'_color_optical'},
            'depth':{'width':640,'height':480,'fps':30,'frame_id':name+'_depth_optical','unit_policy':'normalize-mm'},
            'service':{'generation':'original','status_path':'/old/state/status.json','max_restarts':3,'shutdown_timeout_ms':5000,'recording_control_path':'/recording/control.json'},
            'acquisition':{'preset':'strict-rgbd'},'quality_policy':{'version':1},'quality_runtime':{'version':1},
            'raw_recording':{'queue_capacity':8,'checkpoint_pipeline':True,'checkpoint_observation':False}}


def health_config(cfg):
    return {'camera_name':cfg['ros']['namespace'][1:],'sample_seconds':8,'discovery_seconds':8,'freshness_seconds':1,
            'streams':[{'topic':cfg['ros']['namespace']+'/'+s+'/image_raw','frame_id':cfg[s]['frame_id'],
                        'width':cfg[s]['width'],'height':cfg[s]['height'],'encoding':e,'min_fps':24,
                        'required_reliability':'reliable'} for s,e in (('color','yuyv'),('depth','16UC1'))]}


def bound(manifest):
    cfg=hw.camera_config(source(),manifest,'b'*32,3600,128*1024**3)
    bind={'version':1,'experiment':rt.EXPERIMENT,'image_id':'sha256:'+'c'*64,'run_id':'b'*32,
          'instance_id':'d'*64,'diagnostics':True,'runtime_sha256':hw.digest(manifest)}
    env={'OB_GDP_DIAGNOSTICS':'1','OB_GDP_DIAGNOSTICS_DIR':'/sdk-diagnostics','GDP_CAPTURE_INSTANCE_ID':'d'*64}
    return cfg,bind,env


class RuntimeBindingTests(unittest.TestCase):
    def test_manifest_binds_actual_sdk_patch_all_artifacts_and_licenses(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest=fixture(root)
            self.assertEqual(rt.validate_manifest(manifest,root),manifest)
            self.assertNotIn('/opt/gdp-capture/runtime.json',manifest['files'])
            for name in (rt.LIBRARY,rt.BRIDGE,rt.SERVICE,rt.AUDITOR,rt.HOME+'/sdk-diagnostics.patch',rt.SDK+'/SDK-EULA.txt'):
                p=rt.physical(root,name);before=p.read_bytes();p.write_bytes(before+b'tamper')
                with self.subTest(name=name),self.assertRaises(ValueError):rt.validate_manifest(manifest,root)
                p.write_bytes(before)
            rt.physical(root,rt.SDK+'/unlisted-file').write_text('extra')
            with self.assertRaisesRegex(ValueError,'INVENTORY'):rt.validate_manifest(manifest,root)

    def test_manifest_rejects_source_binary_or_production_impersonation(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest=fixture(root)
            for key,value in (('kind','production'),('experiment','rust-v1'),('sdk_sha256','0'*64)):
                bad=copy.deepcopy(manifest);bad[key]=value
                with self.subTest(key=key),self.assertRaises(ValueError):rt.validate_manifest(bad,root)
            bad=copy.deepcopy(manifest);bad['sdk_source']['commit']='0'*40
            with self.assertRaises(ValueError):rt.validate_manifest(bad,root)
            escape=root/'outside';escape.write_text('x')
            rt.physical(root,rt.SDK+'/escape').symlink_to(escape)
            with self.assertRaisesRegex(ValueError,'SYMLINK_ESCAPE'):rt.inventory(root)

    def test_strict_json_limits_duplicates_and_nonfinite(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'value.json'
            for raw in ('{"a":1,"a":2}','{"a":NaN}','x'*(4*1024**2+1)):
                path.write_text(raw)
                with self.assertRaises(ValueError):rt.read_json(path)
        with self.assertRaisesRegex(ValueError,'SDK_PATCH_DIGEST'):rt.verify_patch(HERE/'README.md')
        rt.verify_patch(HERE/'sdk-diagnostics.patch')

    def test_config_digest_mode_instance_clock_and_runtime_contract_are_required(self):
        with tempfile.TemporaryDirectory() as d:
            manifest=fixture(Path(d));cfg,bind,env=bound(manifest)
            self.assertTrue(rt.validate_config(cfg,bind,manifest,env))
            for group,key,val in (('ros','stamp_source','sdk-system-unix'),('pairing','clock','system'),
                                  ('service','max_restarts',1),('imu','enabled',False),
                                  ('runtime_contract','sdk_sha256','0'*64),('raw_recording','storage_format','files-v2')):
                bad=copy.deepcopy(cfg);bad[group][key]=val
                with self.subTest(key=key),self.assertRaises(ValueError):rt.validate_config(bad,bind,manifest,env)
            for key,val in (('OB_GDP_DIAGNOSTICS','0'),('GDP_CAPTURE_INSTANCE_ID','f'*64),('LD_PRELOAD','/tmp/other.so')):
                with self.subTest(key=key),self.assertRaises(ValueError):rt.validate_config(cfg,bind,manifest,dict(env,**{key:val}))
            for key,val in (('runtime_sha256','0'*64),('diagnostics','true'),('image_id','mutable:latest')):
                with self.subTest(key=key),self.assertRaises(ValueError):rt.validate_config(cfg,dict(bind,**{key:val}),manifest,env)
            bind['diagnostics']=False;env['OB_GDP_DIAGNOSTICS']='0'
            self.assertTrue(rt.validate_config(cfg,bind,manifest,env))

    def test_health_rejects_empty_streams_wrong_source_and_unbounded_waits(self):
        cfg=source();health=health_config(cfg);rt.validate_health(cfg,health)
        for fault in ('empty','topic','frame','qos','timing'):
            bad=copy.deepcopy(health)
            if fault=='empty':bad['streams']=[]
            elif fault=='topic':bad['streams'][0]['topic']='/other/color/image_raw'
            elif fault=='frame':bad['streams'][0]['frame_id']='other_color'
            elif fault=='qos':bad['streams'][0]['required_reliability']='best_effort'
            else:bad['sample_seconds']=60
            with self.subTest(fault=fault),self.assertRaises(ValueError):rt.validate_health(cfg,bad)

    def test_startup_binds_config_health_and_private_diagnostic_owner_before_device_open(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);manifest=fixture(root);cfg,bind,env=bound(manifest)
            cp=root/'config';cp.mkdir();hw.write(cp/'capture.json',cfg);hw.write(cp/'runtime.json',manifest)
            hw.write(cp/'health.json',health_config(cfg))
            bind.update(capture_sha256=rt.sha(cp/'capture.json'),health_sha256=rt.sha(cp/'health.json'))
            hw.write(cp/'experiment.json',bind);diag=root/'sdk-diagnostics';diag.mkdir(mode=0o700)
            with patch.dict(os.environ,env,clear=True),patch.object(rt.subprocess,'run') as call:
                rt.validate(root);call.assert_called_once()
                (cp/'health.json').write_text('{}')
                with self.assertRaisesRegex(ValueError,'HEALTH_CONFIG_BINDING'):rt.validate(root)
                hw.write(cp/'health.json',health_config(cfg));diag.chmod(0o755)
                with self.assertRaisesRegex(ValueError,'PRIVATE_DIAGNOSTIC'):rt.validate(root)


class HardwareTests(unittest.TestCase):
    def prepare_fixture(self,root,mode='on'):
        manifest=fixture(root/'image');cameras=[]
        for name in ('a','b','c'):
            cp=root/('source-'+name);cp.mkdir();hw.write(cp/'capture.json',source(name));hw.write(cp/'health.json',health_config(source(name)))
            cameras.append({'name':name,'capture_config':str(cp/'capture.json')})
        hw.write(root/'plan.json',{'cameras':cameras})
        a=argparse.Namespace(plan=root/'plan.json',image='experiment:tag',run_dir=root/'run',diagnostics_root=root/'tmpfs',
                             mode=mode,pairs=3600,max_bytes=128*1024**3,ros_domain_id=78)
        return a,manifest

    def test_prepare_isolated_three_cameras_keeps_sources_and_quality_unchanged(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a,manifest=self.prepare_fixture(root)
            before=(root/'source-a/capture.json').read_bytes()
            with patch.object(hw,'image_manifest',return_value=('sha256:'+'c'*64,manifest)),patch.object(hw,'command',return_value='{}') as command:
                result=hw.prepare(a);self.assertFalse(result['hardware_started']);self.assertEqual(command.call_count,3)
                run=hw.load_run(a.run_dir)
                cfg=rt.read_json(a.run_dir/'a/config/capture.json');self.assertEqual(cfg['quality_runtime'],source()['quality_runtime'])
                self.assertEqual(cfg['service']['recording_control_path'],'/state/control.json')
                self.assertEqual(cfg['acquisition'],source()['acquisition']);self.assertEqual(cfg['raw_recording']['queue_capacity'],8)
                self.assertTrue(cfg['raw_recording']['checkpoint_pipeline']);self.assertTrue(cfg['raw_recording']['checkpoint_observation'])
                compose=rt.read_json(a.run_dir/'compose.json');self.assertEqual(len(compose['services']),3)
                for service in compose['services'].values():
                    self.assertEqual(service['image'],run['image_id']);self.assertTrue(service['read_only'])
                    self.assertEqual(service['restart'],'no');self.assertNotIn('privileged',service)
                    self.assertTrue(any('/config:ro' in v for v in service['volumes']))
                    self.assertFalse(any('docker.sock' in v for v in service['volumes']))
                self.assertEqual((root/'source-a/capture.json').read_bytes(),before)
                (a.run_dir/'a/config/capture.json').write_text('{}')
                with self.assertRaisesRegex(ValueError,'PREPARED_FILE_CHANGED'):hw.load_run(a.run_dir)

    def test_rollback_metadata_load_does_not_depend_on_broken_image_or_changed_config(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);a,manifest=self.prepare_fixture(root)
            with patch.object(hw,'image_manifest',return_value=('sha256:'+'c'*64,manifest)),patch.object(hw,'command',return_value='{}'):
                hw.prepare(a)
            (a.run_dir/'a/config/capture.json').write_text('changed')
            with patch.object(hw,'image_manifest',side_effect=ValueError('broken image')) as image:
                run=hw.load_run(a.run_dir,verify=False)
                self.assertEqual(run['image_id'],'sha256:'+'c'*64);image.assert_not_called()
                with self.assertRaisesRegex(ValueError,'PREPARED_FILE_CHANGED'):hw.load_run(a.run_dir)

    def test_duplicate_serial_or_missing_imu_rejected_before_hardware_start(self):
        for fault in ('serial','imu'):
            with tempfile.TemporaryDirectory() as d:
                a,manifest=self.prepare_fixture(Path(d));cfg=source('c')
                if fault=='serial':cfg['serial']='SERIAL-a'
                else:cfg['imu']['enabled']=False
                hw.write(Path(d)/'source-c/capture.json',cfg)
                with patch.object(hw,'image_manifest',return_value=('sha256:'+'c'*64,manifest)),self.assertRaises(ValueError):hw.prepare(a)
                self.assertFalse(a.run_dir.exists())

    def test_stop_checks_all_identities_before_signals_and_never_uses_production_name(self):
        run={'run_id':'a'*32,'image_id':'sha256:'+'c'*64,'cameras':[{'container':'exp-'+v} for v in ('a','b','c')]}
        infos=[{'Id':'immutable-'+str(n),'State':{'Running':True}} for n in range(3)]
        with patch.object(hw,'own_container',side_effect=infos),patch.object(hw,'command',side_effect=['exp-a\nexp-b\nexp-c','stopped','stopped','stopped']) as call:
            self.assertFalse(hw.stop(run)['production_changed'])
            self.assertEqual([v.args[0][-1] for v in call.call_args_list[1:]],[v['Id'] for v in infos])
        with patch.object(hw,'own_container',side_effect=[infos[0],ValueError('mismatch')]),patch.object(hw,'command',return_value='exp-a\nexp-b\nexp-c') as call:
            with self.assertRaises(ValueError):hw.stop(run)
            self.assertEqual(call.call_count,1)
        with patch.object(hw,'own_container',return_value=infos[0]),patch.object(hw,'command',side_effect=['exp-a','stopped']) as call:
            self.assertEqual(len(hw.stop(run)['stopped']),1)
            self.assertEqual(call.call_args.args[0][-1],infos[0]['Id'])
        info={'Config':{'Labels':{hw.LABEL:rt.EXPERIMENT,hw.RUN:'other'}},'Image':run['image_id'],'HostConfig':{'RestartPolicy':{'Name':'no'}}}
        with patch.object(hw,'command',return_value=json.dumps([info])),self.assertRaisesRegex(ValueError,'IDENTITY_CHANGED'):
            hw.own_container(run,run['cameras'][0])

    def test_start_refuses_competing_usb_and_reused_run(self):
        a=argparse.Namespace(execute=True,run_dir=Path('/unused'))
        run={'cameras':[]}
        competing=[{'Name':'production-camera','Mounts':[{'Destination':'/dev/bus/usb'}]}]
        with patch.object(hw.sys,'platform','linux'),patch.object(Path,'is_dir',return_value=True),patch.object(hw,'command',side_effect=['container',json.dumps(competing)]) as command:
            with self.assertRaisesRegex(ValueError,'USB_CAPTURE_ALREADY_RUNNING'):hw.start(a,run)
            self.assertEqual(command.call_count,2)
        a.execute=False
        with self.assertRaises(ValueError):hw.start(a,run)

    def test_sample_commands_point_to_real_tools_and_keep_domain_and_namespaces(self):
        a=argparse.Namespace(seconds=120,run_dir=Path('/evidence'))
        run={'image_id':'sha256:'+'c'*64,'ros_domain_id':78,'cameras':[
            {'name':v,'container':'exp-'+v,'namespace':'/'+v,'diagnostics':'/dev/shm/'+v} for v in ('a','b','c')]}
        argv=hw.sample_commands(a,run)
        self.assertNotIn('sample',argv['checkpoint_argv'])
        self.assertIn('ROS_DOMAIN_ID=78',argv['ros_observer_argv'])
        self.assertIn(rt.HOME+'/ros/imu_long_run_observer.py',argv['ros_observer_argv'])
        self.assertEqual(argv['sdk_argv'].count('--source'),3)

    def test_status_cannot_redirect_auditor_to_other_run(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);cp=root/'a/config';cp.mkdir(parents=True)
            hw.write(cp/'experiment.json',{'run_id':'b'*32,'instance_id':'d'*64,'capture_sha256':'e'*64})
            s={'generation':'b'*32,'instance_id':'d'*64,'config_sha256':'e'*64,
               'raw_recording':{'partial_path':'/recordings/capture-abc.partial','job_path':'/recordings/capture-abc'}}
            a=argparse.Namespace(run_dir=root);c={'name':'a'}
            self.assertEqual(hw.recording_path(a,c,s,True),root/'a/recordings/capture-abc.partial')
            s['raw_recording']['partial_path']='/recordings/../other/capture-abc.partial'
            with self.assertRaises(ValueError):hw.recording_path(a,c,s,True)
            s['generation']='other'
            with self.assertRaises(ValueError):hw.recording_path(a,c,s,True)


class NativeAuditControlTests(unittest.TestCase):
    def case(self,root,recovered=False):
        cp=root/'a/config';cp.mkdir(parents=True)
        binding={'run_id':'b'*32,'instance_id':'d'*64,'capture_sha256':'e'*64}
        hw.write(cp/'experiment.json',binding)
        raw={'partial_path':'/recordings/capture-abc.partial','job_path':'/recordings/capture-abc','state':'complete',
             'whole_pair_queue_overflows':0,'written_pairs':2,'max_pairs':2,'imu_samples_submitted':4}
        status={'generation':'b'*32,'instance_id':'d'*64,'config_sha256':'e'*64,'raw_recording':raw}
        hw.write(root/'a/status.json',status)
        path=root/'a/recordings'/('capture-abc.partial' if recovered else 'capture-abc');path.mkdir(parents=True)
        hw.write(path/'checkpoint.json',{'record_count':2})
        if not recovered:(path/'COMPLETE').touch()
        else:hw.write(root/'a/crash.json',{'container_id':'exact-id','checkpoint':{'record_count':1}})
        a=argparse.Namespace(run_dir=root,camera='a',recovered=recovered,max_audit_mib=16)
        run={'image_id':'sha256:'+'c'*64,'cameras':[{'name':'a','status':str(root/'a/status.json')}]}
        summary={'kind':'summary','audit_version':2,'native_hashes_verified':True,'independent_audit_passes':2,
                 'ok':True,'recovered':recovered,'pairs':2,'imu':[2,2]}
        return a,run,path,summary

    def child(self,summary):
        class Child:
            stdout=io.StringIO(json.dumps(summary)+'\n')
            def wait(self,*args,**kwargs):return 0
            def poll(self):return 0
        return Child()

    def test_normal_and_recovery_keep_two_reads_hash_counts_readonly_and_checkpoint(self):
        for recovered in (False,True):
            with self.subTest(recovered=recovered),tempfile.TemporaryDirectory() as d:
                a,run,path,summary=self.case(Path(d),recovered);before=(path/'checkpoint.json').read_bytes()
                with patch.object(hw,'own_container',return_value={'Id':'exact-id','State':{'Running':False}}), \
                     patch.object(hw.subprocess,'Popen',return_value=self.child(summary)) as popen, \
                     patch.object(hw.subprocess,'run',return_value=argparse.Namespace(returncode=1)) as refusal:
                    result=hw.audit(a,run);self.assertTrue(result['ok'])
                    args=popen.call_args.args[0];self.assertIn('--read-only',args)
                    self.assertIn('type=bind,src='+str(path)+',dst=/recording,readonly',args)
                    self.assertEqual((path/'checkpoint.json').read_bytes(),before)
                    self.assertEqual(refusal.call_count,int(recovered))
                    if recovered:self.assertFalse((path/'COMPLETE').exists())

    def test_audit_missing_second_pass_hash_or_counts_cannot_be_accepted(self):
        for key,value in (('independent_audit_passes',1),('native_hashes_verified',False),('imu',[1,2]),('pairs',1)):
            with self.subTest(key=key),tempfile.TemporaryDirectory() as d:
                a,run,path,summary=self.case(Path(d));summary[key]=value
                with patch.object(hw,'own_container',return_value={'Id':'exact-id','State':{'Running':False}}), \
                     patch.object(hw.subprocess,'Popen',return_value=self.child(summary)),self.assertRaises(ValueError):hw.audit(a,run)

    def test_ordinary_reader_accepting_partial_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            a,run,path,summary=self.case(Path(d),True)
            with patch.object(hw,'own_container',return_value={'Id':'exact-id','State':{'Running':False}}), \
                 patch.object(hw.subprocess,'Popen',return_value=self.child(summary)), \
                 patch.object(hw.subprocess,'run',return_value=argparse.Namespace(returncode=0)), \
                 self.assertRaisesRegex(ValueError,'NORMAL_READER_ACCEPTED'):hw.audit(a,run)


class DiagnosticHealthTests(unittest.TestCase):
    def status(self):
        return {'message_timestamp':'global','imu':{'ready':True,'shared_ros_context':True,'error':None,'published':[10,10]},
                'restart_attempt':0,'publish_failures':0,'stamp_failures':0,'pid':12,
                'raw_recording':{'state':'recording','whole_pair_queue_overflows':0}}

    def test_health_requires_actual_current_worker_sdk_snapshots_when_enabled(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);s=self.status();binding={'diagnostics':True}
            with self.assertRaises(ValueError):health.validate_extra(s,binding,root)
            snap={'pid':12,'sdk_base':rt.BASE,'snapshot_monotonic_ns':time.monotonic_ns(),'sink_errors':0,'counters':{'input_frames':10}}
            hw.write(root/'sdk-diag-12.json',snap);health.validate_extra(s,binding,root)
            for field,value in (('pid',99),('snapshot_monotonic_ns',time.monotonic_ns()-20_000_000_000),('sink_errors',1)):
                bad=dict(snap,**{field:value});hw.write(root/'sdk-diag-12.json',bad)
                with self.subTest(field=field),self.assertRaises(ValueError):health.validate_extra(s,binding,root)
            health.validate_extra(s,{'diagnostics':False},root)

    def test_imu_clock_context_errors_overflow_and_frozen_counters_fail_health(self):
        for group,key,value in (('imu','shared_ros_context',False),('imu','error','overflow'),('imu','published',[0,1]),
                                ('raw_recording','whole_pair_queue_overflows',1),('raw_recording','state','failed')):
            s=self.status();s[group][key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):health.validate_extra(s,{'diagnostics':False},Path('/unused'))


if __name__=='__main__':unittest.main()
