// Exercise the real form handlers without HTTP, cameras, or a browser dependency.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const markup = fs.readFileSync('web/index.html','utf8');
for (const id of ['backend-fields','capture-backend','capture-image','camera-profile']) assert.ok(markup.includes(`id="${id}"`),`missing actual form control ${id}`);
const source = fs.readFileSync('web/app.js', 'utf8').split("$('camera-form').addEventListener")[0];
async function check(backend) {
  const elements = new Map();
  const get = id => {
    if (!elements.has(id)) elements.set(id, {id,value:'',checked:false,hidden:false,dataset:{},classList:{toggle(){}},removeAttribute(){},closest(){return get('exposure-details');},querySelector(selector){return get(id+selector);}});
    return elements.get(id);
  };
  let saved;
  const inventory = {schema_version:2,capture_backend:backend,rust_capture:backend==='rust'?{}:undefined,cameras:[{name:'cam',enabled:true,serial_number:'SYNTHETIC',health:{},parameters:{enable_color:true,enable_depth:true,color_width:640,color_height:480,color_fps:30,depth_width:640,depth_height:480,depth_fps:30,color_format:'YUYV',depth_format:'Y16'}}]};
  const context = vm.createContext({structuredClone,console,document:{getElementById:get,querySelector:()=>({content:'token'}),querySelectorAll:selector=>selector.startsWith('#camera-form')?[...elements.values()]:[]},fetch:async (path,options)=>{
    if(options.body) saved=JSON.parse(options.body);
    return {ok:true,json:async()=>path==='/api/config'?{message:'saved'}:{inventory,revision:'next',devices:[],previews:{}}};
  }});
  vm.runInContext(source,context);
  context.fixture=inventory;
  vm.runInContext("draft=structuredClone(fixture); state={devices:[],previews:{}}; revision='old'; showRole(); updateButtons();",context);
  assert.equal(get('exposure-details').hidden,backend==='rust');
  assert.equal(get('color_gain').disabled,backend==='rust');
  get('serial').value='NEW_SYNTHETIC'; get('color_width').value='1280';
  vm.runInContext('readForm()',context);
  await vm.runInContext('saveConfig()',context);
  assert.equal(saved.cameras[0].serial_number,'NEW_SYNTHETIC');
  assert.equal(saved.cameras[0].parameters.color_width,1280);
  if (backend === 'rust') {
    assert.equal(saved.acquisition,null);
    get('acquisition-preset').value='complete';
    vm.runInContext("readForm({target:{id:'acquisition-preset'}})",context);
    assert.equal(get('camera-timestamp-source').value,'global');
    assert.equal(get('camera-timestamp-source').readOnly,true);
    assert.equal(get('acquisition-sync').checked,false);
    assert.equal(get('acquisition-advanced').hidden,false);
    assert.equal(get('acquisition-aggregate').querySelector('option[value="all"]').disabled,true);
    await vm.runInContext('saveConfig()',context);
    assert.equal(saved.acquisition.preset,'complete');
    assert.equal(saved.acquisition.advanced.message_timestamp,'global');
    assert.equal(saved.cameras[0].timestamp_source,'global');
    // Reopen saved values before exercising an advanced override.
    context.fixture.acquisition=saved.acquisition;
    vm.runInContext('draft.rust_capture.acquisition=structuredClone(fixture.acquisition);showRole()',context);
    get('acquisition-timestamp').value='system';
    vm.runInContext("readForm({target:{id:'acquisition-timestamp'}})",context);
    await vm.runInContext('saveConfig()',context);
    assert.equal(saved.acquisition.advanced.message_timestamp,'system');
    assert.equal(saved.cameras[0].timestamp_source,'host');
    get('acquisition-preset').value='strict-rgbd';
    vm.runInContext("readForm({target:{id:'acquisition-preset'}});updateButtons()",context);
    assert.equal(get('acquisition-sync').checked,true);
    assert.equal(get('acquisition-sync').disabled,true);
    await vm.runInContext('saveConfig()',context);
    assert.equal(saved.acquisition.preset,'strict-rgbd');
    assert.equal(saved.acquisition.advanced.sdk_frame_sync,true);
    get('acquisition-preset').value='legacy';
    vm.runInContext("readForm({target:{id:'acquisition-preset'}})",context);
    await vm.runInContext('saveConfig()',context);
    assert.equal(saved.acquisition,null);
    assert.equal(saved.cameras[0].timestamp_source,'host');
  } else {
    assert.equal(get('acquisition-fields').hidden,true);
    assert.equal(Object.hasOwn(saved,'acquisition'),false);
  }
  for(const key of ['color_exposure' ,'color_gain','ir_exposure','ir_gain','enable_color_auto_exposure','enable_ir_auto_exposure']) {
    assert.equal(Object.hasOwn(saved.cameras[0].parameters,key),backend!=='rust');
  }
}
(async()=>{await check('rust');await check('official');console.log('Web form backend regression passed');})().catch(e=>{console.error(e);process.exitCode=1;});

async function checkSelection() {
  const catalog = JSON.parse(fs.readFileSync('driver_profiles.json','utf8')).profiles;
  const elements = new Map();
  const get = id => {
    if(!elements.has(id)) elements.set(id,{id,value:'',checked:false,hidden:false,dataset:{},classList:{toggle(){}},removeAttribute(){},closest(){return get('exposure-details');},querySelector(selector){return get(id+selector);}});
    return elements.get(id);
  };
  let inventory = structuredClone(JSON.parse(fs.readFileSync('examples/official-three-camera/inventory.json','utf8')));
  inventory.image = 'local/custom-official:kept';
  for (const [i,c] of inventory.cameras.entries()) {
    c.enabled=true;c.serial_number='SYNTHETIC_'+i;c.timestamp_source='host';c.calibration_revision='old';c.profiles_verified=true;c.verification={configuration_digest:'synthetic'};
    Object.assign(c.parameters,{color_width:640,color_height:480,color_fps:30,depth_width:640,depth_height:480,depth_fps:30});
  }
  inventory.cameras[0].parameters.color_gain=12;
  const defaults = {depth_unit_policy:'normalize-mm',pairing:{clock:'device',max_delta_us:20000},poll_ms:100,capacity:4,startup_discovery_seconds:2,service:{heartbeat_timeout_ms:10000}};
  const options = {profiles:catalog,rust_capture:defaults,default_images:{rust:'local/test-rust:web',official:'local/test-official:web'},rust_parameters:['enable_color','enable_depth','color_format','depth_format','depth_registration','color_width','color_height','color_fps','depth_width','depth_height','depth_fps','color_qos','depth_qos','color_camera_info_qos','depth_camera_info_qos']};
  let approved=true,saved,calls=[];const prompts=[];
  const tabs=inventory.cameras.map(c=>({dataset:{role:c.name},classList:{toggle(){}},querySelector:selector=>get('tab-'+c.name+'-'+selector)}));
  const context=vm.createContext({structuredClone,console,window:{confirm:message=>{prompts.push(message);return approved;}},document:{getElementById:get,querySelector:()=>({content:'token'}),querySelectorAll:selector=>selector.startsWith('#camera-form')?[...elements.values()]:(selector==='.role-tab'?tabs:[])},fetch:async(path,opts)=>{
    calls.push(path);
    if(opts.body) saved=JSON.parse(opts.body);
    return {ok:true,json:async()=>path==='/api/config'?{message:'saved'}:{inventory,revision:'saved',devices:[],previews:{},configuration_options:options}};
  }});
  vm.runInContext(source,context);
  function reset(data) {
    inventory=structuredClone(data);context.fixture=inventory;context.options=options;
    vm.runInContext("draft=structuredClone(fixture);state={inventory:structuredClone(fixture),devices:[],previews:{},configuration_options:options};revision='old';role=null;dirty=false;discardUnsupported=false;showRole();updateButtons()",context);
  }
  const original=structuredClone(inventory);reset(original);
  assert.match(get('camera-profile').innerHTML,/value="gemini_305" disabled/);
  approved=false;get('capture-backend').value='rust';vm.runInContext("readForm({target:{id:'capture-backend'}})",context);
  assert.equal(vm.runInContext("draft.capture_backend || 'official'",context),'official');
  assert.equal(get('capture-image').value,original.image);assert.equal(vm.runInContext('dirty',context),false);
  approved=true;get('capture-backend').value='rust';vm.runInContext("readForm({target:{id:'capture-backend'}})",context);
  assert.match(prompts.at(-1),/color_gain/);
  assert.equal(get('capture-image').value,options.default_images.rust);
  assert.equal(vm.runInContext('draft.cameras.every(c=>!c.profiles_verified && c.verification===null)',context),true);
  assert.equal(vm.runInContext('draft.rust_capture.service.heartbeat_timeout_ms',context),10000);
  assert.equal(calls.length,0,'selection must not start or save anything');
  await vm.runInContext('refresh(false)',context);
  for(const c of inventory.cameras)assert.equal(get('tab-'+c.name+'-b').textContent,'待验证');
  await vm.runInContext('saveConfig()',context);
  assert.equal(saved.capture_backend,'rust');assert.equal(saved.discard_unsupported_parameters,true);assert.equal(saved.image,options.default_images.rust);
  assert.equal(Object.hasOwn(saved.cameras[0].parameters,'color_gain'),false);
  reset(original);get('camera-profile').value='gemini_2';vm.runInContext("readForm({target:{id:'camera-profile'}})",context);
  assert.equal(get('serial').value,'');assert.equal(get('camera-enabled').checked,false);
  assert.equal(get('tab-camera_a-small').textContent,'Gemini 2');
  assert.equal(get('camera-calibration-revision').value,'');
  assert.equal(vm.runInContext('draft.cameras[1].profiles_verified',context),true);
  await vm.runInContext('saveConfig()',context);assert.equal(saved.cameras[0].profile,'gemini_2');
  reset(original);get('camera-profile').value='gemini_305';vm.runInContext("readForm({target:{id:'camera-profile'}})",context);
  assert.equal(vm.runInContext('draft.cameras[0].profile',context),'gemini_335l');
  reset(original);get('capture-backend').value='rust';vm.runInContext("changeBackend()",context);get('capture-backend').value='official';vm.runInContext('changeBackend()',context);
  assert.equal(vm.runInContext('draft.image',context),original.image);assert.equal(vm.runInContext('draft.cameras[0].parameters.color_gain',context),12);
  const rust=structuredClone(original);rust.capture_backend='rust';rust.image='local/custom-rust:kept';rust.rust_capture=structuredClone(defaults);rust.rust_capture.service.heartbeat_timeout_ms=12000;delete rust.cameras[0].parameters.color_gain;
  reset(rust);get('capture-backend').value='official';vm.runInContext('changeBackend()',context);get('capture-backend').value='rust';vm.runInContext('changeBackend()',context);
  assert.equal(vm.runInContext('draft.rust_capture.service.heartbeat_timeout_ms',context),12000);
  assert.equal(vm.runInContext('draft.image',context),rust.image);
  assert.match(get('backend-policy-note').textContent,/12 秒/);
  rust.cameras[0].imu={enabled:true};reset(rust);get('capture-backend').value='official';vm.runInContext('changeBackend()',context);
  assert.equal(vm.runInContext('draft.capture_backend',context),'rust');assert.equal(vm.runInContext('draft.cameras[0].imu.enabled',context),true);
  delete rust.cameras[0].imu;rust.cameras[0].profile='gemini_305';reset(rust);get('capture-backend').value='official';vm.runInContext('changeBackend()',context);
  assert.equal(vm.runInContext('draft.capture_backend',context),'rust');
  const clocked=structuredClone(rust);clocked.cameras[0].profile='gemini_335l';
  clocked.rust_capture.acquisition={preset:'complete'};
  for(const c of clocked.cameras)c.timestamp_source='global';
  reset(clocked);assert.equal(get('camera-timestamp-source').readOnly,true);
  get('capture-backend').value='official';vm.runInContext('changeBackend()',context);
  assert.equal(get('camera-timestamp-source').readOnly,false);assert.equal(get('color_gain').disabled,false);
  get('capture-backend').value='rust';vm.runInContext('changeBackend()',context);
  assert.equal(get('camera-timestamp-source').readOnly,true);assert.equal(get('color_gain').disabled,true);
  assert.equal(get('camera-timestamp-source').value,'global');
  console.log('Web model/backend selection regression passed');
}
checkSelection().catch(e=>{console.error(e);process.exitCode=1;});
