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
  pages:['tabs','navigate',...PAGE_OPS],
  receive:['cookies_write','cookies_remove','cookies_count','login_context','login_page','login_reload'],
  share:['cookies_read','browser_info'],
};
// Chrome caps a cookie's lifetime at 400 days from when it is set.
const MAX_LIFETIME_S=400*24*60*60;
let port;
let busy=false;
// Bounded by open tabs. No cookie is persisted by the extension.
const navigation=new Map();
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
  if(!a||a.port||p.name!=='press:'+a.watch||s?.id!==chrome.runtime.id||s.tab?.id!==a.tabId||s.frameId!==0||s.documentId!==a.documentId){p.disconnect();return;}
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
      if(a&&a.tabId===m.args.tabId&&a.documentId===m.args.documentId) {
        armed=null;
        const answer=await pressed(a,m.args.navigation===true);
        if(answer)return {result:answer};
      }
    }
    const tab=await focusedTab(m.args.tabId);
    if(!tab||!fresh(m))return {result:{refused:true}};
    // Observe and keys read the top frame; the rest act on the exact document observed.
    const target=['observe','check','keys'].includes(m.op)?{tabId:tab.id,frameIds:[0]}:{tabId:tab.id,documentIds:[m.args.documentId]};
    const zoom=m.op==='locate'?await chrome.tabs.getZoom(tab.id):undefined;
    if(!fresh(m))return refused();
    dispatching=EFFECTS.includes(m.op);
    if(m.op==='locate') {
      armed={watch:crypto.randomUUID(),tabId:tab.id,documentId:m.args.documentId,port:null,press:null,submit:null,leaving:false,gone:false};
      m.watch=armed.watch;
    }
    const result=await chrome.scripting.executeScript({target,world:'ISOLATED',func:documentOperation,args:[m],injectImmediately:true});
    const frame=result.find(r=>r.frameId===0);
    if(m.op==='locate'&&!frame?.result?.watch)armed=null;
    if(!frame?.result)return {error:{execution_not_started:!dispatching}};
    return {result:{...frame.result,documentId:frame.documentId,...(zoom!==undefined?{zoom}:{})}};
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
// A new ibara release brings a new version. Chrome downloads it but holds it
// while this worker is busy (the native port keeps it so), so apply it now.
chrome.runtime.onUpdateAvailable.addListener(()=>chrome.runtime.reload());
chrome.runtime.onStartup.addListener(()=>{chrome.runtime.requestUpdateCheck().catch(()=>{});});
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.alarms.create('reconnect',{periodInMinutes:1});
chrome.alarms.onAlarm.addListener(connect);
connect();
