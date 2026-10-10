import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const source = fs.readFileSync(path.join(root, 'StatusModel.js'), 'utf8').replace('.pragma library', '');
const context = { console };
vm.createContext(context);
vm.runInContext(source + '\nthis.StatusModel = { clip, titleCase, parseEnvelope, taskTitle, taskRuns, accessDenied, evidenceLines, procedureLines, fleetState, fleetStateLabel, fleetActor, healthLines, fleetCounts, activityLine, decorateSession, isoMs, plainError, awayView, attentionView, approvalNoticeBody, standingToasts, toastLayout, needsYouView, problemSince, lastingProblems, pauseAction, whoCanUse, peopleAndAgents, loginDecisions, loginAnswerWords, loginRejectedFresh, loginRowsView, PROBLEM_AFTER_MS };', context);
const M = context.StatusModel;

test('evidence keeps execution, checks, unknown effects and original delivery separate', () => {
  const lines = M.evidenceLines({ task: { principal: 'vesper', contract_version: '3.0' }, receipts: [{ request_id: 'export-1', execution: 'completed', effect: 'applied', verification: 'unknown' }, { request_id: 'submit-2', execution: 'unknown', effect: 'unknown' }], deliveries: [{ destination_host: 'vesper', path: '/report.pdf', state: 'pending' }] }).join('\n');
  assert.match(lines, /execution completed; effect applied; verification unknown/);
  assert.match(lines, /Don't run it again/);
  assert.match(lines, /vesper \/ \/report.pdf · pending/);
  assert.match(M.evidenceLines({ task: { completion_outcome: 'complete' } }).join('\n'), /Legacy \/ unverified/);
});

test('current controller projection uses original host identity and does not upgrade historical completion', () => {
  const text = M.evidenceLines({ task: { contract_version: '3.0', completion_outcome: 'complete' }, completion_projection: { criteria: { required: 2, satisfied: 1 }, delivery: { required: 1, verified: 0 }, unresolved_request_refs: ['req-unknown'], cleanup: 'unsettled', verified_complete: false }, deliveries: [{ host_id: 'vesper-id', destination_path: '/home/riley/report.pdf', revision: 2, state: 'pending' }] }).join('\n');
  assert.match(text, /Desktop outcome: complete/);
  assert.match(text, /Current verified completion: incomplete/);
  assert.match(text, /Required criteria: 1 \/ 2/);
  assert.match(text, /vesper-id \/ \/home\/riley\/report.pdf · pending · revision 2/);
  assert.match(text, /Cleanup: unsettled/);
  assert.match(text, /Unresolved requests: req-unknown/);
});

test('task text stays a plain clipped summary', () => {
  assert.equal(M.taskTitle({ goal: '<b>no html</b>' }).includes('<b>'), true);
});

// Folding could hide a failed/unknown job between runs, join runs across it, lose the
// first receipt's identity on refresh, or swallow the latest step of a live task.
test('command folds stop at failed and unknown jobs and leave the newest live step visible', () => {
  const run = (id, job_state = 'completed') => ({ operation_ref: id, tool: 'computer_exec',
    summary: 'run ["printf","hello"]', execution: 'completed', effect: 'local_change', job_state });
  const old = [run('a'), run('b'), run('c'), run('failed', 'failed'), run('unknown', 'unknown'), run('d'), run('e'), run('f')];
  const groups = M.taskRuns({ receipts: old.toReversed() });
  assert.deepEqual(Array.from(groups, g => [g.key, g.fold, g.items.length]), [['a', true, 3], ['failed', false, 1], ['unknown', false, 1], ['d', true, 3]]);
  const fresh = M.taskRuns({ receipts: [...old, run('g')].toReversed() }, true);
  assert.deepEqual(Array.from(fresh, g => [g.key, g.fold, g.items.length]), [['a', true, 3], ['failed', false, 1], ['unknown', false, 1], ['d', true, 3], ['g', false, 1]]);
});

// What an approval's card and notification show. Failure cases:
// 1. An approval an older computer words as code ("… step {"action": …} · target {"window":
//    {"address": "0x6018…", "pid": 3015645 …") reaches the card or the notification as is.
// 2. A plain approval loses its words.
// 3. Details show the request as code (JSON, a window address, a process id, a tool name), or
//    leave out what it does, where, the page or the window's title.
// 4. Copy Request copies a shortened or reworded request, so it no longer is what the computer sent.
// 5. A request to stop asking or to take control reads as a key press.
const approvalItem = (fields) => ({ items: [{ computer_id: 'computer_1', label: 'Vesper', ref: 'att_1', kind: 'approval', at: '2026-09-28T10:00:00Z', ...fields }] });
const machineWords = /[{}]|0x[0-9a-f]{4,}|\bpid\b|3015645|computer_act/i;
const factLines = item => Array.from(item.facts, f => f.label + ': ' + f.value);

test('an approval worded as code reads as plain words, and Copy Request copies its text', () => {
  const code = 'Approve computer_act send step: key Return in chromium · step {"action":{"kind":"key","keys":"Return"},"effect":"send"} · target {"key":"Return","window":{"address":"0x6018580abef0","pid":3015645,"class":"chromium"}}?';
  const [item] = M.attentionView(approvalItem({ summary: code }));
  assert.doesNotMatch(item.summary, machineWords);
  assert.doesNotMatch(M.approvalNoticeBody(item), machineWords);
  assert.match(M.approvalNoticeBody(item), /Open the ibara console/);
  assert.deepEqual(factLines(item), []);
  assert.equal(item.request, code);
});

test('Details say what, who, where, the page and the window in words; Copy Request copies the request whole', () => {
  const summary = 'codex@vesper wants to press Return in Chromium on localhost:8080/signup, which sends something. For the task “Sign up”.';
  const details = { agent: 'codex@vesper', task: { task_ref: 'task_7c1e0a', goal: 'Sign up' }, tool: 'computer_act', effect: 'send',
    request: { step: { action: { kind: 'key', keys: 'Return' }, effect: 'send' }, target: { key: 'Return', window: { address: '0x5f3a90c1d2e0', pid: 48213, class: 'chromium' } }, note: 'x'.repeat(5000) },
    where: { app: 'Chromium', page: 'localhost:8080/signup', window_title: 'Sign up' } };
  const [item] = M.attentionView(approvalItem({ summary, details }));
  assert.equal(item.summary, summary);
  assert.deepEqual(factLines(item), ['Who: codex@vesper', 'What: Press Return, which sends something', 'Where: Chromium on Vesper',
    'Page: localhost:8080/signup', 'Window title: Sign up', 'Task: Sign up']);
  assert.deepEqual(JSON.parse(item.request), details);

  const [stop] = M.attentionView(approvalItem({ summary: 'codex@vesper asks to stop asking you.', details: { agent: 'codex@vesper', request: { op: 'stop_asking', kinds: ['send', 'spend', 'destructive'] } } }));
  assert.deepEqual(factLines(stop), ['Who: codex@vesper', 'What: Stop asking you before it sends, spends money or deletes', 'Where: Vesper']);
  const [control] = M.attentionView(approvalItem({ summary: 'studio asks to take control.', details: { capability: 'control', request: { op: 'take_control' } } }));
  assert.deepEqual(factLines(control), ['What: Take control', 'Where: Vesper']);
});

// Which toasts last as long as their condition. Failure cases:
// 1. An approval, an agent's question or a request to use this computer is missing on some page
//    (a computer's page of another computer, Add Computer), so an answer can only be given from
//    the fleet, or not at all (Tyler's bar counted a question the console never showed).
// 2. Whatever makes the bar red (a computer that needs a person, one offline or needing
//    attention) has no toast on some page, so the bar shows an alarm the console can't explain.
// 3. A failed send or theme run is not an error, so it leaves on its own before it is read.
// 4. The Connect an Agent pointer shows after the first task, after it was dismissed, off the
//    fleet, or before ibara has given the prompt.
// 5. While ibara isn't running here, computers' problem toasts or answers nobody can send still
//    stand beside the one toast that says so.
test('standing toasts follow their conditions, what waits for an answer first on every page', () => {
  const keys = state => Array.from(M.standingToasts(state), t => t.key);
  const approvals = [{ ref: 'att_1', computer_id: 'a' }, { ref: 'att_2', computer_id: 'b' }];
  const questions = [{ ref: 'att_q', computer_id: 'b', options: ['Yes', 'No'] }];
  const pairRequests = [{ request_id: 'pair_in_bench' }];
  const needs = [{ computer_id: 'c', message: 'Screen sharing on Lumen stopped.', fix: 'restart_viewer' }];
  const problems = [{ computer_id: 'd', heading: 'Kiln isn\'t answering' }];
  for (const route of ['fleet', 'computer', 'add', 'settings'])
    assert.deepEqual(keys({ route, computerId: route === 'computer' ? 'c' : '', approvals, questions, pairRequests, needs, problems }),
      ['approval:att_1', 'approval:att_2', 'question:att_q', 'pair:pair_in_bench', 'need:c', 'problem:d'], route);
  assert.deepEqual(Array.from(M.standingToasts({ route: 'fleet', approvals, questions, needs, problems }), t => t.tone), ['approval', 'approval', 'approval', 'error', 'error']);

  const failedDrop = { files: [{ name: 'a.txt' }], state: 'done', sent: 0, failed: [{ name: 'a.txt', message: 'No room.' }] };
  assert.deepEqual({ ...M.standingToasts({ route: 'computer', computerId: 'c', drop: failedDrop })[0] }, { key: 'drop:c', kind: 'drop', ref: 'c', tone: 'error' });
  assert.equal(M.standingToasts({ route: 'computer', computerId: 'c', drop: { ...failedDrop, state: 'sending', failed: [] } })[0].tone, 'note');
  assert.equal(M.standingToasts({ themeRun: { state: 'done', results: [{ state: 'applied' }, { state: 'failed' }] } })[0].tone, 'error');

  const connect = { route: 'fleet', connectPrompt: 'Connect yourself to ibara…' };
  assert.deepEqual(keys(connect), ['connect']);
  assert.deepEqual(keys({ ...connect, firstTaskDone: true }), []);
  assert.deepEqual(keys({ ...connect, connectHintDone: true }), []);
  assert.deepEqual(keys({ ...connect, route: 'computer', computerId: 'c' }), []);
  assert.deepEqual(keys({ ...connect, connectPrompt: '' }), []);
  assert.deepEqual(keys({ route: 'fleet', awayCount: 2, awayHidden: true }), []);
  assert.deepEqual(keys({ route: 'fleet', approvals, questions, pairRequests, needs, problems, awayCount: 2, serviceStopped: true }), ['stopped']);
});

// Which toasts show while the stack is closed. Failure cases:
// 1. An approval, a question, a request to use this computer or "ibara isn't running" waits
//    behind "+N more" while a note shows (Tyler's tour: "+1 more" hid a take-control approval).
// 2. An older note shows while a newer one is hidden.
// 3. More notes show than there is room for, or fewer while room is left.
// 4. "+N more" counts something that shows, or misses something hidden.
test('what waits for an answer always shows; notes collapse first, oldest first', () => {
  const layout = (kinds, room) => { const l = M.toastLayout(kinds, room); return [Array.from(l.shown, s => s ? 1 : 0).join(''), l.hidden]; };
  assert.deepEqual(layout(['approval', 'question', 'approval', 'message', 'message', 'theme', 'message'], 5), ['1110011', 2]);
  assert.deepEqual(layout(['message', 'approval', 'message', 'pair', 'message', 'message'], 3), ['010101', 3]);
  assert.deepEqual(layout(['approval', 'approval', 'question', 'approval'], 2), ['1111', 0]);
  assert.deepEqual(layout(['message', 'away', 'stopped'], 2), ['011', 1]);
  assert.deepEqual(layout(['message', 'message'], 5), ['11', 0]);
});

test('procedure inspection retains actual controller applicability and evidence warnings', () => {
  const record = {
    kind: 'procedure', procedure_ref: 'procedure_export', status: 'approved',
    definition: { title: 'Export report', steps: ['Use a freshly observed Export control.'] },
    content_sha256: 'a'.repeat(64), applicability_state: 'revalidation_required',
    applicability_reasons: ['Tested controller version differs.', 'Supporting evidence expired.'],
    fresh_verification_required: true, expiry_state: 'expired',
    contradiction_state: 'declared', evidence_redacted: true,
  };
  const text = M.procedureLines(record).join('\n');
  assert.match(text, /Applicability: revalidation_required/);
  assert.match(text, /Tested controller version differs/);
  assert.match(text, /Supporting evidence expired/);
  assert.match(text, /Evidence expiry: expired/);
  assert.match(text, /Contradictions: declared/);
  assert.match(text, /Fresh verification: required/);
  assert.match(text, /Supporting evidence: redacted/);
  assert.match(text, new RegExp('Revision: ' + 'a'.repeat(64)));
  assert.match(M.procedureLines({ ...record, applicability_state: 'prerequisite_failed', applicability_reasons: ['Required capability is missing.'] }).join('\n'), /Applicability: prerequisite_failed\nRequired capability is missing/);
  assert.match(M.procedureLines({ ...record, status: 'quarantined', applicability_state: 'quarantined' }).join('\n'), /Applicability: quarantined/);
});

test('authoritative criterion detail separates current state, stored claims, checks and assessor', () => {
  const detail = { task: { contract_version: '3.0' }, criteria: [
    { id: 'saved', description: 'Document is saved', required: true, policy: { kind: 'deterministic', max_age_seconds: 30 }, state: 'unverified', claims: [{ criterion_id: 'saved', claim: 'met', evidence_refs: ['check_old'] }], evidence: [{ ref: 'check_old', kind: 'check', expired: true, source: 'native', outcome: 'satisfied', checked_at: '2026-09-20T10:00:00Z', summary: 'Saved label visible' }] },
    { id: 'quality', description: 'Report is clear', required: true, policy: { kind: 'assessment', assessor: 'hazel' }, state: 'satisfied', claims: [{ criterion_id: 'quality', claim: 'met', evidence_refs: ['assessment_1'], assessment: { assessor: 'hazel', reason: 'The cited report addresses every requested topic.' } }], evidence: [{ ref: 'assessment_1', kind: 'observation', expired: false, source: 'browser', outcome: null, checked_at: null, summary: 'Report inspected' }] },
    { id: 'signoff', description: 'Operator assessment', required: false, policy: { kind: 'assessment', assessor: 'operator' }, state: 'unverified', claims: [{ criterion_id: 'signoff', claim: 'unknown', evidence_refs: ['human_1'], assessment: { assessor: 'operator', reason: 'Review is incomplete.' } }], evidence: [{ ref: 'human_1', kind: 'check', expired: false, source: 'human_assessment', outcome: 'satisfied', checked_at: null, summary: 'Historical human assessment' }] },
  ] };
  const text = M.evidenceLines(detail).join('\n');
  assert.match(text, /Criterion saved: unverified · required/);
  assert.match(text, /Proof policy: deterministic/);
  assert.match(text, /Freshness limit: 30 seconds/);
  assert.match(text, /Stored claim: met · evidence: check_old/);
  assert.match(text, /Check check_old: outcome satisfied · expired/);
  assert.match(text, /Assessment by hazel: The cited report addresses every requested topic/);
  assert.match(text, /Assessment by operator: Review is incomplete/);
  assert.match(text, /Assessment evidence human_1: human_assessment/);
  assert.doesNotMatch(text, /Check human_1/);
  assert.match(text, /Evidence assessment_1: observation · retained/);
  assert.doesNotMatch(text, /Check assessment_1/);
  assert.match(M.evidenceLines({ task: { contract_version: '2.0' }, criteria: [{ id: 'old', state: 'unverified', policy: null, claims: [], evidence: [] }] }).join('\n'), /Proof policy: unavailable/);
});

const live = (extra = {}) => ({ trust_state: 'verified', connection: 'ready', owner_name: 'none', frame: { url: 'x' }, frame_error: '', ...extra });

test('fleet state: a refusal or a stuck task outranks whoever holds the computer', () => {
  assert.equal(M.fleetState(live({ connection: 'unauthorized', owner_name: 'agent:vesper:task_1' })), 'attention');
  assert.equal(M.fleetState(live({ observation: 'denied', owner_name: 'operator:riley' })), 'attention');
  assert.equal(M.fleetState(live({ trust_state: 'pending', connection: 'unverified' })), 'attention');
  assert.equal(M.fleetState(live({ owner_name: 'agent:vesper:task_1', active_task: { state: 'waiting_for_human' } })), 'attention');
  assert.equal(M.fleetState(live({ owner_name: 'agent:vesper:task_1', active_task: { state: 'interrupted' } })), 'attention');
  // A capture refusal with no picture needs a look; routine waits do not.
  assert.equal(M.fleetState(live({ frame: null, frame_error: 'CAPABILITY_UNAVAILABLE: Desktop session locked or unavailable.' })), 'attention');
  assert.equal(M.fleetState(live({ frame: null, frame_error: 'Preview pending' })), 'ready');
  assert.equal(M.fleetState(live({ frame: null, frame_error: 'Not previewed: at most 20 computers preview at once. Scroll or filter to see this one.' })), 'ready');
});

test('fleet state: no reply is offline, never the last known owner', () => {
  assert.equal(M.fleetState(live({ connection: 'offline', owner_name: 'agent:vesper:task_1' })), 'offline');
  // Offline is not attention even with an error note on the card.
  assert.equal(M.fleetState(live({ connection: 'offline', frame: null, frame_error: 'Selected operator transport closed.' })), 'offline');
});

// A computer that has not answered yet is neither offline nor a problem; it must not
// fill the header with "need attention" or read "No reply" while the console starts.
test('fleet state: a computer with no status answer yet is connecting', () => {
  assert.equal(M.fleetState({ trust_state: 'verified', connection: 'loading', frame_error: 'Preview unavailable' }), 'connecting');
  assert.equal(M.activityLine({ trust_state: 'verified', connection: 'ready' }), 'Connecting…');
  const counts = M.fleetCounts([{ trust_state: 'verified', connection: 'loading' }, live({ connection: 'offline' })]);
  assert.equal(counts.needs_attention, 1);
  assert.equal(counts.connecting, 1);
});

test('fleet state: owner names map to human, working, paused and ready', () => {
  assert.equal(M.fleetState(live({ owner_name: 'operator:riley', holds_control: true })), 'human');
  assert.equal(M.fleetState(live({ owner_name: 'operator:other' })), 'human');
  assert.equal(M.fleetState(live({ owner_name: 'agent:vesper:task_1', active_task: { state: 'active' } })), 'working');
  assert.equal(M.fleetState(live({ owner_name: 'human' })), 'paused');
  assert.equal(M.fleetState(live({ owner_name: 'none' })), 'ready');
});

test('fleet actor names you only for your own control', () => {
  assert.equal(M.fleetActor(live({ owner_name: 'operator:riley', holds_control: true })), 'you');
  assert.equal(M.fleetActor(live({ owner_name: 'operator:riley', operator_principal: 'riley' })), 'you');
  assert.equal(M.fleetActor(live({ owner_name: 'operator:laptop', operator_principal: 'riley' })), 'someone on laptop');
  assert.equal(M.fleetActor(live({ owner_name: 'agent:vesper:task_1' })), 'agent from vesper');
  assert.equal(M.fleetActor(live({ owner_name: 'human' })), '');
});

// Who can use it on the Screen tab. Failure cases:
// 1. The computer's own owner, an unpaired computer or one with every permission denied is listed.
// 2. You are not first, or an agent named like your computer reads as you.
// 3. A permission's mark sits in another column, or Ask First reads as allowed or denied.
// 4. The one-line summary miscounts agents, or says "1 people".
test('who can use it lists paired identities, you first, with each permission in its own column', () => {
  const rows = [
    { subject: 'owner', kind: 'person', capabilities: { watch: 'allow' } },
    { subject: 'relay', kind: 'computer', capabilities: { watch: 'allow', files: 'ask', control: 'deny' } },
    { subject: 'riley@relay', kind: 'agent', capabilities: { agents: 'allow' } },
    { subject: 'old', kind: 'computer', paired: false, capabilities: { watch: 'allow' } },
    { subject: 'quiet', kind: 'computer', capabilities: { watch: 'deny', files: 'bogus' } },
    { subject: 'laptop', kind: 'computer', owner: 'sam', computer_name: 'laptop', capabilities: { administer: 'allow' } },
    { subject: 'riley', kind: 'computer', capabilities: { watch: 'allow', files: 'allow', control: 'allow', agents: 'allow', administer: 'allow' } },
  ];
  // Plain copies: the model's arrays belong to the test's separate context.
  const who = JSON.parse(JSON.stringify(M.whoCanUse({ rows }, 'riley')));
  assert.deepEqual(who.map(row => row.label), ['You (riley)', 'relay', 'riley@relay', "sam's laptop"]);
  assert.deepEqual(who.map(row => row.you), [true, false, false, false]);
  assert.deepEqual(who[1].marks.map(mark => mark.key + ':' + mark.rule), ['watch:allow', 'files:ask', 'control:deny', 'agents:deny', 'administer:deny']);
  assert.deepEqual(who[3].marks.map(mark => mark.rule), ['deny', 'deny', 'deny', 'deny', 'allow']);
  assert.equal(M.peopleAndAgents(M.whoCanUse({ rows }, 'riley')), '3 people, 1 agent');
  assert.equal(M.peopleAndAgents(who.slice(0, 1)), '1 person');
  assert.equal(M.peopleAndAgents(who.slice(2, 3)), '1 agent');
  assert.equal(M.peopleAndAgents([]), '');
});

test('fleet counts group attention with offline and locked, and people with agents', () => {
  const counts = M.fleetCounts([live({ connection: 'offline' }), live({ connection: 'unauthorized' }), live({ locked: true }), live({ owner_name: 'operator:me', holds_control: true }), live({ owner_name: 'agent:a:t' }), live({ owner_name: 'human' }), live(), null]);
  assert.deepEqual({ ...counts }, { total: 7, attention: 1, offline: 1, locked: 1, connecting: 0, human: 1, working: 1, paused: 1, ready: 1, needs_attention: 3, in_use: 3 });
});

// An approval or a question waiting on a computer. Failure cases:
// 1. The header and the Needs Attention filter say 0 while the bar counts what waits for you
//    (Tyler's tour: "Needs Attention 0" beside a red 4).
// 2. An offline computer's waiting approval hides that it is offline.
// 3. A computer that only waits for your answer turns the bar's dot red or gets a problem toast
//    after a minute, beside the approval's own toast.
// 4. Its line under the name hides what it waits for, or loses the task's title.
test('what waits for your answer makes its computer need attention, never a problem', () => {
  const t0 = Date.parse('2026-09-28T12:00:00Z');
  const asking = live({ computer_id: 'c1', label: 'Kiln', owner_name: 'agent:relay:task_1', active_task: { title: 'Sign up', state: 'active' }, waiting: { approvals: 1, questions: 0 } });
  const asked = live({ computer_id: 'c2', label: 'Birch', waiting: { approvals: 1, questions: 2 } });
  const off = live({ computer_id: 'c3', label: 'Onyx', connection: 'offline', waiting: { approvals: 1, questions: 0 } });
  const counts = M.fleetCounts([asking, asked, off, live({ computer_id: 'c4' })]);
  assert.deepEqual([counts.attention, counts.offline, counts.needs_attention, counts.working, counts.ready], [2, 1, 3, 0, 1]);
  assert.equal(M.activityLine(asking), 'Sign up · waiting for your approval');
  assert.equal(M.activityLine(asked), '3 wait for your answer');
  assert.equal(M.activityLine(live({ waiting: { approvals: 0, questions: 1 } })), 'Waiting for your answer');
  const since = M.problemSince([asking, asked, off], {}, t0);
  assert.deepEqual(Object.keys(since), ['c3']);
  assert.deepEqual(Array.from(M.lastingProblems([asking, asked, off], since, t0 + M.PROBLEM_AFTER_MS, {}), p => p.computer_id), ['c3']);
});

// Failure cases for Pause Agents and Resume (header, card menu, card button):
// 1. An approval or question waiting on a computer where an agent works hides Pause Agents.
// 2. A computer a person paused loses Resume while an approval waits there.
// 3. ibara's own pause after a restart offers Resume, or a paused computer offers Pause Agents;
//    or, when the computer leaves resuming to a person (system_wait resume_off), lacks Resume.
// 4. Pause shows where it can't work: offline, someone holding control, a real problem, connecting.
test('pause and resume follow the computer, never what waits for your answer there', () => {
  const approval = { waiting: { approvals: 1, questions: 0 } };
  const act = s => M.pauseAction(M.decorateSession(s));
  const working = live({ owner_name: 'agent:relay:task_1', active_task: { title: 'Sign up', state: 'active' }, ...approval });
  assert.equal(M.decorateSession(working).fleet_state, 'attention');
  assert.equal(act(working), 'pause');
  assert.equal(act(live(approval)), 'pause');
  assert.equal(act(live({ owner_name: 'human', pause_origin: 'person', ...approval })), 'resume');
  assert.equal(act(live({ owner_name: 'human', waiting: { approvals: 0, questions: 2 } })), 'resume');
  const restarted = live({ owner_name: 'human', paused: true, pause_origin: 'system', ...approval });
  assert.equal(act(restarted), '');
  assert.equal(M.activityLine({ ...restarted, waiting: {} }), 'Paused after a restart · resuming by itself');
  const resumeOff = { ...restarted, system_wait: 'resume_off' };
  assert.equal(act(resumeOff), 'resume');
  assert.equal(M.activityLine({ ...resumeOff, waiting: {} }), 'Paused after a restart · Resume agents after a restart is off');
  assert.equal(act(live({ connection: 'offline', owner_name: 'agent:relay:task_1', ...approval })), '');
  assert.equal(act(live({ owner_name: 'operator:riley', holds_control: true, ...approval })), '');
  assert.equal(act(live({ owner_name: 'agent:relay:task_1', needs_person: { message: 'Screen sharing needs a repair.' }, ...approval })), '');
  assert.equal(act({ trust_state: 'verified', connection: 'loading', ...approval }), '');
  assert.equal(M.pauseAction(null), '');
});

test('activity line names the task and its age, and the stable form has no age', () => {
  const started = Date.parse('2026-09-25T10:00:00Z');
  const s = live({ owner_name: 'agent:vesper:task_1', active_task: { task_ref: 'task_1', title: 'Build release 4.2', state: 'active', started_at: '2026-09-25T10:00:00Z' } });
  assert.equal(M.activityLine(s, started + 6 * 60000 + 5000), 'Build release 4.2 · 6 min');
  assert.equal(M.activityLine(s), 'Build release 4.2');
  assert.equal(M.decorateSession(s).activity, 'Build release 4.2');
  assert.equal(M.decorateSession(s).fleet_state, 'working');
  assert.equal(M.activityLine(live({ connection: 'offline' })), 'No reply · last frame shown');
  assert.equal(M.activityLine(live({ owner_name: 'agent:v:t', active_task: { title: 'Clean up', state: 'waiting_for_human' } })), 'Clean up · waiting for you');
  // A ready computer's line says the same word as its state tag, never a second one.
  assert.equal(M.activityLine(live()), M.fleetStateLabel(M.fleetState(live())));
});

// Qt's Date.parse costs milliseconds per call on a small CPU; per-second labels for a
// 15-computer fleet use isoMs instead, so it must agree with Date.parse and refuse junk.
test('isoMs refuses anything but a complete ISO instant', () => {
  for (const bad of ['', 'yesterday', '2026-09-25', '2026-09-25T10:00', '2026-13-01T00:00:00Z', '2026-02-30T00:00:00Z',
    '2026-09-25T24:00:00Z', '2026-09-25T10:60:00Z', '2026-09-25T10:00:00', '2026-09-25T10:00:00+2', ' 2026-09-25T10:00:00Z', null, undefined, 1790000000000])
    assert.ok(Number.isNaN(M.isoMs(bad)), String(bad));
});

test('isoMs matches Date.parse for UTC, offsets and fractions', () => {
  for (const good of ['2026-09-25T10:00:00Z', '2026-09-25T10:00:00.5Z', '2026-09-25T10:00:00.123456Z', '2026-12-31T23:59:59-07:00', '2026-01-01T00:30:00+05:30', '2024-02-29T12:00:00Z'])
    assert.equal(M.isoMs(good), Date.parse(good), good);
});

test('transport failures name the computer instead of showing raw stderr', () => {
  const banner = 'Connection timed out during banner exchange\nConnection to UNKNOWN port 65535 timed out';
  const retried = M.plainError(banner, 'Tulip0', true);
  assert.match(retried, /^Tulip0 /);
  assert.doesNotMatch(retried, /banner|port|65535/);
  assert.match(retried, /keep trying/);
  const once = M.plainError('ssh: connect to host tulip1 port 22: Connection refused', 'Tulip1', false);
  assert.match(once, /^Tulip1 /);
  assert.doesNotMatch(once, /ssh|port 22|refused|keep trying/);
  assert.doesNotMatch(M.plainError('spawnSync tailscale ETIMEDOUT', '', true), /spawnSync|ETIMEDOUT/);
  const missing = M.plainError('spawnSync tailscale ENOENT', 'Tulip0', false);
  assert.match(missing, /tailscale/);
  assert.doesNotMatch(missing, /spawnSync|ENOENT/);
  // Already plain messages pass through unchanged.
  assert.equal(M.plainError('Moonlight pairing timed out. Try again.', 'Tulip0', false), 'Moonlight pairing timed out. Try again.');
  assert.equal(M.plainError('', 'Tulip0', true), '');
});

test("core texts that say 'inspect the target' become messages naming the computer", () => {
  for (const core of [
    'OPERATOR_TRANSPORT_UNAVAILABLE: Operator request failed; inspect target status.',
    'Control response changed target binding; inspect the target before retrying.',
    'Handback did not prove settled ownership; inspect the target.',
    'PERMISSION_DENIED: Target binding changed.',
  ]) {
    const shown = M.plainError(core, 'Tulip1', false);
    assert.doesNotMatch(shown, /inspect|binding|OPERATOR_|PERMISSION_/i, core);
    assert.match(shown, /^Tulip1 /, core);
  }
  assert.doesNotMatch(M.plainError('The operator reply was lost. Inspect controller state and the original operation before repeating this action.', 'Tulip1', false), /inspect/i);
});

// Failure cases for "While you were away" with a busy computer:
// 1. More than 50 new events: the oldest 50 are kept and the newest dropped.
// 2. "+N earlier" counts only the kept events, not all that came.
test('while you were away keeps the newest events and counts every one that came', () => {
  const events = Array.from({ length: 60 }, (_, i) => ({ at: new Date(Date.UTC(2026, 8, 27, 10, i)).toISOString(), kind: 'task', summary: `event ${i}` }));
  const view = M.awayView({ since: 0, computers: [{ computer_id: 'computer_1', label: 'Tulip1', events }] });
  const shown = view.computers[0];
  assert.equal(shown.events.length, 50);
  assert.equal(shown.events[0].summary, 'event 10');
  assert.equal(shown.events[49].summary, 'event 59');
  assert.equal(shown.total, 60);
});

// What makes the bar red, and when. Failure cases:
// 1. An agent's question loses its options, or has one shortened, so the answer the person
//    chooses matches none and the computer refuses it; an approval grows options.
// 2. A computer offline for a moment (an update, a restart) turns the bar red.
// 3. A lasting problem never shows, starts over when offline turns into needing attention,
//    or stays once the computer answers again.
// 4. A problem put away shows again with the same words, or never again once they change.
// 5. A computer that needs a person shows twice (as a need and as a problem), or its need
//    shows again after it was put away with the same message.
test('an agent\'s question keeps its options exactly as the agent sent them', () => {
  const long = 'Use the terminal instead and '.repeat(6).trim();
  const [question, approval] = M.attentionView({ items: [
    { computer_id: 'computer_1', label: 'Juniper', ref: 'att_q', kind: 'question', summary: 'May I use the file manager?', options: ['Approve file manager', long, 7, ''], at: '2026-09-28T20:38:34.371Z' },
    { computer_id: 'computer_1', label: 'Juniper', ref: 'att_a', kind: 'approval', summary: 'codex@lumen wants to press Return in Mousepad.', options: ['approve', 'deny'], at: '2026-09-28T20:39:00.000Z' },
  ] });
  assert.deepEqual(Array.from(question.options), ['Approve file manager', long]);
  assert.deepEqual(Array.from(approval.options), []);
});

// An agent's answer options on their buttons. Failure cases:
// 1. An option shows in the agent's own casing ("Approve file manager"), unlike every other button.
// 2. A short joining word mid-label is capitalized, or a first or last word isn't.
// 3. "ibara", a name with capitals or a number is changed.
// (The answer sent is the option itself, untouched: QuestionCard sends modelData.)
test('answer options read in Title Case on their buttons', () => {
  assert.equal(M.titleCase('Approve file manager'), 'Approve File Manager');
  assert.equal(M.titleCase('use the terminal instead of a browser'), 'Use the Terminal Instead of a Browser');
  assert.equal(M.titleCase('send it to'), 'Send It To');
  assert.equal(M.titleCase('open ibara on FICTIONAL 02 with iPhone'), 'Open ibara on FICTIONAL 02 with iPhone');
  assert.equal(M.titleCase('retry the sign-up in 3rd tab'), 'Retry the Sign-Up in 3rd Tab');
});

test('the bar turns red for a problem only once it has lasted a minute, and says why', () => {
  const t0 = 1_790_000_000_000;
  const tater = { computer_id: 'c1', label: 'Juniper', trust_state: 'verified', connection: 'offline', wake: { mac: '00:11:22:33:44:55' } };
  let since = M.problemSince([tater], {}, t0);
  assert.deepEqual(Array.from(M.lastingProblems([tater], since, t0 + 30_000, {})), [], 'a restart or an update never raises it');
  since = M.problemSince([tater], since, t0 + 30_000);
  const [offline] = M.lastingProblems([tater], since, t0 + M.PROBLEM_AFTER_MS, {});
  assert.equal(offline.heading, 'Juniper isn\'t answering');
  assert.match(offline.body, /Check that it's on and online, or choose Wake\./);

  // Back, but its access not confirmed yet: one problem since it went offline, in new words.
  const denied = { ...tater, connection: 'unauthorized' };
  since = M.problemSince([denied], since, t0 + 70_000);
  const [attention] = M.lastingProblems([denied], since, t0 + 70_000, {});
  assert.equal(attention.heading, 'Juniper needs attention');
  assert.match(attention.body, /couldn't confirm that you may use it/);

  // Put away: gone while its words stay, back once they change, and ended when it answers.
  const hidden = { c1: attention.signature };
  assert.deepEqual(Array.from(M.lastingProblems([denied], since, t0 + 80_000, hidden)), []);
  assert.equal(M.lastingProblems([tater], since, t0 + 80_000, hidden).length, 1, 'offline again is new news');
  const ready = { ...tater, connection: 'ready', owner_name: 'none', frame: { path: '/tmp/f' } };
  since = M.problemSince([ready], since, t0 + 90_000);
  assert.deepEqual({ ...since }, {});
  assert.deepEqual(Array.from(M.lastingProblems([ready], since, t0 + 200_000, {})), []);
});

test('a computer that needs a person shows once, as a need, until it is put away', () => {
  const t0 = 1_790_000_000_000;
  const lumen = { computer_id: 'c2', label: 'Lumen', trust_state: 'verified', connection: 'ready', owner_name: 'none', frame: { path: '/tmp/f' },
    needs_person: { message: 'Screen sharing on Lumen stopped.', fix: 'restart_viewer' } };
  const items = [{ computer_id: 'c2', label: 'Lumen', kind: 'repair', ref: 'restart_viewer', summary: 'Screen sharing on Lumen stopped.' },
    { computer_id: 'c2', label: 'Lumen', kind: 'question', ref: 'att_q', summary: 'Which file?' }];
  const needs = M.needsYouView(items, [lumen], {});
  assert.deepEqual(Array.from(needs, n => [n.computer_id, n.message, n.fix]), [['c2', 'Screen sharing on Lumen stopped.', 'restart_viewer']]);
  const since = M.problemSince([lumen], {}, t0);
  assert.deepEqual(Array.from(M.lastingProblems([lumen], since, t0 + 120_000, {})), [], 'its need toast already says why');
  assert.deepEqual(Array.from(M.needsYouView(items, [lumen], { c2: 'Screen sharing on Lumen stopped.' })), []);
  const worse = [{ ...items[0], summary: 'ibara on Lumen stopped answering.', ref: 'restart_ibara' }];
  assert.equal(M.needsYouView(worse, [], { c2: 'Screen sharing on Lumen stopped.' })[0].fix, 'restart_ibara');
});

// File sizes read as the core's refusals word them: decimal units counted from the bytes
// (1 MB is 1,000,000 bytes). Failure cases:
// 1. A 300 MiB file shows as "300.0 MiB" in the Files tab while the refusal says "315 MB".
// 2. The default limit of 250,000,000 bytes reads anything but "250 MB", or a 256 MiB file "256 MB".
// 3. Rounding shows "1000 KB" instead of the next unit.
test('file sizes read in decimal units, as the refusals say them', () => {
  const size = context.bytesLabel;
  assert.equal(size(300 * 1024 * 1024), '315 MB');
  assert.equal(size(250000000), '250 MB');
  assert.equal(size(256 * 1024 * 1024), '268 MB');
  assert.equal(size(1572864), '1.6 MB');
  assert.equal(size(1288490189), '1.3 GB');
  assert.equal(size(1), '1 byte');
  assert.equal(size(999), '999 bytes');
  assert.equal(size(1000), '1 KB');
  assert.equal(size(9960), '10 KB');
  assert.equal(size(999999), '1 MB');
  assert.equal(size(999600000), '1 GB');
  assert.equal(size(-1), 'size unknown');
});

// Which approvals offer Always Allow, and which are an agent's request to stop asking.
// Failure cases:
// 1. Always Allow is offered on a request it can't answer: an access request (take control,
//    change a setting), a question, an approval from an older computer without details, or a
//    step that only changes something.
// 2. An agent's send, spend or delete step does not offer it.
// 3. An agent's request to stop asking reads as an ordinary approval, or offers Always Allow.
test('Always Allow is offered only on an agent step that sends, spends or deletes', () => {
  const approval = (details, kind = 'approval') => M.attentionView({ items: [{ computer_id: 'computer_1', label: 'FICTIONAL 06', ref: 'att_1', kind, summary: 'codex@lumen wants to press Return.', details }] })[0];
  for (const effect of ['send', 'spend', 'destructive']) {
    const item = approval({ agent: 'codex@lumen', effect, request: { step: { action: { kind: 'key', keys: 'Return' } } } });
    assert.deepEqual([item.effect, item.agent, item.stopAsking], [effect, 'codex@lumen', false], effect);
  }
  const none = [
    approval({ capability: 'control', request: { op: 'take_control' } }),
    approval({ agent: 'codex@lumen', effect: 'change' }),
    approval(null),
    approval({ agent: 'codex@lumen', effect: 'send' }, 'question'),
    approval({ agent: '{"x":1}', effect: 'send' }),
  ];
  for (const item of none) assert.equal(item.effect, '', JSON.stringify(item));
  const stop = approval({ agent: 'codex@lumen', effect: 'send', request: { op: 'stop_asking', kinds: ['send', 'spend', 'destructive'] } });
  assert.deepEqual([stop.stopAsking, stop.effect], [true, '']);
});

// How busy a computer is depends on its processors: a load of 6 is heavy for one, moderate for eight.
// Without the processor count only a load that is light for any computer is said.
test('health says how busy a computer is from its load per processor', () => {
  const busy = health => M.healthLines(health).find(line => line.startsWith('Busy: ')) || null;
  assert.equal(busy({ load: 6, cpus: 8 }), 'Busy: moderate');
  assert.equal(busy({ load: 9, cpus: 8 }), 'Busy: heavy');
  assert.equal(busy({ load: 0.3, cpus: 1 }), 'Busy: light');
  assert.equal(busy({ load: 0.3 }), 'Busy: light');
  assert.equal(busy({ load: 3 }), null);
  assert.ok(M.healthLines({ uptime_s: 86400 + 5 }).includes('On for 1 day'));
});

// A login request (attention kind `login`) as the console reads it. Failure cases:
// 1. A request with no site a person can read shows as an empty approval, or a repeated or
//    malformed site (a URL, a path) is offered to share.
// 2. A two-site sign-in names a site as coming "through" itself.
// 3. A login request turns into a "needs you" toast, or waits behind "+N more".
test('a login request lists each site once, with the site it signs in through, and waits for an answer', () => {
  const item = (sites, extra = {}) => ({ items: [{ computer_id: 'fictional-03', label: 'Birch', ref: 'att_l0', kind: 'login', at: '2026-09-30T10:00:00Z',
    summary: 'codex@relay, working on “File Q3 estimated tax” on Birch, wants your logins for 3 sites',
    details: { agent: 'codex@relay', task: { task_ref: 'task_1', goal: 'File Q3 estimated tax' }, sites, page: 'https://sa.www4.irs.gov/ola/', own: true, source_label: 'Studio', ...extra } }] });
  const [login] = M.attentionView(item([{ site: 'IRS.gov', via: 'id.me' }, { site: 'id.me', via: 'id.me' }, { site: 'irs.gov' }, { site: 'https://evil.example/x' }, { site: 'oktap.tax.ok.gov' }]));
  assert.deepEqual(JSON.parse(JSON.stringify(login.login.sites)), [{ site: 'irs.gov', via: 'id.me', state: '', method: '' }, { site: 'id.me', via: '', state: '', method: '' }, { site: 'oktap.tax.ok.gov', via: '', state: '', method: '' }]);
  assert.equal(login.agent, 'codex@relay');
  assert.equal(M.attentionView(item([{ site: 'not a site' }])).length, 0);
  assert.equal(M.needsYouView([login], [], {}).length, 0);
  assert.deepEqual(Array.from(M.standingToasts({ route: 'fleet', approvals: [{ ref: 'att_1' }], logins: [login], questions: [{ ref: 'att_q' }] }), t => t.key),
    ['approval:att_1', 'login:att_l0', 'question:att_q']);
  const layout = M.toastLayout(['message', 'message', 'login', 'login-setup', 'message'], 3);
  assert.deepEqual([Array.from(layout.shown, s => s ? 1 : 0).join(''), layout.hidden], ['00111', 2]);
});

// One answer for the whole request. Failure cases: an unticked site is shared anyway, or Don't
// Share leaves a ticked site shared.
test('an answer gives the ticked sites the choice and every other site Don\'t Share', () => {
  const sites = [{ site: 'irs.gov' }, { site: 'id.me' }, { site: 'oktap.tax.ok.gov' }];
  assert.deepEqual({ ...M.loginDecisions(sites, { 'id.me': true }, 'share_all') }, { 'irs.gov': 'share_all', 'oktap.tax.ok.gov': 'share_all' });
  assert.deepEqual({ ...M.loginDecisions(sites, {}, 'decline') }, { 'irs.gov': 'decline', 'id.me': 'decline', 'oktap.tax.ok.gov': 'decline' });
  assert.deepEqual({ ...M.loginDecisions(sites, { 'irs.gov': true }, 'never') }, { 'id.me': 'never', 'oktap.tax.ok.gov': 'never' });
});

// What each outcome of an answer leaves. Failure cases: a signed-out site is reported as shared
// (the request would close and the agent wait forever), a site waiting for the browser or not
// confirmed is lost among the notes, or an outcome ibara doesn't know is said as something else.
test('an answer\'s outcomes: shared and refused sites are notes, the rest say what to do', () => {
  const words = M.loginAnswerWords([{ site: 'irs.gov', outcome: 'shared' }, { site: 'id.me', outcome: 'signed_out_there' }, { site: 'chase.com', outcome: 'denied' },
    { site: 'usps.com', outcome: 'waiting_for_browser' }, { site: 'sos.ok.gov', outcome: 'unknown' }, { site: 'x.org', outcome: 'exploded' }], 'Brave', 'Birch');
  assert.equal(words.note, 'Shared your login for irs.gov with Birch. chase.com is never shared now, on any computer.');
  assert.deepEqual(Array.from(words.signedOut), ['id.me']);
  assert.match(words.waiting, /^Open Brave to share your login for usps\.com\./);
  assert.match(words.unknown, /sos\.ok\.gov/);
  assert.equal(words.off, false);
  assert.equal(M.loginAnswerWords([{ site: 'irs.gov', outcome: 'sharing_off' }], 'Brave', 'Birch').off, true);
});

// Sites that rejected a shared login. Failure cases: the same rejection toasts again on every
// read, or a console just started raises every rejection the daemon still remembers.
test('a rejected login shows once, and none from before the console started', () => {
  const since = 1000;
  const list = [{ computer: 'fictional-03', site: 'id.me', at: 900 }, { computer: 'fictional-03', site: 'id.me', at: 1500 }, { computer: 'fictional-04', site: 'irs.gov', at: 2000 }];
  const first = M.loginRejectedFresh(list, {}, since);
  assert.deepEqual(Array.from(first, r => r.key), ['fictional-03|id.me|1500', 'fictional-04|irs.gov|2000']);
  const seen = Object.fromEntries(first.map(r => [r.key, true]));
  assert.equal(M.loginRejectedFresh(list, seen, since).length, 0);
  assert.deepEqual(Array.from(M.loginRejectedFresh(list.concat([{ computer: 'fictional-03', site: 'id.me', at: 2600 }]), seen, since), r => r.key), ['fictional-03|id.me|2600']);
});

// A computer's Logins tab rows. Failure case: a row with a rule ibara doesn't know is offered
// in the menu, or an unknown last result reads as Worked, or a result shows a date it doesn't have.
test('login rows keep known rules and results with their dates, sorted by site', () => {
  const view = M.loginRowsView({ rows: [{ site: 'irs.gov', rule: 'ask', own_rule: 'ask', last_result: 'worked', last_result_at_ms: 7, last_shared_ms: 5 }, { site: 'id.me', rule: 'maybe' },
    { site: 'github.com', rule: 'allow', all_rule: 'allow', last_result: 'odd', last_result_at_ms: 9 }, { site: 'usps.com', rule: 'ask', last_result: null, last_result_at_ms: null }], denied_all: ['chase.com', 'not a site'] });
  assert.deepEqual(JSON.parse(JSON.stringify(view.rows.map(r => [r.site, r.rule, r.lastResult, r.lastResultAt]))), [['github.com', 'allow', '', 0], ['irs.gov', 'ask', 'worked', 7], ['usps.com', 'ask', '', 0]]);
  assert.deepEqual(Array.from(view.deniedAll), ['chase.com']);
});
