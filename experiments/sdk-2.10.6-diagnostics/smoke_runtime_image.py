#!/usr/bin/env python3
"""No-device integration checks for the assembled experimental runtime image.

All containers are read-only, network-disabled and have no USB/socket mounts.
The full SDK source/probe tests remain separate; this checks packaging/bindings.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import experiment_runtime as rt
import hardware as hw


def main():
    os.umask(0o077)
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--image',required=True);p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    rt.require(a.output.is_absolute() and not a.output.exists(),'FRESH_ABSOLUTE_OUTPUT')
    a.output.mkdir(mode=0o700,parents=True)
    identity,manifest=hw.image_manifest(a.image)
    hw.write(a.output/'runtime.json',manifest)
    hw.write(a.output/'sdk-build.json',manifest['sdk_source'])
    base=['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL',
          '--tmpfs','/tmp:rw,nosuid,nodev,size=128m']
    cases=[]
    with tempfile.TemporaryDirectory(prefix='sdk-runtime-negative-') as directory:
        damaged=Path(directory)/'damaged';damaged.write_text('deliberate corruption');damaged.chmod(0o644)
        fake=Path(directory)/'runtime.json';hw.write(fake,dict(manifest,kind='production'));fake.chmod(0o644)
        for label,destination,source,code in (
            ('sdk-bytes',rt.SDK+'/lib/libOrbbecSDK.so.2.10.6',damaged,'RUNTIME_FILE_INVENTORY'),
            ('patch-bytes',rt.HOME+'/sdk-diagnostics.patch',damaged,'RUNTIME_FILE_INVENTORY'),
            ('manifest-kind',rt.MANIFEST,fake,'EXPERIMENT_RUNTIME_KIND')):
            result=subprocess.run(base+['--mount','type=bind,src='+str(source)+',dst='+destination+',readonly',
                                         identity,'python3',rt.HOME+'/experiment_runtime.py','smoke'],
                                  capture_output=True,text=True,timeout=120)
            (a.output/(label+'.log')).write_text(result.stdout+result.stderr)
            rt.require(result.returncode!=0 and code in result.stdout,'NEGATIVE_CHECK_FAILED:'+label)
            cases.append({'case':label,'rejected':True,'expected':code})
    result=subprocess.run(base+[identity],capture_output=True,text=True,timeout=120)
    (a.output/'missing-config.log').write_text(result.stdout+result.stderr)
    rt.require(result.returncode!=0 and '/config/runtime.json' in result.stdout,'UNBOUND_DEFAULT_START_ACCEPTED')
    cases.append({'case':'unbound-default-start','rejected':True})
    report={'ok':True,'experiment':rt.EXPERIMENT,'image_id':identity,'runtime_sha256':hw.digest(manifest),
            'sdk_source':manifest['sdk_source'],'gdp_revision':manifest['gdp_revision'],'inventory_files':len(manifest['files']),
            'negative_checks':cases,'hardware_opened':False,'production_sdk_changed':False,
            'scope':'public-base runtime packaging, SDK version/ABI and binary/config binding only; no physical camera or performance claim'}
    hw.write(a.output/'summary.json',report)
    print(json.dumps(report,indent=2))

if __name__=='__main__':main()
