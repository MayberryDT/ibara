// ibara page reader: service worker. Connects to ibara through Chrome native
// messaging (`ibara chrome-host`) and answers one request at a time about the
// focused tab. It never clicks or types; ibara does that with real input.
// Each native connection starts with only the `pages` job; ibara's `jobs`
// request adds `receive` (agent computers) or `share` (the sharing computer).
import { documentOperation } from './document.js';
const HOST='io.ibara.chrome';
const PAGE_OPS=['observe','check','locate','verify','selected','reveal','field','keys'];
// Operations that change the page (a refusal after they start is not "not started").
const EFFECTS=['navigate','reveal','cookies_write','cookies_remove','login_reload'];
// What each job lets ibara ask for. Anything else is refused unstarted.
const JOBS={
  pages:['tabs','lifecycle_inventory','navigate',...PAGE_OPS],
  receive:['cookies_write','cookies_remove','cookies_count','login_context','login_page','login_reload'],
  share:['cookies_read','browser_info'],
};
// Chrome caps a cookie's lifetime at 400 days from when it is set.
const MAX_LIFETIME_S=400*24*60*60;
let port;
let busy=false;
// Bounded by open tabs. No cookie is persisted by the extension.
const navigation=new Map();
const frameEpoch=new Map(),pendingFrames=new Map();
chrome.webNavigation.onBeforeNavigate.addListener(d=>{
  frameEpoch.set(d.tabId,(frameEpoch.get(d.tabId)||0)+1);
  let frames=pendingFrames.get(d.tabId);if(!frames){frames=new Set();pendingFrames.set(d.tabId,frames);}frames.add(d.frameId);
});
const frameSettled=d=>{const frames=pendingFrames.get(d.tabId);frames?.delete(d.frameId);if(!frames?.size)pendingFrames.delete(d.tabId);};
// Navigation stops blocking discovery once the new document has committed.
// Captures and input still bind its document ID and the navigation epoch.
chrome.webNavigation.onCommitted.addListener(frameSettled);
chrome.webNavigation.onCompleted.addListener(frameSettled);
chrome.webNavigation.onErrorOccurred.addListener(frameSettled);
chrome.tabs.onRemoved.addListener(id=>{frameEpoch.delete(id);pendingFrames.delete(id);for(const [key,value] of captures)if(value.tabId===id)captures.delete(key);});
chrome.webNavigation.onBeforeNavigate.addListener(d=>{
  if(d.frameId!==0)return;
  const old=navigation.get(d.tabId)||{};
  navigation.set(d.tabId,{...old,requested:old.requested||d.url});
});
chrome.webNavigation.onCommitted.addListener(d=>{
  if(d.frameId!==0)return;
  const old=navigation.get(d.tabId);
  const from=old?.requested&&old.requested!==d.url?old.requested:old?.url;
  navigation.set(d.tabId,{url:d.url,previous:from===d.url?old?.previous:from});
});
chrome.tabs.onRemoved.addListener(id=>navigation.delete(id));
const domain=c=>c.domain.replace(/^\./,'').toLowerCase();
const inSite=(c,site)=>domain(c)===site||domain(c).endsWith('.'+site);
// Always the secure scheme: cookies are scheme-bound, and from an `http://`
// URL Chrome refuses a non-secure cookie that shares its name with a secure
// one. A secure URL may set and remove non-secure cookies too.
const cookieUrl=c=>'https://'+domain(c)+(c.path||'/');
async function siteCookies(site){
  // An empty partitionKey selects every partition, including unpartitioned
  // cookies. The controller supplies a validated registrable domain.
  return (await chrome.cookies.getAll({domain:site,partitionKey:{}})).filter(c=>inSite(c,site));
}
// When a persistent cookie expires, clamped to Chrome's cap.
const expiry=(c,now)=>Math.min(c.expirationDate,now/1000+MAX_LIFETIME_S);
function cookieSet(c,now){
  const d={url:cookieUrl(c),name:c.name,value:c.value,path:c.path,secure:c.secure,httpOnly:c.httpOnly,sameSite:c.sameSite};
  if(!c.hostOnly)d.domain=c.domain;
  if(!c.session)d.expirationDate=expiry(c,now);
  if(c.partitionKey)d.partitionKey={topLevelSite:c.partitionKey.topLevelSite,hasCrossSiteAncestor:c.partitionKey.hasCrossSiteAncestor};
  return d;
}
const topSite=k=>(k?.topLevelSite||'').toLowerCase().replace(/\/$/,'');
// The first field Chrome stored differently from what was asked, compared as
// Chrome normalizes it, or null. Names a field, never a value.
function mismatch(saved,c,now){
  if(saved.value!==c.value)return 'value';
  if(domain(saved)!==domain(c))return 'domain';
  if(saved.hostOnly!==c.hostOnly)return 'hostOnly';
  if(saved.path!==c.path)return 'path';
  if(saved.secure!==c.secure)return 'secure';
  if(saved.httpOnly!==c.httpOnly)return 'httpOnly';
  if(saved.sameSite!==c.sameSite)return 'sameSite';
  if(saved.session!==c.session)return 'session';
  if(!c.session&&!(Math.abs(saved.expirationDate-expiry(c,now))<=2))return 'expirationDate';
  if(!!saved.partitionKey!==!!c.partitionKey||topSite(saved.partitionKey)!==topSite(c.partitionKey))return 'partitionKey';
  if(c.partitionKey&&(saved.partitionKey.hasCrossSiteAncestor??false)!==c.partitionKey.hasCrossSiteAncestor)return 'partitionKey';
  return null;
}
// Each cookie is set on its own; one that fails never stops the rest.
async function writeCookies(m,cookies){
  const failed=[];
  let written=0;
  for(const [index,c] of cookies.entries()){
    if(!fresh(m)){failed.push({index,field:'deadline'});continue;}
    const now=Date.now();
    let saved=null;
    try{saved=await chrome.cookies.set(cookieSet(c,now));}catch{}
    if(!saved){failed.push({index,field:'set'});continue;}
    let field=mismatch(saved,c,now);
    if(field&&fresh(m)){
      // With same-name host and domain cookies, the set callback can name
      // the other cookie. Confirm the exact value and every attribute in
      // the store before calling the write successful.
      try{
        const stored=await chrome.cookies.getAll({domain:domain(c),name:c.name,partitionKey:{}});
        if(stored.some(candidate=>!mismatch(candidate,c,now)))field=null;
      }catch{}
    }
    if(field)failed.push({index,field});else written++;
  }
  return {written,failed};
}
async function removeCookies(m,cookies){
  const failed=[];
  let removed=0;
  for(const [index,c] of cookies.entries()){
    if(!fresh(m)){failed.push({index,field:'deadline'});continue;}
    let gone=null;
    try{gone=await chrome.cookies.remove({url:cookieUrl(c),name:c.name,...(c.partitionKey?{partitionKey:c.partitionKey}:{})});}catch{}
    if(gone)removed++;else failed.push({index,field:'remove'});
  }
  return {removed,failed};
}
function validCookie(c,site){
  return c&&typeof c.domain==='string'&&inSite(c,site)&&typeof c.name==='string'&&typeof c.value==='string'&&typeof c.path==='string'&&c.path.startsWith('/')&&['hostOnly','httpOnly','secure','session'].every(k=>typeof c[k]==='boolean')&&['unspecified','lax','strict','no_restriction'].includes(c.sameSite)&&(c.sameSite!=='no_restriction'||c.secure)&&(c.session||Number.isFinite(c.expirationDate))&&(!c.partitionKey||(c.secure&&typeof c.partitionKey.hasCrossSiteAncestor==='boolean'&&/^https?:\/\/([a-z0-9-]+\.)+[a-z0-9-]+$/.test(c.partitionKey.topLevelSite)));
}
// The press the last locate armed the page to watch for. The page opens a
// port named after the watch before the press and reports over it the
// press, a form it submits and its leaving the moment each happens
// (document.js), so verify can answer even after the press has navigated the
// page away; the port closes when that page goes. Only this extension's own
// scripts can connect: pages have no externally_connectable route to it.
let armed=null;
chrome.runtime.onConnect.addListener(p=>{
  const a=armed,s=p.sender;
  if(!a||a.port||p.name!=='press:'+a.watch||s?.id!==chrome.runtime.id||s.tab?.id!==a.tabId||s.documentId!==a.documentId){p.disconnect();return;}
  a.port=p;
  p.onMessage.addListener(m=>{
    if(m?.press?.watch===a.watch&&!a.press){a.press={hit:m.press.hit===true};a.pressedAt=Date.now();}
    else if(m?.submit?.watch===a.watch&&!a.submit)a.submit={hit:m.submit.hit===true};
    else if(m?.leaving?.watch===a.watch)a.leaving=true;
  });
  p.onDisconnect.addListener(()=>{void chrome.runtime.lastError;a.gone=true;});
});
const fresh=m=>Number.isFinite(m.deadline)&&Date.now()<m.deadline;
const refused=()=>({error:{execution_not_started:true}});
const web=url=>/^https?:/.test(url||'');
const sleep=ms=>new Promise(done=>setTimeout(done,ms));
async function until(test,ms){const end=Date.now()+ms;while(!test()&&Date.now()<end)await sleep(20);return test();}
// Captures retain child attribution privately while the public protocol stays stable.
const captures=new Map();
function embedding(url,point,scroll,token,index) {
  const roots=[document];for(let i=0;i<roots.length;i++)for(const el of roots[i].querySelectorAll('*'))if(el.shadowRoot)roots.push(el.shadowRoot);
  const bindings=globalThis.__ibaraFrameBindingsV1 ||= {ids:new WeakMap(),targets:new Map()};
  const matches=roots.flatMap(root=>[...root.querySelectorAll('iframe,frame')]).filter(el=>el.src===url);
  if(matches.length!==1)return {refused:true,reason:'ambiguous or redirected frame embedding'};
  const el=matches[0];
  if(el.hasAttribute("srcdoc")||!Number.isSafeInteger(index)||index<0||el.contentWindow!==window.frames[index])return {refused:true,reason:"frame document attribution changed"};
  if(token&&bindings.targets.get(token)!==el)return {refused:true,reason:'frame embedding changed'};
  if(!token){token=bindings.ids.get(el)||crypto.randomUUID();bindings.ids.set(el,token);bindings.targets.set(token,el);while(bindings.targets.size>128)bindings.targets.delete(bindings.targets.keys().next().value);}
  if(!el.isConnected)return {refused:true};
  const parent=p=>p.assignedSlot||p.parentElement||p.getRootNode()?.host||null;
  const contains=(root,node)=>{for(let p=node;p;p=parent(p))if(p===root)return true;return false;};
  const visible=node=>{for(let p=node;p;p=parent(p))if(p.matches('[hidden],[aria-hidden="true"],[inert]'))return false;return node.checkVisibility({visibilityProperty:true});};
  const dialogs=roots.flatMap(root=>[...root.querySelectorAll('dialog,[role="dialog"],[role="alertdialog"]')]).filter(visible);
  let focus=document.activeElement;while(focus?.shadowRoot?.activeElement)focus=focus.shadowRoot.activeElement;
  let focusedDialog=null;for(let p=focus;p;p=parent(p))if(dialogs.includes(p)){focusedDialog=p;break;}
  const modals=dialogs.filter(p=>p.matches(':modal,[aria-modal="true"]'));
  const activeDialog=modals.includes(focusedDialog)?focusedDialog:modals.at(-1)||(dialogs.includes(focusedDialog)?focusedDialog:null);
  if(activeDialog&&!contains(activeDialog,el))return {refused:true,reason:'frame outside active dialog'};
  for(let p=el;p;p=parent(p)){
    const style=getComputedStyle(p);
    if(p.matches('[hidden],[aria-hidden="true"],[inert]')||style.transform!=='none'||style.translate!=='none'||style.rotate!=='none'||style.scale!=='none'||style.zoom!=='1')return {refused:true,reason:'hidden or transformed frame embedding'};
  }
  const style=getComputedStyle(el);
  if(['paddingLeft','paddingRight','paddingTop','paddingBottom'].some(k=>parseFloat(style[k])!==0))return {refused:true,reason:'padded frame embedding'};
  if(!el.checkVisibility({visibilityProperty:true}))return {refused:true,reason:'hidden frame embedding'};
  if(scroll)el.scrollIntoView({block:'nearest',inline:'nearest',behavior:'instant'});
  const r=el.getBoundingClientRect();
  if(!r.width||!r.height)return {refused:true};
  if(!point)return {ready:true,token};
  const x=r.left+el.clientLeft+point.x,y=r.top+el.clientTop+point.y;
  if(point.x<0||point.y<0||point.x>=el.clientWidth||point.y>=el.clientHeight)return {covered:true};
  let hit=document.elementFromPoint(x,y);while(hit?.shadowRoot){const next=hit.shadowRoot.elementFromPoint(x,y);if(!next||next===hit)break;hit=next;}
  return hit===el?{x,y,outer:[outerWidth,outerHeight],inner:[innerWidth,innerHeight]}:{covered:true};
}
async function framePath(tabId,documentId,rootId) {
  const frames=await chrome.webNavigation.getAllFrames({tabId});
  const current=frames?.find(f=>f.documentId===documentId),root=frames?.find(f=>f.frameId===0);
  if(!current||root?.documentId!==rootId)return null;
  const path=[];let child=current;
  while(child.frameId!==0){
    if(path.length>=16)return null;
    const parent=frames.find(f=>f.frameId===child.parentFrameId);
    if(!parent||frames.filter(f=>f.parentFrameId===child.parentFrameId&&f.url===child.url).length!==1)return null;
    path.push({child,parent});child=parent;
  }
  return path;
}
async function mapPoint(tabId,path,point,scroll=false) {
  for(const step of path){
    const {child,parent}=step;
    const childReply=await chrome.scripting.executeScript({target:{tabId,documentIds:[child.documentId]},world:'ISOLATED',func:()=>{for(let i=0;i<window.parent.length;i++)if(window.parent[i]===window)return i;return -1;}});
    const index=childReply.find(r=>r.documentId===child.documentId)?.result;
    if(!Number.isSafeInteger(index)||index<0)return {refused:true};
    const reply=await chrome.scripting.executeScript({target:{tabId,documentIds:[parent.documentId]},world:'ISOLATED',func:embedding,args:[child.url,point,scroll,step.token??null,index]});
    const result=reply.find(r=>r.documentId===parent.documentId)?.result;
    if(!result||result.refused||result.covered)return result||{refused:true};
    if(result.token)step.token=result.token;
    if(point)point=result;
  }
  return point||{ready:true};
}
async function observeFrames(m,tab,rootId) {
  let snapshot;
  if(m.args.capture){
    snapshot=captures.get(m.args.capture);
    if(pendingFrames.has(tab.id)||snapshot?.epoch!==(frameEpoch.get(tab.id)||0)||!snapshot||snapshot.tabId!==tab.id||snapshot.rootId!==rootId||snapshot.query!==(m.args.query||'')||Date.now()-snapshot.at>30000||!Number.isSafeInteger(m.args.offset)||m.args.offset<0||m.args.offset>=snapshot.nodes.length)return {result:{refused:true}};
  }else{
    const epoch=frameEpoch.get(tab.id)||0;
    if(pendingFrames.has(tab.id))return {result:{refused:true}};
    const responses=await chrome.scripting.executeScript({target:{tabId:tab.id,allFrames:true},world:'ISOLATED',func:documentOperation,args:[{...m,args:{...m.args,limit:1000}}],injectImmediately:true});
    const root=responses.find(r=>r.frameId===0);
    if(root?.documentId!==rootId||!root.result?.capture)return {result:{refused:true}};
    const capture=crypto.randomUUID(),nodes=[],routes=new Map();let truncated=false;
    for(const frame of responses.sort((a,b)=>a.frameId-b.frameId)){
      if(!frame.result?.capture)continue;
      const path=await framePath(tab.id,frame.documentId,rootId);
      if(!path||!(await mapPoint(tab.id,path,null)).ready)continue;
      for(const node of frame.result.nodes||[]){
        if(nodes.length>=1000){truncated=true;break;}
        const token=crypto.randomUUID();routes.set(token,{documentId:frame.documentId,capture:frame.result.capture,token:node.token,path});
        nodes.push({...node,token,elementId:frame.documentId+':'+node.elementId});
      }
      if(frame.result.truncated)truncated=true;
    }
    snapshot={...root.result,capture,tabId:tab.id,rootId,at:Date.now(),epoch,query:m.args.query||'',nodes,routes,truncated};
    if(pendingFrames.has(tab.id)||(frameEpoch.get(tab.id)||0)!==epoch)return {result:{refused:true}};
    captures.set(capture,snapshot);while(captures.size>8)captures.delete(captures.keys().next().value);
  }
  const offset=m.args.offset||0,nodes=snapshot.nodes.slice(offset,offset+m.args.limit),next=offset+nodes.length<snapshot.nodes.length?offset+nodes.length:null;
  return {result:{...snapshot,routes:undefined,nodes,count:snapshot.nodes.length,documentId:rootId,next_offset:next,truncated:snapshot.truncated||next!==null}};
}
async function focusedTab(tabId) {
  const tab=await chrome.tabs.get(tabId);
  const win=await chrome.windows.get(tab.windowId);
  return !tab.incognito&&tab.active&&win.focused&&web(tab.url)?tab:null;
}
// What the armed press did, from the page's own reports: which element it
// reached and, for a plain click (navigation), where the page went when the
// press or the form it submitted made the page leave. Reports still on their
// way get a moment. `null` when the page could not report (no port).
async function pressed(a,navigation) {
  if(!a.port)return null;
  await until(()=>a.press||a.submit||a.gone,500);
  const hit=a.press?a.press.hit:a.submit?a.submit.hit:null;
  if(hit===null)return {observed:false,hit:null,...(a.gone?{gone:true}:{})};
  if(!navigation||!hit)return {observed:true,hit};
  // A form submits and the page leaves just after the press; a pending
  // address means the browser has begun loading the next page.
  await until(()=>a.leaving||a.gone,150);
  const pending=async()=>(await chrome.tabs.get(a.tabId).catch(()=>null))?.pendingUrl;
  if(!a.leaving&&!a.gone&&!await pending())return {observed:true,hit};
  await until(()=>a.gone,3000);
  const tab=await chrome.tabs.get(a.tabId).catch(()=>null);
  const url=tab?.pendingUrl||tab?.url;
  return url?{observed:true,hit,navigated:url,arrived:a.gone&&!tab.pendingUrl}:{observed:true,hit};
}
async function handle(m,jobs) {
  if(!fresh(m)||busy)return refused();
  if(m.op==='jobs') {
    const asked=m.args?.jobs;
    if(!Array.isArray(asked)||!asked.every(j=>Object.hasOwn(JOBS,j)))return refused();
    jobs.clear();
    for(const j of asked)jobs.add(j);
    return {result:{jobs:[...jobs]}};
  }
  if(![...jobs].some(j=>JOBS[j].includes(m.op)))return refused();
  busy=true;
  let dispatching=false;
  try {
    if(m.op==='browser_info')return {result:{userAgent:navigator.userAgent,extensionId:chrome.runtime.id}};
    if(['cookies_read','cookies_count','cookies_write','cookies_remove'].includes(m.op)) {
      const site=m.args.site;
      if(typeof site!=='string'||!/^([a-z0-9-]+\.)+[a-z0-9-]+$/.test(site))return refused();
      if(m.op==='cookies_read')return {result:{cookies:await siteCookies(site)}};
      if(m.op==='cookies_count')return {result:{count:(await siteCookies(site)).length}};
      if(m.op==='cookies_remove') {
        const cookies=await siteCookies(site);
        dispatching=true;
        return {result:await removeCookies(m,cookies)};
      }
      const cookies=m.args.cookies;
      if(!Array.isArray(cookies)||cookies.length>2000||cookies.some(c=>!validCookie(c,site)))return refused();
      dispatching=true;
      return {result:await writeCookies(m,cookies)};
    }
    if(m.op==='login_context') {
      const tab=await focusedTab(m.args.tabId);
      if(!tab)return {result:{refused:true}};
      return {result:{url:tab.url,previous:navigation.get(tab.id)?.previous}};
    }
    if(m.op==='login_reload') {
      const tab=await focusedTab(m.args.tabId);
      if(!tab||!fresh(m))return refused();
      dispatching=true;await chrome.tabs.reload(tab.id);
      return {result:{reloaded:true}};
    }
    if(m.op==='login_page') {
      const tab=await focusedTab(m.args.tabId);
      if(!tab)return {result:{page:'unknown'}};
      const frames=await chrome.scripting.executeScript({target:{tabId:tab.id,allFrames:true},world:'ISOLATED',func:()=>({password:[...document.querySelectorAll('input[type="password"]')].some(e=>e.checkVisibility?.({visibilityProperty:true,opacityProperty:true})??e.getClientRects().length>0),origin:performance.timeOrigin,ready:document.readyState==='complete',frames:document.querySelectorAll('iframe,frame').length})});
      const root=frames.find(f=>f.frameId===0)?.result;
      // Reload returns before the new document commits. Never read the old form.
      if(Number.isFinite(m.args.after_ms)&&(!root||root.origin<m.args.after_ms))return {result:{page:'unknown'}};
      const password=frames.some(f=>f.result?.password);
      // Unreadable frames or a loading document cannot establish success.
      const known=root?.ready&&frames.every(f=>f.result?.ready)&&frames.length>=1+(root?.frames||0);
      return {result:{page:password?'still_sign_in':known?'left_sign_in':'unknown',document_ms:root?.origin}};
    }
    if(m.op==='lifecycle_inventory') {
      const windows=await chrome.windows.getAll({populate:true});
      return {result:{windows:windows.map(w=>({id:w.id,focused:w.focused,type:w.type,incognito:w.incognito,
        tabs:(w.tabs||[]).map(t=>({id:t.id,active:t.active,blank:t.url==='about:blank'}))}))}};
    }
    if(m.op==='tabs') {
      const tabs=await chrome.tabs.query({active:true,lastFocusedWindow:true});
      const tab=tabs[0];
      if(!tab||tab.incognito||!web(tab.url))return {result:{tabs:[]}};
      const win=await chrome.windows.get(tab.windowId);
      return {result:{tabs:[{id:tab.id,title:tab.title,url:tab.url,focused:win.focused&&tab.active}]}};
    }
    if(m.op==='navigate') {
      let url;
      try{url=new URL(m.args.url);}catch{return refused();}
      if(!['http:','https:'].includes(url.protocol))return refused();
      // The focused window's active tab, whatever it shows (a new tab page too).
      const [tab]=await chrome.tabs.query({active:true,lastFocusedWindow:true});
      const win=tab&&await chrome.windows.get(tab.windowId);
      if(!tab||tab.incognito||!win.focused||!fresh(m))return {result:{refused:true}};
      dispatching=true;
      await chrome.tabs.update(tab.id,{url:url.href});
      return {result:{navigated:true}};
    }
    if(!PAGE_OPS.includes(m.op))return refused();
    if(m.op==='verify') {
      const a=armed;
      if(a&&a.tabId===m.args.tabId&&a.rootId===m.args.documentId) {
        armed=null;
        const answer=await pressed(a,m.args.navigation===true);
        if(answer)return {result:answer};
      }
    }
    const tab=await focusedTab(m.args.tabId);
    if(!tab||!fresh(m))return {result:{refused:true}};
    const inventory=await chrome.webNavigation.getAllFrames({tabId:tab.id});
    const rootId=inventory?.find(f=>f.frameId===0)?.documentId;
    if(!rootId)return refused();
    if(m.op==='observe')return await observeFrames(m,tab,rootId);
    if(['check','keys'].includes(m.op)){
      const epoch=frameEpoch.get(tab.id)||0;
      if(pendingFrames.has(tab.id))return {result:{refused:true}};
      const replies=await chrome.scripting.executeScript({target:{tabId:tab.id,allFrames:true},world:'ISOLATED',func:documentOperation,args:[m]});
      const results=[];
      for(const reply of replies){
        const path=await framePath(tab.id,reply.documentId,rootId);
        if(path&&(await mapPoint(tab.id,path,null)).ready)results.push(reply);
      }
      if(pendingFrames.has(tab.id)||(frameEpoch.get(tab.id)||0)!==epoch)return {result:{refused:true}};
      return {result:m.op==='check'?{found:results.some(r=>r.result?.found),truncated:results.some(r=>r.result?.truncated)}:{page:results.some(r=>r.result?.page),submits:results.some(r=>r.result?.submits)}};
    }
    const snapshot=captures.get(m.args.capture),route=snapshot?.routes.get(m.args.token);
    if(pendingFrames.has(tab.id)||snapshot?.epoch!==(frameEpoch.get(tab.id)||0))return {result:{refused:true}};
    if(!route||snapshot.tabId!==tab.id||snapshot.rootId!==rootId||m.args.documentId!==rootId)return {result:{refused:true}};
    const path=await framePath(tab.id,route.documentId,rootId);
    if(!path||path.length!==route.path.length||path.some((step,i)=>step.parent.documentId!==route.path[i].parent.documentId||step.child.documentId!==route.path[i].child.documentId))return {result:{refused:true}};
    for(let i=0;i<path.length;i++)path[i].token=route.path[i].token;
    const zoom=m.op==='locate'?await chrome.tabs.getZoom(tab.id):undefined;
    if(!fresh(m))return refused();
    dispatching=EFFECTS.includes(m.op);
    if(['locate','reveal'].includes(m.op)){
      const ready=await mapPoint(tab.id,[...path].reverse(),null,true);
      if(!ready.ready)return {result:ready};
    }
    if(m.op==='locate'){
      armed={watch:crypto.randomUUID(),tabId:tab.id,documentId:route.documentId,rootId,port:null,press:null,submit:null,leaving:false,gone:false};
      m.watch=armed.watch;
    }
    const result=await chrome.scripting.executeScript({target:{tabId:tab.id,documentIds:[route.documentId]},world:'ISOLATED',func:documentOperation,args:[{...m,args:{...m.args,documentId:route.documentId,capture:route.capture,token:route.token}}],injectImmediately:true});
    const frame=result.find(r=>r.documentId===route.documentId);
    if(m.op==='locate'&&!frame?.result?.watch)armed=null;
    if(!frame?.result)return {error:{execution_not_started:!dispatching}};
    let answer=frame.result;
    if(m.op==='locate'&&answer.watch){
      const freshPath=await framePath(tab.id,route.documentId,rootId);
      if(freshPath?.length===path.length)for(let i=0;i<path.length;i++)freshPath[i].token=path[i].token;
      answer=freshPath&&!pendingFrames.has(tab.id)&&snapshot.epoch===(frameEpoch.get(tab.id)||0)?{...answer,...await mapPoint(tab.id,freshPath,answer)}:{refused:true};
      if(answer.refused||answer.covered)armed=null;
    }
    return {result:{...answer,documentId:rootId,...(zoom!==undefined?{zoom}:{})}};
  } catch {return {error:{execution_not_started:!dispatching}};}
  finally {busy=false;}
}
function connect() {
  if(port)return;
  try {
    const p=chrome.runtime.connectNative(HOST);port=p;
    // A new connection may come from another ibara role: it starts over.
    const jobs=new Set(['pages']);
    p.onMessage.addListener(async m=>{const response=await handle(m,jobs);try{p.postMessage({id:m.id,...response});}catch{}});
    p.onDisconnect.addListener(()=>{void chrome.runtime.lastError;if(port===p)port=undefined;setTimeout(connect,2000);});
    p.postMessage({hello:1});
  } catch {setTimeout(connect,2000);}
}
// Keep the current reader for this browser session. Reloading on an update
// can cut an in-flight native reply and leave a real effect uncertain.
// Chromium applies the downloaded update after the browser closes normally.
chrome.runtime.onUpdateAvailable.addListener(()=>{});
chrome.runtime.onStartup.addListener(()=>{chrome.runtime.requestUpdateCheck().catch(()=>{});});
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.alarms.create('reconnect',{periodInMinutes:1});
chrome.alarms.onAlarm.addListener(connect);
connect();
