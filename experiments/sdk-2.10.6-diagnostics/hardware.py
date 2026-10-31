#!/usr/bin/env python3
"""Prepare/start/sample/audit/stop an isolated three-camera SDK experiment on Linux.

No production SDK, configuration, management gate or existing container is changed.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import time
import uuid

import experiment_runtime as rt

LABEL = 'org.geminidepthport.sdk-experiment'
RUN = 'org.geminidepthport.sdk-experiment-run'
HERE = Path(__file__).resolve().parent


def digest(value):
    return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()


def write(path,value):
    path.write_text(json.dumps(value,sort_keys=True,indent=2)+'\n')
    path.chmod(0o600)


def command(args,timeout=60):
    try:
        return subprocess.check_output(args,text=True,timeout=timeout,stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as error:
        raise ValueError('COMMAND_FAILED:'+str(error.returncode)+':'+error.output[-4096:]) from error


def image_manifest(image):
    info=json.loads(command(['docker','image','inspect',image]))[0]
    labels=info['Config'].get('Labels') or {}
    rt.require(labels.get(LABEL)==rt.EXPERIMENT and labels.get('org.geminidepthport.capture-backend')==rt.ROLE,'EXPERIMENT_IMAGE_REQUIRED')
    identity=info['Id']
    rt.require(re.fullmatch('sha256:[0-9a-f]{64}',identity) is not None,'IMAGE_ID')
    manifest=json.loads(command(['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL',
                                 '--entrypoint','cat',identity,rt.MANIFEST]))
    rt.require(manifest['experiment']==rt.EXPERIMENT and manifest['kind']=='gdp-sdk-diagnostics-runtime'
               and manifest['sdk_source']['commit']==rt.BASE and manifest['sdk_source']['patch_sha256']==rt.PATCH
               and manifest['gdp_revision']==labels.get('org.opencontainers.image.revision'),'IMAGE_MANIFEST_IDENTITY')
    result=json.loads(command(['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL',
                             '--tmpfs','/tmp:rw,nosuid,nodev,size=128m',identity,
                             'python3',rt.HOME+'/experiment_runtime.py','smoke'],timeout=120).splitlines()[-1])
    rt.require(result.get('ok') is True and result.get('hardware_opened') is False,'IMAGE_SMOKE_FAILED')
    return identity,manifest


def camera_config(source,manifest,run_id,pair_limit,byte_limit):
    result=copy.deepcopy(source)
    rt.require(source['imu']['enabled'] is True and source['pairing']['clock']=='device'
               and source['ros']['stamp_source']=='sdk-global-unix'
               and source.get('quality_policy') and source.get('quality_runtime'),'VERIFIED_IMU_GLOBAL_QUALITY_CONFIG_REQUIRED')
    result['sdk_library']=rt.LIBRARY
    result['ros']['bridge']=rt.BRIDGE
    result['runtime_contract']={key:manifest[key] for key in ('sdk_sha256','bridge_sha256','service_sha256')}
    result['service'].update(status_path='/state/status.json',generation=run_id,max_restarts=0)
    if 'recording_control_path' in result['service']:result['service']['recording_control_path']='/state/control.json'
    # Preserve the tested queue/checkpoint settings when supplied. Never enable pipeline implicitly.
    raw=copy.deepcopy(source.get('raw_recording') or {})
    raw.update(directory='/recordings',name='capture',storage_format='chunked-v3',max_pairs=pair_limit,max_bytes=byte_limit)
    raw.setdefault('queue_capacity',16)
    raw.setdefault('checkpoint_pipeline',False)
    raw['checkpoint_observation']=True
    result['raw_recording']=raw
    if result.get('usb_recovery'):
        helper='/opt/gdp-ros/bin/gdp-usb-recover'
        result['usb_recovery']={'helper':helper,'sha256':manifest['files'][helper]}
    return result


def prepare(a):
    rt.require(a.run_dir.is_absolute() and not a.run_dir.exists() and a.diagnostics_root.is_absolute()
               and not a.diagnostics_root.exists(),'FRESH_ABSOLUTE_DIRECTORIES_REQUIRED')
    rt.require(1<=a.ros_domain_id<=232 and a.pairs>0 and a.max_bytes>0,'EXPLICIT_DOMAIN_AND_RECORDING_LIMITS')
    plan=rt.read_json(a.plan);cameras=plan['cameras']
    rt.require(len(cameras)==3,'THREE_CAMERAS_REQUIRED')
    sources=[];health=[]
    for c in cameras:
        rt.require(re.fullmatch('[a-z][a-z0-9-]{0,23}',c['name']) is not None,'CAMERA_NAME')
        path=Path(c['capture_config']);hp=Path(c.get('health_config',str(path.with_name('health.json'))))
        rt.require(path.is_absolute() and hp.is_absolute(),'ABSOLUTE_SOURCE_CONFIG_REQUIRED')
        src=rt.read_json(path);rt.validate_health(src,rt.read_json(hp))
        sources.append(src);health.append(hp.read_bytes())
    for key,values in (('name',[c['name'] for c in cameras]),('serial',[s['serial'] for s in sources]),
                       ('namespace',[s['ros']['namespace'] for s in sources])):
        rt.require(len(set(values))==3,'DUPLICATE_'+key)
    frames=[s[stream]['frame_id'] for s in sources for stream in ('color','depth')]
    frames += [s['imu'][kind+'_frame_id'] for s in sources for kind in ('accel','gyro')]
    rt.require(len(set(frames))==12,'DUPLICATE_FRAME_ID')
    identity,manifest=image_manifest(a.image)
    run_id=uuid.uuid4().hex
    configs=[camera_config(s,manifest,run_id,a.pairs,a.max_bytes) for s in sources]
    a.run_dir.mkdir(mode=0o700,parents=True);a.diagnostics_root.mkdir(mode=0o700,parents=True)
    compose={'name':'gdp-sdkdiag-'+run_id[:12],'services':{}}
    records=[];files={}
    for c,source,config,hraw in zip(cameras,sources,configs,health):
        name=c['name'];root=a.run_dir/name;root.mkdir(mode=0o700)
        for part in ('config','state','recordings'):(root/part).mkdir(mode=0o700)
        diagnostics=a.diagnostics_root/name;diagnostics.mkdir(mode=0o700)
        cp=root/'config';write(cp/'capture.json',config);write(cp/'runtime.json',manifest)
        (cp/'health.json').write_bytes(hraw);(cp/'health.json').chmod(0o600)
        instance=uuid.uuid4().hex+uuid.uuid4().hex
        binding={'version':1,'experiment':rt.EXPERIMENT,'image_id':identity,'run_id':run_id,'instance_id':instance,
                 'runtime_sha256':digest(manifest),'diagnostics':a.mode=='on','capture_sha256':rt.sha(cp/'capture.json'),
                 'health_sha256':rt.sha(cp/'health.json'),'source_config_sha256':rt.sha(Path(c['capture_config']))}
        write(cp/'experiment.json',binding)
        env={'OB_GDP_DIAGNOSTICS':str(int(a.mode=='on')),'OB_GDP_DIAGNOSTICS_DIR':'/sdk-diagnostics',
             'GDP_CAPTURE_INSTANCE_ID':instance,'ROS_DOMAIN_ID':str(a.ros_domain_id)}
        rt.validate_config(config,binding,manifest,env)
        # Full manifest, digest and Rust config checks run without USB or network access.
        check=['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL',
               '--user',str(os.getuid())+':'+str(os.getgid()),'--tmpfs','/tmp:rw,nosuid,nodev,size=128m',
               '-v',str(cp)+':/config:ro','-v',str(diagnostics)+':/sdk-diagnostics:rw']
        for key,value in env.items():check+=['-e',key+'='+value]
        command(check+[identity,'python3',rt.HOME+'/experiment_runtime.py','validate'],timeout=120)
        container='gdp-sdkdiag-'+run_id[:12]+'-'+name
        mounts=[str(cp)+':/config:ro',str(root/'state')+':/state:rw',str(root/'recordings')+':/recordings:rw',
                str(diagnostics)+':/sdk-diagnostics:rw','/dev/bus/usb:/dev/bus/usb:rw','/run/udev:/run/udev:ro']
        compose['services'][name]={'image':identity,'pull_policy':'never','container_name':container,'init':True,
            'restart':'no','read_only':True,'cap_drop':['ALL'],'security_opt':['no-new-privileges:true'],
            'user':str(os.getuid())+':'+str(os.getgid()),'group_add':[str(v) for v in os.getgroups()],
            'network_mode':'host','ipc':'private','device_cgroup_rules':['c 189:* rmw'],
            'tmpfs':['/tmp:rw,nosuid,nodev,size=256m'],'volumes':mounts,'environment':env,
            'labels':{LABEL:rt.EXPERIMENT,RUN:run_id},'stop_signal':'SIGINT',
            'stop_grace_period':str(max(30,(config['service']['shutdown_timeout_ms']+999)//1000+5))+'s',
            'logging':{'driver':'json-file','options':{'max-size':'10m','max-file':'2'}},
            'healthcheck':{'test':['CMD','python3',rt.HOME+'/health.py'],'interval':'30s','timeout':'30s',
                           'start_period':'60s','retries':3}}
        for p in cp.iterdir():files[str(p.relative_to(a.run_dir))]=rt.sha(p)
        records.append({'name':name,'container':container,'namespace':config['ros']['namespace'],
                        'diagnostics':str(diagnostics),'status':str(root/'state/status.json')})
    write(a.run_dir/'compose.json',compose);files['compose.json']=rt.sha(a.run_dir/'compose.json')
    metadata={'version':1,'experiment':rt.EXPERIMENT,'run_id':run_id,'image_id':identity,'runtime_sha256':digest(manifest),
              'mode':a.mode,'ros_domain_id':a.ros_domain_id,'cameras':records,'files':files,'created_unix_ns':time.time_ns()}
    write(a.run_dir/'run.json',metadata)
    return {'ok':True,'prepared':True,'hardware_started':False,'run':str(a.run_dir/'run.json'),'image_id':identity}


def load_run(root,verify=True):
    root=root.resolve();run=rt.read_json(root/'run.json')
    rt.require(run['experiment']==rt.EXPERIMENT and re.fullmatch('[0-9a-f]{32}',run['run_id']) is not None,'RUN_IDENTITY')
    rt.require(len(run['cameras'])==3 and len({c['container'] for c in run['cameras']})==3,'THREE_CAMERAS_REQUIRED')
    rt.require(re.fullmatch('sha256:[a-f0-9]{64}',run['image_id']) is not None,'RUN_IMAGE_ID')
    # Stopping must remain possible when config files changed or an image self-test fails.
    # Actual container role/run/image identities are still all rechecked before signals.
    if not verify:return run
    for name,value in run['files'].items():
        rt.require(not Path(name).is_absolute() and '..' not in Path(name).parts and rt.sha(root/name)==value,'PREPARED_FILE_CHANGED')
    identity,manifest=image_manifest(run['image_id'])
    rt.require(identity==run['image_id'] and digest(manifest)==run['runtime_sha256'],'RUN_IMAGE_CHANGED')
    return run


def own_container(run,camera):
    info=json.loads(command(['docker','inspect',camera['container']]))[0]
    labels=info['Config'].get('Labels') or {}
    rt.require(labels.get(LABEL)==rt.EXPERIMENT and labels.get(RUN)==run['run_id']
               and info['Image']==run['image_id'] and info['HostConfig']['RestartPolicy']['Name']=='no','CONTAINER_IDENTITY_CHANGED')
    return info


def start(a,run):
    rt.require(a.execute and sys.platform.startswith('linux'),'EXPLICIT_EXECUTE_ON_LINUX_REQUIRED')
    rt.require(Path('/dev/bus/usb').is_dir() and Path('/run/udev').is_dir(),'LINUX_USB_UDEV_REQUIRED')
    # Refuse competing USB users. Production protection/stop must use its normal management flow.
    ids=command(['docker','ps','-q']).split()
    if ids:
        for info in json.loads(command(['docker','inspect']+ids)):
            if any(m['Destination']=='/dev/bus/usb' for m in info.get('Mounts',[])):
                raise ValueError('USB_CAPTURE_ALREADY_RUNNING:'+info['Name'])
    for c in run['cameras']:
        rt.require(not (a.run_dir/c['name']/'state/status.json').exists()
                   and not any((a.run_dir/c['name']/'recordings').iterdir()),'RUN_ALREADY_USED_PREPARE_A_NEW_RUN')
    command(['docker','compose','-f',str(a.run_dir/'compose.json'),'up','-d','--no-build','--pull','never'],timeout=120)
    return {'ok':True,'started':True,'acceptance_verified':False}


def stop(run):
    # Partial Compose startup may leave only one or two containers. Missing is safe;
    # an existing name with a different identity is never safe to stop.
    names=set(command(['docker','ps','-a','--format','{{.Names}}']).splitlines())
    infos=[own_container(run,c) for c in run['cameras'] if c['container'] in names]
    for info in infos:
        if info['State']['Running']:command(['docker','stop','--time','35',info['Id']],timeout=120)
    return {'ok':True,'stopped':[i['Id'] for i in infos],'recordings_retained':True,'production_changed':False}


def status(run):
    rows=[]
    for c in run['cameras']:
        info=own_container(run,c)
        row={'name':c['name'],'container_id':info['Id'],'running':info['State']['Running'],
             'health':info['State'].get('Health',{}).get('Status','none')}
        path=Path(c['status'])
        if path.exists():row['capture']=rt.read_json(path)
        rows.append(row)
    return {'ok':True,'cameras':rows,'acceptance_verified':False}


def recording_path(a,c,status,recovered):
    binding=rt.read_json(a.run_dir/c['name']/'config/experiment.json')
    rt.require(status['generation']==binding['run_id'] and status['instance_id']==binding['instance_id']
               and status['config_sha256']==binding['capture_sha256'],'STATUS_BINDING')
    raw=status['raw_recording'];path=Path(raw['partial_path' if recovered else 'job_path'])
    rt.require(path.parent==Path('/recordings') and path.name.startswith('capture-')
               and path.name.endswith('.partial')==recovered,'RECORDING_STATUS_PATH')
    return a.run_dir/c['name']/'recordings'/path.name


def audit(a,run):
    results=[]
    for c in run['cameras']:
        if a.camera and c['name']!=a.camera:continue
        info=own_container(run,c);rt.require(not info['State']['Running'],'STOP_CONTAINER_BEFORE_AUDIT')
        s=rt.read_json(c['status']);path=recording_path(a,c,s,a.recovered)
        raw=s['raw_recording']
        rt.require(raw['whole_pair_queue_overflows']==0,'RECORDING_OVERFLOW')
        if a.recovered:rt.require(not (path/'COMPLETE').exists(),'RECOVERY_MUST_NOT_FABRICATE_COMPLETE')
        else:
            rt.require(raw['state']=='complete' and raw['written_pairs']==raw['max_pairs'],'NORMAL_RECORDING_NOT_COMPLETE')
        checkpoint=(path/'checkpoint.json').read_bytes();cp=json.loads(checkpoint)
        injection=path.parent.parent/'crash.json'
        if a.recovered:
            rt.require(injection.is_file(),'RECORDED_CRASH_EVIDENCE_REQUIRED')
            crash=rt.read_json(injection)
            rt.require(crash['container_id']==info['Id'] and cp['record_count']>=crash['checkpoint']['record_count'],'RECOVERY_PREFIX_REGRESSED')
        out=path.parent.parent/('recovered-audit.jsonl' if a.recovered else 'audit.jsonl')
        args=['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL','--tmpfs','/tmp:rw,nosuid,nodev,size=64m',
              '--user',str(os.getuid())+':'+str(os.getgid()),'--mount','type=bind,src='+str(path)+',dst=/recording,readonly',
              '--entrypoint',rt.AUDITOR,run['image_id'],'/recording']+(['--recovered'] if a.recovered else [])
        summary=None;used=0
        with out.open('x') as dest, out.with_suffix('.stderr').open('x') as err:
            child=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=err,text=True)
            try:
                while True:
                    line=child.stdout.readline(4*1024*1024+1)
                    if not line:break
                    used+=len(line.encode());rt.require(used<=a.max_audit_mib*1024**2 and len(line)<=4*1024**2,'AUDIT_OUTPUT_LIMIT')
                    row=json.loads(line);rt.require(summary is None,'DATA_AFTER_SUMMARY')
                    if row['kind']=='summary':summary=row
                    dest.write(line)
                rt.require(child.wait()==0,'NATIVE_AUDIT_FAILED')
            finally:
                if child.poll() is None:child.kill();child.wait()
                child.stdout.close()
        rt.require(summary and summary.get('audit_version')==2 and summary.get('ok') is True
                   and summary.get('native_hashes_verified') is True and summary.get('independent_audit_passes')==2
                   and summary.get('recovered')==a.recovered,'AUDIT_INCOMPLETE')
        rt.require((path/'checkpoint.json').read_bytes()==checkpoint,'AUDIT_CHANGED_CHECKPOINT')
        if a.recovered:
            rt.require(summary['pairs']==cp['record_count'] and not (path/'COMPLETE').exists(),'RECOVERED_PREFIX_COUNT')
            refusal=subprocess.run(args[:-1],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=120)
            rt.require(refusal.returncode!=0,'NORMAL_READER_ACCEPTED_INCOMPLETE_RECORDING')
        else:rt.require(summary['pairs']==raw['written_pairs'] and sum(summary['imu'])==raw['imu_samples_submitted'],'FINAL_SAMPLE_COUNTS')
        results.append({'camera':c['name'],'summary':summary,'output':str(out),'checkpoint_sha256':hashlib.sha256(checkpoint).hexdigest()})
    rt.require(results,'CAMERA_NOT_FOUND')
    return {'ok':True,'audits':results,'hardware_delivery_certified':False}


def verify_dds(a,run):
    # Reuse the current mainline pure comparator, not the old pinned-commit launcher.
    sys.path.insert(0,str(HERE.parents[1]/'scripts/validation'))
    from imu_long_test import compare_native
    rt.require(0<=a.trim_seconds<=60,'TRIM_LIMIT')
    dbpath=a.run_dir/'wire.sqlite3'
    summary=rt.read_json(str(dbpath)+'.summary.json')
    rt.require(summary.get('ok') is True,'DDS_OBSERVER_FAILED')
    results=[]
    with sqlite3.connect(dbpath.resolve().as_uri()+'?mode=ro',uri=True) as db:
        for c in run['cameras']:
            rt.require(not own_container(run,c)['State']['Running'],'STOP_BEFORE_DDS_VERIFICATION')
            result=compare_native(db,c['namespace'],a.run_dir/c['name']/'audit.jsonl',a.trim_seconds)
            result['camera']=c['name'];results.append(result)
    report={'ok':True,'cameras':results,'trim_seconds':a.trim_seconds,
            'scope':'two native reads and DDS sample/hash/Global/context comparison; source sequence gaps reported separately'}
    write(a.run_dir/'dds-native-comparison.json',report)
    return report


def crash(a,run):
    rt.require(a.execute,'EXPLICIT_EXECUTE_REQUIRED')
    c=next(c for c in run['cameras'] if c['name']==a.camera);info=own_container(run,c)
    rt.require(info['State']['Running'],'CRASH_TARGET_NOT_RUNNING')
    s=rt.read_json(c['status']);rt.require(s['ready'] is True and s['raw_recording']['state'] not in ('complete','failed'),'ACTIVE_RECORDING_REQUIRED')
    path=recording_path(a,c,s,True);rt.require(not (path/'COMPLETE').exists(),'ACTIVE_PARTIAL_REQUIRED')
    evidence=path.parent.parent/'crash.json';rt.require(not evidence.exists(),'CRASH_EVIDENCE_EXISTS')
    cp=(path/'checkpoint.json').read_bytes()
    write(evidence,{'container_id':info['Id'],'status':s,'checkpoint':json.loads(cp),'checkpoint_sha256':hashlib.sha256(cp).hexdigest(),'time_ns':time.time_ns()})
    rt.require(own_container(run,c)['Id']==info['Id'],'CONTAINER_CHANGED')
    command(['docker','kill','--signal','KILL',info['Id']])
    return {'ok':True,'injected':True,'recovery_verified':False,'next':'audit --recovered --camera '+a.camera}


def sample_commands(a,run):
    # Print argv arrays; no shell interpolation or implicit hardware start.
    root=HERE.parents[1]
    targets=[v for c in run['cameras'] for v in ('--target',c['container']+':/state/status.json')]
    sources=[v for c in run['cameras'] for v in ('--source',c['name']+'='+c['diagnostics'])]
    return {'checkpoint_argv':['python3',str(root/'scripts/validation/checkpoint_probe.py'),*targets,
                               '--seconds',str(a.seconds),'--output',str(a.run_dir/'checkpoint-samples')],
            'sdk_argv':['python3',str(HERE/'collect.py'),*sources,'--seconds',str(a.seconds),
                        '--output',str(a.run_dir/'sdk-samples')],
            'ros_observer_argv':['docker','run','--rm','--network','host','--read-only','--cap-drop','ALL',
                '--user',str(os.getuid())+':'+str(os.getgid()),'--tmpfs','/tmp:rw,nosuid,nodev,size=128m',
                '-e','ROS_DOMAIN_ID='+str(run['ros_domain_id']),'-v',str(a.run_dir)+':/evidence:rw',run['image_id'],
                'python3',rt.HOME+'/ros/imu_long_run_observer.py','--namespaces',*[c['namespace'] for c in run['cameras']],
                '--seconds',str(a.seconds),'--output','/evidence/wire.sqlite3']}


def sample(a,run):
    rt.require(1<=a.seconds<=86400,'SAMPLING_DURATION')
    for c in run['cameras']:
        info=own_container(run,c)
        rt.require(info['State']['Running'] and info['State'].get('Health',{}).get('Status')=='healthy','CAPTURE_NOT_HEALTHY')
        s=rt.read_json(c['status']);recording_path(a,c,s,True)
        rt.require(s['ready'] is True and s['message_timestamp']=='global' and s['imu']['shared_ros_context'] is True
                   and 0<=time.time_ns()//10**6-s['updated_unix_ms']<=5000,'CAPTURE_NOT_FRESH')
    argv=sample_commands(a,run)
    if run['mode']=='off':argv.pop('sdk_argv')
    if a.perf:argv['checkpoint_argv']+=['--perf','--perf-seconds','30','--perf-frequency','49']
    for disk in a.disk:argv['checkpoint_argv']+=['--disk',disk]
    children=[];logs=[]
    try:
        for name,args in argv.items():
            log=(a.run_dir/(name+'.log')).open('x');logs.append(log)
            children.append((name,subprocess.Popen(args,stdout=log,stderr=subprocess.STDOUT)))
        results={name:child.wait(timeout=a.seconds+180) for name,child in children}
        write(a.run_dir/'sampling.json',{'results':results,'commands':argv,'diagnostics_mode':run['mode']})
        rt.require(all(v==0 for v in results.values()),'SAMPLER_FAILED_SEE_LOGS')
        return {'ok':True,'results':results,'recording_verified':False}
    finally:
        for _,child in children:
            if child.poll() is None:child.terminate();child.wait(timeout=20)
        for log in logs:log.close()


def main():
    os.umask(0o077)
    p=argparse.ArgumentParser(description=__doc__);subs=p.add_subparsers(dest='action',required=True)
    s=subs.add_parser('prepare');s.add_argument('--plan',type=Path,required=True);s.add_argument('--image',required=True)
    s.add_argument('--diagnostics-root',type=Path,required=True);s.add_argument('--mode',choices=('on','off'),required=True)
    s.add_argument('--pairs',type=int,required=True);s.add_argument('--max-bytes',type=int,required=True);s.add_argument('--ros-domain-id',type=int,required=True)
    for name in ('start','status','stop','rollback','audit','crash','sample-commands','sample','verify-dds'):subs.add_parser(name)
    for name,s in subs.choices.items():
        s.add_argument('--run-dir',type=Path,required=True)
        if name in ('start','crash'):s.add_argument('--execute',action='store_true')
        if name in ('audit','crash'):s.add_argument('--camera',required=name=='crash')
        if name=='audit':s.add_argument('--recovered',action='store_true');s.add_argument('--max-audit-mib',type=int,default=2048)
        if name in ('sample-commands','sample'):s.add_argument('--seconds',type=int,default=120)
        if name=='verify-dds':s.add_argument('--trim-seconds',type=float,default=5)
        if name=='sample':s.add_argument('--perf',action='store_true');s.add_argument('--disk',action='append',default=[])
    a=p.parse_args();a.run_dir=a.run_dir.absolute()
    try:
        if a.action=='prepare':result=prepare(a)
        else:
            run=load_run(a.run_dir,verify=a.action not in ('stop','rollback'))
            if a.action=='start':result=start(a,run)
            elif a.action in ('stop','rollback'):result=stop(run)
            elif a.action=='status':result=status(run)
            elif a.action=='audit':result=audit(a,run)
            elif a.action=='crash':result=crash(a,run)
            elif a.action=='sample':result=sample(a,run)
            elif a.action=='verify-dds':result=verify_dds(a,run)
            else:result=sample_commands(a,run)
        print(json.dumps(result,indent=2));return 0
    except Exception as e:
        print(json.dumps({'ok':False,'code':'SDK_EXPERIMENT_FAILED','detail':str(e)}),file=sys.stderr);return 1

if __name__=='__main__':raise SystemExit(main())
