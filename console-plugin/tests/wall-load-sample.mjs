#!/usr/bin/env node
// Development-only native load sampler. Reads the running shell's previewStats IPC
// and /proc for the Quickshell process tree; never touches credentials or transport.
// Usage: wall-load-sample.mjs --target PLUGIN_ID --seconds N --interval MS --out FILE.jsonl
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
const option = (name, fallback) => { const at = process.argv.indexOf(name); return at < 0 ? fallback : process.argv[at + 1]; };
const target = option('--target');
const seconds = Number(option('--seconds', 60));
const interval = Number(option('--interval', 500));
const out = option('--out');
if (!/^io\.zet\.ibara(\.wall-[a-f0-9]{8})?$/.test(String(target)) || !out) throw new Error('Explicit --target plugin ID and --out file required.');
const tick = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim());
const shell = Number(execFileSync('pgrep', ['-o', '-x', 'quickshell'], { encoding: 'utf8' }).trim());
function processTree() {
  const rows = [];
  for (const name of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const stat = fs.readFileSync(`/proc/${name}/stat`, 'utf8');
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      rows.push({ pid: Number(name), ppid: Number(fields[1]), cpu_ticks: Number(fields[11]) + Number(fields[12]), children_ticks: Number(fields[13]) + Number(fields[14]), comm: stat.slice(stat.indexOf('(') + 1, stat.lastIndexOf(')')) });
    } catch {}
  }
  const tree = new Set([shell]);
  for (let grew = true; grew;) { grew = false; for (const row of rows) if (!tree.has(row.pid) && tree.has(row.ppid)) { tree.add(row.pid); grew = true; } }
  const members = rows.filter(row => tree.has(row.pid));
  const rss = Number((fs.readFileSync(`/proc/${shell}/status`, 'utf8').match(/VmRSS:\s+(\d+)/) || [])[1] || 0) * 1024;
  return { shell_rss_bytes: rss, members: members.map(row => ({ pid: row.pid, comm: row.comm, cpu_ticks: row.cpu_ticks, children_ticks: row.children_ticks })) };
}
const started = Date.now();
fs.writeFileSync(out, '');
while (Date.now() - started < seconds * 1000) {
  const at = Date.now();
  let stats = null;
  try { stats = JSON.parse(execFileSync('omarchy-shell', [target, 'previewStats'], { encoding: 'utf8', timeout: 5000 })); } catch (error) { stats = { error: String(error.message || error).slice(0, 200) }; }
  fs.appendFileSync(out, JSON.stringify({ sampled_ms: at, stats, process: processTree(), clock_ticks: tick }) + '\n');
  const wait = interval - (Date.now() - at);
  if (wait > 0) await new Promise(resolve => setTimeout(resolve, wait));
}
