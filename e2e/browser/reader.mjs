// The page reader (chrome-extension/) in a real Chromium, asked what ibarad
// asks it over native messaging, one request at a time. Fixture pages come
// from a local HTTP server. Trusted pointer input comes from the DevTools
// protocol, standing in for Cua's pointer: the page sees both as trusted.
//
// Run: node e2e/browser/reader.mjs ARTIFACT_DIR
// Needs Node 22 or later and Chromium (CHROMIUM names another binary). The
// browser runs headless with its sandbox on; the artifact records the
// Chromium version, the reader's hash, the sandbox check and every answer.
//
// Failure cases this must catch, written before the changes they cover:
// 1. A frame that is not the chosen element's own frame navigates, or is
//    still loading, after the page was observed, and the element is refused
//    although its page, its frame and the element itself are unchanged.
// 2. The element's own page or its own frame goes to another document, and
//    the element is still located (it must never be).
// 3. The page itself is still loading another address, and the element is
//    located, or refused without saying why.
// 4. An element whose card text, name or enabled state changed, or which is
//    hidden now, is located, or refused without saying which of these it was.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const extension = path.resolve(here, '../../chrome-extension');
const artifacts = path.resolve(process.argv[2] || fail('usage: node e2e/browser/reader.mjs ARTIFACT_DIR'));
const chromium = process.env.CHROMIUM || 'chromium';
function fail(message) { console.error(message); process.exit(2); }
const sleep = ms => new Promise(done => setTimeout(done, ms));
async function until(test, ms, what) {
  const end = Date.now() + ms;
  for (;;) {
    const value = await test();
    if (value) return value;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(50);
  }
}

// Fixture pages. Each is plain HTML; tests change them through Runtime.evaluate.
const pages = {
  '/frames': `<title>Frames</title><button>Next</button><iframe src="/ad"></iframe>`,
  '/ad': `<title>Ad</title><p>An advertisement</p>`,
  '/other': `<title>Other</title><button>Next</button>`,
  '/inframe': `<title>Outer</title><p>Outer page</p><iframe src="/inner"></iframe>`,
  '/inner': `<title>Inner</title><button>Inner</button>`,
  '/card': `<title>Card</title><div><h3>Order 12</h3><span id=count>3 items</span><button id=add>Add</button></div><button id=other>Other</button>`,
};
const server = http.createServer(async (request, response) => {
  const route = request.url.split('?')[0];
  if (route === '/slow') await sleep(4000);
  response.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' });
  response.end(pages[route] || `<title>Slow</title><p>${route}</p>`);
});
await new Promise(done => server.listen(0, '127.0.0.1', done));
const site = `http://127.0.0.1:${server.address().port}`;

// The extension is unpacked, so its id comes from its folder; the native
// host manifest in the throwaway profile admits only that id.
const work = fs.mkdtempSync(path.join(process.env.TMPDIR || '/tmp', 'ibara-reader-'));
const profile = path.join(work, 'profile');
const id = [...crypto.createHash('sha256').update(extension).digest('hex').slice(0, 32)].map(c => String.fromCharCode(97 + parseInt(c, 16))).join('');
const socketPath = path.join(work, 'host.sock');
const hostScript = path.join(work, 'host.sh');
fs.mkdirSync(path.join(profile, 'NativeMessagingHosts'), { recursive: true });
fs.writeFileSync(hostScript, `#!/bin/sh\nexec '${process.execPath}' '${path.join(here, 'host.mjs')}' '${socketPath}'\n`, { mode: 0o755 });
fs.writeFileSync(path.join(profile, 'NativeMessagingHosts', 'io.ibara.chrome.json'),
  JSON.stringify({ name: 'io.ibara.chrome', description: 'ibara reader test', path: hostScript, type: 'stdio', allowed_origins: [`chrome-extension://${id}/`] }));

// The reader's side of native messaging: the host connects here.
let reader = null;
let greeted = false;
const replies = new Map();
const listener = net.createServer(connection => {
  reader = connection;
  let line = '';
  connection.on('data', chunk => {
    line += chunk;
    for (let end; (end = line.indexOf('\n')) >= 0; line = line.slice(end + 1)) {
      const message = JSON.parse(line.slice(0, end));
      if (message.hello) greeted = true;
      else replies.get(message.id)?.(message);
    }
  });
});
await new Promise(done => listener.listen(socketPath, done));
let asked = 0;
function ask(op, args) {
  return new Promise((done, failed) => {
    const requestId = `chrome_${++asked}`;
    const timer = setTimeout(() => failed(new Error(`no answer to ${op}`)), 8000);
    replies.set(requestId, message => { clearTimeout(timer); replies.delete(requestId); done(message); });
    reader.write(JSON.stringify({ id: requestId, op, args, deadline: Date.now() + 4000 }) + '\n');
  });
}

const browser = spawn(chromium, ['--headless=new', `--user-data-dir=${profile}`, `--load-extension=${extension}`, '--no-first-run',
  '--no-default-browser-check', '--remote-debugging-port=0', '--window-size=1000,800', `${site}/frames`], { stdio: ['ignore', 'ignore', 'pipe'] });
let browserLog = '';
browser.stderr.on('data', chunk => { browserLog = (browserLog + chunk).slice(-20000); });

// The DevTools protocol on the page: navigation, page changes and trusted input.
const port = await until(() => { try { return fs.readFileSync(path.join(profile, 'DevToolsActivePort'), 'utf8').split('\n')[0]; } catch { return null; } }, 15000, 'DevTools');
const version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
const pageTarget = await until(async () => (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find(t => t.type === 'page'), 15000, 'the page');
const devtools = new WebSocket(pageTarget.webSocketDebuggerUrl);
await new Promise(done => { devtools.onopen = done; });
let sent = 0;
const pending = new Map();
const loaded = [];
devtools.onmessage = event => {
  const message = JSON.parse(event.data);
  if (message.id) pending.get(message.id)?.(message);
  if (message.method === 'Page.loadEventFired') loaded.push(Date.now());
};
const cdp = (method, params = {}) => new Promise(done => { const n = ++sent; pending.set(n, done); devtools.send(JSON.stringify({ id: n, method, params })); });
await cdp('Page.enable');
const evaluate = async expression => (await cdp('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })).result?.result?.value;
async function open(route) {
  const before = loaded.length;
  await cdp('Page.navigate', { url: site + route });
  await until(() => loaded.length > before, 10000, `${route} to load`);
  await sleep(200);
}
async function press(x, y) {
  for (const type of ['mousePressed', 'mouseReleased']) await cdp('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 });
}

await until(() => greeted, 15000, 'the extension to connect');
async function tab() {
  const answer = await ask('tabs', {});
  return answer.result?.tabs?.[0]?.id;
}
async function observe() {
  const answer = await ask('observe', { tabId: await tab(), query: '', view: 'elements', limit: 100, maxChars: 2000 });
  if (!answer.result?.nodes) throw new Error(`observe refused: ${JSON.stringify(answer)}`);
  return answer.result;
}
// What ibarad sends to name an element of an observation.
function target(observation, name) {
  const node = observation.nodes.find(n => n.name === name);
  if (!node) throw new Error(`${name} is not in the observation`);
  return { tabId: observation.tabId, documentId: observation.documentId, capture: observation.capture, token: node.token };
}
const locate = async t => (await ask('locate', t)).result;
const located = answer => Number.isFinite(answer?.x) && Number.isFinite(answer?.y);

const results = [];
async function scenario(name, run) {
  const record = { name, checks: [] };
  const check = (kind, what, ok, seen) => record.checks.push({ kind, what, ok: !!ok, seen });
  try { await run(check); } catch (e) { record.checks.push({ kind: 'error', what: String(e?.stack || e), ok: false }); }
  record.ok = record.checks.every(c => c.ok);
  results.push(record);
  console.log(`${record.ok ? 'ok  ' : 'FAIL'} ${name}`);
  for (const c of record.checks.filter(c => !c.ok)) console.log(`     ${c.kind}: ${c.what}: ${JSON.stringify(c.seen)}`);
}
// Tokens and watch ids are random; the artifact keeps what the answer says.
const plain = answer => answer && Object.fromEntries(Object.entries(answer).filter(([k]) => !['watch', 'documentId'].includes(k)));

await scenario('another frame that navigated leaves the element actionable', async check => {
  await open('/frames');
  const next = target(await observe(), 'Next');
  await evaluate(`new Promise(done => { const f = document.querySelector('iframe'); f.onload = () => done(true); f.src = '/ad?again'; })`);
  const answer = await locate(next);
  check('wanted', 'located after an unrelated frame navigated', located(answer), plain(answer));
});

await scenario('another frame still loading leaves the element actionable', async check => {
  await open('/frames');
  const next = target(await observe(), 'Next');
  await evaluate(`document.querySelector('iframe').src = '/slow'; true`);
  await sleep(300);
  const answer = await locate(next);
  check('wanted', 'located while an unrelated frame loads', located(answer), plain(answer));
});

await scenario('the page going to another document stales the element', async check => {
  await open('/frames');
  const next = target(await observe(), 'Next');
  await open('/other');
  const answer = await locate(next);
  check('safe', 'not located', !located(answer), plain(answer));
  check('wanted', 'refused as navigated', answer?.refused && answer.why === 'navigated', plain(answer));
});

await scenario('the element\'s own frame going to another document stales it', async check => {
  await open('/inframe');
  const inner = target(await observe(), 'Inner');
  await evaluate(`new Promise(done => { const f = document.querySelector('iframe'); f.onload = () => done(true); f.src = '/inner?again'; })`);
  const answer = await locate(inner);
  check('safe', 'not located', !located(answer), plain(answer));
  check('wanted', 'refused as navigated', answer?.refused && answer.why === 'navigated', plain(answer));
});

await scenario('a page still loading another address refuses and says so', async check => {
  await open('/frames');
  const next = target(await observe(), 'Next');
  await evaluate(`setTimeout(() => { location.href = '/slow'; }, 0); true`);
  await sleep(300);
  const answer = await locate(next);
  check('safe', 'not located', !located(answer), plain(answer));
  check('wanted', 'refused as loading', answer?.refused && answer.why === 'loading', plain(answer));
  await sleep(4200);
});

for (const [change, script, what] of [
  ['card text', `document.getElementById('count').textContent = '4 items'`, 'context'],
  ['name', `document.getElementById('add').textContent = 'Add more'`, 'name'],
  ['enabled state', `document.getElementById('add').disabled = true`, 'disabled'],
  ['visibility', `document.getElementById('add').style.display = 'none'`, 'hidden'],
]) {
  await scenario(`a changed ${change} is refused and named`, async check => {
    await open('/card');
    const add = target(await observe(), 'Add');
    await evaluate(script);
    const answer = await locate(add);
    check('safe', 'not located', !located(answer), plain(answer));
    check('wanted', `refused as changed: ${what}`, answer?.refused && answer.why === 'changed' && answer.changed === what, plain(answer));
  });
}

// The sandbox stays on: no --no-sandbox, and renderers run under seccomp.
const processes = fs.readdirSync('/proc').filter(p => /^\d+$/.test(p)).flatMap(p => {
  try { const cmdline = fs.readFileSync(`/proc/${p}/cmdline`, 'utf8'); return cmdline.includes(profile) ? [{ cmdline, status: fs.readFileSync(`/proc/${p}/status`, 'utf8') }] : []; } catch { return []; }
});
const sandbox = {
  no_sandbox_flag: processes.some(p => p.cmdline.includes('--no-sandbox')),
  renderers: processes.filter(p => p.cmdline.includes('--type=renderer')).length,
  renderers_seccomp: processes.filter(p => p.cmdline.includes('--type=renderer') && /Seccomp:\s+2/.test(p.status)).length,
};
const readerFiles = ['manifest.json', 'worker.js', 'document.js'].map(f => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(extension, f))).digest('hex')]);
const ok = results.every(r => r.ok) && !sandbox.no_sandbox_flag && sandbox.renderers > 0 && sandbox.renderers === sandbox.renderers_seccomp;
fs.mkdirSync(artifacts, { recursive: true });
fs.writeFileSync(path.join(artifacts, 'reader-e2e.json'), JSON.stringify({
  at: new Date().toISOString(), ok, chromium: version.Browser, node: process.version, reader: Object.fromEntries(readerFiles), sandbox, scenarios: results,
}, null, 2) + '\n');
console.log(`sandbox ${JSON.stringify(sandbox)}; ${results.filter(r => r.ok).length}/${results.length} scenarios; artifact ${path.join(artifacts, 'reader-e2e.json')}`);
devtools.close();
browser.kill();
server.close();
listener.close();
fs.rmSync(work, { recursive: true, force: true });
process.exit(ok ? 0 : 1);
