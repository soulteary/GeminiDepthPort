#!/usr/bin/env python3
"""Test-only CLI process fixture. No Docker, SDK library or physical devices.

The backend and telemetry are intentionally synthetic. This exercises the real
orchestrator, process groups, CLI signal handling, evidence files and cleanup.
"""
import json
import os
from pathlib import Path
import sys
import time
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'experiments/sdk-2.10.6-diagnostics'))
import run_hardware as cli


def child(root,scenario,name):
    (root/('pid-'+name)).write_text(str(os.getpid()))
    if name.endswith('_argv'):
        (root/'sampler-ready').touch()
        if scenario=='cancel':time.sleep(60)
        if scenario=='sample-fail' and name=='sdk_argv':print('synthetic sampler failure',flush=True);return 7
        time.sleep(.15)
        if name=='checkpoint_argv':path=root/'capture/checkpoint-samples/summary.json';value={'errors':{},'interrupted':False,'elapsed_seconds':1,'targets':[{'observation_supported':True,'observation_enabled':True,'lost_events_after_start':0} for _ in range(3)]}
        elif name=='sdk_argv':path=root/'capture/sdk-samples/summary.json';value={'ok':True,'elapsed_seconds':1}
        else:path=root/'capture/wire.sqlite3.summary.json';value={'ok':True,'elapsed_seconds':1}
        path.parent.mkdir(parents=True,exist_ok=True);path.write_text(json.dumps(value));return 0
    if name=='verify-dds':
        settings=json.loads((root/'settings.json').read_text())
        report={'ok':True,'cameras':[{'camera':n,'zero_source_gaps':True,'sequence_gaps':{},'summary':{
            'ok':True,'audit_version':2,'native_hashes_verified':True,'independent_audit_passes':2,
            'recovered':False,'pairs':settings['pairs']}} for n in 'abc']}
        (root/'capture/dds-native-comparison.json').write_text(json.dumps(report))
    print('synthetic backend '+name,flush=True);return 0


def parent(root,scenario):
    run={'run_id':'a'*32,'image_id':'sha256:'+'b'*64,'runtime_sha256':'c'*64,'ros_domain_id':78,'mode':'on',
         'cameras':[{'name':n,'container':'synthetic-'+n,'namespace':'/'+n,'diagnostics':str(root/'diag'/n)} for n in 'abc']}
    def settings(a):
        a.pairs=100;a.completion_timeout=5;a.audit_timeout=10;a.build_timeout=10
        return a
    def prepare(a):
        a.run_dir.mkdir()
        for camera in run['cameras']:Path(camera['diagnostics']).mkdir(parents=True,mode=0o700)
        cli.atomic(a.run_dir/'run.json',run)
        (a.run_dir/'recording.partial').write_bytes(b'synthetic original evidence')
    def start(a,_run):(root/'capture-started').touch()
    def stop(_run):(root/'capture-stopped').touch();return {'ok':True,'production_changed':False}
    def snapshot(_run):
        complete=json.loads((root/'result.json').read_text())['stage'] in ('wait-complete','stop','audit','verify-dds','complete','failed')
        return {'cameras':[{'name':n,'running':True,'health':'healthy','capture':{
            'state':'ready','ready':True,'updated_unix_ms':time.time_ns()//10**6,'message_timestamp':'global',
            'imu':{'ready':True,'shared_ros_context':True,'error':None},
            'raw_recording':{'state':'complete' if complete else 'recording','written_pairs':100 if complete else 1,
                             'max_pairs':100,'whole_pair_queue_overflows':0}}} for n in 'abc']}
    real_spawn=cli.Runner.spawn
    def spawn(self,argv,name):
        if argv[0]=='docker' and '--user' in argv:assert argv[argv.index('--user')+1]==str(os.getuid())+':'+str(os.getgid())
        return real_spawn(self,[sys.executable,__file__,'child',str(root),scenario,name],name)
    argv=['run_hardware.py','run','--plan',str(root.parent/'unused-plan.json'),'--image','synthetic-only',
          '--root',str(root),'--seconds','1','--trim-seconds','0','--poll-seconds','1',
          '--max-bytes','10000000','--ros-domain-id','78','--execute']
    with patch.object(cli,'local_rootful'),patch.object(cli,'settings',side_effect=settings), \
         patch.object(cli.hw,'prepare',side_effect=prepare),patch.object(cli.hw,'load_run',return_value=run), \
         patch.object(cli.hw,'start',side_effect=start),patch.object(cli.hw,'stop',side_effect=stop), \
         patch.object(cli.hw,'status',side_effect=snapshot),patch.object(cli.hw,'recording_path'), \
         patch.object(cli.hw,'own_container',return_value={'Id':'synthetic-not-docker'}), \
         patch.object(cli.hw,'command',return_value=''),patch.object(cli.Runner,'spawn',spawn),patch.object(sys,'argv',argv):
        return cli.main()

if __name__=='__main__':
    if sys.argv[1]=='child':raise SystemExit(child(Path(sys.argv[2]),sys.argv[3],sys.argv[4]))
    raise SystemExit(parent(Path(sys.argv[2]),sys.argv[1]))
