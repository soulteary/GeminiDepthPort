#!/usr/bin/env python3
"""Alternating, frozen-config SDK on/off rounds. No production control or deletion."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import sys
import time

import ab_metrics as metrics
import experiment_runtime as rt
import hardware as hw
import run_hardware as one

HERE=Path(__file__).resolve().parent


def schedule(rounds,first):
    rt.require(3<=rounds<=10,'AT_LEAST_THREE_ROUNDS_MAX_TEN')
    modes=(first,'off' if first=='on' else 'on')
    return [{'round':r,'mode':mode,'name':mode+'-'+str(r).zfill(2)} for r in range(1,rounds+1) for mode in modes]


def freeze(plan,root):
    source=rt.read_json(plan);rt.require(len(source['cameras'])==3,'THREE_CAMERAS_REQUIRED')
    frozen={'cameras':[]};files={};configs={}
    for camera in source['cameras']:
        name=camera['name'];rt.require(re.fullmatch('[a-z][a-z0-9-]{0,23}',name) is not None and name not in configs,'CAMERA_NAME')
        directory=root/'frozen'/name;directory.mkdir(mode=0o700,parents=True)
        cp=Path(camera['capture_config']);hp=Path(camera.get('health_config',str(cp.with_name('health.json'))))
        rt.require(cp.is_absolute() and hp.is_absolute(),'ABSOLUTE_SOURCE_PATHS')
        for source_path,target,limit in ((cp,directory/'capture.json',65536),(hp,directory/'health.json',4*1024**2)):
            with source_path.open('rb') as reader:data=reader.read(limit+1)
            rt.require(len(data)<=limit,'SOURCE_LIMIT')
            target.write_bytes(data);target.chmod(0o600)
            files[str(target.relative_to(root))]=rt.sha(target)
        rt.validate_health(rt.read_json(directory/'capture.json'),rt.read_json(directory/'health.json'))
        configs[name]=rt.sha(directory/'capture.json')
        frozen['cameras'].append({'name':name,'capture_config':str(directory/'capture.json'),'health_config':str(directory/'health.json')})
    one.atomic(root/'frozen/plan.json',frozen);files['frozen/plan.json']=rt.sha(root/'frozen/plan.json')
    return files,configs


def check_frozen(root,manifest):
    for name,digest in manifest['frozen_files'].items():
        path=root/name
        rt.require(not Path(name).is_absolute() and '..' not in Path(name).parts
                   and not path.is_symlink() and rt.sha(path)==digest,'FROZEN_CONFIG_CHANGED')


def forward(a,mode,root,image):
    args=['run','--plan',str(a.root/'frozen/plan.json'),'--image',image,'--root',str(root),
          '--mode',mode,'--seconds',str(a.seconds),'--max-bytes',str(a.max_bytes),'--ros-domain-id',str(a.ros_domain_id),
          '--healthy-timeout',str(a.healthy_timeout),'--audit-timeout',str(a.audit_timeout),
          '--poll-seconds',str(a.poll_seconds),'--trim-seconds',str(a.trim_seconds),'--execute']
    if a.pairs is not None:args+=['--pairs',str(a.pairs)]
    if a.completion_timeout is not None:args+=['--completion-timeout',str(a.completion_timeout)]
    for disk in a.disk:args+=['--disk',disk]
    if a.perf:args+=['--perf']
    return args


def render(root,report):
    one.atomic(root/'summary.json',report)
    lines=['# SDK on/off batch','',f"Complete: {report['complete']}; accepted groups: {report['accepted_groups']}",'',
           'CPU/RSS: sampled workers. P99/verification: whole native recording. SDK off: unavailable, not zero.',
           'P99 values are histogram bounds; worst-camera upper bounds are not pooled P99. No significance claim.','']
    for group in report['groups']:
        lines.append('- '+group['name']+': '+('accepted' if group.get('summary',{}).get('ok') else 'failed/incomplete'))
    if report['comparison']:
        lines+=['','## Descriptive medians','']
        for key,value in report['comparison'].items():lines.append(f"- {key}: on={value['on_median']}, off={value['off_median']}")
    (root/'summary.md').write_text('\n'.join(lines)+'\n')


def rollback(a,mark_abort=True):
    result=rt.read_json(a.root/'result.json')
    rt.require(result['uid']==os.getuid() and result['gid']==os.getgid(),'BATCH_UID_GID_MISMATCH')
    if mark_abort:one.atomic(a.root/'ABORT.json',{'token':result['run_token'],'time_ns':time.time_ns()})
    errors=one.stop_saved_children(a.root/'controller',result['run_token'])
    outputs=[]
    for group in result['groups']:
        rt.require(re.fullmatch('(on|off)-[0-9]{2}',group['name']) is not None,'BATCH_GROUP_PATH')
        path=a.root/group['name']
        if not (path/'result.json').exists():continue
        try:
            child_result=rt.read_json(path/'result.json')
            if child_result.get('ok') is True and not child_result.get('cleanup_errors'):
                owned=hw.load_run(path/'capture',verify=False)
                if all(not hw.own_container(owned,c)['State']['Running'] for c in owned['cameras']):
                    outputs.append({'name':group['name'],'ok':True,'already_stopped':True});continue
            value=one.rollback(argparse.Namespace(root=path));outputs.append({'name':group['name'],**value})
            if not value['ok']:errors.append(group['name']+':child-cleanup-failed')
        except Exception as error:errors.append(group['name']+':'+str(error))
    value={'ok':not errors,'errors':errors,'groups':outputs,'production_changed':False,'recordings_retained':True}
    one.atomic(a.root/('rollback-'+str(time.time_ns())+'.json'),value)
    return value


def controller_command(controller,args,name,timeout):
    try:controller.command(args,name,timeout)
    finally:
        # Completed round leaders have already been reaped; do not later treat
        # a reused historical PID as an outstanding batch resource.
        live={child.pid for child,_ in controller.children if child.poll() is None}
        controller.child_bindings=[v for v in controller.child_bindings if v['pid'] in live]
        one.atomic(controller.root/'children.json',{'version':1,'uid':os.getuid(),'token':controller.token,'processes':controller.child_bindings})


def execute(a):
    rt.require(a.execute,'EXPLICIT_EXECUTE_REQUIRED')
    groups=schedule(a.rounds,a.first)
    rt.require(0<=a.cooldown_seconds<=600,'COOLDOWN_LIMIT')
    rt.require(not a.root.exists(),'FRESH_BATCH_ROOT_REQUIRED');a.root.mkdir(mode=0o700,parents=True)
    (a.root/'controller').mkdir(mode=0o700)
    controller=one.Runner(argparse.Namespace(root=a.root/'controller'));controller.save()
    result={'version':1,'ok':False,'uid':os.getuid(),'gid':os.getgid(),'run_token':controller.token,'groups':[],
            'planned_groups':groups,'production_changed':False,'rollback_argv':[sys.executable,str(HERE/'run_ab.py'),'rollback','--root',str(a.root)]}
    def save():one.atomic(a.root/'result.json',result)
    save();manifest=None
    def cancelled(sig,_frame):raise one.Cancelled('SIGNAL:'+str(sig))
    previous={s:signal.signal(s,cancelled) for s in (signal.SIGINT,signal.SIGTERM)}
    try:
        files,configs=freeze(a.plan,a.root)
        # Resolve default limits once, before any camera starts; all child runs get these same values.
        probe=one.settings(one.parser().parse_args(forward(a,'on',a.root/'probe',a.image)))
        a.pairs=probe.pairs;a.completion_timeout=probe.completion_timeout
        if a.build:controller_command(controller,['bash',str(HERE/'build_runtime.sh'),a.image,a.platform],'build',a.build_timeout)
        image,inventory=hw.image_manifest(a.image)
        manifest={'version':1,'image_id':image,'runtime_sha256':hw.digest(inventory),'uid':os.getuid(),'gid':os.getgid(),
                  'source_configs':configs,'frozen_files':files,'rounds':a.rounds,'groups':groups,
                  'settings':{k:str(v) if isinstance(v,Path) else v for k,v in vars(a).items()}}
        one.atomic(a.root/'manifest.json',manifest);result.update(image_id=image,runtime_sha256=manifest['runtime_sha256']);save()
        for group in groups:
            result['active_group']=group['name'];save()
            rt.require(not (a.root/'ABORT.json').exists(),'BATCH_ABORTED');check_frozen(a.root,manifest)
            entry=dict(group);result['groups'].append(entry);save()
            print(json.dumps({'group':group['name'],'image_id':image}),flush=True)
            limit=a.healthy_timeout+a.seconds+a.completion_timeout+2*a.audit_timeout+600
            controller_command(controller,[sys.executable,str(HERE/'run_hardware.py'),*forward(a,group['mode'],a.root/group['name'],image)],group['name'],limit)
            summary=metrics.summarize(a.root/group['name'],manifest,group['mode'])
            if result['groups'][0].get('summary'):
                rt.require(summary['normalized_config_hashes']==result['groups'][0]['summary']['normalized_config_hashes'],'CROSS_RUN_CONFIG_CHANGED')
            entry['summary']=summary;save()
            preview=metrics.aggregate(result['groups'],a.rounds)
            preview.update(complete=False,comparison=None,batch_in_progress=True);render(a.root,preview)
            # Require every owned capture stopped before starting another group.
            child=hw.load_run(a.root/group['name']/'capture',verify=False)
            rt.require(all(not hw.own_container(child,c)['State']['Running'] for c in child['cameras']),'PREVIOUS_GROUP_STILL_RUNNING')
            if group!=groups[-1]:time.sleep(a.cooldown_seconds)
        check_frozen(a.root,manifest)
        result['ok']=True
    except Exception as error:
        result['error']=type(error).__name__+':'+str(error)
        result['failed_group']=result.get('active_group')
        if result['groups'] and result['groups'][-1]['name']==result.get('active_group'):result['groups'][-1]['error']=result['error']
    finally:
        for sig in previous:signal.signal(sig,signal.SIG_IGN)
        try:
            for child,_ in controller.children:controller.terminate(child)
            controller.save()
            cleanup=rollback(a,mark_abort=not result['ok']);result['cleanup']=cleanup
            if not cleanup['ok']:result['ok']=False
        except Exception as error:result.update(ok=False,cleanup_error=str(error))
        for log in controller.logs:log.close()
        try:
            report=metrics.aggregate(result['groups'],a.rounds)
            report['batch_ok']=result['ok']
            if not result['ok']:report.update(complete=False,comparison=None)
            render(a.root,report)
        except Exception as error:result.update(ok=False,summary_error=str(error))
        result['finished_unix_ns']=time.time_ns();save()
        for sig,handler in previous.items():signal.signal(sig,handler)
    return 0 if result['ok'] else 1


def main():
    os.umask(0o077);p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='action',required=True)
    r=sub.add_parser('run');r.add_argument('--plan',type=Path,required=True);r.add_argument('--image',required=True)
    r.add_argument('--rounds',type=int,default=3);r.add_argument('--first',choices=('on','off'),default='on')
    r.add_argument('--build',action='store_true');r.add_argument('--platform',choices=('linux/amd64','linux/arm64'),default='linux/amd64')
    r.add_argument('--seconds',type=int,default=600);r.add_argument('--max-bytes',type=int,required=True);r.add_argument('--ros-domain-id',type=int,required=True)
    r.add_argument('--pairs',type=int);r.add_argument('--healthy-timeout',type=int,default=300);r.add_argument('--completion-timeout',type=int)
    r.add_argument('--audit-timeout',type=int,default=3600);r.add_argument('--build-timeout',type=int,default=3600)
    r.add_argument('--poll-seconds',type=float,default=5);r.add_argument('--trim-seconds',type=float,default=5)
    r.add_argument('--cooldown-seconds',type=float,default=30);r.add_argument('--disk',action='append',default=[])
    r.add_argument('--perf',action='store_true');r.add_argument('--execute',action='store_true')
    sub.add_parser('rollback')
    for s in sub.choices.values():s.add_argument('--root',type=Path,required=True)
    a=p.parse_args();a.root=a.root.absolute()
    try:
        one.local_rootful(require_compose=a.action=='run')
        if a.action=='rollback':value=rollback(a);print(json.dumps(value,indent=2));return 0 if value['ok'] else 1
        return execute(a)
    except Exception as error:print(json.dumps({'ok':False,'error':str(error)}),file=sys.stderr);return 1

if __name__=='__main__':raise SystemExit(main())
