const $ = (selector) => document.querySelector(selector);
const store = { state: null, project: null, conversation: null, projectId: null, conversationId: null, poll: null, urls: [], contextScope: null };
const escapeHtml = (value) => String(value ?? '').replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char]));
const parse = (value) => { try { return JSON.parse(value); } catch { return null; } };
const number = (value, digits=1) => Number(value || 0).toFixed(digits);
const providerLabel = (provider) => ({anthropic:'Anthropic',openrouter:'OpenRouter',openai:'OpenAI'})[provider]||'LLM';
const defaultLlmModel = (provider) => ({anthropic:'claude-sonnet-4-6',openrouter:'anthropic/claude-sonnet-4.6',openai:'gpt-6-astra'})[provider]||'gpt-6-astra';
function setLlmProvider(provider, resetModel=false) {
  const link=$('#llm-key-link');
  link.href=({anthropic:'https://console.anthropic.com/settings/keys',openrouter:'https://openrouter.ai/settings/keys',openai:'https://platform.openai.com/api-keys'})[provider]||'https://platform.openai.com/api-keys';
  link.textContent=`Create an ${providerLabel(provider)} API key`;
  $('#llm-model').placeholder=defaultLlmModel(provider);
  const suggestions=({anthropic:['claude-sonnet-4-6','claude-haiku-4-5'],openrouter:['anthropic/claude-sonnet-4.6'],openai:['gpt-6-astra','gpt-6.1-sol','gpt-6-luna']})[provider]||[];
  $('#llm-models').innerHTML=suggestions.map(model=>`<option value="${model}">`).join('');
  if(resetModel) {$('#llm-model').value=defaultLlmModel(provider);$('#llm-api-key').value='';}
}
const openDialog = (selector) => { const dialog=$(selector); if(typeof dialog.showModal==='function') dialog.showModal(); else dialog.setAttribute('open',''); dialog.scrollTop=0; };
const closeDialog = (selector) => { const dialog=$(selector); if(typeof dialog.close==='function') dialog.close(); else dialog.removeAttribute('open'); };

async function api(path, options={}) {
  const headers = {...(options.headers || {})};
  if (options.method && !['GET','HEAD'].includes(options.method.toUpperCase())) headers['X-SliceAgent-Request'] = '1';
  if (options.body && !(options.body instanceof FormData)) headers['Content-Type'] = 'application/json';
  const response = await fetch(`/api${path}`, {...options, headers, credentials:'same-origin', cache:'no-store'});
  if (response.status === 401) {
    showLogin();
    throw new Error('Sign in required');
  }
  if (!response.ok) throw new Error((await response.text()).trim() || `Request failed: ${response.status}`);
  return response.json();
}

function toast(message, error=false) {
  const node=$('#toast'); node.textContent=message; node.classList.remove('hidden');
  node.classList.toggle('error',error);
  clearTimeout(toast.timer); toast.timer=setTimeout(()=>node.classList.add('hidden'),4800);
}

function showLogin() {
  const dialog=$('#login-dialog');
  if (!dialog.open) openDialog('#login-dialog');
  $('#login-token').focus();
}

async function bootstrap() {
  const response=await fetch('/api/auth/status',{credentials:'same-origin',cache:'no-store'});
  if (!response.ok) throw new Error('Could not check app access');
  if (!(await response.json()).authenticated) {showLogin();return;}
  await refreshState();
}

async function refreshState() {
  store.state=await api('/state');
  $('#slicer-dot').className=`dot ${store.state.slicer_available?'ok':'warn'}`;
  $('#slicer-dot').title=store.state.slicer_available?'PrusaSlicer is installed on this server':'PrusaSlicer was not found on this server';
  $('#connection-label').textContent=store.state.slicer_available?'':'Slicer unavailable';
  $('#agent-mode').textContent=store.state.llm_available?`${providerLabel(store.state.llm.provider)} · ${store.state.llm.model}`:'Connect LLM API · basic mode';
  renderProjectList(); renderPresetList();
  if (!store.projectId && store.state.projects.length) await selectProject(store.state.projects[0].id);
  const active=latestJob();
  if(active && ['queued','interpreting','slicing'].includes(active.status))pollJob(active.id);
}

function renderProjectList() {
  $('#project-list').innerHTML=store.state.projects.map(p=>`<button class="project-item ${p.id===store.projectId?'active':''}" data-project="${p.id}"><span class="project-icon">▦</span><span>${escapeHtml(p.name)}</span></button>`).join('');
  $('#project-list').querySelectorAll('[data-project]').forEach(button=>button.onclick=()=>selectProject(button.dataset.project));
  $('#workspace').classList.toggle('hidden',!store.projectId);
  $('#empty-state').classList.toggle('hidden',!!store.projectId);
}

async function selectProject(id) {
  store.projectId=id; store.conversationId=null;
  await refreshProject();
  if (store.project.conversations.length) await selectConversation(store.project.conversations[0].id);
  renderProjectList();
}

async function refreshProject() {
  if (!store.projectId) return;
  store.project=await api(`/projects/${store.projectId}`);
  const p=store.project.project;
  $('#project-title').textContent=p.name;
  $('#asset-count').textContent=store.project.assets.length;
  renderAssets(); renderDefaults(); renderConversations(); renderResults(); renderTotals();
}

function renderAssets() {
  const assets=store.project.assets;
  $('#assets').innerHTML=assets.length?assets.map(a=>{
    const info=parse(a.info_json);
    const dims=info?.size_mm?.map(v=>number(v,0)).join(' × ');
    const meta=a.kind==='stl'?(dims?`${dims} mm · ${info.triangles} triangles`:'STL · needs inspection'):'3MF · stored source';
    return `<div class="asset"><span class="asset-glyph">◇</span><span class="asset-main"><span class="asset-name" title="${escapeHtml(a.name)}">${escapeHtml(a.name)}</span><span class="asset-meta">${escapeHtml(meta)}</span></span></div>`;
  }).join(''):'<p class="quiet">Add STL files to begin. Dragging them into the upload control works on desktop or phone.</p>';
}

function renderDefaults() {
  const project=store.project.project;
  const preset=store.state.presets.find(p=>p.id===project.effective_preset_id);
  const globalPreset=store.state.presets.find(p=>p.id===store.state.settings?.default_preset_id);
  $('#default-summary').innerHTML=`<label class="setting-row">Preset<select id="inline-preset"><option value="global">Global default · ${escapeHtml(globalPreset?.name||'none')}</option>${store.state.presets.map(p=>`<option value="${p.id}">${escapeHtml(p.location||'Unassigned')} · ${escapeHtml(p.printer)} · ${escapeHtml(p.filament_profile)} · ${escapeHtml(p.name)}</option>`).join('')}</select></label><div class="setting-row"><span>Location</span><strong>${escapeHtml(preset?.location||'Unassigned')}</strong></div><div class="setting-row"><span>Printer</span><strong>${escapeHtml(preset?.printer||'Choose a preset')}</strong></div><div class="setting-row"><span>Print profile</span><strong>${escapeHtml(preset?.print_profile||'—')}</strong></div><div class="setting-row"><span>Filament</span><strong>${escapeHtml(preset?.filament_profile||'—')}</strong></div>`;
  $('#inline-preset').value=project.default_preset_id||'global';
  $('#inline-preset').onchange=async event=>{try{const value=event.target.value;await api(`/projects/${store.projectId}`,{method:'PATCH',body:JSON.stringify(value==='global'?{inherit_preset:true}:{default_preset_id:value})});await refreshProject();toast('Project preset saved')}catch(error){toast(error.message,true)}};
  $('#inline-hours').value=number(project.default_max_minutes/60,1);
  $('#inline-hours').onchange=async event=>{try{const hours=Number(event.target.value);if(!Number.isFinite(hours)||hours<=0)throw new Error('Enter a positive number of hours');await api(`/projects/${store.projectId}`,{method:'PATCH',body:JSON.stringify({default_max_minutes:hours*60})});await refreshProject();toast('Project time limit saved')}catch(error){toast(error.message,true)}};
  const hasText=!!project.context_text?.trim();const file=store.project.context_document?.name;
  $('#project-context-summary').textContent=hasText||file?[hasText?'Instructions added':null,file?`File: ${file}`:null].filter(Boolean).join(' · '):'No project instructions added.';
}

function renderConversations() {
  $('#conversation-list').innerHTML=store.project.conversations.map(c=>`<button class="conversation-pill ${c.id===store.conversationId?'active':''}" data-conversation="${c.id}" title="${escapeHtml(c.title)}">${escapeHtml(c.title)}</button>`).join('');
  $('#conversation-list').querySelectorAll('[data-conversation]').forEach(button=>button.onclick=()=>selectConversation(button.dataset.conversation));
  if (!store.project.conversations.length) {
    $('#chat-context-button').disabled=true;
    $('#chat-settings-button').disabled=true;
    $('#chat-title').textContent='New conversation';
    $('#messages').innerHTML='<div class="starter"><strong>Tell me what you need to print.</strong><p>Try “slice these parts for MK4S” or “fit as many as possible on each plate under 4 hours and 300 grams.” I’ll use your saved defaults.</p></div>';
  }
}

async function selectConversation(id) {
  store.conversationId=id;
  store.conversation=await api(`/conversations/${id}`);
  await refreshProject();
  $('#chat-title').textContent=store.conversation.conversation.title;
  $('#chat-context-button').disabled=false;
  $('#chat-settings-button').disabled=false;
  renderMessages();
}

function currentConversationJobs() {
  if(store.conversation?.conversation?.id===store.conversationId)return store.conversation.jobs||[];
  return (store.project?.jobs||[]).filter(job=>job.conversation_id===store.conversationId);
}

function jobProgressLabel(job) {
  const progress=parse(job?.progress_json);
  return job?.status==='queued'?'Queued':job?.status==='interpreting'?'Reading your request':progress?.phase==='exporting plate'?`Exporting plate ${progress.plate}`:progress?.total?`Trying plate ${progress.plate} · part ${progress.part} of ${progress.total}`:'Preparing the models';
}

function renderMessages() {
  const messages=store.conversation?.messages || [];
  const job=latestJob();
  const working=job && ['queued','interpreting','slicing'].includes(job.status);
  const label=jobProgressLabel(job);
  $('#messages').innerHTML=(messages.length?messages.map(m=>`<div class="message ${m.role}">${m.role==='assistant'?'<span class="role">SLICEAGENT</span>':''}<div>${escapeHtml(m.content).replace(/\n/g,'<br>')}</div><time>${new Date(m.created_at).toLocaleString()}</time></div>`).join(''):'<div class="starter"><strong>Ready when you are.</strong><p>Upload or import your models, then ask for a plate. Saved defaults apply automatically.</p></div>')+(working?`<div class="message assistant working" role="status"><span class="role">SLICEAGENT · WORKING</span><div>${escapeHtml(label)}…</div></div>`:'');
  $('#messages').scrollTop=$('#messages').scrollHeight;
}

function latestJob() {
  return currentConversationJobs()[0];
}

async function protectedImage(url, img) {
  try {
    const response=await fetch(url,{credentials:'same-origin',cache:'no-store'});if(response.status===401){showLogin();return}if(!response.ok)return;
    const objectUrl=URL.createObjectURL(await response.blob());store.urls.push(objectUrl);img.src=objectUrl;
  } catch {}
}

function renderResults() {
  store.urls.forEach(URL.revokeObjectURL);store.urls=[];
  const job=latestJob();
  const indicator=$('#job-indicator');
  indicator.className=`job-indicator ${job?.status||''}`;
  indicator.textContent=job?({queued:'Queued',interpreting:'Understanding request',slicing:'Slicing plates',planned:'Parts reviewed',partial:'Partial result',complete:'Complete',failed:'Needs attention'}[job.status]||job.status):'No run yet';
  $('#status-help').classList.toggle('hidden',!job);
  const container=$('#results');
  if(!job){container.className='';container.innerHTML='';return;}
  const available=currentConversationJobs().map(item=>({job:item,result:parse(item.result_json)})).find(item=>item.result?.plates?.length);
  const result=available?.result;
  const hasPlateResult=!!result;
  const currentIsRunning=['queued','interpreting','slicing'].includes(job.status);
  const statusNote=job.status==='failed'?`<div class="warning">${escapeHtml(job.error||'The job failed.')}</div>`:
    currentIsRunning?`<p class="quiet" role="status">${escapeHtml(jobProgressLabel(job))}…${hasPlateResult&&available.job.id===job.id?` ${result.plates.length} plate${result.plates.length===1?'':'s'} ready.`:''}</p>`:
    job.status==='planned'&&hasPlateResult?'<p class="quiet">Latest request reviewed parts. Showing plates from the previous slicing request.</p>':'';
  if(!hasPlateResult){container.className='';container.innerHTML=statusNote;return;}
  const earlierResult=available.job.id!==job.id;
  const sourceNote=earlierResult&&job.status!=='planned'?'<p class="quiet">Showing plates from the previous slicing request.</p>':'';
  const plates=result.plates||[];
  const omitted=(result.excluded_details?.length?result.excluded_details:(result.excluded||[]).map(name=>({name,reason:'Could not meet the current bed, time, or material limit.'})));
  const totalParts=result.total_parts||plates.reduce((sum,plate)=>sum+plate.placements.length,0)+omitted.length;
  const feedback=`${omitted.map(item=>`<div class="warning"><strong>Not sliced: ${escapeHtml(item.name)}</strong><br>${escapeHtml(item.reason)}</div>`).join('')}${(result.warnings||[]).filter(w=>!omitted.length||!w.startsWith('Could not place')).map(w=>`<div class="warning model-note">${escapeHtml(w)}</div>`).join('')}`;
  container.className='';
  container.innerHTML=`${statusNote}${sourceNote}${feedback}${plates.map(plate=>`<article class="plate-card"><div class="plate-body"><div class="plate-title"><strong>Plate ${plate.number}</strong><span>${escapeHtml(result.printer)} · ${escapeHtml(result.preset_name)}</span></div><div class="part-list">${plate.placements.map((p,i)=>`${i+1}. ${escapeHtml(p.name)} · ${escapeHtml(p.orientation)}`).join('<br>')}</div><div class="metrics"><div class="metric"><strong>${number(plate.metrics.minutes/60,2)} h</strong><span>PRINT TIME</span></div><div class="metric"><strong>${number(plate.metrics.grams)} g</strong><span>FILAMENT · ${number(plate.metrics.metres,2)} m</span></div><div class="metric"><strong>CHF ${number(plate.estimated_cost_chf,2)}</strong><span>MAKERSPACE EST.</span></div><div class="metric"><strong>${plate.placements.length}/${totalParts}</strong><span>PARTS</span></div></div>${plate.filament_cost_chf!=null?`<p class="material-cost">Filament material: CHF ${number(plate.filament_cost_chf,2)} at CHF ${number(result.filament_price_chf_per_kg,2)}/kg</p>`:''}<div class="downloads"><span class="export-label">Export:</span><button class="download-link" data-download="${plate.files.project}" data-name="${escapeHtml(result.printer)}-plate-${plate.number}.3mf">.3mf</button><button class="download-link" data-download="${plate.files.bgcode}" data-name="${escapeHtml(result.printer)}-plate-${plate.number}.bgcode">Print .bgcode</button></div></div><div class="plate-preview"><img data-preview="${plate.number}" alt="Plate ${plate.number} layout preview"></div></article>`).join('')}`;
  plates.forEach(plate=>{const img=container.querySelector(`[data-preview="${plate.number}"]`);if(img)protectedImage(plate.files.preview,img)});
  container.querySelectorAll('[data-download]').forEach(button=>button.onclick=()=>downloadFile(button.dataset.download,button.dataset.name));
}

async function downloadFile(url,name) {
  try {
    const response=await fetch(url,{credentials:'same-origin',cache:'no-store'});if(response.status===401){showLogin();return}if(!response.ok)throw new Error('Download failed');
    const objectUrl=URL.createObjectURL(await response.blob());const link=document.createElement('a');link.href=objectUrl;link.download=name;link.click();setTimeout(()=>URL.revokeObjectURL(objectUrl),10000);
  } catch(error){toast(error.message,true)}
}

function renderTotals() {
  const jobs=store.project.jobs.filter(j=>['complete','partial'].includes(j.status)).map(j=>parse(j.result_json)).filter(Boolean);
  const grams=jobs.reduce((sum,j)=>sum+(j.total_grams||0),0);
  const chf=jobs.reduce((sum,j)=>sum+(j.total_cost_chf||0),0);
  const material=jobs.flatMap(j=>j.plates||[]).reduce((sum,p)=>sum+(p.filament_cost_chf||0),0);
  $('#project-totals').textContent=jobs.length?`${number(grams)} g planned · CHF ${number(chf,2)} makerspace${material?` · CHF ${number(material,2)} filament`:''}`:'No slicing costs yet';
  const usage=store.project.usage||[];
  const tokens=usage.reduce((sum,u)=>sum+(u.input_tokens||0)+(u.output_tokens||0),0);
  const cost=usage.reduce((sum,u)=>sum+(u.cost_usd||0),0);
  const priced=usage.every(u=>u.cost_usd!==null);
  const known=usage.some(u=>u.cost_usd!==null);
  $('#llm-totals').textContent=tokens?`LLM: ${tokens.toLocaleString()} tokens${known?` · $${number(cost,cost<0.01?6:3)}${priced?'':' recorded; some calls unpriced'}`:' · set API rates to estimate cost'}`:'LLM: no calls yet';
}

function renderPresetList() {
  if(!store.state)return;
  $('#preset-list').innerHTML=store.state.presets.map(p=>{const price=store.state.filament_prices?.find(x=>x.location===p.location&&x.filament_profile===p.filament_profile);return `<div class="preset-row"><div><strong>${escapeHtml(p.name)}</strong><small>${escapeHtml(p.location||'Unassigned')} · ${escapeHtml(p.printer)} · ${escapeHtml(p.filament_profile)} · ${p.bed_width} × ${p.bed_depth} mm · ${p.ini_path?'INI loaded':'named profiles'}${price?` · CHF ${number(price.chf_per_kg,2)}/kg`:''}</small></div><div><button class="small-button ghost" data-edit-preset="${p.id}">Edit</button> <button class="small-button ghost" data-price-preset="${p.id}">Price</button> <label class="small-button">Upload INI<input type="file" accept=".ini" data-ini="${p.id}"></label></div></div>`}).join('');
  $('#preset-list').querySelectorAll('[data-edit-preset]').forEach(button=>button.onclick=()=>editPreset(button.dataset.editPreset));
  $('#preset-list').querySelectorAll('[data-price-preset]').forEach(button=>button.onclick=()=>{const p=store.state.presets.find(x=>x.id===button.dataset.pricePreset);const price=store.state.filament_prices?.find(x=>x.location===p.location&&x.filament_profile===p.filament_profile);$('#price-location').value=p.location;$('#price-filament').value=p.filament_profile;$('#price-per-kg').value=price?.chf_per_kg??'';$('#filament-price-form').scrollIntoView({behavior:'smooth',block:'start'});});
  $('#preset-list').querySelectorAll('[data-ini]').forEach(input=>input.onchange=()=>uploadIni(input.dataset.ini,input.files[0]));
}

function editPreset(id) {
  const p=store.state.presets.find(item=>item.id===id);if(!p)return;
  const form=$('#preset-form');$('#preset-edit-id').value=id;
  ['location','name','printer','printer_profile','print_profile','filament_profile','bed_width','bed_depth','gap'].forEach(key=>form.elements[key].value=p[key]??'');
  const settings=parse(p.settings_json)||{};
  ['layer_height','fill_density','perimeters','support_material','brim_width','top_solid_layers','bottom_solid_layers'].forEach(key=>form.elements[key].value=settings[key]?.replace?.('%','')??'');
  $('#preset-form-title').textContent='Edit preset';$('#preset-save').textContent='Save changes';
  form.scrollIntoView({behavior:'smooth',block:'start'});
}

function resetPresetForm(){const form=$('#preset-form');form.reset();$('#preset-edit-id').value='';$('#preset-form-title').textContent='Add a preset';$('#preset-save').textContent='Add preset'}

async function uploadIni(id,file) {
  if(!file)return;const form=new FormData();form.append('file',file);
  try{await api(`/presets/${id}/ini`,{method:'POST',body:form});toast('Preset INI loaded');await refreshState();if(store.projectId)await refreshProject()}
  catch(error){toast(error.message,true)}
}

async function pollJob(jobId) {
  clearInterval(store.poll);
  store.poll=setInterval(async()=>{
    try{const job=await api(`/jobs/${jobId}`);if(store.conversationId)await selectConversation(store.conversationId);if(['complete','partial','failed','planned'].includes(job.status)){clearInterval(store.poll);toast(job.status==='complete'?'Plates are ready':job.status==='partial'?'Partial plate is ready':job.status==='planned'?'Parts review is ready':job.error||'Job failed',job.status==='failed')}}catch(error){clearInterval(store.poll);toast(error.message,true)}
  },2500);
}

function wireDialogs() {
  document.querySelectorAll('.close-dialog').forEach(button=>button.onclick=()=>{const dialog=button.closest('dialog');if(typeof dialog.close==='function')dialog.close();else dialog.removeAttribute('open')});
  ['new-project','empty-new-project'].forEach(id=>$('#'+id).onclick=()=>openDialog('#project-dialog'));
  $('#import-repo').onclick=()=>openDialog('#import-dialog');
  $('#edit-defaults').onclick=()=>{
    const p=store.project.project;
    $('#settings-price').value=p.price_rate;$('#settings-unit').value=p.price_unit;$('#settings-email').value=p.email||'';
    openDialog('#settings-dialog');
  };
  $('#open-global-settings').onclick=()=>{
    $('#global-preset').innerHTML=store.state.presets.map(p=>`<option value="${p.id}">${escapeHtml(p.name)}</option>`).join('');
    $('#global-preset').value=store.state.settings?.default_preset_id||'';
    $('#global-email').value=store.state.settings?.default_email||'';
    $('#llm-provider').value=store.state.llm?.provider||'openai';
    setLlmProvider($('#llm-provider').value);
    $('#llm-model').value=store.state.llm?.model||defaultLlmModel($('#llm-provider').value);
    $('#llm-api-key').value='';
    $('#llm-connection-status').textContent=store.state.llm_available?`Configured: ${providerLabel(store.state.llm.provider)} · ${store.state.llm.model} (${store.state.llm.source==='environment'?'server environment':'saved on this server'}).`:'No LLM API configured; requests currently use the basic parser.';
    $('#llm-remove').disabled=!store.state.llm?.saved_configuration;
    openDialog('#global-settings-dialog');
  };
  $('#agent-mode').onclick=()=>$('#open-global-settings').click();
  $('#status-help').onclick=()=>{
    const status=latestJob()?.status;
    const explanations={complete:'All selected parts were placed and slicing files were generated. This does not mean the parts were printed.',partial:'Files were generated for some selected parts. Other parts were omitted; their reasons appear above the plates.',planned:'The files were reviewed without slicing.',failed:'The request stopped. The error appears above any plates already exported.',queued:'The request is waiting to start.',interpreting:'SliceAgent is reading the request.',slicing:'SliceAgent is testing layouts and checking PrusaSlicer estimates.'};
    const hasEarlierPlates=currentConversationJobs().slice(1).some(job=>parse(job.result_json)?.plates?.length);
    $('#status-explanation').textContent=(explanations[status]||'No slicing request has run in this chat.')+(hasEarlierPlates&&['planned','failed','queued','interpreting','slicing'].includes(status)?' The Output panel keeps the most recent available plates visible.':'');
    openDialog('#status-dialog');
  };
  $('#open-presets').onclick=()=>openDialog('#presets-dialog');
  $('#chat-settings-button').onclick=()=>{
    const chat=store.conversation?.conversation;if(!chat)return;
    const projectPreset=store.state.presets.find(p=>p.id===store.project.project.effective_preset_id);
    $('#chat-preset').innerHTML=`<option value="">Use project · ${escapeHtml(projectPreset?.name||'global default')}</option>${store.state.presets.map(p=>`<option value="${p.id}">${escapeHtml(p.name)}</option>`).join('')}`;
    $('#chat-preset').value=chat.preset_id||'';
    $('#chat-hours').value=chat.max_minutes?number(chat.max_minutes/60,1):'';
    $('#chat-grams').value=chat.max_grams??'';
    $('#chat-email').value=chat.email||'';
    openDialog('#chat-settings-dialog');
  };
  $('#chat-settings-form').onsubmit=async event=>{event.preventDefault();try{
    const preset=$('#chat-preset').value,hours=$('#chat-hours').value,grams=$('#chat-grams').value,email=$('#chat-email').value.trim();
    await api(`/conversations/${store.conversationId}/settings`,{method:'PATCH',body:JSON.stringify({...(preset?{preset_id:preset}:{inherit_preset:true}),...(hours?{max_minutes:Number(hours)*60}:{inherit_minutes:true}),...(grams?{max_grams:Number(grams)}:{inherit_grams:true}),...(email?{email}:{inherit_email:true})})});
    closeDialog('#chat-settings-dialog');await selectConversation(store.conversationId);toast('Chat settings saved');
  }catch(error){toast(error.message,true)}};
  $('#project-context-button').onclick=()=>openContext('project');
  $('#chat-context-button').onclick=()=>openContext('conversation');
  $('#project-form').onsubmit=async event=>{event.preventDefault();try{const result=await api('/projects',{method:'POST',body:JSON.stringify({name:$('#project-name').value})});closeDialog('#project-dialog');$('#project-form').reset();await refreshState();await selectProject(result.id);toast('Project created')}catch(error){toast(error.message,true)}};
  $('#settings-form').onsubmit=async event=>{event.preventDefault();try{const email=$('#settings-email').value.trim();await api(`/projects/${store.projectId}`,{method:'PATCH',body:JSON.stringify({price_rate:Number($('#settings-price').value||0),price_unit:$('#settings-unit').value,...(email?{email}:{inherit_email:true})})});closeDialog('#settings-dialog');await refreshProject();toast('Project options saved')}catch(error){toast(error.message,true)}};
  $('#global-settings-form').onsubmit=async event=>{event.preventDefault();try{await api('/settings',{method:'PATCH',body:JSON.stringify({default_preset_id:$('#global-preset').value,default_email:$('#global-email').value.trim()})});closeDialog('#global-settings-dialog');await refreshState();if(store.projectId)await refreshProject();toast('Global defaults saved')}catch(error){toast(error.message,true)}};
  $('#llm-provider').onchange=event=>setLlmProvider(event.target.value,true);
  $('#llm-form').onsubmit=async event=>{event.preventDefault();try{await api('/llm/config',{method:'POST',body:JSON.stringify({provider:$('#llm-provider').value,model:$('#llm-model').value.trim(),api_key:$('#llm-api-key').value.trim()||null})});$('#llm-api-key').value='';await refreshState();$('#llm-connection-status').textContent=`Saved ${providerLabel(store.state.llm.provider)} · ${store.state.llm.model}. Use Test connection to verify the key and model.`;$('#llm-remove').disabled=false;toast('LLM connection saved')}catch(error){toast(error.message,true)}};
  $('#llm-test').onclick=async()=>{const button=$('#llm-test');button.disabled=true;$('#llm-connection-status').textContent=`Checking ${providerLabel($('#llm-provider').value)}…`;try{const result=await api('/llm/test',{method:'POST',body:JSON.stringify({provider:$('#llm-provider').value,model:$('#llm-model').value.trim(),api_key:$('#llm-api-key').value.trim()||null})});$('#llm-connection-status').textContent=`Connection verified for ${result.model}.${$('#llm-api-key').value?' Click Save connection to use this key.':''}`;toast(`${providerLabel(result.provider)} connection verified`)}catch(error){$('#llm-connection-status').textContent=error.message;toast(error.message,true)}finally{button.disabled=false}};
  $('#llm-remove').onclick=async()=>{try{await api('/llm/config',{method:'DELETE'});$('#llm-api-key').value='';await refreshState();$('#llm-connection-status').textContent=store.state.llm_available?'Saved key removed; the server environment key is still active.':'Disconnected. Requests now use the basic parser.';$('#llm-remove').disabled=true;toast('Saved LLM connection removed')}catch(error){toast(error.message,true)}};
  $('#context-form').onsubmit=async event=>{event.preventDefault();try{const route=contextRoute();await api(route,{method:'PATCH',body:JSON.stringify({text:$('#context-text').value})});await refreshProject();if(store.conversationId)await selectConversation(store.conversationId);toast('Context text saved')}catch(error){toast(error.message,true)}};
  $('#context-upload-form').onsubmit=async event=>{event.preventDefault();const file=$('#context-file').files[0];if(!file){toast('Choose a Markdown or text file',true);return}try{const form=new FormData();form.append('file',file);const result=await api(contextRoute(),{method:'POST',body:form});$('#context-file-status').textContent=`File: ${result.name}`;$('#context-file').value='';await refreshProject();if(store.conversationId)await selectConversation(store.conversationId);toast('Context file saved')}catch(error){toast(error.message,true)}};
  $('#import-form').onsubmit=async event=>{event.preventDefault();try{toast('Importing STL files…');const result=await api(`/projects/${store.projectId}/import`,{method:'POST',body:JSON.stringify({url:$('#import-url').value})});closeDialog('#import-dialog');$('#import-form').reset();await refreshProject();toast(`${result.asset_ids.length} STL files imported`)}catch(error){toast(error.message,true)}};
  $('#preset-form').onsubmit=async event=>{event.preventDefault();const form=event.target;const fields=Object.fromEntries(new FormData(form).entries());['bed_width','bed_depth','gap'].forEach(k=>fields[k]=Number(fields[k]));const settings={};['layer_height','fill_density','perimeters','support_material','brim_width','top_solid_layers','bottom_solid_layers'].forEach(k=>{if(fields[k]!=='')settings[k]=k==='fill_density'?`${fields[k]}%`:fields[k];delete fields[k]});fields.settings_json=JSON.stringify(settings);const edit=$('#preset-edit-id').value;try{await api(edit?`/presets/${edit}`:'/presets',{method:edit?'PATCH':'POST',body:JSON.stringify(fields)});resetPresetForm();await refreshState();if(store.projectId)await refreshProject();toast(edit?'Preset updated':'Preset added')}catch(error){toast(error.message,true)}};
  $('#filament-price-form').onsubmit=async event=>{event.preventDefault();try{await api('/filament-prices',{method:'POST',body:JSON.stringify({location:$('#price-location').value.trim(),filament_profile:$('#price-filament').value.trim(),chf_per_kg:Number($('#price-per-kg').value)})});await refreshState();toast('Filament price saved')}catch(error){toast(error.message,true)}};
  $('#preset-reset').onclick=resetPresetForm;
}

function contextRoute(){return store.contextScope==='project'?`/projects/${store.projectId}/context`:`/conversations/${store.conversationId}/context`}
function openContext(scope){
  if(scope==='conversation'&&!store.conversationId)return;
  store.contextScope=scope;
  const target=scope==='project'?store.project:store.conversation;
  const item=scope==='project'?target?.project:target?.conversation;
  $('#context-scope-label').textContent=scope==='project'?'PROJECT':'CHAT';
  $('#context-text').value=item?.context_text||'';
  $('#context-file-status').textContent=target?.context_document?.name?`File: ${target.context_document.name}`:'No file uploaded.';
  openDialog('#context-dialog');
}

function wireWorkspace() {
  $('#upload-input').onchange=async event=>{const files=[...event.target.files];if(!files.length)return;const form=new FormData();files.forEach(file=>form.append('files',file));try{toast('Adding model files…');await api(`/projects/${store.projectId}/assets`,{method:'POST',body:form});event.target.value='';await refreshProject();toast(`${files.length} file${files.length===1?'':'s'} added`)}catch(error){toast(error.message,true)}};
  $('#new-conversation').onclick=async()=>{try{const result=await api('/conversations',{method:'POST',body:JSON.stringify({project_id:store.projectId,title:'New conversation'})});await selectConversation(result.id)}catch(error){toast(error.message,true)}};
  $('#composer').onsubmit=async event=>{event.preventDefault();const text=$('#prompt').value.trim();if(!text)return;try{if(!store.conversationId){const conversation=await api('/conversations',{method:'POST',body:JSON.stringify({project_id:store.projectId,title:'New conversation'})});store.conversationId=conversation.id}const result=await api(`/conversations/${store.conversationId}/messages`,{method:'POST',body:JSON.stringify({text})});$('#prompt').value='';await selectConversation(store.conversationId);pollJob(result.job_id)}catch(error){toast(error.message,true)}};
  $('#prompt').onkeydown=event=>{if(event.key==='Enter'&&!event.shiftKey){event.preventDefault();$('#composer').requestSubmit()}};
  document.querySelectorAll('.suggestion').forEach(button=>button.onclick=()=>{$('#prompt').value=button.dataset.prompt;$('#prompt').focus()});
}

$('#login-dialog').addEventListener('cancel',event=>event.preventDefault());
$('#login-form').onsubmit=async event=>{
  event.preventDefault();
  const token=$('#login-token').value;
  $('#login-error').classList.add('hidden');
  try {
    const response=await fetch('/api/auth/login',{method:'POST',credentials:'same-origin',cache:'no-store',headers:{'Content-Type':'application/json','X-SliceAgent-Request':'1'},body:JSON.stringify({token})});
    if (!response.ok) throw new Error(response.status===401?'Invalid app access token':'Could not unlock the app');
    $('#login-token').value='';
    closeDialog('#login-dialog');
    await refreshState();
  } catch(error) {
    $('#login-error').textContent=error.message;
    $('#login-error').classList.remove('hidden');
  }
};
$('#lock-app').onclick=async()=>{
  try {await fetch('/api/auth/logout',{method:'POST',credentials:'same-origin',cache:'no-store',headers:{'X-SliceAgent-Request':'1'}});}
  finally {window.location.reload();}
};
wireDialogs();wireWorkspace();bootstrap().catch(error=>toast(error.message,true));
