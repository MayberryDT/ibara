// A private archive, separate from the public website and its database.
const reply = (body, status = 200) => new Response(JSON.stringify(body), {status, headers:{'content-type':'application/json','cache-control':'no-store'}});
const keyPattern = /^raw\/v1\/\d{4}\/\d{2}\/\d{2}\/\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}\.\d{3}Z-[a-f0-9]{32}\.jsonl\.gz$/;
const kinds = new Set(['call','transition','operator','admin','transfer','recorder.loss']);
export default {
  async fetch(request, env) {
    const token = request.headers.get('authorization');
    const write = request.method === 'PUT';
    const expected = write ? env.WRITE_TOKEN : env.READ_TOKEN;
    if (!expected || token !== `Bearer ${expected}`) return reply({error:'unauthorized'},401);
    const url = new URL(request.url);
    if (request.method === 'GET' && url.pathname === '/v1/archive') {
      const result = await env.ARCHIVE.list({prefix:'raw/v1/', limit:1000, cursor:url.searchParams.get('cursor') || undefined});
      return reply({objects:result.objects.map(o=>({key:o.key,size:o.size})), ...(result.truncated ? {cursor:result.cursor} : {})});
    }
    let key;
    try { key = decodeURIComponent(url.pathname.slice('/v1/archive/'.length)); } catch { return reply({error:'key'},400); }
    if (!url.pathname.startsWith('/v1/archive/') || !keyPattern.test(key)) return reply({error:'key'},400);
    if (request.method === 'GET') {
      const obj = await env.ARCHIVE.get(key);
      if (!obj) return reply({error:'missing'},404);
      return new Response(obj.body,{headers:{'content-type':'application/gzip','cache-control':'no-store'}});
    }
    if (!write) return reply({error:'method'},405);
    const reader = request.body?.getReader();
    if (!reader) return reply({error:'body'},400);
    const parts=[]; let size=0;
    for (;;) { const {done,value}=await reader.read(); if (done) break; size+=value.length; if(size>3*1024*1024) {await reader.cancel();return reply({error:'size'},413);} parts.push(value); }
    const bytes = new Uint8Array(size); let offset=0; for(const p of parts){bytes.set(p,offset);offset+=p.length;}
    let text='';
    try {
      const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
      const r = stream.getReader(); const decoder = new TextDecoder('utf-8',{fatal:true}); let length=0;
      for (;;) {const {done,value}=await r.read();if(done)break;length+=value.length;if(length>3*1024*1024){await r.cancel();return reply({error:'expanded size'},413);}text+=decoder.decode(value,{stream:true});}
      text+=decoder.decode();
    } catch {return reply({error:'gzip'},400);}
    if (!text.endsWith('\n')) return reply({error:'jsonl'},400);
    let rows;
    try {
      rows=text.trimEnd().split('\n').map(line=>JSON.parse(line));
      if(rows.length > 20000) throw Error();
      for (const f of rows) {
        if(!kinds.has(f.kind)||!Number.isSafeInteger(f.t)||f.t<=0||!['human','bot','agent','system'].includes(f.who)||typeof f.session!=='string'||typeof f.actor!=='string'||typeof f.build!=='string')throw Error();
        if(Object.keys(f).some(k=>!['kind','t','session','actor','who','build','schema','fields','lines'].includes(k)))throw Error();
        const legacy=['tool','event','status','duration_ms','op','outcome','effect_class','replay','complete'];
        const categories={error_code:['BUSY','LEASE_EXPIRED','HUMAN_CONTROL','STALE_TARGET','NOT_FOUND','DISPLAY_CHANGED','AMBIGUOUS_TARGET','BLOCKED_BY_DIALOG','CAPABILITY_UNAVAILABLE','REQUEST_CONFLICT','OUTCOME_UNKNOWN','POSTCONDITION_FAILED','BUDGET_EXCEEDED','DELIVERY_UNAVAILABLE','WRONG_TOOL','PERMISSION_DENIED','SESSION_UNAVAILABLE','INVALID_ARGUMENT','TIMEOUT','INTERNAL_ERROR','CONTROL_UNSETTLED','UNSUPPORTED_CONTRACT','VIDEO_UNSUPPORTED'],execution:['not_started','running','completed','unknown'],verification:['not_requested','satisfied','unsatisfied','unknown'],recovery:['observe','reconcile','correct_arguments','follow_next'],aggregation:['poll_summary']};
        for(const [k,v] of Object.entries(f.fields||{})) {
          if(legacy.includes(k))continue;
          if(k==='samples'||k==='duration_ms_total') {if(!Number.isSafeInteger(v)||v<0||(k==='samples'&&v===0))throw Error();}
          else if(!Object.hasOwn(categories,k)||(v!==null&&!categories[k].includes(v)))throw Error();
        }
      }
    } catch {return reply({error:'schema'},400);}
    const hash = [...new Uint8Array(await crypto.subtle.digest('SHA-256',bytes))].map(b=>b.toString(16).padStart(2,'0')).join('');
    const existing=await env.ARCHIVE.head(key);
    if(existing) return existing.customMetadata?.sha256===hash ? reply({saved:true,sha256:hash,duplicate:true}) : reply({error:'immutable'},409);
    const put=await env.ARCHIVE.put(key,bytes,{onlyIf:{etagDoesNotMatch:'*'}, httpMetadata:{contentType:'application/gzip'},customMetadata:{sha256:hash,schema:'1',lines:String(rows.length),from:String(Math.min(...rows.map(r=>r.t))),to:String(Math.max(...rows.map(r=>r.t)))}});
    if (!put) return reply({error:'retry conflict'},409);
    return reply({saved:true,sha256:hash,lines:rows.length});
  }
};
