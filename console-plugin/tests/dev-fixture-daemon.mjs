#!/usr/bin/env node
// Disposable acceptance daemon. Never included in an installed plugin payload.
// It speaks ibarad's operator socket protocol for a fictional wall: each newline-delimited
// request {id, command, args} gets one reply {id, envelope}; operator-observe
// is answered by a preview event {event:'preview', computer_id, data} instead.
// Usage: IBARA_DEV_WALL_COUNT=N node dev-fixture-daemon.mjs SOCKET (or IBARA_DEV_SOCKET=SOCKET).
// IBARA_DEV_TRACE=1 writes each answered command (not operator reads or previews) to stderr.
// IBARA_DEV_LIVE_VIDEO=1 starts with Live Video on. Every computer says it can stream, except
// the control.json list video_incapable, and `video open` answers that it can't.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const socketArg = process.argv[2] || process.env.IBARA_DEV_SOCKET || '';
if (!socketArg) { process.stderr.write('Usage: IBARA_DEV_WALL_COUNT=N node dev-fixture-daemon.mjs SOCKET\n'); process.exit(2); }
const socketPath = path.resolve(socketArg);
const wallCount = Number(process.env.IBARA_DEV_WALL_COUNT);
if (![0,1,3,10,15,20,100].includes(wallCount)) { process.stderr.write('Set IBARA_DEV_WALL_COUNT to 0, 1, 3, 10, 15, 20 or 100.\n'); process.exit(2); }
// Choosing a local file is real even on the fictional fleet: pick-file and pick-folder go to the
// operator's real ibarad, whose desktop chooser runs, so the Files tab can be exercised end to end.
const realSocket = process.env.XDG_RUNTIME_DIR ? path.join(process.env.XDG_RUNTIME_DIR, 'ibara', 'ibarad.sock') : '';
if (socketPath === realSocket) { process.stderr.write('The fictional daemon never takes the real ibarad socket.\n'); process.exit(2); }
const generation = 'fictional-grant';
const valid = id => (/^fictional-[0-9]{2,3}$/.test(id) && Number(id.slice(10)) < wallCount) || addedNode(id) !== '';
// Count 15 is the fictional fleet from the UI redesign mockups: names, holders, live tasks and
// screens (docs/design/ui-redesign-20260925). Agent principals name the operator computers
// that run them (Relay, Studio, Laptop); "riley" is this operator.
const FLEET = [
  { name: 'Iris', screen: 8, agent: 'relay', task: 'Clean up the release branch', state: 'interrupted', minutes: 12 },
  { name: 'Onyx', screen: 14, offline: true },
  { name: 'Dune', screen: 3, you: true },
  { name: 'Birch', screen: 1, agent: 'relay', task: 'Build release 4.2', minutes: 6 },
  { name: 'Cinder', screen: 2, agent: 'studio', task: 'Refactor billing module', minutes: 21 },
  { name: 'Ember', screen: 4, agent: 'laptop', task: 'Reconcile Q3 budget', minutes: 3 },
  { name: 'Garnet', screen: 6, agent: 'relay', task: 'Triage support inbox', minutes: 9 },
  { name: 'Harbor', screen: 7, agent: 'studio', task: 'Update install guide', minutes: 14 },
  { name: 'Kiln', screen: 10, agent: 'relay', task: 'Export icon set', minutes: 2 },
  { name: 'Moss', screen: 12, agent: 'laptop', task: 'Check price feed', minutes: 5 },
  { name: 'Lumen', screen: 11, paused: true },
  // Omarchy's update (omarchy): Fjord's fails 15 s after the console first reads it, Juniper's is
  // still running, and Pike's finished and needs a restart.
  { name: 'Fjord', screen: 5, ibara_current: true, omarchy: 'fails' },
  { name: 'Juniper', screen: 9, omarchy: 'running' },
  { name: 'Nimbus', screen: 13, older_ibara: true },
  { name: 'Pike', screen: 15, omarchy: 'restart' },
];
const fleet = wallCount === 15;
const member = id => fleet && /^fictional-/.test(id) && valid(id) ? FLEET[Number(id.slice(10))] : null;
const hostOf = id => member(id) ? member(id).name.toLowerCase() : 'fictional-only';
const taskRefOf = id => `task_${hostOf(id)}_live`;
// control.json simulates independent endpoint conditions: deny, offline, hang, revoked,
// epoch (a controller restart), displays ({computer_id: count}, default 1),
// scoped_delay_ms (every per-computer read of tasks, results, access, logs, health and windows for one
// computer answers that much later, as a slow computer does) and one delayed "late" reply.
// Runtime files live beside the socket, outside the plugin folder, because the shell
// hot-reloads a plugin when its files change.
const runtime = path.dirname(socketPath);
fs.mkdirSync(runtime, { recursive: true, mode: 0o700 });
const control = () => { try { return JSON.parse(fs.readFileSync(path.join(runtime, 'control.json'), 'utf8')); } catch { return {}; } };
const listed = (name, id) => Array.isArray(control()[name]) && control()[name].includes(id);
// A computer whose ibara restarted (operator-repair restart_ibara) answers as a new controller epoch.
const ibaraRestarts = new Map(); // id → restarts this run
const currentEpoch = id => String(control().epoch || 'fictional-epoch') + (ibaraRestarts.get(id) ? `-restart-${ibaraRestarts.get(id)}` : '');
// What the real routes return once a target revokes the operator and closes its peer socket.
const revokedMessage = 'OPERATOR_TRANSPORT_UNAVAILABLE: Operator request failed; inspect target status.';
const respond = (requestId, command, data, error, connection) => ({version:2,request_id:requestId,command,target:'fictional-only',observed_at:new Date().toISOString(),connection:connection || (data?'ready':'unsupported'),desktop:'ready',owner:{availability:'ready',paused:false,human_control:false,unsettled:false},capabilities:{},stale:false,data,error:error || (data?null:{code:'DEVELOPMENT_ONLY',message:'No live operation is available in the fictional wall.',retry_safe:false})});
// The live routes report every selected-operator refusal as OPERATOR_REFUSED/unauthorized.
const refused = (requestId, command, message) => respond(requestId, command, null, {code:'OPERATOR_REFUSED',message,retry_safe:false}, 'unauthorized');

// ---- first run, at any wall count: the tailnet, adding computers, requests to use this
// computer and the prompt that connects an agent. control.json keys:
//   tailscale: "running" (default), "logged_out", "stopped" or "not_installed".
//   pair: {node: {state, after_ms}}: how and when another person's computer answers
//     (PAIR_ANSWERS below by default; any other waits until the request expires).
//   incoming: true (or a number) makes other computers ask to use this one.
//   first_task_done: true once an agent has begun a task here.
// Your own computers add themselves after a moment; an added computer joins the directory and
// answers operator status and previews like the fictional fleet.
const TAILNET = [
  {node:'studio', owner:'sam@example.com', self:true, online:true, ibara:'ready'},
  {node:'tulip1', owner:'sam@example.com', online:true, ibara:'ready'},
  {node:'hazel', owner:'sam@example.com', online:true, ibara:'ready'},
  {node:'tulip0', owner:'sam@example.com', online:true, ibara:'not_installed'},
  {node:'attic', owner:'sam@example.com', online:false, ibara:'offline'},
  {node:'bench', owner:'alex@example.com', online:true, ibara:'ready'},
  {node:'loft', owner:'casey@example.com', online:true, ibara:'ready'},
];
const SELF = TAILNET.find(c => c.self);
const PAIR_ANSWERS = {bench:{state:'paired', after_ms:9000}, loft:{state:'declined', after_ms:6000}};
const CODES = {bench:'482913', loft:'615204', tulip1:'307518', hazel:'559042', studio:'120786'};
const INCOMING = [
  {request_id:'pair_in_bench', from_owner:'alex@example.com', from_computer:'bench', code:'482 913'},
  {request_id:'pair_in_loft', from_owner:'casey@example.com', from_computer:'loft', code:'615 204'},
];
// The text core's `onboarding::CONNECT_PROMPT` gives (`connect-prompt`, `ibara prompt`).
const CONNECT_PROMPT = "Connect yourself to ibara, which is installed on this computer. Do these three things:\n\n1. Add its MCP server, the command `ibara mcp` (stdio, no arguments), to your own MCP settings as a user-level server named `ibara`, keeping all your other settings. If a server named `ibara` already exists, replace it.\n2. Link the folder /usr/share/ibara/skills/ibara into your user-level skills folder as `ibara`, so updates reach you. If your harness has no skills folder, skip this; step 3 names the file. If an `ibara` skill is already there and is not that link, leave it and tell me.\n3. Add this block to your user-level instructions file (AGENTS.md, CLAUDE.md or your harness's equivalent). If a block with the same markers is there, replace it:\n\n<!-- ibara:start -->\n## ibara\nThe MCP server `ibara` lets you use real computers your person owns: desktop apps, a signed-in browser, the screen and files. When a task needs real input, a real browser session, a visual check or work on another computer, call ibara's `computer_status` and follow the ibara skill (/usr/share/ibara/skills/ibara/SKILL.md). Keep code, git, tests and pages you can fetch where you are.\n<!-- ibara:end -->\n\nThen call ibara's `computer_status` tool. If its tools won't appear until you restart, say so and ask me to restart you and paste this prompt again; doing it twice is safe. Tell me which computers you can use and which of the three steps you did.";
const started = Date.now();
const pairs = new Map();      // request_id → {node, mode, code, started, canceled}
const added = new Map();      // node → directory row
const removedIds = new Set(); // computers taken out with Remove Computer
const answered = new Map();   // incoming request_id → state
let pairCounter = 0;
// Share This Computer: invites made here. control.json `invites: true` starts with one a friend
// used and one still unused; `self_computer` names the fleet card that is this computer. The
// friend's code that works in Add Computer is FRIEND_CODE; any other code is refused.
let invites = null;
const FRIEND_CODE = '4H7K92QX';
const LEVELS = ['watch', 'use_with_approval', 'take_control'];
const LASTS = {hour:3600000, day:86400000, week:604800000, never:null};
function inviteList() {
  if (invites) return invites;
  const now = Date.now();
  invites = control().invites ? [
    {id:'inv_fictional_used', level:'watch', created_at:now - 3600000, expires_at:now + 20 * 3600000, used:{at:now - 1800000, login:'alex@example.com', computer:'Bench'}},
    {id:'inv_fictional_open', level:'take_control', created_at:now - 600000, expires_at:null, used:null},
  ] : [];
  return invites;
}
const nameOf = node => node.charAt(0).toUpperCase() + node.slice(1);
function addedNode(id) {
  const node = /^added-([a-z0-9-]+)$/.exec(String(id || ''))?.[1] || '';
  return node && added.has(node) ? node : '';
}
function addComputer(node) {
  if (added.has(node)) return added.get(node);
  const computer = 'added-' + node;
  const row = {computer_id:computer, label:nameOf(node), endpoint_id:computer + '-endpoint', binding_revision:'fictional-revision', authorization_generation:generation, trust_state:'verified', host:node, user:'riley'};
  added.set(node, row);
  return row;
}
function pairState(p) {
  if (p.canceled) return 'canceled';
  const elapsed = Date.now() - p.started;
  const answer = p.mode === 'own_computer' || p.mode === 'invite' ? {state:'paired', after_ms:p.mode === 'invite' ? 0 : 1500} : control().pair?.[p.node] || PAIR_ANSWERS[p.node];
  const state = answer && elapsed >= Number(answer.after_ms || 0) ? String(answer.state) : elapsed > 5 * 60000 ? 'expired' : 'waiting';
  if (state === 'paired') addComputer(p.node);
  return state;
}
// An added computer's screen: a desktop in its own color, with a window on it.
function addedFrame(node) {
  const width = 480, height = 270, pixels = Buffer.alloc(width * height * 3);
  const hue = [...node].reduce((sum, ch) => sum + ch.charCodeAt(0), 0);
  for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
    const at = (y * width + x) * 3, bar = y < 14, win = x > 60 && x < 420 && y > 40 && y < 230, title = win && y < 58;
    pixels[at] = bar ? 30 : title ? 60 + hue % 90 : win ? 235 : 20 + hue % 40;
    pixels[at + 1] = bar ? 34 : title ? 90 : win ? 236 : 40 + (hue * 3) % 60;
    pixels[at + 2] = bar ? 44 : title ? 150 : win ? 240 : 70 + (hue * 7) % 80;
  }
  return Buffer.concat([Buffer.from(`P6\n${width} ${height}\n255\n`, 'latin1'), pixels]);
}
function firstRun(requestId, command, args) {
  const ok = data => respond(requestId, command, data);
  const fail = (code, message) => respond(requestId, command, null, {code, message, retry_safe:true}, 'failed');
  if (command === 'tailnet') {
    const state = control().tailscale || 'running';
    if (state !== 'running') return ok({tailscale:{state, login:null, self_node:state === 'not_installed' ? null : SELF.node, login_url:state === 'logged_out' ? 'https://login.tailscale.com/a/fictional0000' : null}, computers:[]});
    return ok({tailscale:{state, login:SELF.owner, self_node:SELF.node, login_url:null}, computers:TAILNET.map((c, i) => ({
      node:c.node, dns_name:`${c.node}.tail0000.ts.net`, ip:`100.64.0.${i + 1}`, online:c.online, owner:c.owner, same_owner:c.owner === SELF.owner,
      is_self:!!c.self, ibara:c.ibara, changed:false, ...(c.self && control().self_computer ? {paired:true, computer_id:String(control().self_computer), label:null}
        : {paired:added.has(c.node), computer_id:added.get(c.node)?.computer_id ?? null, label:added.get(c.node)?.label ?? null})}))});
  }
  if (command === 'invites') return ok({invites:[...inviteList()].reverse(), tailscale:{address:'100.64.0.1', share_url:'https://login.tailscale.com/admin/machines/100.64.0.1'}});
  if (command === 'invite-create') {
    if (!LEVELS.includes(args[0]) || !(args[1] in LASTS)) return fail('INVALID_ARGUMENT', 'Choose a level and how long it lasts.');
    const now = Date.now(), invite = {id:`inv_fictional_${++pairCounter}`, level:args[0], created_at:now, expires_at:LASTS[args[1]] === null ? null : now + LASTS[args[1]], used:null};
    inviteList().push(invite);
    return ok({invite:{...invite, code:'9MXR-4TQ7'}});
  }
  if (command === 'invite-revoke') {
    const list = inviteList(), at = list.findIndex(i => i.id === args[0]);
    if (at < 0) return fail('INVITE_UNKNOWN', 'That invite is gone already: it was revoked, or it ended.');
    const [removed] = list.splice(at, 1);
    return ok({id:args[0], ended:!!removed.used});
  }
  if (command === 'pair-start') {
    const c = TAILNET.find(item => item.node === args[0]);
    if (!c) return fail('NOT_FOUND', `There is no computer named ${args[0]} on your tailnet.`);
    if (args.length > 1) {
      if (String(args[1]).toUpperCase().replace(/[ -]/g, '') !== FRIEND_CODE) return fail('INVITE_REFUSED', "That invite code didn't work. Check it with the person who shared the computer, or ask them for a new one.");
      const id = `pair_${c.node}_${++pairCounter}`;
      pairs.set(id, {node:c.node, mode:'invite', code:'904517', started:Date.now() - 60000, canceled:false});
      addComputer(c.node);
      return ok({request_id:id, code:'904 517', mode:'invite', state:'paired', computer_id:`added-${c.node}`, label:nameOf(c.node)});
    }
    if (!c.online) return fail('OFFLINE', `${nameOf(c.node)} is offline. Turn it on, then try again.`);
    if (c.ibara === 'not_installed') return fail('NOT_INSTALLED', `ibara isn't installed on ${nameOf(c.node)} yet.`);
    const id = `pair_${c.node}_${++pairCounter}`, code = CODES[c.node] || '904517';
    const p = {node:c.node, mode:c.owner === SELF.owner ? 'own_computer' : 'needs_approval', code, started:Date.now(), canceled:false};
    pairs.set(id, p);
    return ok({request_id:id, code:`${code.slice(0, 3)} ${code.slice(3)}`, mode:p.mode, state:'waiting'});
  }
  if (command === 'pair-status' || command === 'pair-cancel') {
    const p = pairs.get(args[0]);
    if (!p) return fail('NOT_FOUND', 'That request is gone. Choose Add again.');
    if (command === 'pair-cancel') {
      if (pairState(p) === 'paired') return fail('ALREADY_PAIRED', `${nameOf(p.node)} was already added.`);
      p.canceled = true;
      return ok({request_id:args[0], state:'canceled'});
    }
    const state = pairState(p), row = added.get(p.node);
    return ok({request_id:args[0], state, code:`${p.code.slice(0, 3)} ${p.code.slice(3)}`, mode:p.mode,
      ...(state === 'paired' ? {computer_id:row.computer_id, label:row.label} : {}), message:state === 'failed' ? `${nameOf(p.node)} stopped answering while it was being added.` : null});
  }
  if (command === 'pair-requests') {
    const count = control().incoming === true ? 1 : Number(control().incoming) || 0;
    return ok({requests:INCOMING.slice(0, count).filter(r => !answered.has(r.request_id)).map(r => ({...r, expires_at:started + 5 * 60000}))});
  }
  if (command === 'pair-answer') {
    const r = INCOMING.find(item => item.request_id === args[0]);
    if (!r || answered.has(r.request_id) || !['accept','decline'].includes(args[1])) return fail('NOT_FOUND', 'That request is gone; it may have expired.');
    answered.set(r.request_id, args[1] === 'accept' ? 'paired' : 'declined');
    return ok({request_id:r.request_id, state:answered.get(r.request_id)});
  }
  if (command === 'connect-prompt') return ok({prompt:CONNECT_PROMPT, first_task_done:control().first_task_done === true});
  return null;
}
// First-run commands that take a moment on a real computer.
const FIRST_RUN_DELAY_MS = {'pair-start':600, 'pair-answer':400};

const log = path.join(runtime, 'requests.jsonl');
let profile = {tile:[[0,0]],selected:[[0,0]]};
try { profile = JSON.parse(fs.readFileSync(path.join(root, 'fixtures', 'wall-load.json'), 'utf8')); } catch {}
// Frame files live where ibarad writes them, numbered on from any a previous run left.
const previews = path.join(runtime, 'ibara', 'previews');
const frameNumber = name => Number((/-([0-9]+)\.ppm$/.exec(name) || [])[1] || 0);
let frameCounter = (() => { try { return Math.max(0, ...fs.readdirSync(previews).map(frameNumber)); } catch { return 0; } })();
const frameFile = (id, quality, sequence) => {
  const variants = quality === 'selected' ? 2 : 4;
  const file = path.join(root, 'fixtures', 'wall-previews', `${id}-${quality}-${sequence % variants}.ppm`);
  return fs.existsSync(file) ? file : path.join(root, 'fixtures', 'wall-previews', `${id}.ppm`);
};
// Like ibarad: a frame file is P6 PPM scaled to fit the shown size the request names
// (--width/--height, else 448×256 for a tile and 1152×648 for a selected frame),
// keeping its aspect and never enlarging. Each output pixel averages the source box it covers.
const SHOWN = { tile: [448, 256], selected: [1152, 648] };
function shownFrame(source, box) {
  const header = /^P6\s+(\d+)\s+(\d+)\s+255\s/.exec(source.subarray(0, 32).toString('latin1'));
  if (!header) throw new Error('A fixture frame is not P6 PPM.');
  const width = Number(header[1]), height = Number(header[2]), pixels = source.subarray(header[0].length);
  const scale = Math.min(1, box[0] / width, box[1] / height);
  const w = Math.max(1, Math.round(width * scale)), h = Math.max(1, Math.round(height * scale));
  if (w === width && h === height) return source;
  const out = Buffer.alloc(w * h * 3);
  for (let y = 0; y < h; y++) {
    const y0 = Math.floor(y * height / h), y1 = Math.max(y0 + 1, Math.floor((y + 1) * height / h));
    for (let x = 0; x < w; x++) {
      const x0 = Math.floor(x * width / w), x1 = Math.max(x0 + 1, Math.floor((x + 1) * width / w));
      let r = 0, g = 0, b = 0;
      for (let sy = y0; sy < y1; sy++) for (let sx = x0; sx < x1; sx++) {
        const at = (sy * width + sx) * 3;
        r += pixels[at]; g += pixels[at + 1]; b += pixels[at + 2];
      }
      const n = (y1 - y0) * (x1 - x0), at = (y * w + x) * 3;
      out[at] = Math.round(r / n); out[at + 1] = Math.round(g / n); out[at + 2] = Math.round(b / n);
    }
  }
  return Buffer.concat([Buffer.from(`P6\n${w} ${h}\n255\n`, 'latin1'), out]);
}
// One operator-observe request: a preview event after the load profile's modeled round trip.
// `sequences` is per connection, as it was per preview-service process.
function observe(request, sequences, send) {
  const id = String(request.id || ''), argv = Array.isArray(request.args) ? request.args.map(String) : [];
  const option = key => { const i = argv.indexOf(key); return i < 0 ? '' : argv[i + 1]; };
  const computer = option('--computer'), quality = option('--quality'), started = Date.now();
  const write = (value, outcome, bytes = 0) => {
    send({event:'preview', computer_id:computer, data:value});
    fs.appendFileSync(log, JSON.stringify({request_id:id, computer, quality, started_ms:started, finished_ms:Date.now(), outcome, bytes}) + '\n');
  };
  if (!valid(computer) || !['tile','selected'].includes(quality)) return write(refused(id, 'operator-observe', 'PERMISSION_DENIED: Observation not granted.'), 'invalid');
  if (option('--epoch') !== currentEpoch(computer)) return write(refused(id, 'operator-observe', 'PERMISSION_DENIED: Target binding changed.'), 'stale-epoch');
  const epoch = currentEpoch(computer);
  const state = control();
  if (listed('hang', computer)) return setTimeout(() => write(refused(id, 'operator-observe', 'Selected operator transport timed out.'), 'timeout'), 10000);
  const samples = profile[quality] || [[0,0]];
  const [roundTrip, arrivalAge] = samples[Math.floor(Math.random() * samples.length)];
  const late = state.late && state.late.computer === computer && !fs.existsSync(path.join(runtime, 'late-sent'));
  if (late) fs.writeFileSync(path.join(runtime, 'late-sent'), id);
  setTimeout(() => {
    const now = control();
    if (!late && (listed('deny', computer) || (Array.isArray(now.deny) && now.deny.includes(computer)))) return write(refused(id, 'operator-observe', 'PERMISSION_DENIED: Observation not granted.'), 'denied');
    if (!late && listed('revoked', computer)) return write(refused(id, 'operator-observe', revokedMessage), 'revoked');
    if (!late && down(computer)) return write(refused(id, 'operator-observe', 'Selected operator transport closed.'), 'offline');
    if (!late && lockedNow(computer)) return write(refused(id, 'operator-observe', 'HUMAN_CONTROL: The graphical session is locked.'), 'locked');
    const sequence = late ? 999999 : (sequences.get(computer) || 0) + 1;
    if (!late) sequences.set(computer, sequence);
    // Like ibarad: the picture is a private file under previews/, never base64 in the line.
    const shown = Number(option('--width')) > 0 && Number(option('--height')) > 0 ? [Number(option('--width')), Number(option('--height'))] : SHOWN[quality];
    const source = late ? fs.readFileSync(path.join(root, 'fixtures', 'wall-previews', 'late.ppm'))
      : addedNode(computer) ? addedFrame(addedNode(computer)) : fs.readFileSync(frameFile(computer, quality, sequence));
    const frame = shownFrame(source, shown);
    fs.mkdirSync(previews, {recursive:true, mode:0o700});
    const file = path.join(previews, `${computer}-${quality}-${++frameCounter}.ppm`);
    fs.writeFileSync(file, frame, {mode:0o600});
    // Like ibarad: each computer and quality keeps only its newest three frame files.
    const prefix = `${computer}-${quality}-`;
    const older = fs.readdirSync(previews).filter(name => name.startsWith(prefix) && /^[0-9]+\.ppm$/.test(name.slice(prefix.length)))
      .sort((a, b) => frameNumber(b) - frameNumber(a)).slice(3);
    for (const name of older) try { fs.unlinkSync(path.join(previews, name)); } catch {}
    const endpoint = computer + '-endpoint';
    write(respond(id, 'operator-observe', {computer_id:computer,endpoint_id:endpoint,binding_revision:'fictional-revision',controller_epoch:epoch,result:{endpoint_id:endpoint,controller_epoch:epoch,authorization_generation:generation,display_revision:'fictional-1',file,bytes:frame.length,capture_time:new Date(Date.now() - (late ? 0 : arrivalAge)).toISOString(),frame_sequence:sequence}}), late ? 'late' : 'frame', frame.length);
  }, late ? Number(state.late.delay_ms || 14000) : roundTrip);
}

// Each fleet computer's access table, shaped like ibarad's `access` reply (core src/access.rs
// view): identities, grants, one row per identity and a revision every change bumps. Changes
// apply in memory for this run only. "riley" is this operator's computer identity.
const CAPABILITIES = ['watch','files','control','agents','administer'];
const EFFECT_CLASSES = ['observe','change','send','spend','destructive','access'];
const DEFAULT_EFFECTS = {observe:'allow',change:'allow',send:'ask',spend:'ask',destructive:'ask',access:'ask'};
const RULE_ORDER = ['allow','ask','deny'];
const accessModels = new Map();
function accessModel(scopedId) {
  if (accessModels.has(scopedId)) return accessModels.get(scopedId);
  const pike = member(scopedId)?.name === 'Pike';
  const identities = {
    owner:{kind:'person',computer:'owner',key:'local administrator credential'},
    riley:{kind:'computer',computer:'riley',key:'SHA256:fictional-riley'},
    relay:{kind:'computer',computer:'relay',key:'SHA256:fictional-relay'},
    studio:{kind:'computer',computer:'studio',key:'SHA256:fictional-studio'},
    laptop:{kind:'computer',computer:'laptop',key:'SHA256:fictional-laptop'},
    'codex@relay':{kind:'agent',computer:'relay',key:'SHA256:fictional-relay'},
  };
  const grants = {};
  const put = (subject, capability, rule, effects = {}) => { grants[`${subject}:${capability}`] = {subject,capability,rule,expires_at:null,effects}; };
  for (const capability of CAPABILITIES) { put('owner', capability, 'allow'); put('riley', capability, 'allow'); }
  put('relay','watch','allow'); put('relay','files','ask'); put('relay','agents','allow',{spend:'deny'});
  put('studio','watch','allow'); put('studio','agents','allow');
  put('laptop','agents',pike ? 'deny' : 'ask');
  put('codex@relay','agents','allow');
  const model = {revision:7,identities,grants,pairings:{riley:{active:true},relay:{active:true},studio:{active:true},laptop:{active:!pike}}};
  accessModels.set(scopedId, model);
  return model;
}
// Send, spend and delete (ASKED) follow core's order: denied anywhere, then Ask First set for the
// agent or its computer, then Allowed set for either, then, for agents from a computer that may
// administer this one, its Ask before agents send, spend or delete; any other computer's agents
// ask. An agent's own grants count only while its computer holds one.
const ASKED = ['send','spend','destructive'];
function asksFirst(scopedId) {
  const own = (settingValues.get(scopedId) || {}).ask_first;
  if (own === 'on' || own === 'off') return own === 'on';
  return (settingValues.get('console') || {}).agents_ask_first !== false;
}
const computerOf = (model, subject) => model.identities[subject]?.computer || subject.split('@').pop();
function matching(model, subject, capability) {
  const computer = computerOf(model, subject);
  const paired = subject === 'owner' || !!model.pairings[computer]?.active;
  const grants = paired ? Object.values(model.grants).filter(g => g.capability === capability && (g.subject === subject || g.subject === computer)) : [];
  return subject !== computer && !grants.some(g => g.subject === computer) ? [] : grants;
}
// Whether the agent's computer's own rule asks first for `kind`: an agent's Allowed doesn't outrank it.
function computerAsks(model, subject, kind) {
  const computer = computerOf(model, subject);
  return subject !== computer && matching(model, subject, 'agents').some(g => g.subject === computer && g.effects[kind] === 'ask');
}
function accessView(model, scopedId) {
  const askFirst = asksFirst(scopedId);
  const paired = subject => subject === 'owner' || !!model.pairings[computerOf(model, subject)]?.active;
  const strictest = rules => rules.reduce((worst, rule) => RULE_ORDER.indexOf(rule) > RULE_ORDER.indexOf(worst) ? rule : worst, rules.length ? 'allow' : 'deny');
  const rule = (subject, capability) => strictest(matching(model, subject, capability).map(g => g.rule));
  const effect = (subject, kind) => {
    const grants = matching(model, subject, 'agents');
    if (!grants.length || grants.some(g => g.rule === 'deny' || g.effects[kind] === 'deny')) return 'deny';
    if (!ASKED.includes(kind)) return strictest(grants.map(g => g.effects[kind] || DEFAULT_EFFECTS[kind]));
    const set = grants.map(g => g.effects[kind]).filter(Boolean);
    const ownComputer = rule(computerOf(model, subject), 'administer') === 'allow';
    return set.length ? strictest(set) : askFirst || !ownComputer ? 'ask' : 'allow';
  };
  const ownEffects = subject => Object.assign({}, ...Object.values(model.grants).filter(g => g.subject === subject && g.capability === 'agents').map(g => g.effects));
  const rows = Object.keys(model.identities).map(subject => ({subject,kind:model.identities[subject].kind,paired:paired(subject),
    capabilities:Object.fromEntries(CAPABILITIES.map(c => [c, rule(subject, c)])),effects:Object.fromEntries(EFFECT_CLASSES.map(c => [c, effect(subject, c)])),own_effects:ownEffects(subject)}));
  return {revision:model.revision,identities:model.identities,pairings:model.pairings,grants:model.grants,rows,can_administer:rule('owner','administer') === 'allow',ask_first:askFirst,
    boundary:"ibara asks first only for steps it can recognize or that an agent declares. An agent that can run commands or use the desktop can do anything the person signed in there can."};
}
// Always Allow, or Allow on an agent's request to stop asking: the agent's own rules for those
// kinds become allow on that computer, except a kind its computer's own rule asks first for.
function allowWithoutAsking(scopedId, agent, kinds) {
  const model = accessModel(scopedId), key = `${agent}:agents`;
  if (!model.identities[agent]) model.identities[agent] = {kind:'agent',computer:agent.split('@')[1] || agent,key:'SHA256:fictional-agent'};
  const grant = model.grants[key] || {subject:agent,capability:'agents',rule:'allow',expires_at:null,effects:{}};
  const allowed = kinds.filter(kind => !computerAsks(model, agent, kind));
  model.grants[key] = {...grant, effects:{...grant.effects, ...Object.fromEntries(allowed.map(kind => [kind, 'allow']))}};
  model.revision += 1;
}
// access-set, access-remove and access-unpair, checked against the revision the table showed.
function changeAccess(model, command, body) {
  if (Number(body.expected_revision) !== model.revision) return {code:'REQUEST_CONFLICT',message:'Access changed; reload the table before editing.',retry_safe:true};
  const subject = String(body.subject || '');
  if (!model.identities[subject] || subject === 'owner') return {code:'INVALID_ARGUMENT',message:'Unknown identity.',retry_safe:false};
  if (command === 'access-set') {
    if (!CAPABILITIES.includes(body.capability) || !RULE_ORDER.includes(body.rule)) return {code:'INVALID_ARGUMENT',message:'Unknown capability or rule.',retry_safe:false};
    const key = `${subject}:${body.capability}`;
    model.grants[key] = {subject,capability:body.capability,rule:body.rule,expires_at:null,effects:body.effects || model.grants[key]?.effects || {}};
  } else {
    for (const key of Object.keys(model.grants)) if (model.grants[key].subject === subject) delete model.grants[key];
    if (command === 'access-unpair') model.pairings[model.identities[subject].computer] = {active:false};
  }
  model.revision += 1;
  return null;
}

// ---- everyday commands. Every per-computer command takes --computer ID --epoch E and answers
// like operator-status (opReply). Changes apply in memory for this run only; control.json only
// seeds them. control.json keys:
//   approvals: [{computer, summary, details?, minutes_ago?, unreachable?}…]: agents on those
//     computers (computer_id) asking for approval; each gets the ref att_<index>, and answering
//     one removes it. details is the request behind the words, as a computer sends it.
//     unreachable: true lists it as kept from before a failed read (its computer is named in
//     fleet-attention's unreachable).
//   questions: [{computer, summary, options?, minutes_ago?}…]: agents on those computers asking
//     a person something; each gets the ref att_q<index>. It is answered with one of its options
//     (any text when it has none) or dismissed, which removes it. questions_old: true answers
//     like an ibara from before questions could be answered here ("Choose approve or deny.").
//   needs_person: {computer_id: fix}: a repair the computer couldn't finish by itself, fix being
//     reconnect_display, restart_viewer or restart_ibara; Fix It clears it.
//   repair_fails: [ids] whose Fix It doesn't work.
//   system_paused: [ids] the system paused (a person paused fleet15's Lumen); resume_off: [ids]
//     of those whose Resume agents after a restart setting is off, so they wait for Resume.
//   power_denied: [ids] that refuse restart, shut down, sleep, lock and update.
//   disk_password: [ids] that ask for a disk password when they start.
//   wake: {computer_id: {kind:'wifi'|'ethernet', from_off}}: computers that can be woken over the
//     network (fleet15's Onyx can by default); wake_via: the computer (label or id) that sends
//     the wake, else this one; wake_sure: false says this computer sent it without being able
//     to tell whether it is on the sleeping computer's network.
//   theme_failed: [ids] that refuse a new theme.
//   away: true shows "while you were away" until away-seen; away_events: N gives Iris (fleet15)
//     N finished tasks instead, oldest first, as a busy agent computer has.
//   control_error: {computer_id: "CODE: message"}: pause and resume there fail with that code.
//   send_fails: [file names] whose send finds the same name already in the shared folder.
//   locked: [ids] whose screen is locked: status says locked and pictures are refused
//     (HUMAN_CONTROL) until 8 s after a person takes control there, as if they typed the password
//     in the viewer.
//   held: [fleet15 ids] this person holds control of from the start, as fleet15's Dune; Hand Back
//     gives it back.
// A restart takes a computer offline for 12 s; shut down and sleep until it is woken, and a
// wake brings it back 8 s later.
const ago = minutes => new Date(Date.now() - minutes * 60000).toISOString().replace(/\.\d{3}Z$/, 'Z');
// Times fixed at the daemon's start, so a list read twice shows the same moments.
const before = minutes => new Date(started - minutes * 60000).toISOString().replace(/\.\d{3}Z$/, 'Z');
const FIXES = {
  reconnect_display:{code:'display_missing', message:n => `${n} lost its screen and couldn't add one by itself.`, done:'Reconnected its screen.', broken:n => `${n} still has no screen. Restart it, or plug in a monitor.`},
  restart_viewer:{code:'viewer_unavailable', message:n => `Screen sharing on ${n} stopped and didn't start again.`, done:'Restarted screen sharing.', broken:n => `Screen sharing on ${n} still won't start. Restart ${n}.`},
  restart_ibara:{code:'work_unsettled', message:n => `ibara on ${n} stopped answering its own checks.`, done:'Restarted ibara.', broken:n => `ibara on ${n} still isn't answering. Restart ${n}.`},
};
const pauses = new Map();            // id → 'person' | null, once paused or resumed here
const controls = new Set();          // fleet15 ids a person took control of here, until Hand Back
if (fleet) FLEET.forEach((m, i) => { const id = 'fictional-' + String(i).padStart(2,'0'); if (m.you || listed('held', id)) controls.add(id); });
const unlockAt = new Map();          // locked ids → when the person who took control unlocks them
const lockedNow = id => listed('locked', id) && !(unlockAt.get(id) <= Date.now());
// The viewer Take Control and Open Viewer open, as a stand-in: `sleep` copied beside the socket
// as `ibara-view`, so the console sees a process by that name (no window) until Hand Back or
// until you end it (`pkill -x ibara-view`) as a person closing the viewer would.
const viewers = new Map();           // id → the stand-in viewer's pid
function closeViewer(id) { const pid = viewers.get(id); viewers.delete(id); if (pid) try { process.kill(pid, 'SIGTERM'); } catch {} }
function openViewer(id) {
  closeViewer(id);
  const bin = path.join(runtime, 'ibara-view');
  if (!fs.existsSync(bin)) { fs.copyFileSync('/usr/bin/sleep', bin); fs.chmodSync(bin, 0o700); }
  const child = spawn(bin, ['infinity'], {detached:true, stdio:'ignore'});
  child.unref();
  viewers.set(id, child.pid);
  return child.pid;
}
const ownershipChanges = new Map();  // id → pause changes, for ownership_revision
const fixed = new Set();             // ids whose needs_person Fix It cleared
const lastRepair = new Map();        // id → {at, summary}
const answeredApprovals = new Set(); // att_<index>, att_q<index>
const powerOff = new Map();          // id → when it answers again (Infinity until woken)
const booted = new Map();            // id → when it last started
const woken = new Set();             // fleet15 computers woken this run
// Omarchy's update: id → {started, ends (ms, Infinity while it runs; null until first read for
// 'fails'), outcome 'done'|'failed', restart}. Update Omarchy runs one for 15 s that ends needing a
// restart; a restart clears that.
const omarchyRuns = new Map();
if (fleet) FLEET.forEach((m, i) => {
  const id = 'fictional-' + String(i).padStart(2,'0');
  if (m.omarchy === 'running') omarchyRuns.set(id, {started:started - 4 * 60000, ends:Infinity, outcome:'done', restart:true});
  if (m.omarchy === 'restart') omarchyRuns.set(id, {started:started - 31 * 60000, ends:started - 25 * 60000, outcome:'done', restart:true});
  if (m.omarchy === 'fails') omarchyRuns.set(id, {started:null, ends:null, outcome:'failed', restart:false});
});
// operator-status `omarchy_update`, as the core reports it (times in epoch ms).
function omarchyOf(id) {
  const run = omarchyRuns.get(id);
  if (!run) return null;
  if (run.ends === null) { run.started = Date.now(); run.ends = Date.now() + 15000; }
  if (Date.now() < run.ends) return {state:'running', started_at:run.started, finished_at:null, restart_needed:false, message:null};
  const failed = run.outcome === 'failed';
  return {state:run.outcome, started_at:run.started, finished_at:run.ends, restart_needed:!failed && run.restart,
    message:failed ? `${labelOf(id)}'s Omarchy update failed (exit 1). See journalctl -u ibara-omarchy-update.` : null};
}
const cancelledTasks = new Set();    // task refs revoked this run
const settingValues = new Map();     // 'console' or a computer id → {key: value}
if (process.env.IBARA_DEV_LIVE_VIDEO === '1') settingValues.set('console', {live_video:true});
let awaySeen = false;
const computerIds = () => Array.from({length:wallCount}, (_, i) => 'fictional-' + String(i).padStart(2,'0')).concat([...added.values()].map(row => row.computer_id));
const baseLabel = id => member(id) ? member(id).name : addedNode(id) ? added.get(addedNode(id)).label : 'FICTIONAL ' + String(Number(id.slice(10)) + 1).padStart(2,'0');
const labelOf = id => settingValues.get(id)?.name ?? baseLabel(id);
const poweredOff = id => (powerOff.get(id) || 0) > Date.now();
// Nothing answers for it: the offline list, a power action, or fleet15's Onyx until woken.
const down = id => listed('offline', id) || poweredOff(id) || (!!member(id)?.offline && !woken.has(id));
const pauseOf = id => pauses.has(id) ? pauses.get(id) : listed('system_paused', id) ? 'system' : member(id)?.paused ? 'person' : null;
const ownerOf = id => { const m = member(id); return controls.has(id) ? 'operator:riley' : pauseOf(id) ? 'human' : m?.agent && !cancelledTasks.has(taskRefOf(id)) ? `agent:${m.agent}:${taskRefOf(id)}` : 'none'; };
const ownershipRevision = (id, owner) => `${currentEpoch(id)}:${ownershipChanges.get(id) || 0}:${owner}`;
function needsPerson(id) {
  const fix = String(control().needs_person?.[id] || ''), f = FIXES[fix];
  return f && !fixed.has(id) ? {code:f.code, message:f.message(labelOf(id)), fix} : null;
}
const repairView = id => ({last:lastRepair.get(id) || (member(id)?.name === 'Lumen' ? {at:before(95), summary:'Reconnected its screen.'} : null), needs_person:needsPerson(id)});
function wakeOf(id) {
  const seeds = control().wake && typeof control().wake === 'object' ? control().wake : {};
  const seed = id in seeds ? seeds[id] : member(id)?.offline ? {kind:'ethernet', from_off:true} : null;
  if (!seed) return null;
  const kind = seed.kind === 'wifi' ? 'wifi' : 'ethernet';
  const mac = '3c:22:fb:' + crypto.createHash('sha256').update(id).digest('hex').slice(0, 6).match(/../g).join(':');
  return {mac, ifname:kind === 'wifi' ? 'wlan0' : 'enp3s0', kind, subnet:'10.0.4.0/24', from_off:!!seed.from_off};
}
function tasksOf(id) {
  const m = member(id), host = m ? hostOf(id) : id;
  const live = m?.task ? {task_ref:taskRefOf(id),goal:m.task,principal:m.agent,state:m.state || 'active',created_at:ago(m.minutes),updated_at:ago(0),contract_version:'3.0',visibility:'private',live:true} : null;
  const earlier = {task_ref:`task_${host}_earlier`,goal:'Rotate the log archive',principal:m?.agent || 'relay',state:'completed',created_at:ago(180),updated_at:ago(170),contract_version:'3.0',visibility:'private',completion_outcome:'completed'};
  return [live, earlier].filter(Boolean).map(task => cancelledTasks.has(task.task_ref) ? {...task, state:'cancelled', live:false} : task);
}
// A task's steps, newest first, shaped like ibarad's receipt summaries (core controller/admin.rs
// receipt_summary): the live task's newest step is a minute old.
function receiptsOf(task) {
  const steps = task.live
    ? [['Ran the test suite', 'shell.run', 1], ['Edited 3 files', 'files.write', 2], ['Read the build log', 'files.read', 4], ['Opened a terminal', 'desktop.type', 5]]
    : [['Moved 12 old archives to storage', 'files.move', 171], ['Listed the log folder', 'files.read', 175]];
  return steps.map(([summary, tool, minutes], n) => ({operation_ref:`${task.task_ref}_op_${n}`, task_ref:task.task_ref, tool, summary, created_at:ago(minutes),
    execution:'completed', effect:'applied', verification:'verified', dependency_state:'settled', error:null}));
}
// Windows (operator-windows, -window-close, -window-move): Hyprland workspaces with the windows in
// each, each saying whether it runs in a terminal (core tells from its process, not its class).
// Where fleet15's agent works, its terminal is in use, and its browser answers Close or Move To…
// with BUSY the first time (the agent took it after the list was read). The text editor asks to
// save, so Close leaves it open. btop, a terminal started with its own app id as Omarchy starts
// it, was left by an earlier agent. Dune shows an empty workspace 3 on screen. Changes last until
// the daemon stops.
const windowSets = new Map();        // computer id → its windows
function windowsOf(id) {
  if (windowSets.has(id)) return windowSets.get(id);
  const m = member(id), host = m ? hostOf(id) : id, live = m?.task ? taskRefOf(id) : null;
  const seed = parseInt(crypto.createHash('sha256').update(id).digest('hex').slice(0, 6), 16);
  const rows = [
    {class:'chromium', title:'Sign up · Example', workspace:1, task:live, claimed:false},
    {class:'foot', title:'npm run build', workspace:1, task:live, terminal:true},
    {class:'org.omarchy.btop', title:'btop', workspace:2, task:`task_${host}_earlier`, terminal:true},
    {class:'org.gnome.TextEditor', title:'Untitled 1 — Text Editor', workspace:2, task:null, asks_to_save:true},
    {class:'org.gnome.Nautilus', title:'Downloads', workspace:2, task:null},
    {class:'Spotify', title:'Spotify Premium', workspace:-98, task:null},
  ].map((w, n) => ({...w, address:'0x' + (0x55d0a0000000 + seed * 0x100 + n * 0x40).toString(16), pid:4100 + n * 7}));
  windowSets.set(id, rows);
  return rows;
}
const liveTaskOf = id => member(id)?.task && !cancelledTasks.has(taskRefOf(id)) ? {task_ref:taskRefOf(id), goal:member(id).task} : null;
function windowTask(id, w) {
  if (!w.task || (w.claimed === false)) return null;
  const task = tasksOf(id).find(t => t.task_ref === w.task);
  return task ? {task_ref:task.task_ref, goal:task.goal, running:liveTaskOf(id)?.task_ref === task.task_ref} : null;
}
function windowsView(id) {
  const active = member(id)?.you ? 3 : 1, windows = windowsOf(id);
  const ids = [...new Set(windows.map(w => w.workspace).concat([active]))].sort((a, b) => (a < 0) - (b < 0) || a - b);
  return {workspaces:ids.map(ws => ({id:ws, name:ws < 0 ? 'special:scratchpad' : String(ws), special:ws < 0, active:ws === active,
    windows:windows.filter(w => w.workspace === ws).map(w => ({address:w.address, pid:w.pid, class:w.class, title:w.title, terminal:w.terminal === true,
      floating:ws < 0, fullscreen:false, focused:ws === active && w === windows.find(x => x.workspace === ws), task:windowTask(id, w)}))})),
    agent:liveTaskOf(id)};
}
// Settings, shaped like ibarad's: sections of typed settings, each with its value and default.
const bool = (key, title, help) => ({key, title, help, type:'bool', default:true});
const computerSettings = id => [
  {id:'general', title:'General', settings:[{key:'name', title:'Computer name', help:'The name every console shows for this computer.', type:'text', default:baseLabel(id), trim:true,
    check:value => value.trim().length >= 1 && value.trim().length <= 64 && !/[\r\n]/.test(value) ? null : 'A computer name needs 1–64 characters.'}]},
  {id:'display', title:'Display', settings:[
    {key:'virtual_display_size', title:'Screen size without a monitor', help:'Used when no monitor is plugged in, so agents and you still have a screen to work on.', type:'choice',
      choices:[{value:'1280x720', label:'1280 × 720'}, {value:'1920x1080', label:'1920 × 1080'}, {value:'2560x1440', label:'2560 × 1440'}], default:'1920x1080', invalid:'Choose one of the listed sizes.'},
    {key:'preview_seconds', title:'Picture refresh', help:'Seconds between pictures this computer sends while someone watches it.', type:'number', min:1, max:30, default:3}]},
  {id:'recovery', title:'Recovery', settings:[
    bool('auto_resume', 'Resume after restart', 'Let agents carry on by themselves after this computer restarts, when nothing needs a person.'),
    bool('self_repair', 'Repair by itself', 'Reconnect the screen and restart screen sharing on its own when they stop.')]},
  {id:'power', title:'Power', settings:[bool('wake_on_network', 'Wake over the network', 'Let other computers on the same network turn this one on when it sleeps or is off.')]},
  {id:'control', title:'Take Control', settings:[bool('shared_clipboard', 'Shared clipboard', 'While another computer has control of this one, text and pictures copied on either can be pasted on the other.')]},
  {id:'agents', title:'Agents', settings:[{key:'ask_first', title:'Ask before agents send, spend or delete', type:'choice', default:'same', invalid:'Choose same, on or off.',
    help:"Off: agents from your own computers send, spend money and delete here without asking you first. Agents from someone else's computer still ask, unless you choose Always Allow for one. Same as in Settings follows the switch of that name in ibara's Settings. Ask First and Denied set under Access still apply.",
    choices:[{value:'same', label:'Same as in Settings (' + ((settingValues.get('console') || {}).agents_ask_first === false ? 'off' : 'on') + ')'}, {value:'on', label:'On'}, {value:'off', label:'Off'}]}]},
];
const consoleSettings = () => [
  {id:'approvals', title:'Approvals', settings:[bool('agents_ask_first', 'Ask before agents send, spend or delete',
    "On: an agent stops and waits for you to approve each step that sends something, spends money or deletes. Off: agents from your own computers take those steps without asking you, on every computer whose own Settings tab says Same as in Settings. Agents from someone else's computer still ask. Denied permissions, Administer and Take Control stay as they are.")]},
  {id:'notifications', title:'Notifications', settings:[
    bool('notifications', 'Notifications', 'Tell you on this desktop when a task you follow finishes, a file arrives, or another computer asks to use this one.'),
    bool('approval_notifications', 'Approval requests', 'Ask on this desktop when an agent needs your approval, with Approve and Deny on the notification.')]},
  {id:'files', title:'Files', settings:[{key:'download_folder', title:'Download folder', help:'Where files from other computers are saved. Leave empty for your Downloads folder.', type:'text', default:'',
    check:value => value === '' || path.isAbsolute(value) ? null : 'Use a full folder path, like /home/sam/Downloads.'}]},
  {id:'fleet', title:'Fleet', settings:[{key:'fleet_preview_seconds', title:'Fleet picture interval', help:'Seconds between new pictures of each computer on the fleet page. Longer uses less network.', type:'number', min:2, max:60, default:5},
    {key:'live_video', title:'Live Video (Preview)', help:'New and not yet stable. Uses more memory and bandwidth.', type:'bool', default:false}]},
];
// get | set KEY VALUE | reset KEY | reset --section ID. Values arrive as text: 'true'/'false',
// decimal numbers, a choice's value. Returns {data} or {error}.
function settingsReply(sections, store, scope, words, sectionId) {
  const values = settingValues.get(store) || {};
  settingValues.set(store, values);
  const shown = ({check, invalid, trim, ...setting}) => ({...setting, value:setting.key in values ? values[setting.key] : setting.default, default:setting.default, scope});
  const view = list => list.map(section => ({id:section.id, title:section.title, settings:section.settings.map(shown)}));
  const bad = message => ({error:{code:'INVALID_ARGUMENT', message, retry_safe:false}});
  const [verb, key, raw] = words;
  if (verb === 'get') return {data:{sections:view(sections)}};
  if (verb === 'reset' && sectionId) {
    const section = sections.find(item => item.id === sectionId);
    if (!section) return bad("That section doesn't exist.");
    for (const setting of section.settings) delete values[setting.key];
    return {data:{sections:view([section])}};
  }
  if (verb !== 'set' && verb !== 'reset') return bad('Use get, set or reset.');
  const setting = sections.flatMap(section => section.settings).find(item => item.key === key);
  if (!setting) return bad("That setting doesn't exist.");
  if (verb === 'reset') { delete values[key]; return {data:shown(setting)}; }
  if (raw === undefined) return bad('Give a value for that setting.');
  let value = raw;
  if (setting.type === 'bool') { if (raw !== 'true' && raw !== 'false') return bad('Choose on or off.'); value = raw === 'true'; }
  else if (setting.type === 'number') { value = /^[0-9]+$/.test(raw) ? Number(raw) : NaN; if (!(value >= setting.min && value <= setting.max)) return bad(`Choose a number from ${setting.min} to ${setting.max}.`); }
  else if (setting.type === 'choice') { if (!setting.choices.some(choice => choice.value === raw)) return bad(setting.invalid || 'Choose one of the listed options.'); }
  else { const message = setting.check?.(raw); if (message) return bad(message); if (setting.trim) value = raw.trim(); }
  values[key] = value;
  return {data:shown(setting)};
}
// ---- login sharing, with this computer (Studio) as the sharing computer. control.json keys:
//   logins: "on" (default: sharing from Brave · Default), "undecided" (the setup card shows),
//     "off" (no computer shares), "elsewhere" (Laptop shares), "target" (this computer also runs
//     agents) or "old" (an ibara without login sharing: every login command is unknown).
//   login_requests: [{computer, agent, goal, sites: [site | {site, via}], page, own, minutes_ago}]:
//     agents asking for logins (attention kind `login`, refs att_l0, att_l1, …).
//   login_signed_out: sites you're not signed in to in Brave, until the first Retry.
//   login_browser_closed: true while Brave isn't running (shares wait for it).
//   login_unknown: sites whose write can't be confirmed.
//   login_rejected: [{computer, site, minutes_ago}]: sites that rejected a shared login.
//   login_pinned_elsewhere: {computer: label}: computers that take logins from another sharing
//     computer (their requests name it), even while this one shares.
// Fleet15's Nimbus runs an older ibara, as elsewhere; Onyx is off, so it gets logins later.
const LOGIN_BROWSERS = [{browser:'brave', name:'Brave', profiles:['Default', 'Work'], supported:true}, {browser:'chromium', name:'Chromium', profiles:['Default'], supported:true}];
let loginState = null;
function logins() {
  if (loginState) return loginState;
  const mode = control().logins || 'on';
  const on = mode === 'on';
  const now = Date.now(), h = hours => now - hours * 3600000;
  const ids = computerIds();
  const rules = {}, history = {};
  if (fleet) {
    rules['fictional-03'] = {'sos.ok.gov':'allow', 'github.com':'allow', 'id.me':'ask', 'irs.gov':'ask'};
    history['fictional-03'] = {'sos.ok.gov':{first_goal:'Renew my business license', last_shared_ms:h(2), last_result:'worked', last_result_at_ms:h(2)},
      'github.com':{first_goal:'Build release 4.2', last_shared_ms:h(30), last_result:'worked', last_result_at_ms:h(30)},
      'id.me':{first_goal:'File Q3 estimated tax', last_shared_ms:h(50), last_result:'site_rejected', last_result_at_ms:h(49)},
      'irs.gov':{first_goal:'File Q3 estimated tax', last_shared_ms:0, last_result:null, last_result_at_ms:null}};
    rules['fictional-04'] = {'stripe.com':'allow'};
    history['fictional-04'] = {'stripe.com':{first_goal:'Refactor billing module', last_shared_ms:h(5), last_result:'worked', last_result_at_ms:h(5)}};
  } else if (ids.length) {
    rules[ids[0]] = {'example.org':'allow'};
    history[ids[0]] = {'example.org':{first_goal:'Rotate the log archive', last_shared_ms:h(3), last_result:'worked', last_result_at_ms:h(3)}};
  }
  loginState = {mode, enabled:on, decided:mode !== 'undecided', browser:on ? 'brave' : '', profile:on ? 'Default' : '',
    allRules:{'chase.com':'deny', 'gmail.com':'deny', 'usps.com':'allow'}, rules, history, answered:new Set(), retried:new Set(), open:new Set()};
  return loginState;
}
const LOGIN_ORDER = {deny:0, ask:1, allow:2};
const loginOlder = id => !!member(id)?.older_ibara || listed('login_older', id);
function loginEffective(id, site) {
  const own = logins().rules[id]?.[site], all = logins().allRules[site];
  const layers = [own, all].filter(Boolean);
  return layers.length ? layers.sort((a, b) => LOGIN_ORDER[a] - LOGIN_ORDER[b])[0] : 'ask';
}
const pinnedElsewhere = () => control().login_pinned_elsewhere && typeof control().login_pinned_elsewhere === 'object' ? control().login_pinned_elsewhere : {};
function loginRequests() {
  const list = Array.isArray(control().login_requests) ? control().login_requests : [];
  const s = logins();
  return list.map((r, i) => ({r, ref:'att_l' + i})).filter(({r, ref}) => valid(String(r?.computer || '')) && !s.answered.has(ref)).map(({r, ref}) => {
    const sites = (Array.isArray(r.sites) ? r.sites : []).map(entry => typeof entry === 'string' ? {site:entry} : entry);
    const label = labelOf(r.computer), agent = String(r.agent || 'codex@relay');
    const what = sites.length === 1 ? `your login for ${sites[0].site}` : `your logins for ${sites.length} sites`;
    return {computer_id:r.computer, label, ref, kind:'login', summary:`${agent}, working on “${r.goal}” on ${label}, wants ${what}`, at:before(Number(r.minutes_ago) || 0),
      details:{agent, principal:agent.split('@')[0], task:{task_ref:`task_${ref}`, goal:String(r.goal || '')}, sites, page:String(r.page || ''), own:r.own !== false,
        source_label:s.mode === 'elsewhere' ? 'Laptop' : pinnedElsewhere()[r.computer] || (s.enabled ? 'Studio' : '')}};
  });
}
function loginCommand(requestId, command, option, commandArgs) {
  if (!command.startsWith('login-')) return null;
  const ok = data => respond(requestId, command, data);
  const fail = (code, message, data = null) => respond(requestId, command, data, {code, message, retry_safe:false}, 'failed');
  const s = logins();
  if (s.mode === 'old') return fail('INVALID_ARGUMENT', `Unknown command: ${command}.`);
  const ids = computerIds().filter(id => !removedIds.has(id));
  const brave = () => s.enabled && control().login_browser_closed !== true;
  if (command === 'login-settings') {
    const computers = Object.fromEntries(ids.map(id => [id, {configured:s.enabled && !loginOlder(id), sites_allowed:s.enabled ? new Set(Object.keys(s.rules[id] || {}).concat(Object.keys(s.allRules)).filter(site => loginEffective(id, site) === 'allow')).size : 0,
      ...(s.mode === 'elsewhere' ? {source_elsewhere:'Laptop'} : pinnedElsewhere()[id] ? {source_elsewhere:pinnedElsewhere()[id]} : {}), ...(loginOlder(id) ? {older:true} : {})}]));
    const rejected = (Array.isArray(control().login_rejected) ? control().login_rejected : []).filter(r => valid(String(r?.computer || '')))
      .map(r => ({computer:r.computer, site:String(r.site), at_ms:started - (Number(r.minutes_ago) || 0) * 60000}));
    return ok({enabled:s.enabled, decided:s.decided, browser:s.browser, profile:s.profile, label:s.enabled ? 'Studio' : '', connected:brave(), installable:true,
      browsers:LOGIN_BROWSERS, all_rules:s.allRules, computers, target_role:s.mode === 'target', rejected});
  }
  if (command === 'login-not-now') { s.decided = true; return ok({decided:true}); }
  if (command === 'login-on') {
    const browser = LOGIN_BROWSERS.find(b => b.browser === option('--browser'));
    if (!browser || !browser.profiles.includes(option('--profile'))) return fail('INVALID_ARGUMENT', 'Choose a browser and profile ibara found.');
    if (s.mode === 'target') return fail('TARGET_ROLE', "This computer also runs agents, so it can't share logins yet. Turn on sharing from the computer you use.");
    if (s.mode === 'elsewhere' && !commandArgs.includes('--replace')) return fail('ANOTHER_SOURCE', 'Logins for your computers come from Laptop.', {label:'Laptop'});
    Object.assign(s, {mode:'on', enabled:true, decided:true, browser:browser.browser, profile:option('--profile')});
    return ok({enabled:true});
  }
  // login_extension_stays: ibara couldn't take its extension out (no power service here); login_extension_error: why it failed.
  if (command === 'login-off') {
    s.enabled = false; if (s.mode === 'on') s.mode = 'off';
    const why = typeof control().login_extension_error === 'string' ? control().login_extension_error : '';
    return ok({enabled:false, extension_removed:!why && control().login_extension_stays !== true, ...(why ? {extension_error:why} : {})});
  }
  if (!s.enabled) return fail('SHARING_OFF', 'Login sharing is off on this computer. Turn it on in Settings under Logins.');
  const site = String(option('--site') || '').toLowerCase(), target = option('--computer');
  if (command === 'login-rule') {
    if (!['allow','ask','deny'].includes(option('--rule')) || !site) return fail('INVALID_ARGUMENT', 'Choose allow, ask or deny for a site.');
    if (target === 'all') s.allRules[site] = option('--rule');
    else if (valid(target)) (s.rules[target] ||= {})[site] = option('--rule');
    else return fail('NOT_FOUND', 'That computer is not on your fleet.');
    return ok({site, computer:target, rule:option('--rule')});
  }
  if (command === 'login-rows') {
    if (!valid(target)) return fail('NOT_FOUND', 'That computer is not on your fleet.');
    // Sites with a rule here, or Allowed or Ask First for All Computers; Never Share sites are listed apart.
    const sites = [...new Set(Object.keys(s.rules[target] || {}).concat(Object.keys(s.allRules).filter(site => s.allRules[site] !== 'deny')))];
    return ok({rows:sites.map(site => {
      const past = s.history[target]?.[site] || {};
      return {site, rule:loginEffective(target, site), own_rule:s.rules[target]?.[site] || null, all_rule:s.allRules[site] || null,
        first_goal:past.first_goal || '', last_shared_ms:past.last_shared_ms || 0, last_result:past.last_result || null, last_result_at_ms:past.last_result_at_ms || null};
    }), denied_all:Object.keys(s.allRules).filter(site => s.allRules[site] === 'deny')});
  }
  const deliver = (id, site, goal) => {
    const past = ((s.history[id] ||= {})[site] ||= {first_goal:goal || '', last_shared_ms:0, last_result:null});
    past.last_shared_ms = Date.now();
  };
  if (command === 'login-answer') {
    const request = loginRequests().find(item => item.ref === option('--att'));
    if (!request || request.computer_id !== target) return fail('NOT_FOUND', 'That request is no longer waiting.');
    let decisions = {};
    try { decisions = JSON.parse(option('--decisions') || '{}'); } catch {}
    const retry = commandArgs.includes('--retry');
    const signedOut = Array.isArray(control().login_signed_out) ? control().login_signed_out : [];
    const unknown = Array.isArray(control().login_unknown) ? control().login_unknown : [];
    const sites = request.details.sites.map(entry => {
      const choice = decisions[entry.site];
      if (choice === 'never') { s.allRules[entry.site] = 'deny'; return {site:entry.site, outcome:'denied'}; }
      if (choice !== 'share' && choice !== 'share_all') return {site:entry.site, outcome:'declined'};
      if (choice === 'share_all') s.allRules[entry.site] = 'allow'; else (s.rules[target] ||= {})[entry.site] = 'allow';
      if (retry) s.retried.add(entry.site);
      if (signedOut.includes(entry.site) && !s.retried.has(entry.site)) return {site:entry.site, outcome:'signed_out_there'};
      if (!brave()) return {site:entry.site, outcome:'waiting_for_browser'};
      if (unknown.includes(entry.site)) return {site:entry.site, outcome:'unknown'};
      deliver(target, entry.site, request.details.task.goal);
      return {site:entry.site, outcome:'shared'};
    });
    if (!sites.some(o => o.outcome === 'signed_out_there')) s.answered.add(request.ref);
    return ok({sites});
  }
  if (command === 'login-share-with') {
    const to = option('--to') === 'all' ? ids : String(option('--to') || '').split(',').filter(valid);
    if (!site || !to.length) return fail('INVALID_ARGUMENT', 'Name a site and the computers to share it with.');
    if (option('--to') === 'all') s.allRules[site] = 'allow'; else for (const id of to) (s.rules[id] ||= {})[site] = 'allow';
    const older = to.filter(loginOlder), able = to.filter(id => !loginOlder(id));
    const unknown = Array.isArray(control().login_unknown) && control().login_unknown.includes(site) ? able.slice(0, 1) : [];
    const delivered = brave() ? able.filter(id => !down(id) && !unknown.includes(id)) : [];
    for (const id of delivered) deliver(id, site, '');
    return ok({site, delivered, deferred:able.filter(id => !delivered.includes(id) && !unknown.includes(id)), unknown, older});
  }
  if (command === 'login-remove') {
    if (!valid(target) || !site) return fail('INVALID_ARGUMENT', 'Name a computer and a site.');
    delete s.rules[target]?.[site];
    delete s.history[target]?.[site];
    // Off: queued. login_remove_partial: some cookies went, the rest are queued.
    const partial = Array.isArray(control().login_remove_partial) && control().login_remove_partial.includes(site);
    return ok(down(target) ? {site, removed:null, deferred:true} : partial ? {site, removed:2, deferred:true} : {site, removed:5});
  }
  if (command === 'login-sync') {
    const allowed = [...new Set(ids.flatMap(id => Object.keys(s.rules[id] || {}).filter(site => s.rules[id][site] === 'allow')))].filter(site => s.allRules[site] !== 'allow' && s.allRules[site] !== 'deny');
    const signedOut = (Array.isArray(control().login_signed_out) ? control().login_signed_out : []).filter(site => allowed.includes(site));
    const reached = ids.filter(id => !loginOlder(id));
    const unknown = commandArgs.includes('--dry-run') ? [] : (Array.isArray(control().login_sync_unknown) ? control().login_sync_unknown : []).filter(valid);
    if (!commandArgs.includes('--dry-run')) for (const site of allowed) s.allRules[site] = 'allow';
    return ok({sites:allowed.length, computers:reached.length, signed_out:signedOut, older:ids.filter(loginOlder), delivered:reached.filter(id => !down(id) && !unknown.includes(id)), deferred:reached.filter(down), unknown});
  }
  return fail('INVALID_ARGUMENT', `Unknown command: ${command}.`);
}
function attentionItems() {
  const approvals = Array.isArray(control().approvals) ? control().approvals : [];
  const items = approvals.map((approval, i) => ({approval, ref:'att_' + i}))
    .filter(({approval, ref}) => valid(String(approval?.computer || '')) && !answeredApprovals.has(ref))
    .map(({approval, ref}) => ({computer_id:approval.computer, label:labelOf(approval.computer), ref, kind:'approval', summary:String(approval.summary || ''), at:before(Number(approval.minutes_ago) || 0),
      details:approval.details && typeof approval.details === 'object' ? approval.details : null, ...(approval.unreachable === true ? {unreachable:true} : {})}));
  const questions = Array.isArray(control().questions) ? control().questions : [];
  questions.forEach((question, i) => {
    const ref = 'att_q' + i, computer = String(question?.computer || '');
    if (!valid(computer) || answeredApprovals.has(ref)) return;
    items.push({computer_id:computer, label:labelOf(computer), ref, kind:'question', summary:String(question.summary || ''), details:null,
      options:Array.isArray(question.options) ? question.options.map(String) : [], at:before(Number(question.minutes_ago) || 0)});
  });
  items.push(...loginRequests());
  for (const id of computerIds()) {
    const need = needsPerson(id);
    if (need && !down(id)) items.push({computer_id:id, label:labelOf(id), ref:need.fix, kind:'repair', summary:need.message, at:before(7)});
  }
  const unreachable = [...new Set(items.filter(item => item.unreachable).map(item => item.computer_id))];
  return {items, count:items.length, unreachable};
}
// "While you were away": what each computer did, newest last.
function awayView() {
  const since = started - 4 * 3600000;
  if (control().away !== true || awaySeen) return {since, computers:[]};
  const event = (minutes, kind, actor, summary) => ({at:before(minutes), kind, actor, summary});
  const plan = fleet ? {
    'fictional-00':[event(200, 'task_began', 'relay', 'Began “Clean up the release branch”'), event(130, 'step', 'relay', 'Rebased 14 commits onto main'), event(15, 'attention.raised', 'relay', 'Asked for approval to push to main')],
    'fictional-03':[event(170, 'task_finished', 'relay', 'Finished “Build release 4.2”'), event(60, 'auto_resumed', 'ibarad', 'Resumed after restart; nothing needed a person.')],
    'fictional-10':[event(95, 'repair', 'ibarad', 'Reconnected its screen.')],
  } : Object.fromEntries(computerIds().filter(id => /^fictional-/.test(id)).slice(0, 2).map(id => [id,
    [event(170, 'task_finished', 'relay', 'Finished “Rotate the log archive”'), event(60, 'auto_resumed', 'ibarad', 'Resumed after restart; nothing needed a person.')]]));
  const many = Math.min(100, Number(control().away_events) || 0);
  if (fleet && many) plan['fictional-00'] = Array.from({length:many}, (_, i) => event(20 + many - i, 'task_finished', 'relay', `Finished import batch ${i + 1}`));
  return {since, computers:Object.entries(plan).map(([computer_id, events]) => ({computer_id, label:labelOf(computer_id), events}))};
}
// The fleet's live moments: Birch's agent takes a new step every 4 s (a click on most), and Fjord
// finishes a task every 20 s, so the console's current step, click ripples and Done can be seen.
const BIRCH_STEPS = [['Opening Chrome'], ['Clicking Sign Up', 0.72, 0.28], ['Typing in Email', 0.45, 0.52], ['Clicking Send', 0.6, 0.78]];
function liveStep(m) {
  if (m?.name !== 'Birch') return {};
  const tick = Math.floor(Date.now() / 4000), [summary, x, y] = BIRCH_STEPS[tick % BIRCH_STEPS.length], at = new Date(tick * 4000).toISOString();
  return {last_step:{summary, at}, ...(x === undefined ? {} : {last_point:{x, y, at}})};
}
function lastTaskOf(m) {
  if (m?.name !== 'Fjord') return null;
  const tick = Math.floor(Date.now() / 20000);
  return {ref:`task_fjord_${tick}`, title:'Tidy the downloads folder', outcome:'done', finished_at:new Date(tick * 20000).toISOString()};
}
// One computer's answer, as operator-status gives it: the binding, then the result.
function opReply(requestId, command, id, payload) {
  const epoch = currentEpoch(id), endpoint = id + '-endpoint';
  return respond(requestId, command, {computer_id:id,endpoint_id:endpoint,binding_revision:'fictional-revision',controller_epoch:epoch,result:{endpoint_id:endpoint,controller_epoch:epoch,authorization_generation:generation,...payload}});
}
// Per-computer commands that check the epoch the console holds.
const EPOCH_COMMANDS = new Set(['operator-status','operator-tasks','operator-task','operator-artifacts','operator-procedures','operator-procedure','operator-task-extend',
  'operator-task-revoke','operator-procedure-review','operator-artifact-save','operator-access','operator-access-set','operator-access-remove','operator-access-unpair',
  'operator-logs','operator-health','operator-repair','operator-answer-attention','operator-power','operator-settings','operator-windows','operator-window-close','operator-window-move']);
// The reads that replaced the administrator route; scoped_delay_ms slows them.
const SCOPED_READS = new Set(['operator-tasks','operator-task','operator-artifacts','operator-procedures','operator-procedure','operator-access','operator-logs','operator-health','operator-windows']);
const POWER_ACTIONS = ['restart','shutdown','sleep','lock','update_ibara','update_omarchy'];
const FICTIONAL_IBARA = '0.1.0-40';

// Every other command: the envelope the fictional wall gives.
function answer(request) {
  const requestId = String(request.id || '');
  const command = String(request.command || '');
  const commandArgs = Array.isArray(request.args) ? request.args.map(String) : [];
  const option = key => { const i = commandArgs.indexOf(key); return i < 0 ? '' : commandArgs[i + 1]; };
  // Positional arguments: everything that is not a --flag or a flag's value.
  const words = commandArgs.filter((arg, i) => !arg.startsWith('--') && !(i > 0 && commandArgs[i - 1].startsWith('--')));
  const id = option('--computer');
  const endpoint = id + '-endpoint';
  const epoch = currentEpoch(id);
  const rows = Array.from({length:wallCount}, (_, i) => {
    const computer = 'fictional-' + String(i).padStart(2,'0');
    const row = {computer_id:computer,label:labelOf(computer),endpoint_id:computer + '-endpoint',binding_revision:'fictional-revision',authorization_generation:generation,trust_state:'verified'};
    return fleet ? {...row, host:hostOf(computer), user:'riley', wake:wakeOf(computer)} : {...row, wake:wakeOf(computer)};
  });
  const hex = (seed, length) => Array.from({length}, (_, i) => ((seed.charCodeAt(i % seed.length) + i * 7) % 16).toString(16)).join('');
  const fail = (code, message, retrySafe = false) => respond(requestId, command, null, {code, message, retry_safe:retrySafe}, 'failed');
  const settled = reply => reply.error ? fail(reply.error.code, reply.error.message) : respond(requestId, command, reply.data);
  // Fictional human files: sends hash the chosen local file and report a verified receipt without
  // writing anywhere; receives fail, because fictional computers hold no real bytes.
  const fleetFiles = () => {
    const identity = {computer_id:id,endpoint_id:endpoint,binding_revision:'fictional-revision',controller_epoch:epoch};
    const result = extra => ({...identity,result:{endpoint_id:endpoint,controller_epoch:epoch,authorization_generation:generation,...extra}});
    if (option('--epoch') !== epoch) return refused(requestId, command, 'PERMISSION_DENIED: Target binding changed.');
    if (command === 'operator-files' && option('--op') === 'files_roots') return respond(requestId, command, result({roots:[{root_id:'documents'},{root_id:'shared'}]}));
    if (command === 'operator-files' && option('--op') === 'files_list') return respond(requestId, command, result({root_id:option('--root'),entries:[
      {name:'Reports',kind:'directory',size:null,modified:ago(60)},
      {name:'Quarterly budget.xlsx',kind:'file',size:48213,modified:ago(35)},
      {name:'Install guide.md',kind:'file',size:12877,modified:ago(14)},
      {name:'Release notes 4.2.pdf',kind:'file',size:1843200,modified:ago(6)},
      {name:'icon-set.zip',kind:'file',size:7340032,modified:ago(2)}]}));
    if (command === 'operator-file-send') {
      const sent = path.basename(option('--remote') || option('--local'));
      if (Array.isArray(control().send_fails) && control().send_fails.includes(sent)) return respond(requestId, command, null, {code:'DESTINATION_EXISTS',message:`${sent} is already in the shared folder on ${labelOf(id)}. Rename it and send it again.`,retry_safe:false}, 'failed');
      const bytes = fs.readFileSync(option('--local'));
      return respond(requestId, command, {...identity,root_id:option('--root'),remote_path:option('--remote'),job_id:'operator_file_fictional_' + hex(option('--remote'), 12),state:'verified',size_bytes:bytes.length,sha256:crypto.createHash('sha256').update(bytes).digest('hex')});
    }
    if (command === 'operator-file-receive') return respond(requestId, command, null, {code:'DEVELOPMENT_ONLY',message:'Fictional computers hold no real files. Nothing was saved.',retry_safe:true}, 'failed');
    return respond(requestId, command, null);
  };
  if (command === 'remove-computer') {
    const row = rows.concat([...added.values()]).find(r => r.computer_id === id);
    if (!row || removedIds.has(id)) return respond(requestId, command, null, {code:'REMOVE_REFUSED', message:'No computer has that ID.', retry_safe:false}, 'failed');
    removedIds.add(id);
    for (const [node, a] of added) if (a.computer_id === id) added.delete(node);
    return respond(requestId, command, {removed:{computer_id:id, label:labelOf(id)}});
  }
  const loginReply = loginCommand(requestId, command, option, commandArgs);
  if (loginReply) return loginReply;
  if (command === 'directory') return respond(requestId, command, {computers:rows.filter(r => !removedIds.has(r.computer_id)).concat([...added.values()].map(row => ({...row, label:labelOf(row.computer_id), wake:wakeOf(row.computer_id)})))});
  // The console keeps no station; its computers are its directory.
  if (command === 'status') return respond(requestId, command, {station_configured:false});
  if (command === 'settings') return settled(settingsReply(consoleSettings(), 'console', 'console', words, option('--section')));
  if (command === 'fleet-attention') return respond(requestId, command, attentionItems());
  if (command === 'away') return respond(requestId, command, awayView());
  if (command === 'away-seen') { awaySeen = true; return respond(requestId, command, {state:'seen'}); }
  if (command === 'theme-fleet') return respond(requestId, command, {theme:'Tokyo Night', results:computerIds().map(computer => {
    const label = labelOf(computer);
    if (down(computer)) return {computer_id:computer, label, state:'offline', message:`${label} is offline. It gets the theme next time you apply it.`};
    if (listed('theme_failed', computer)) return {computer_id:computer, label, state:'failed', message:`${label} doesn't let you change its theme.`};
    return {computer_id:computer, label, state:'applied', message:''};
  })});
  if (command === 'wake') {
    const target = words[0] || id;
    if (!valid(target)) return respond(requestId, command, null);
    if (!wakeOf(target)) return fail('NO_WAKE_ROUTE', `No computer on ${labelOf(target)}'s network is on to wake it.`, true);
    setTimeout(() => {
      if (down(target)) booted.set(target, Date.now());
      powerOff.delete(target);
      if (member(target)?.offline) woken.add(target);
    }, 8000);
    const via = String(control().wake_via || '');
    return respond(requestId, command, {sent_via:via ? (valid(via) ? labelOf(via) : via) : 'this computer', state:'sent', sure:control().wake_sure !== false});
  }
  const firstRunReply = firstRun(requestId, command, commandArgs);
  if (firstRunReply) return firstRunReply;
  // Like ibarad: delete every frame file the shell no longer names.
  if (command === 'preview-release') {
    const keep = new Set(commandArgs);
    let names = [];
    try { names = fs.readdirSync(previews); } catch {}
    for (const name of names) if (!keep.has(name)) try { fs.unlinkSync(path.join(previews, name)); } catch {}
    return respond(requestId, command, {released:true});
  }
  // Live Video: fictional computers have no screen to stream.
  if (command === 'video') {
    if (!valid(words[1] || '')) return respond(requestId, command, null);
    if (words[0] === 'close') return respond(requestId, command, {closed:true});
    return respond(requestId, command, {unsupported:"Fictional computers have no screen to stream."});
  }
  if (!valid(id)) return respond(requestId, command, null);
  if (command === 'open-terminal') return respond(requestId, command, {opened:true});
  if (command === 'open-viewer') {
    if (!controls.has(id)) return fail('HUMAN_CONTROL', 'Choose Take Control first.');
    return respond(requestId, command, {computer_id:id, viewer_started:true, viewer_pid:openViewer(id)});
  }
  // A computer that is off answers nothing, from the first request on: no session, status or picture.
  if (down(id) && command.startsWith('operator-')) return refused(requestId, command, 'Selected operator transport failed.');
  if (listed('revoked', id) && command.startsWith('operator-')) return refused(requestId, command, revokedMessage);
  if (listed('session_refused', id) && command === 'operator-session') return refused(requestId, command, 'PERMISSION_DENIED: Operator grant unavailable.');
  if (command === 'operator-session') return opReply(requestId, command, id, {});
  const pauseOp = command === 'operator-control' && ['pause','resume'].includes(option('--op'));
  const controlOp = fleet && command === 'operator-control' && ['take_control','handback'].includes(option('--op'));
  if ((EPOCH_COMMANDS.has(command) || pauseOp || controlOp) && option('--epoch') !== epoch) return refused(requestId, command, 'PERMISSION_DENIED: Target binding changed.');
  const name = labelOf(id);
  if (command === 'operator-status') {
    const denied = listed('deny', id), m = member(id), owner = ownerOf(id), pause = pauseOf(id);
    const activeTask = !denied && m?.task && !cancelledTasks.has(taskRefOf(id)) ? {task_ref:taskRefOf(id),title:m.task,principal:m.agent,state:m.state || 'active',started_at:ago(m.minutes),...liveStep(m)} : null;
    const displayCount = Number.isInteger(control().displays?.[id]) ? control().displays[id] : 1;
    const outputs = denied ? [] : Array.from({length:displayCount}, (_, n) => ({display_id:n ? `fictional-display-${n + 1}` : 'fictional-display',display_revision:'fictional-1',label:n ? `Fictional screen ${n + 1}` : 'Fictional screen'}));
    return opReply(requestId, command, id, {observation:denied?'denied':'available_if_desktop_ready',active_task_ref:activeTask?activeTask.task_ref:null,active_task:activeTask,outputs,files:fleet?'available_if_root_approved':'denied',owner,ownership_revision:ownershipRevision(id, owner),interactive_control:fleet?'available_if_exclusive':'unsupported_without_verified_viewer_adapter',holds_control:controls.has(id),locked:lockedNow(id),
      paused:!!pause,pause_origin:pause,system_wait:pause === 'system' ? (listed('resume_off', id) ? 'resume_off' : 'starting') : null,
      repair:repairView(id),wake:wakeOf(id),disk_password:listed('disk_password', id),last_task:denied ? null : lastTaskOf(m),omarchy_update:omarchyOf(id),
      video:listed('video_incapable', id) ? {capable:false,reason:"This computer can't stream video efficiently."} : {capable:true,reason:null}});
  }
  if (pauseOp) {
    const controlError = String(control().control_error?.[id] || '');
    if (controlError) return fail(controlError.split(':')[0].trim(), controlError, true);
    pauses.set(id, option('--op') === 'pause' ? 'person' : null);
    ownershipChanges.set(id, (ownershipChanges.get(id) || 0) + 1);
    const owner = ownerOf(id);
    return opReply(requestId, command, id, {paused:!!pauseOf(id), pause_origin:pauseOf(id), availability:'ready', owner, ownership_revision:ownershipRevision(id, owner)});
  }
  // Take Control and Hand Back on fleet15: the owner changes as on a real computer, and a stand-in
  // viewer process opens (see openViewer); Hand Back ends it.
  if (controlOp) {
    if (option('--op') === 'take_control') { controls.add(id); if (lockedNow(id) && !unlockAt.has(id)) unlockAt.set(id, Date.now() + 8000); }
    else { controls.delete(id); closeViewer(id); }
    ownershipChanges.set(id, (ownershipChanges.get(id) || 0) + 1);
    const owner = ownerOf(id);
    const reply = opReply(requestId, command, id, {owner, ownership_revision:ownershipRevision(id, owner), viewer_ready:true, pause_origin:pauseOf(id)});
    if (option('--op') === 'take_control') Object.assign(reply.data, {viewer_started:true, viewer_pid:openViewer(id)});
    return reply;
  }
  const task = ref => tasksOf(id).find(item => item.task_ref === ref);
  if (command === 'operator-tasks') return opReply(requestId, command, id, {tasks:tasksOf(id)});
  if (['operator-task','operator-task-extend','operator-task-revoke'].includes(command)) {
    const found = task(option('--task'));
    if (!found) return refused(requestId, command, 'NOT_FOUND: That task is gone.');
    if (command === 'operator-task') return opReply(requestId, command, id, {task:found, live:!!found.live, receipts:receiptsOf(found)});
    if (command === 'operator-task-extend') return opReply(requestId, command, id, {task_ref:found.task_ref, state:'active'});
    cancelledTasks.add(found.task_ref);
    return opReply(requestId, command, id, {task_ref:found.task_ref, state:'cancelled'});
  }
  // fleet15's Nimbus runs an ibara from before window management, which doesn't know these.
  if (['operator-windows','operator-window-close','operator-window-move'].includes(command)) {
    if (member(id)?.older_ibara) return fail('INVALID_ARGUMENT', 'Unknown operator operation.', true);
    if (command === 'operator-windows') return opReply(requestId, command, id, windowsView(id));
    const address = option('--address'), pid = option('--pid'), workspace = option('--workspace');
    if (!/^0x[0-9a-f]{1,16}$/.test(address || '') || !/^[1-9][0-9]*$/.test(pid || '')) return fail('INVALID_ARGUMENT', 'Name the window by its address and process ID.', true);
    if (command === 'operator-window-move' && !/^([1-9]|10)$/.test(workspace || '')) return fail('INVALID_ARGUMENT', 'Choose a workspace from 1 to 10.', true);
    const windows = windowsOf(id), w = windows.find(x => x.address === address && x.pid === Number(pid));
    if (!w) return fail('NOT_FOUND', 'That window is gone.', true);
    if (w.task && liveTaskOf(id)?.task_ref === w.task) { w.claimed = true; return fail('BUSY', 'An agent is using this window. Stop its task first.', true); }
    if (command === 'operator-window-move') { w.workspace = Number(workspace); return opReply(requestId, command, id, {moved:true, workspace:w.workspace}); }
    if (w.asks_to_save) return opReply(requestId, command, id, {closed:false});
    windows.splice(windows.indexOf(w), 1);
    return opReply(requestId, command, id, {closed:true});
  }
  if (command === 'operator-artifacts') return opReply(requestId, command, id, {items:[], next_cursor:null, total:0});
  if (command === 'operator-artifact-save') return respond(requestId, command, null, {code:'DEVELOPMENT_ONLY',message:'Fictional computers hold no real results. Nothing was saved.',retry_safe:true}, 'failed');
  if (command === 'operator-procedures') return opReply(requestId, command, id, {items:[]});
  if (command === 'operator-procedure') return refused(requestId, command, 'NOT_FOUND: That procedure is gone.');
  if (command === 'operator-procedure-review') {
    const decision = option('--decision');
    if (decision !== 'approve' && decision !== 'quarantine') return fail('INVALID_ARGUMENT', 'Choose approve or quarantine.');
    return opReply(requestId, command, id, {procedure_ref:option('--ref'), state:decision === 'approve' ? 'approved' : 'quarantined'});
  }
  if (command === 'operator-access') return opReply(requestId, command, id, accessView(accessModel(id), id));
  if (['operator-access-set','operator-access-remove','operator-access-unpair'].includes(command)) {
    let body = {};
    try { body = JSON.parse(words[0] || '{}'); } catch {}
    const model = accessModel(id), error = changeAccess(model, command.slice('operator-'.length), body);
    return error ? respond(requestId, command, null, error, 'failed') : opReply(requestId, command, id, accessView(model, id));
  }
  if (command === 'operator-logs') {
    const which = option('--which') || 'ibara', count = Number(option('--lines'));
    if (which !== 'ibara' && which !== 'viewer') return fail('INVALID_ARGUMENT', 'Choose the ibara or viewer log.');
    const clock = minutes => new Date(Date.now() - minutes * 60000).toTimeString().slice(0, 8);
    const lines = which === 'viewer'
      ? [`${clock(42)} viewer: screen sharing started on ${name}`, `${clock(42)} viewer: sharing 1920 × 1080 at 30 frames a second`, `${clock(9)} viewer: riley connected`, `${clock(3)} viewer: riley disconnected`]
      : [`${clock(44)} ibara: ${name} started with 1 screen`, `${clock(30)} ibara: relay connected`, `${clock(12)} ibara: preview served`, `${clock(1)} ibara: preview served`];
    return opReply(requestId, command, id, {lines:count > 0 ? lines.slice(-count) : lines});
  }
  if (command === 'operator-health') return opReply(requestId, command, id, {load:0.42, cpus:8, memory:{used_mb:6500,total_mb:15900}, disk:{used_gb:300,total_gb:512},
    uptime_s:Math.max(0, Math.round((Date.now() - (booted.get(id) ?? started - 3 * 86400000)) / 1000)), repair:repairView(id), wake:wakeOf(id), disk_password:listed('disk_password', id)});
  if (command === 'operator-repair') {
    const fix = words[0], f = FIXES[fix];
    if (!f) return fail('INVALID_ARGUMENT', "That isn't a repair ibara knows.");
    if (listed('repair_fails', id)) return opReply(requestId, command, id, {state:'still_broken', message:f.broken(name)});
    // Restarting ibara always happens; its controller answers as a new epoch 3 s later.
    if (fix === 'restart_ibara') {
      if (needsPerson(id)?.fix === fix) { fixed.add(id); lastRepair.set(id, {at:ago(0), summary:f.done}); }
      setTimeout(() => ibaraRestarts.set(id, (ibaraRestarts.get(id) || 0) + 1), 3000);
      return opReply(requestId, command, id, {state:'restarting', message:`${name} is restarting ibara. It's back in a few seconds.`});
    }
    if (needsPerson(id)?.fix !== fix) return opReply(requestId, command, id, {state:'fixed', message:`Nothing needed fixing on ${name}.`});
    fixed.add(id);
    lastRepair.set(id, {at:ago(0), summary:f.done});
    return opReply(requestId, command, id, {state:'fixed', message:`${name} is working again.`});
  }
  if (command === 'operator-answer-attention' && /^att_q[0-9]+$/.test(words[0] || '')) {
    const ref = words[0], question = (control().questions || [])[Number(ref.slice(5))], given = option('--answer');
    if (control().questions_old === true || (!commandArgs.includes('--dismiss') && !given)) return fail('INVALID_ARGUMENT', 'Choose approve or deny.');
    if (!question || question.computer !== id || answeredApprovals.has(ref)) return fail('NOT_FOUND', 'That question was already answered.');
    if (given && Array.isArray(question.options) && question.options.length && !question.options.includes(given)) return fail('INVALID_ARGUMENT', "The answer must be one of the item's options.");
    answeredApprovals.add(ref);
    return opReply(requestId, command, id, {item:{att_ref:ref, kind:'question', state:given ? 'answered' : 'expired', answer:given || null}});
  }
  if (command === 'operator-answer-attention') {
    const [ref, decision] = words, index = /^att_([0-9]+)$/.exec(ref || '')?.[1];
    if (!['approve', 'deny', 'always'].includes(decision)) return fail('INVALID_ARGUMENT', 'Choose approve, deny or always.');
    const approval = Array.isArray(control().approvals) && index !== undefined ? control().approvals[Number(index)] : null;
    if (!approval || approval.computer !== id || answeredApprovals.has(ref)) return fail('NOT_FOUND', 'That request was already answered.');
    const details = approval.details && typeof approval.details === 'object' ? approval.details : {};
    const stopAsking = details.request?.op === 'stop_asking';
    const kinds = decision === 'always' && !stopAsking && ASKED.includes(details.effect) ? [details.effect]
      : decision === 'approve' && stopAsking ? ASKED.filter(kind => (details.request.kinds || []).includes(kind)) : [];
    if (decision === 'always' && !kinds.length) return fail('INVALID_ARGUMENT', "Always Allow is only for an agent's step that sends, spends or deletes.");
    const agent = String(details.agent || '');
    if (decision === 'always' && computerAsks(accessModel(id), agent, kinds[0])) {
      const computer = agent.split('@')[1] || agent, verb = {send:'send', spend:'spend', destructive:'delete'}[kinds[0]];
      return fail('INVALID_ARGUMENT', `${computer}'s agents ask before they ${verb} here, and any agent on ${computer} can use the name ${agent.split('@')[0]}. Approve this step, or change ${computer}'s rule in Access.`);
    }
    answeredApprovals.add(ref);
    if (kinds.length) allowWithoutAsking(id, String(details.agent), kinds);
    return opReply(requestId, command, id, {ref, state:decision === 'deny' ? 'denied' : 'approved', ...(kinds.length ? {allowed:{agent:details.agent, kinds}} : {})});
  }
  if (command === 'operator-power') {
    const action = option('--action');
    // fleet15's Nimbus runs an ibara from before Update ibara, which refuses both updates as older ones do.
    if (!POWER_ACTIONS.includes(action) || (['update_ibara','update_omarchy'].includes(action) && member(id)?.older_ibara)) return fail('INVALID_ARGUMENT', 'Choose restart, shutdown, sleep, lock or update.');
    if (listed('power_denied', id)) return refused(requestId, command, "OPERATOR_REFUSED: This computer doesn't let you do that. Its owner can allow it in Access.");
    if (action === 'restart') { powerOff.set(id, Date.now() + 12000); booted.set(id, Date.now() + 12000); if (omarchyRuns.has(id)) omarchyRuns.get(id).restart = false; }
    if (action === 'shutdown' || action === 'sleep') powerOff.set(id, Infinity);
    const omarchyRunning = omarchyOf(id)?.state === 'running';
    if (action === 'update_ibara') return opReply(requestId, command, id, omarchyRunning
      ? {action, state:'running', message:"Omarchy is updating on this computer; ibara updates once it's done."}
      : member(id)?.ibara_current
      ? {action, state:'current', message:`ibara is already up to date (${FICTIONAL_IBARA}).`}
      : {action, state:'started', message:'ibara is updating to the latest release. It may restart its bar when it finishes.'});
    if (action === 'update_omarchy') {
      if (omarchyRunning) return opReply(requestId, command, id, {action, state:'running', message:'Omarchy is already updating on this computer.'});
      omarchyRuns.set(id, {started:Date.now(), ends:Date.now() + 15000, outcome:'done', restart:true});
      return opReply(requestId, command, id, {action, state:'started', message:'Omarchy is updating. The computer stays usable; it says when a restart is needed.'});
    }
    return opReply(requestId, command, id, {action, state:'started', ...(action === 'restart' && listed('disk_password', id) ? {disk_password_warning:true} : {})});
  }
  if (command === 'operator-settings') {
    const reply = settingsReply(computerSettings(id), id, 'computer', words, option('--section'));
    return reply.error ? fail(reply.error.code, reply.error.message) : opReply(requestId, command, id, reply.data);
  }
  if (fleet && ['operator-files','operator-file-send','operator-file-receive'].includes(command)) return fleetFiles();
  return respond(requestId, command, null);
}
// Commands that answer later, as they take a moment on a real computer (computed then, too).
const EVERYDAY_DELAY_MS = {'operator-power':800, 'operator-repair':2500, 'theme-fleet':2500, wake:1200, 'operator-answer-attention':400, 'operator-control':600, 'operator-file-send':1500};
function delayOf(command, args) {
  if ((command === 'settings' || command === 'operator-settings') && (args.includes('set') || args.includes('reset'))) return 150;
  if (SCOPED_READS.has(command)) return Number(control().scoped_delay_ms) || 0;
  // Update ibara on All and Update Omarchy on All send every computer's update at once: they
  // answer one after another, as real computers starting their updates would.
  if (command === 'operator-power' && ['update_ibara','update_omarchy'].includes(args[args.indexOf('--action') + 1])) {
    const id = args[args.indexOf('--computer') + 1] || '';
    return 800 + (Number(id.slice(10)) || 0) * 350;
  }
  return FIRST_RUN_DELAY_MS[command] || EVERYDAY_DELAY_MS[command] || 0;
}

// pick-file and pick-folder: the same request line goes to the real ibarad over its own
// connection, and its one reply for that id comes back unchanged.
function forwardPick(line, request) {
  const id = String(request.id || ''), command = String(request.command || '');
  const failed = (code, message, connection) => ({id, envelope:respond(id, command, null, {code, message, retry_safe:true}, connection)});
  const missing = failed('MISSING_DEPENDENCY', `Choosing a local file needs the real ibarad; nothing answers at ${realSocket || '$XDG_RUNTIME_DIR/ibara/ibarad.sock'}.`, 'missing-dependency');
  if (!realSocket) return Promise.resolve(missing);
  return new Promise(resolve => {
    const real = net.createConnection(realSocket);
    let connected = false, settled = false, buffered = '';
    const finish = message => { if (settled) return; settled = true; real.destroy(); resolve(message); };
    real.setEncoding('utf8');
    real.on('connect', () => { connected = true; real.write(line + '\n'); });
    real.on('data', chunk => {
      buffered += chunk;
      for (let at; (at = buffered.indexOf('\n')) >= 0;) {
        const reply = buffered.slice(0, at);
        buffered = buffered.slice(at + 1);
        let message; try { message = JSON.parse(reply); } catch { continue; }
        if (message && String(message.id) === id) return finish(message);
      }
    });
    const closed = () => finish(connected ? failed('DAEMON_UNAVAILABLE', 'The real ibarad closed the connection before answering the chooser.', 'offline') : missing);
    real.on('error', closed);
    real.on('close', closed);
  });
}

function serveConnection(socket) {
  socket.setEncoding('utf8');
  const sequences = new Map();
  const send = message => { if (socket.writable) socket.write(JSON.stringify(message) + '\n'); };
  const handle = line => {
    let request; try { request = JSON.parse(line); } catch { return; }
    if (!request || typeof request !== 'object') return;
    const command = String(request.command || '');
    if (command === 'operator-observe') return observe(request, sequences, send);
    if (command === 'pick-file' || command === 'pick-folder') return void forwardPick(line, request).then(send);
    const reply = () => {
      let envelope;
      // A one-shot bridge failure (for example an unreadable --local file) became an error exit;
      // here it becomes that request's failure so the daemon keeps serving.
      try { envelope = answer(request); }
      catch (error) { envelope = respond(String(request.id || ''), command, null, {code:'COMMAND_FAILED',message:String(error?.message || error),retry_safe:false}, 'failed'); }
      if (process.env.IBARA_DEV_TRACE && !/^(operator-|preview-)/.test(command)) process.stderr.write(`TRACE ${request.id} ${command} ${JSON.stringify(request.args)} -> ${JSON.stringify(envelope.data)} ${JSON.stringify(envelope.error)}\n`);
      send({id:String(request.id || ''), envelope});
    };
    // A slow command's effect lands when its answer does.
    const delay = delayOf(command, Array.isArray(request.args) ? request.args.map(String) : []);
    if (delay > 0) setTimeout(reply, delay);
    else reply();
  };
  let buffered = '';
  socket.on('data', chunk => {
    buffered += chunk;
    for (let at; (at = buffered.indexOf('\n')) >= 0;) {
      const line = buffered.slice(0, at);
      buffered = buffered.slice(at + 1);
      if (line) handle(line);
    }
  });
  socket.on('error', () => {});
}

// A socket something still answers on is left alone; only a stale one is removed.
const live = await new Promise(resolve => {
  const probe = net.createConnection(socketPath);
  probe.on('connect', () => { probe.destroy(); resolve(true); });
  probe.on('error', () => resolve(false));
});
if (live) { process.stderr.write(`Something already answers on ${socketPath}.\n`); process.exit(1); }
try { if (fs.lstatSync(socketPath).isSocket()) fs.unlinkSync(socketPath); } catch {}
const server = net.createServer(serveConnection);
server.on('error', error => { process.stderr.write(`Could not listen on ${socketPath}: ${error.message}\n`); process.exit(1); });
const mask = process.umask(0o177);
server.listen(socketPath, () => {
  process.umask(mask);
  fs.chmodSync(socketPath, 0o600);
  process.stdout.write(`Fictional ${wallCount}-computer wall listening on ${socketPath}\n`);
});
const stop = () => { server.close(); try { fs.unlinkSync(socketPath); } catch {} process.exit(0); };
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
process.on('exit', () => { for (const id of [...viewers.keys()]) closeViewer(id); });
