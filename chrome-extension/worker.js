// ibara page reader: service worker. Connects to ibara through Chrome native
// messaging (`ibara chrome-host`) and answers one request at a time about the
// focused tab. It never clicks or types; ibara does that with real input.
import { documentOperation } from './document.js';
const HOST='io.ibara.chrome';
const PAGE_OPS=['observe','check','locate','verify','selected','reveal','field','keys'];
// Operations that change the page (a refusal after they start is not "not started").
const EFFECTS=['navigate','reveal'];
let port;
let busy=false;
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
async function handle(m) {
  if(!fresh(m)||busy)return refused();
  busy=true;
  let dispatching=false;
  try {
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
    p.onMessage.addListener(async m=>{const response=await handle(m);try{p.postMessage({id:m.id,...response});}catch{}});
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
