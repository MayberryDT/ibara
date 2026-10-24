// HTTP process boundary with actual worker validation and an in-memory R2 adapter.
import http from 'node:http';import fs from 'node:fs';import zlib from 'node:zlib';import assert from 'node:assert/strict';import crypto from 'node:crypto';
if(!globalThis.crypto)Object.defineProperty(globalThis,"crypto",{value:crypto.webcrypto});
const worker=(await import('data:text/javascript;base64,'+Buffer.from(fs.readFileSync('facts-service/worker.js','utf8')).toString('base64'))).default;
const objects=new Map();const env={READ_TOKEN:'read-fixture',WRITE_TOKEN:'write-fixture',ARCHIVE:{head:async k=>objects.get(k),put:async(k,b,o)=>{objects.set(k,{customMetadata:o.customMetadata});return {};}}};
const server=http.createServer(async(req,res)=>{try{const chunks=[];for await(const c of req)chunks.push(c);const r=await worker.fetch(new Request('http://localhost'+req.url,{method:req.method,headers:req.headers,...(req.method==='PUT'?{body:Buffer.concat(chunks)}:{})}),env);res.writeHead(r.status,Object.fromEntries(r.headers));res.end(Buffer.from(await r.arrayBuffer()));}catch(e){res.writeHead(500);res.end(String(e));}});
await new Promise(r=>server.listen(0,'127.0.0.1',r));
try {
const key='raw/v1/2026/10/24/2026-10-24T17-30-00.000Z-1234567890abcdef1234567890abcdef.jsonl.gz';let i=0;const outcomes=[];
const send=async(fields,token='write-fixture')=>{const fact={kind:'call',t:1792863000000,session:'fixture',actor:'1',who:'bot',build:'activity-e2e',schema:1,fields};const url=`http://127.0.0.1:${server.address().port}/v1/archive/${encodeURIComponent(key.replace('1234567890abcdef1234567890abcdef',String(++i).padStart(32,'0')))}`;const r=await fetch(url,{method:'PUT',headers:{authorization:'Bearer '+token},body:zlib.gzipSync(JSON.stringify(fact)+'\n')});outcomes.push({status:r.status,reply:await r.json()});return r.status;};
assert.equal(await send({tool:'browser_act',status:'error',duration_ms:2,error_code:'STALE_TARGET',execution:'not_started',verification:'not_requested',recovery:'observe'}),200);
assert.equal(await send({op:'attention',status:'ok',samples:30,duration_ms_total:23,aggregation:'poll_summary'}),200);
assert.equal(await send({error_code:null,execution:null,recovery:null}),200);
assert.equal(await send({error_code:'https://private.invalid/'}),400);
assert.equal(await send({samples:'private text'}),400);
assert.equal(await send({text:'privacy canary'}),400);
assert.equal(await send({tool:'computer_status'},'read-fixture'),401);
fs.mkdirSync(process.argv[2],{recursive:true});fs.writeFileSync(process.argv[2]+'/archive-schema.json',JSON.stringify({outcomes,stored_objects:objects.size},null,2));
} finally {server.close();}
