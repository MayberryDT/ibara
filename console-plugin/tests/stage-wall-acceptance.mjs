#!/usr/bin/env node
// Creates a disposable *fictional* hosted-QML package. Never install as a live plugin.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const count = Number(process.argv[2]);
if (![0,1,3,10,15,20,100].includes(count)) throw new Error('Choose 0, 1, 3, 10, 15, 20 or 100 fictional computers.');
// Count 15 is a fictional fleet with its own screen pictures, screen-01.jpg to screen-15.jpg, from --assets DIR.
const assetsAt = process.argv.indexOf('--assets');
const fleetAssets = assetsAt > 0 ? process.argv[assetsAt + 1] : '';
const FLEET_SCREENS = [8, 14, 3, 1, 2, 4, 6, 7, 10, 12, 11, 5, 9, 13, 15];
if (count === 15 && !(fleetAssets && fs.existsSync(path.join(fleetAssets, 'screen-01.jpg')))) throw new Error(fleetAssets ? `Fleet screens not found in ${fleetAssets}.` : 'Count 15 needs its screen pictures: pass --assets DIR.');
// --load PROFILE.json: measured real [round_trip_ms, arrival_age_ms] samples per quality.
const loadAt = process.argv.indexOf('--load');
const loadProfile = loadAt > 0 ? JSON.parse(fs.readFileSync(process.argv[loadAt + 1], 'utf8')) : null;
if (loadProfile && !['tile','selected'].every(q => Array.isArray(loadProfile[q]) && loadProfile[q].length && loadProfile[q].every(s => Array.isArray(s) && s.length === 2 && s.every(Number.isFinite)))) throw new Error('Load profile needs tile and selected [round_trip_ms, arrival_age_ms] samples.');
const destination = fs.mkdtempSync(path.join(os.tmpdir(), 'ibara-wall-acceptance-'));
// Fresh ID defeats Qt's cached child-component path after a QML dependency edit.
const devId = `io.zet.ibara.wall-${crypto.randomBytes(4).toString('hex')}`;
// Every top-level QML and JS file ships, so the staged package cannot drift from the plugin.
for (const name of fs.readdirSync(source).filter(name => /\.(qml|js)$/.test(name))) {
  const text = fs.readFileSync(path.join(source, name), 'utf8');
  fs.writeFileSync(path.join(destination, name), text.replaceAll('io.zet.ibara', devId));
}
fs.copyFileSync(path.join(source, 'manifest.json'), path.join(destination, 'manifest.json'));
const manifestPath = path.join(destination, 'manifest.json');
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
manifest.id = devId;
manifest.name = 'FICTIONAL ibara wall acceptance';
manifest.description = 'Development-only fictional hosted-QML wall. No live ibara operations.';
manifest.kinds = ['service', 'panel'];
delete manifest.entryPoints.barWidget;
delete manifest.barWidget;
fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
// The fictional daemon ships beside the QML; you start it on the staged socket (the second line printed below).
fs.mkdirSync(path.join(destination, 'scripts'));
fs.copyFileSync(path.join(source, 'tests/dev-fixture-daemon.mjs'), path.join(destination, 'scripts/dev-fixture-daemon.mjs'));
fs.cpSync(path.join(source, 'fixtures'), path.join(destination, 'fixtures'), {recursive:true});
const previewDir = path.join(destination, 'fixtures', 'wall-previews');
fs.mkdirSync(previewDir);
// Preview frames are raw PPM (P6), as ibarad writes them; the fictional daemon scales them
// to the shown size the service names.
const ppm = (width, height, rgb) => Buffer.concat([Buffer.from(`P6\n${width} ${height}\n255\n`, 'ascii'), rgb]);
// The largest frame file the service accepts for each quality.
const frameLimit = quality => (quality === 'tile' ? 480 * 270 : 1280 * 720) * 3 + 32;
function fictionalScreen(index) {
  const width = 320, height = 180, pixels = Buffer.alloc(width * height * 3);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const at = (y * width + x) * 3;
      const header = y < 22, tile = x > 14 && x < 305 && y > 36 && y < 164;
      const bit = y > 47 && y < 75 && x > 30 && x < 286 ? Math.floor((x - 30) / 32) : -1;
      const marked = bit >= 0 && ((index + 1) & (1 << bit));
      pixels[at] = header ? 38 : marked ? 75 + index % 5 * 24 : tile ? 22 + index % 4 * 8 : 14;
      pixels[at + 1] = header ? 58 + index % 6 * 18 : marked ? 180 : tile ? 44 + index % 7 * 7 : 24;
      pixels[at + 2] = header ? 80 : marked ? 235 : tile ? 66 : 36;
    }
  }
  return ppm(width, height, pixels);
}
if (count === 15) {
  // Tile and selected frames converted from the mockup screens; each must fit the Service frame budget.
  for (let i = 0; i < count; i++) {
    const id = `fictional-${String(i).padStart(2, '0')}`, screen = path.join(fleetAssets, `screen-${String(FLEET_SCREENS[i]).padStart(2, '0')}.jpg`);
    const convert = (size, file, quality) => {
      const done = spawnSync('magick', [screen, '-resize', `${size}!`, '-strip', `PPM:${file}`], { encoding: 'utf8' });
      if (done.status !== 0) throw new Error(`Could not convert ${screen}: ${done.stderr || done.error}`);
      if (fs.statSync(file).size > frameLimit(quality)) throw new Error(`${file} exceeds the preview budget.`);
    };
    convert('480x270', path.join(previewDir, `${id}.ppm`), 'tile');
    convert('1280x720', path.join(previewDir, `${id}-selected-0.ppm`), 'selected');
    fs.copyFileSync(path.join(previewDir, `${id}-selected-0.ppm`), path.join(previewDir, `${id}-selected-1.ppm`));
  }
} else for (let i = 0; i < count; i++) fs.writeFileSync(path.join(previewDir, `fictional-${String(i).padStart(2, '0')}.ppm`), fictionalScreen(i));
// Load frames look like a desktop: blocks plus noisy "text" rows, a per-computer bit
// marker and a progress bar that moves with every frame so each capture is visibly new.
function loadFrame(index, variant, variants, width, height, textRows) {
  const noise = crypto.randomBytes(width * height * 3);
  const pixels = Buffer.alloc(width * height * 3);
  for (let y = 0; y < height; y++) {
    const header = y < height * 0.05, bar = y > height * 0.94;
    const inWindow = y > height * 0.12 && y < height * 0.9;
    const textLine = inWindow && ((y * 2654435761 >>> 0) % 1000) < textRows * 1000;
    for (let x = 0; x < width; x++) {
      const at = (y * width + x) * 3;
      const bit = y > height * 0.2 && y < height * 0.35 && x > width * 0.06 && x < width * 0.6 ? Math.floor((x - width * 0.06) / (width * 0.54 / 8)) : -1;
      const marked = bit >= 0 && ((index + 1) & (1 << bit));
      let r = 16 + index % 5 * 6, g = 22 + index % 7 * 5, b = 34;
      if (header) { r = 40; g = 60 + index % 6 * 18; b = 90; }
      else if (bar) { const filled = x < width * (variant + 1) / variants; r = filled ? 230 : 40; g = filled ? 190 : 40; b = filled ? 60 : 48; }
      else if (marked) { r = 80 + index % 5 * 24; g = 190; b = 240; }
      else if (textLine && x > width * 0.1 && x < width * 0.92) { r = noise[at]; g = noise[at + 1]; b = noise[at + 2]; }
      else if (inWindow && x > width * 0.08 && x < width * 0.94) { r = 28; g = 32; b = 44; }
      pixels[at] = r; pixels[at + 1] = g; pixels[at + 2] = b;
    }
  }
  return ppm(width, height, pixels);
}
if (loadProfile) {
  for (let i = 0; i < count; i++) {
    const id = `fictional-${String(i).padStart(2, '0')}`;
    for (let k = 0; k < 4; k++) fs.writeFileSync(path.join(previewDir, `${id}-tile-${k}.ppm`), loadFrame(i, k, 4, 480, 270, 0.62));
    if (count <= 20) for (let k = 0; k < 2; k++) fs.writeFileSync(path.join(previewDir, `${id}-selected-${k}.ppm`), loadFrame(i, k, 2, 1280, 720, 0.5));
  }
  // The late-reply frame is solid red: it must never become visible.
  const red = Buffer.alloc(1280 * 720 * 3);
  for (let at = 0; at < red.length; at += 3) red.set([220, 20, 20], at);
  fs.writeFileSync(path.join(previewDir, 'late.ppm'), ppm(1280, 720, red));
  fs.writeFileSync(path.join(destination, 'fixtures', 'wall-load.json'), JSON.stringify(loadProfile) + '\n');
}
// The staged service talks to the fictional daemon's socket, never the real ibarad. The socket
// and the daemon's runtime files (control.json, requests.jsonl) live outside the plugin folder
// because the shell hot-reloads a plugin whenever its files change.
const runtime = path.join(os.tmpdir(), 'ibara-dev-wall-' + devId);
const socketPath = path.join(runtime, 'ibarad.sock');
const servicePath = path.join(destination, 'Service.qml');
const service = fs.readFileSync(servicePath, 'utf8');
const socketProperty = 'readonly property string daemonSocketPath: {\n    var dir = String(Quickshell.env("XDG_RUNTIME_DIR") || "")\n    return dir ? dir + "/ibara/ibarad.sock" : ""\n  }';
if (!service.includes(socketProperty)) throw new Error('Production service transport changed; inspect before staging.');
fs.writeFileSync(servicePath, service.replace(socketProperty, () => `readonly property string daemonSocketPath: ${JSON.stringify(socketPath)}`));
const start = `IBARA_DEV_WALL_COUNT=${count} node ${path.join(destination, 'scripts/dev-fixture-daemon.mjs')} ${socketPath}`;
fs.writeFileSync(path.join(destination, 'DEVELOPMENT-ONLY'), `Plugin ID: ${devId}\nFictional wall count: ${count}\nFictional daemon: ${start}\nNo live credentials or target operations.\n`);
console.log(destination);
console.log(start);
