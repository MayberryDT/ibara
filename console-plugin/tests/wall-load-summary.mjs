#!/usr/bin/env node
// Summarizes wall-load-sample.mjs output. Visible age is the age the wall label shows:
// the frame's age on arrival plus local time since arrival (displayed_age_ms).
import fs from 'node:fs';
const file = process.argv[2];
const option = (name, fallback) => { const at = process.argv.indexOf(name); return at < 0 ? fallback : process.argv[at + 1]; };
const from = Number(option('--from', 0)), to = Number(option('--to', Infinity));
const rows = fs.readFileSync(file, 'utf8').trim().split('\n').map(line => JSON.parse(line)).filter(row => row.sampled_ms >= from && row.sampled_ms <= to && row.stats && !row.stats.error);
if (rows.length < 2) throw new Error('Not enough samples in the window.');
const pct = (values, p) => { if (!values.length) return null; const v = [...values].sort((a, b) => a - b); return v[Math.min(v.length - 1, Math.ceil(p / 100 * v.length) - 1)]; };
const describe = values => ({ n: values.length, p50: pct(values, 50), p95: pct(values, 95), max: values.length ? Math.max(...values) : null });
const tileAges = [], selectedAges = [], perComputer = {}, missing = {};
const lanes = [], queue = [], reads = [], readQueue = [], retained = [], rss = [];
for (const row of rows) {
  const s = row.stats;
  lanes.push(s.lanes_active); queue.push(s.queue_length); reads.push(s.reads_active); readQueue.push(s.read_queue_length);
  retained.push(s.retained_bytes_estimate); rss.push(row.process.shell_rss_bytes);
  for (const c of s.computers) {
    const selected = s.console_open && c.computer_id === s.selected;
    if (!c.visible && !selected) continue;
    if (c.connection !== 'ready') continue;
    if (!c.capture_time) { missing[c.computer_id] = (missing[c.computer_id] || 0) + 1; continue; }
    const age = typeof c.displayed_age_ms === 'number' ? c.displayed_age_ms : row.sampled_ms - Date.parse(c.capture_time);
    (selected ? selectedAges : tileAges).push(age);
    (perComputer[c.computer_id] ||= []).push(age);
  }
}
const ticks = row => row.process.members.reduce((sum, m) => sum + m.cpu_ticks + m.children_ticks, 0);
const first = rows[0], last = rows.at(-1), elapsed = (last.sampled_ms - first.sampled_ms) / 1000;
const summary = {
  window: { from_ms: first.sampled_ms, to_ms: last.sampled_ms, seconds: elapsed, samples: rows.length },
  tile_visible_age_ms: describe(tileAges),
  selected_visible_age_ms: describe(selectedAges),
  per_computer_p95_ms: Object.fromEntries(Object.entries(perComputer).map(([id, v]) => [id, pct(v, 95)])),
  samples_without_frame: missing,
  operator_cpu_percent_of_one_core: Math.round((ticks(last) - ticks(first)) / first.clock_ticks / elapsed * 1000) / 10,
  shell_rss_bytes: describe(rss),
  retained_bytes_estimate: describe(retained),
  preview_lanes_active: describe(lanes), preview_queue_length: describe(queue),
  read_lanes_active: describe(reads), read_queue_length: describe(readQueue),
};
const requestsFile = option('--requests');
if (requestsFile) {
  const requests = fs.readFileSync(requestsFile, 'utf8').trim().split('\n').map(line => JSON.parse(line)).filter(r => r.finished_ms >= first.sampled_ms && r.finished_ms <= last.sampled_ms);
  const byOutcome = {};
  for (const r of requests) byOutcome[r.outcome] = (byOutcome[r.outcome] || 0) + 1;
  summary.requests = { count: requests.length, per_second: Math.round(requests.length / elapsed * 100) / 100, bytes: requests.reduce((s, r) => s + r.bytes, 0), bytes_per_second: Math.round(requests.reduce((s, r) => s + r.bytes, 0) / elapsed), outcomes: byOutcome, round_trip_ms: describe(requests.map(r => r.finished_ms - r.started_ms)) };
}
console.log(JSON.stringify(summary, null, 2));
