#!/usr/bin/env python3
"""Independent, fail-closed experiment runtime. Never accepted as production rust-v1."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

EXPERIMENT = 'sdk-2.10.6-diagnostics-v1'
ROLE = 'rust-sdk-diagnostics-v1'
BASE = '575347ca742d219ef61b1e214e97043f17f48d23'
PATCH = '0db54c7c1bcafa9d18cd51bddcd4cf0d0c733c967ff0f9542f7796b770a0757c'
SDK = '/opt/gdp-sdk-experiment'
HOME = '/opt/gdp-experiment'
MANIFEST = HOME + '/runtime.json'
SERVICE = '/usr/local/bin/capture-service'
AUDITOR = '/usr/local/bin/audit_imu_long_run'
BRIDGE = '/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so'
LIBRARY = SDK + '/lib/libOrbbecSDK.so'
ROOTS = (SDK, HOME, '/opt/gdp-ros', '/app')
REQUIRED_LICENSES = ('LICENSE.txt', 'SDK-EULA.txt', 'extensions-LICENSE.txt',
                     'licenses/THIRD_PARTY_LICENSES.md', 'licenses/libusb/COPYING',
                     'licenses/live555/COPYING', 'licenses/live555/COPYING.LESSER')


def require(ok, code):
    if not ok: raise ValueError(code)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda:source.read(1024*1024), b''):digest.update(block)
    return digest.hexdigest()


def pairs(values):
    result = {}
    for key,value in values:
        require(key not in result, 'DUPLICATE_JSON_KEY')
        result[key]=value
    return result


def read_json(path):
    with Path(path).open('rb') as source: raw=source.read(4*1024*1024+1)
    require(len(raw)<=4*1024*1024, 'JSON_LIMIT')
    return json.loads(raw,object_pairs_hook=pairs,parse_constant=lambda _:require(False,'NONFINITE_JSON'))


def physical(root, path):
    require(path.startswith('/') and '..' not in Path(path).parts and str(Path(path))==path, 'RUNTIME_PATH')
    return Path(root)/path.lstrip('/')


def inventory(root=Path('/')):
    result={}
    for prefix in ROOTS:
        for path in sorted(physical(root,prefix).rglob('*')):
            if not path.is_file():continue
            name='/'+str(path.relative_to(root))
            if name==MANIFEST:continue
            resolved=path.resolve()
            require(any(resolved.is_relative_to(physical(root,allowed).resolve()) for allowed in ROOTS), 'RUNTIME_SYMLINK_ESCAPE')
            result[name]=sha(path)
    for path in (SERVICE,AUDITOR):result[path]=sha(physical(root,path))
    return result


def verify_patch(path):
    require(sha(path)==PATCH,'SDK_PATCH_DIGEST')


def sdk_record(source, patch, exports, output):
    verify_patch(patch)
    head=subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip()
    require(head==BASE,'SDK_SOURCE_COMMIT')
    headers={}
    for path in sorted((source/'include').rglob('*')):
        if not path.is_file():continue
        relative=str(path.relative_to(source))
        original=subprocess.check_output(['git','-C',str(source),'show',BASE+':'+relative])
        require(path.read_bytes()==original,'SDK_PUBLIC_HEADER_CHANGED')
        headers[str(path.relative_to(source/'include'))]=sha(path)
    installed={str(p.relative_to(Path(SDK)/'include')):sha(p)
               for p in sorted((Path(SDK)/'include').rglob('*')) if p.is_file()}
    require(len(headers)==33 and set(installed)==set(headers)|{'libobsensor/h/Export.h'}
            and all(installed[k]==v for k,v in headers.items()),'SDK_INSTALLED_PUBLIC_HEADERS')
    names=subprocess.check_output(['nm','-D','--defined-only',SDK+'/lib/libOrbbecSDK.so.2.10.6'],text=True)
    actual=sorted(line.split()[-1] for line in names.splitlines() if line.split() and line.split()[-1].startswith('ob_'))
    expected=Path(exports).read_text().splitlines()
    require(actual==expected and len(actual)==406,'SDK_PUBLIC_EXPORTS')
    for name in REQUIRED_LICENSES:require((Path(SDK)/name).is_file(),'SDK_LICENSE_MISSING:'+name)
    value={'version':1,'experiment':EXPERIMENT,'repository':'https://github.com/orbbec/OrbbecSDK_v2','commit':BASE,
           'sdk_version':'2.10.6','patch_sha256':PATCH,'sdk_sha256':sha(LIBRARY),
           'public_c_exports':len(actual),'public_exports_sha256':sha(exports),
           'public_headers_sha256':hashlib.sha256(json.dumps(headers,sort_keys=True).encode()).hexdigest(),
           'public_header_count':len(headers),'installed_public_header_count':len(installed),
           'installed_public_headers_sha256':hashlib.sha256(json.dumps(installed,sort_keys=True).encode()).hexdigest(),
           'compiler':subprocess.check_output(['c++','--version'],text=True).splitlines()[0],
           'architecture':subprocess.check_output(['dpkg','--print-architecture'],text=True).strip()}
    output.write_text(json.dumps(value,sort_keys=True,indent=2)+'\n')


def make_manifest(root=Path('/'), revision=None):
    revision=revision or os.environ.get('GDP_EXPERIMENT_REVISION','')
    require(re.fullmatch('[a-f0-9]{40}',revision) is not None,'GDP_REVISION_REQUIRED')
    build=read_json(physical(root,HOME+'/sdk-build.json'))
    files=inventory(root)
    value={'version':1,'kind':'gdp-sdk-diagnostics-runtime','experiment':EXPERIMENT,'gdp_revision':revision,
           'sdk_source':build,'sdk_sha256':files[LIBRARY],'bridge_sha256':files[BRIDGE],'service_sha256':files[SERVICE],
           'auditor_sha256':files[AUDITOR],'files':files}
    physical(root,MANIFEST).write_text(json.dumps(value,sort_keys=True,separators=(',',':'))+'\n')
    validate_manifest(value,root)
    return value


def validate_manifest(value,root=Path('/')):
    require(type(value.get('version')) is int and value['version']==1 and value.get('kind')=='gdp-sdk-diagnostics-runtime' and value.get('experiment')==EXPERIMENT,'EXPERIMENT_RUNTIME_KIND')
    require(re.fullmatch('[a-f0-9]{40}',str(value.get('gdp_revision',''))) is not None,'GDP_REVISION')
    build=value.get('sdk_source',{})
    require(build.get('experiment')==EXPERIMENT and build.get('commit')==BASE
            and build.get('repository')=='https://github.com/orbbec/OrbbecSDK_v2'
            and build.get('sdk_version')=='2.10.6' and build.get('patch_sha256')==PATCH
            and build.get('public_c_exports')==406 and build.get('public_header_count')==33,'SDK_EXPERIMENT_SOURCE')
    require(read_json(physical(root,HOME+'/sdk-build.json'))==build,'SDK_BUILD_BINDING')
    headers={str(p.relative_to(physical(root,SDK+'/include'))):sha(p)
             for p in sorted(physical(root,SDK+'/include').rglob('*')) if p.is_file()}
    require(len(headers)==34 and build.get('installed_public_header_count')==34
            and hashlib.sha256(json.dumps(headers,sort_keys=True).encode()).hexdigest()
                ==build.get('installed_public_headers_sha256'),'SDK_INSTALLED_HEADER_BINDING')
    require('libobsensor/h/Export.h' in headers,'SDK_GENERATED_EXPORT_HEADER')
    original={k:v for k,v in headers.items() if k!='libobsensor/h/Export.h'}
    require(hashlib.sha256(json.dumps(original,sort_keys=True).encode()).hexdigest()
            ==build.get('public_headers_sha256'),'SDK_SOURCE_HEADER_BINDING')
    require(inventory(root)==value.get('files'),'RUNTIME_FILE_INVENTORY')
    for path,key in ((LIBRARY,'sdk_sha256'),(BRIDGE,'bridge_sha256'),(SERVICE,'service_sha256'),(AUDITOR,'auditor_sha256')):
        require(value.get(key)==value['files'].get(path),'RUNTIME_BINARY_BINDING')
    require(build.get('sdk_sha256')==value['sdk_sha256'],'SDK_BINARY_SOURCE_BINDING')
    require(value['files'].get(SDK+'/lib/libOrbbecSDK.so.2.10.6')==value['sdk_sha256'],'SDK_SONAME_BINDING')
    require(value['files'].get(HOME+'/sdk-diagnostics.patch')==PATCH,'SDK_PATCH_BINDING')
    require(value['files'].get(HOME+'/public-c-exports.txt')==build['public_exports_sha256'],'SDK_EXPORTS_BINDING')
    for name in REQUIRED_LICENSES:require(SDK+'/'+name in value['files'],'SDK_LICENSE_INVENTORY')
    require(HOME+'/experiment_runtime.py' in value['files'] and HOME+'/entrypoint.sh' in value['files'],'EXPERIMENT_VALIDATOR_INVENTORY')
    return value


def validate_config(config, binding, manifest, environment):
    require(type(binding.get('version')) is int and binding['version']==1 and binding.get('experiment')==EXPERIMENT and type(binding.get('diagnostics')) is bool,'EXPERIMENT_BINDING')
    require(binding.get('runtime_sha256')==hashlib.sha256(json.dumps(manifest,sort_keys=True,separators=(',',':')).encode()).hexdigest(),'EXPERIMENT_MANIFEST_BINDING')
    require(re.fullmatch('sha256:[a-f0-9]{64}',binding.get('image_id','')) is not None,'IMAGE_ID')
    require(re.fullmatch('[a-f0-9]{32}',binding.get('run_id','')) is not None,'RUN_ID')
    require(config['service']['generation']==binding['run_id'],'EXPERIMENT_GENERATION')
    require(config['sdk_library']==LIBRARY and config['ros']['bridge']==BRIDGE,'EXPERIMENT_BINARY_PATHS')
    require(config['runtime_contract']=={k:manifest[k] for k in ('sdk_sha256','bridge_sha256','service_sha256')},'CAPTURE_BINARY_DIGESTS')
    require(config['service']['status_path']=='/state/status.json' and config['service']['max_restarts']==0,'EXPERIMENT_SUPERVISION')
    require(config['pairing']['clock']=='device' and config['ros']['stamp_source']=='sdk-global-unix','EXPERIMENT_CLOCK')
    require(config['imu']['enabled'] is True and 'quality_policy' in config and 'quality_runtime' in config,'IMU_QUALITY_REQUIRED')
    require(config['raw_recording']['directory']=='/recordings' and config['raw_recording']['storage_format']=='chunked-v3','V3_RECORDING_REQUIRED')
    require(environment.get('OB_GDP_DIAGNOSTICS')==('1' if binding['diagnostics'] else '0')
            and environment.get('OB_GDP_DIAGNOSTICS_DIR')=='/sdk-diagnostics','DIAGNOSTIC_ENVIRONMENT')
    require(environment.get('GDP_CAPTURE_INSTANCE_ID')==binding['instance_id'] and re.fullmatch('[a-f0-9]{64}',binding['instance_id']) is not None,'EXPERIMENT_INSTANCE')
    require(not environment.get('LD_PRELOAD') and not environment.get('LD_AUDIT'),'UNBOUND_PRELOAD')
    return True


def validate_health(config,health):
    streams=health['streams']
    require(len(streams)==2,'TWO_HEALTH_IMAGE_STREAMS_REQUIRED')
    by_topic={s['topic']:s for s in streams}
    require(len(by_topic)==2,'DUPLICATE_HEALTH_TOPIC')
    for stream,encoding in (('color','yuyv'),('depth','16UC1')):
        expected=config['ros']['namespace']+'/'+stream+'/image_raw'
        spec=by_topic.get(expected,{})
        require(spec.get('frame_id')==config[stream]['frame_id'] and spec.get('encoding')==encoding
                and spec.get('width')==config[stream]['width'] and spec.get('height')==config[stream]['height']
                and spec.get('required_reliability')=='reliable'
                and 0<spec.get('min_fps',0)<=config[stream]['fps']
                and config[stream]['frame_id'].startswith(health['camera_name']+'_'),'HEALTH_CAPTURE_BINDING')
    require(0<health['sample_seconds']<=20 and 0<health['freshness_seconds']<=20
            and 0<=health['discovery_seconds']<=20,'BOUNDED_HEALTH_TIMING')
    require(health['sample_seconds']+health['discovery_seconds']<24,'HEALTH_TIMEOUT_BUDGET')


def validate(root=Path('/')):
    manifest=validate_manifest(read_json(physical(root,MANIFEST)),root)
    require(read_json(physical(root,'/config/runtime.json'))==manifest,'EXPECTED_RUNTIME_MISMATCH')
    config=read_json(physical(root,'/config/capture.json'))
    binding=read_json(physical(root,'/config/experiment.json'))
    validate_config(config,binding,manifest,os.environ)
    require(sha(physical(root,'/config/capture.json'))==binding['capture_sha256'],'CAPTURE_CONFIG_BINDING')
    require(sha(physical(root,'/config/health.json'))==binding['health_sha256'],'HEALTH_CONFIG_BINDING')
    validate_health(config,read_json(physical(root,'/config/health.json')))
    require(physical(root,'/sdk-diagnostics').is_dir(),'DIAGNOSTIC_DIRECTORY')
    info=physical(root,'/sdk-diagnostics').stat()
    require(info.st_uid==os.geteuid() and not info.st_mode&0o077 and not physical(root,'/sdk-diagnostics').is_symlink(),'PRIVATE_DIAGNOSTIC_DIRECTORY')
    subprocess.run([SERVICE,'--validate-config','/config/capture.json'],check=True,timeout=30)
    return manifest


def smoke():
    manifest=validate_manifest(read_json(MANIFEST))
    require(not Path('/opt/gdp-capture/runtime.json').exists(),'PRODUCTION_MANIFEST_MUST_BE_ABSENT')
    lib=ctypes.CDLL(LIBRARY)
    require(lib.ob_get_version()==21006,'SDK_VERSION')
    for name in ('ob_pipeline_enable_frame_sync','ob_pipeline_disable_frame_sync','ob_device_get_sensor',
                 'ob_sensor_start','ob_sensor_stop','ob_frame_get_global_timestamp_us'):
        require(hasattr(lib,name),'SDK_SYMBOL:'+name)
    bridge=ctypes.CDLL(BRIDGE)
    require(bridge.gdp_ros_context_abi_version()==1,'ROS_CONTEXT_ABI')
    # Validate real service contracts without creating an SDK context/device.
    with tempfile.TemporaryDirectory(prefix='gdp-sdk-experiment-smoke-') as directory:
        config={'version':1,'sdk_library':LIBRARY,'serial':'SYNTHETIC_NO_DEVICE',
                'color':{'width':2,'height':2,'fps':30,'frame_id':'test_color'},
                'depth':{'width':2,'height':2,'fps':30,'frame_id':'test_depth','unit_policy':'normalize-mm'},
                'pairing':{'clock':'device','max_delta_us':20000},'poll_ms':100,'capacity':4,
                'ros':{'bridge':BRIDGE,'namespace':'/sdk_experiment_smoke','stamp_source':'sdk-system-unix'},
                'runtime_contract':{k:manifest[k] for k in ('sdk_sha256','bridge_sha256','service_sha256')},
                'service':{'status_path':directory+'/status.json','generation':'experiment-smoke',
                           'idle_timeout_ms':2000,'startup_timeout_ms':15000,'heartbeat_ms':500,
                           'heartbeat_timeout_ms':10000,'max_restarts':0,'restart_backoff_ms':1000,
                           'shutdown_timeout_ms':5000,'ready_min_pairs':5}}
        path=Path(directory)/'capture.json';path.write_text(json.dumps(config))
        subprocess.run([SERVICE,'--validate-config',str(path)],check=True,timeout=30)
        config['runtime_contract']['sdk_sha256']='0'*64;path.write_text(json.dumps(config))
        refusal=subprocess.run([SERVICE,'--validate-config',str(path)],capture_output=True,timeout=30)
        require(refusal.returncode!=0,'SERVICE_MUST_REJECT_WRONG_SDK_DIGEST')
    return {'ok':True,'experiment':EXPERIMENT,'hardware_opened':False,'production_manifest_absent':True}


def main():
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='action',required=True)
    s=sub.add_parser('verify-patch');s.add_argument('--patch',type=Path,required=True)
    s=sub.add_parser('sdk-build-record');s.add_argument('--sdk-source',type=Path,required=True);s.add_argument('--patch',type=Path,required=True);s.add_argument('--exports',type=Path,required=True);s.add_argument('--output',type=Path,required=True)
    sub.add_parser('make-manifest');sub.add_parser('validate');sub.add_parser('smoke')
    a=p.parse_args()
    try:
        if a.action=='verify-patch':verify_patch(a.patch);result={'ok':True}
        elif a.action=='sdk-build-record':sdk_record(a.sdk_source,a.patch,a.exports,a.output);result={'ok':True}
        elif a.action=='make-manifest':make_manifest();result={'ok':True}
        elif a.action=='validate':validate();result={'ok':True,'hardware_opened':False}
        else:result=smoke()
        print(json.dumps(result));return 0
    except Exception as error:
        print(json.dumps({'ok':False,'code':'SDK_EXPERIMENT_RUNTIME_INVALID','detail':str(error)}));return 1

if __name__=='__main__':raise SystemExit(main())
