'use strict';
const $ = id => document.getElementById(id);
const escapeHTML = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const csrf = document.querySelector('meta[name="csrf-token"]').content;
const numeric = ['color_width','color_height','color_fps','depth_width','depth_height','depth_fps','color_exposure','color_gain','ir_exposure','ir_gain'];
const booleans = ['enable_color','enable_depth','enable_color_auto_exposure','enable_ir_auto_exposure'];
const officialOnly = ['color_exposure','color_gain','ir_exposure','ir_gain','enable_color_auto_exposure','enable_ir_auto_exposure'];
const parameterSupported = key => draft?.capture_backend !== 'rust' || !officialOnly.includes(key);
const formats = ['color_format','depth_format'];
let state, draft, revision, role = null, dirty = false, pending = false, jobBusy = false, polling = false, logMode = 'job';
let saving = false;
function notify(message, kind='success') { $('notice').hidden = false; $('notice').className = kind; $('notice').textContent = message; }
async function api(path, body) {
  const response = await fetch(path, body === undefined ? {cache:'no-store'} : {method:'POST', headers:{'Content-Type':'application/json','X-CSRF-Token':csrf}, body:JSON.stringify(body)});
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
  return data;
}
const profiles = {gemini_335l:'Gemini 335L', gemini_2:'Gemini 2', gemini_305:'Gemini 305'};
const model = c => c.model || profiles[c.profile] || c.profile;
const healthKeys = ['discovery_seconds','sample_seconds','freshness_seconds','min_fps_ratio'];
function camera() { return draft.cameras.find(c => c.name === role); }
function setDirty(value) { dirty = value; $('dirty-badge').textContent = value ? '有未保存修改' : '已保存'; }
function bindings() {
  const cameraData = camera(); const devices = state.devices || [];
  const options = devices.filter(d => d.model === model(cameraData) && d.serial_number);
  if (!options.some(d => d.serial_number === cameraData.serial_number)) {
    options.unshift({serial_number:cameraData.serial_number, product:!cameraData.serial_number || cameraData.serial_number.startsWith('REPLACE_') ? '请选择扫描到的设备' : '已保存 / 当前未连接'});
  }
  $('serial').innerHTML = options.map(d => `<option value="${escapeHTML(d.serial_number)}">${escapeHTML(d.product)} · ${escapeHTML(d.serial_number)}</option>`).join('');
  $('serial').value = cameraData.serial_number;
}
function showAcquisition() {
  const rust = draft.capture_backend === 'rust';
  $('acquisition-fields').hidden = !rust;
  if (!rust) return;
  const policy = draft.rust_capture?.acquisition;
  const preset = policy?.preset || 'legacy';
  const advanced = policy?.advanced || {};
  $('acquisition-preset').value = preset;
  $('acquisition-timestamp').value = advanced.message_timestamp || 'global';
  $('acquisition-aggregate').value = advanced.aggregate || 'any';
  $('acquisition-sync').checked = advanced.sdk_frame_sync ?? preset === 'strict-rgbd';
  $('acquisition-advanced').hidden = preset === 'legacy';
  $('acquisition-aggregate').querySelector('option[value="all"]').disabled = preset === 'complete';
  $('acquisition-aggregate').querySelector('option[value="disabled"]').disabled = preset === 'strict-rgbd';
  $('acquisition-note').textContent = preset === 'legacy' ? '保留原有 SDK 行为和时间戳；选择预设后才会应用新策略。' :
    preset === 'complete' ? '有效彩色和深度图像独立发布；SDK 默认关闭软件同步，配对失败不影响原始图像发布。' :
    preset === 'strict-rgbd' ? 'SDK 开启软件同步；仅发布通过时间差检查的 RGB-D 对，未配对帧不发布。' : '硬件同步尚无配置和验证通路，无法启动。';
  const stamped = rust && Boolean(policy);
  $('camera-timestamp-source').readOnly = stamped;
}
function readAcquisition(event) {
  if (!draft || draft.capture_backend !== 'rust') return;
  if (!event?.target?.id?.startsWith('acquisition-')) return;
  const preset = $('acquisition-preset').value;
  if (preset === 'legacy') {
    delete draft.rust_capture.acquisition;
    for (const c of draft.cameras) c.timestamp_source = 'host';
  } else {
    if (event.target.id === 'acquisition-preset') {
      $('acquisition-timestamp').value = 'global';
      $('acquisition-aggregate').value = 'any';
      $('acquisition-sync').checked = preset === 'strict-rgbd';
    }
    const message_timestamp = $('acquisition-timestamp').value;
    draft.rust_capture.acquisition = {preset,advanced:{
      sdk_frame_sync:$('acquisition-sync').checked,
      aggregate:$('acquisition-aggregate').value,message_timestamp,
    }};
    for (const c of draft.cameras) c.timestamp_source = message_timestamp === 'global' ? 'global' : 'host';
  }
  for (const c of draft.cameras) { c.profiles_verified = false; c.verification = null; }
  $('camera-timestamp-source').value = camera().timestamp_source;
  showAcquisition();
}
function showRole() {
  if (!draft) return;
  if (!draft.cameras.some(c => c.name === role)) role = draft.cameras[0]?.name;
  const c = camera();
  if (!c) return;
  for (const stream of ['color','depth']) { $(stream+'-image').hidden=true; $(stream+'-image').removeAttribute('src'); delete $(stream+'-image').dataset.loading; }
  document.querySelectorAll('.role-tab').forEach(tab => tab.classList.toggle('active', tab.dataset.role === role));
  $('role-title').textContent = c.name;
  $('role-subtitle').textContent = `${model(c)} · ${c.namespace || '/' + role + '_camera'}`;
  bindings();
  $('color_exposure').closest('details').hidden = draft.capture_backend === 'rust';
  numeric.forEach(key => $(key).value = c.parameters[key] ?? (key.endsWith('exposure') || key.endsWith('gain') ? -1 : ''));
  $('v2-fields').hidden = draft.schema_version !== 2;
  $('camera-enabled').checked = c.enabled;
  for (const key of ['namespace','frame_prefix','timestamp_source','calibration_revision']) $('camera-'+key.replaceAll('_','-')).value = c[key] || '';
  for (const key of healthKeys) $('health-'+key).value = c.health[key] ?? '';
  $('color_format').disabled = draft.schema_version === 2;
  $('depth_format').disabled = draft.schema_version === 2;
  booleans.forEach(key => $(key).checked = c.parameters[key] ?? true);
  formats.forEach(key => $(key).value = c.parameters[key]);
  $('profile-note').textContent = '尺寸、帧率与时间来源需按实机填写；出帧确认不替代标定及语义验收。';
  for (const stream of ['color','depth']) for (const key of ['width','height','fps']) $(stream+'_'+key).required = c.enabled && c.parameters['enable_'+stream];
  showAcquisition();
  updatePreview();
}
function readForm(event) {
  if (!draft) return;
  readAcquisition(event);
  const c = camera(); c.serial_number = $('serial').value;
  numeric.forEach(key => {if (parameterSupported(key) && $(key).value !== '') c.parameters[key] = Number($(key).value);});
  if (draft.schema_version === 2) {
    c.enabled = $('camera-enabled').checked;
    for (const key of ['namespace','frame_prefix','timestamp_source','calibration_revision']) c[key] = $('camera-'+key.replaceAll('_','-')).value;
    for (const key of healthKeys) if ($('health-'+key).value !== '') c.health[key] = Number($('health-'+key).value);
    c.verification = null;
  }
  booleans.filter(parameterSupported).forEach(key => c.parameters[key] = $(key).checked);
  formats.forEach(key => c.parameters[key] = $(key).value);
  for (const stream of ['color','depth']) for (const key of ['width','height','fps']) $(stream+'_'+key).required = c.enabled && c.parameters['enable_'+stream];
  c.profiles_verified = false; setDirty(true);
}
function updatePreview() {
  if (!state || !draft) return;
  const preview = state.previews[role] || {}; const running = preview.running;
  $('preview-status').textContent = running ? '采集中' : '未启动';
  $('preview-start').disabled = pending || jobBusy || Boolean(state.docker_error) || running;
  $('preview-stop').disabled = pending || jobBusy || !running;
  $('confirm').disabled = pending || jobBusy || !running || dirty;
  $('preview-logs').disabled = pending || jobBusy || Boolean(state.docker_error);
  for (const stream of ['color','depth']) {
    const stats = preview.streams?.[stream] || {};
    const fresh = running && (Date.now()/1000 - (stats.received_at || 0) < 3) && !stats.error;
    const enabled = camera().parameters['enable_'+stream];
    $(stream+'-stats').textContent = fresh ? `${stats.width}×${stats.height} / ${stats.fps ?? 0} FPS` : '—';
    $(stream+'-image').hidden = !fresh;
    $(stream+'-empty').hidden = fresh;
    $(stream+'-empty').textContent = !enabled ? '此流已禁用' : stats.error || (running ? '等待有效画面…' : '预览未启动');
    if (!fresh) $(stream+'-image').removeAttribute('src');
  }
  if (preview.error) $('preview-status').textContent = '预览进程已退出';
}
async function saveConfig() {
  if (!draft) return;
  const cameras = draft.cameras.map(c => ({name:c.name,serial_number:c.serial_number,
    ...(draft.schema_version === 2 ? Object.fromEntries(['enabled','namespace','frame_prefix','timestamp_source','calibration_revision','health'].map(k=>[k,c[k]])) : {}),
    parameters:Object.fromEntries([...numeric,...booleans,...formats].filter(k => parameterSupported(k) && k in c.parameters).map(k=>[k,c.parameters[k]]))}));
  const result = await api('/api/config', {revision,cameras,...(draft.capture_backend === 'rust' ? {acquisition:draft.rust_capture?.acquisition || null} : {})});
  const fresh = await api('/api/state'); state = fresh; revision = fresh.revision;
  draft = structuredClone(fresh.inventory); setDirty(false); showRole(); notify(result.message);
}
async function operation(fn) {
  if (pending || jobBusy) return;
  pending = true; document.body.classList.add('busy'); updateButtons();
  try { await fn(); } catch (error) { notify(error.message, 'error'); }
  finally { pending = false; document.body.classList.remove('busy'); await refresh(false); updateButtons(); }
}
function updateButtons() {
  document.querySelectorAll('[data-action]').forEach(button => button.disabled = !state || pending || jobBusy || Boolean(state?.docker_error));
  $('save').disabled = pending || jobBusy || !draft;
  document.querySelectorAll('.role-tab').forEach(button => button.disabled = pending);
  $('refresh').disabled = pending;
  document.querySelectorAll('#camera-form input,#camera-form select').forEach(input => input.disabled = pending || jobBusy || !parameterSupported(input.id) || (draft?.schema_version === 2 && formats.includes(input.id)));
  if (draft?.rust_capture?.acquisition?.preset === 'strict-rgbd') $('acquisition-sync').disabled = true;
  updatePreview();
}
async function refresh(manual=false) {
  if (polling) return;
  polling = true;
  try {
    const previousRevision = revision;
    state = await api('/api/state');
    if (!draft || (!dirty && !pending && !saving)) {
      draft = structuredClone(state.inventory); revision = state.revision;
      if (!previousRevision || previousRevision !== revision || manual) {
        $('camera-tabs').innerHTML = draft.cameras.map(c => `<button class="role-tab" data-role="${escapeHTML(c.name)}"><span>${escapeHTML(c.name)}</span><small>${escapeHTML(model(c))}</small><b>${c.profiles_verified ? '已确认' : '待验证'}</b></button>`).join('');
        showRole();
      }
    }
    $('host-state').textContent = state.platform === 'Linux' && !state.docker_error ? 'Linux 主机已连接' : '硬件环境未就绪';
    $('host-indicator').classList.toggle('ready', state.platform === 'Linux' && !state.docker_error);
    $('scan-count').textContent = `发现 ${state.devices.length} 个 Orbbec USB 设备`;
    $('device-list').innerHTML = state.devices.length ? state.devices.map(d => `<div class="device"><strong>${escapeHTML(d.model || '未识别型号')}</strong><div class="meta">SN ${escapeHTML(d.serial_number || '无序列号')}<br>USB ${escapeHTML(d.port)} · ${escapeHTML(d.speed_mbps || '未知')} Mb/s<br>${escapeHTML(d.product)}</div></div>`).join('') : '<div class="empty-devices">尚未发现相机。请在 Linux 主机连接设备，安装 udev 规则后重新扫描。界面不会生成模拟设备。</div>';
    $('docker-warning').hidden = !state.docker_error;
    $('docker-warning').textContent = state.docker_error ? `Docker 不可用：${state.docker_error}。可以编辑已保存的配置；预览与部署需要 Linux Docker 主机。` : '';
    document.querySelectorAll('.role-tab').forEach(tab=>{const c=state.inventory.cameras.find(c=>c.name===tab.dataset.role);if(c)tab.querySelector('b').textContent=c.profiles_verified?'已确认':'待验证';});
    jobBusy = state.job?.status === 'running';
    $('job-status').textContent = state.job ? `${state.job.action} / ${{running:'运行中',passed:'通过',failed:'失败'}[state.job.status]}` : '空闲';
    if (state.job && logMode === 'job') $('logs').textContent = state.job.log.join('\n') || '操作执行中，等待输出…';
    $('services').innerHTML = state.services.length ? state.services.map(s=>`<span class="service-item">${escapeHTML(s.Names)} · ${escapeHTML(s.Status)}</span>`).join('') : '没有正式相机容器。';
    $('config-state').textContent = dirty ? '浏览器有未保存修改。' : (state.generated_current ? '部署配置与保存内容一致。' : '配置尚未生成或已有更新，启动前需要重新生成。');
    if (manual && draft && !dirty) bindings();
    updateButtons();
  } catch (error) { $('host-state').textContent = '连接中断'; notify(error.message, 'error'); }
  finally { polling = false; }
}
$('camera-form').addEventListener('input',readForm);
$('camera-form').addEventListener('change',readForm);
$('camera-form').addEventListener('submit',event=>{ event.preventDefault(); operation(async()=>{saving=true;try{await saveConfig();}finally{saving=false;}}); });
$('camera-tabs').addEventListener('click',event=>{const button=event.target.closest('.role-tab');if(button&&!pending){role=button.dataset.role;showRole();}});
$('refresh').addEventListener('click',()=>refresh(true));
$('preview-start').addEventListener('click',()=>operation(async()=>{if(dirty) await saveConfig();const result=await api('/api/preview/start',{role});notify(result.message);}));
$('preview-stop').addEventListener('click',()=>operation(async()=>notify((await api('/api/preview/stop',{role})).message)));
$('confirm').addEventListener('click',()=>operation(async()=>notify((await api('/api/confirm',{role})).message)));
$('preview-logs').addEventListener('click',()=>operation(async()=>{const data=await api('/api/preview/logs?role='+role);logMode='preview';$('logs').textContent=data.log||'暂无预览日志。';}));
document.querySelectorAll('[data-action]').forEach(button=>button.addEventListener('click',()=>operation(async()=>{
  if(dirty && ['render','preflight','up'].includes(button.dataset.action)) await saveConfig();
  logMode='job'; await api('/api/job',{action:button.dataset.action});notify('操作已提交，结果将显示在下方日志中。');
})));
setInterval(()=>refresh(false),2500);
setInterval(()=>{if(!state)return;for(const stream of ['color','depth']){const image=$(stream+'-image');if(!image.hidden&&!image.dataset.loading){image.dataset.loading='1';image.src=`/api/frame?role=${role}&stream=${stream}&t=${Date.now()}`;}}},350);
for(const stream of ['color','depth']){$(stream+'-image').addEventListener('load',()=>delete $(stream+'-image').dataset.loading);$(stream+'-image').addEventListener('error',()=>{delete $(stream+'-image').dataset.loading;$(stream+'-image').hidden=true;$(stream+'-empty').hidden=false;$(stream+'-empty').textContent='画面已过期，等待新帧…';});}
window.addEventListener('beforeunload',event=>{if(dirty){event.preventDefault();event.returnValue='';}});
updateButtons();
refresh(true);
$('refresh-quality').addEventListener('click', async () => {
  const results = [];
  for (const camera of state?.inventory?.cameras || []) {
    if (!camera.enabled) continue;
    try {
      const result = await api('/api/quality/current?camera=' + encodeURIComponent(camera.name));
      const quality = result.result.quality;
      results.push({camera:camera.name, session_kind:result.session_kind, state:quality.state, latest_event_sequence:quality.latest_event_sequence});
    } catch (_) { results.push({camera:camera.name, state:'观测不可用'}); }
  }
  $('quality-summary').textContent = JSON.stringify(results, null, 2);
});
