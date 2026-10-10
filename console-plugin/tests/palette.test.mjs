import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const source = fs.readFileSync(path.join(root, 'Palette.js'), 'utf8').replace('.pragma library', '');
const context = {};
vm.createContext(context);
vm.runInContext(source + '\nthis.Palette = { deltaE2000, difference, distinct };', context);
const P = context.Palette;

test('CIEDE2000 matches the reference pairs of Sharma, Wu and Dalal', () => {
  for (const [a, b, expected] of [
    [[50, 2.6772, -79.7751], [50, 0, -82.7485], 2.0425],
    [[50, 0, 0], [50, -1, 2], 2.3669],
    [[50, 2.5, 0], [73, 25, -18], 27.1492],
    [[2.0776, 0.0795, -1.135], [0.9033, -0.0636, -0.5514], 0.9082],
  ]) assert.ok(Math.abs(P.deltaE2000(a, b) - expected) < 1e-4, `${a} / ${b}: ${P.deltaE2000(a, b)}`);
});

// Agents working take the first theme color clearly apart from a person in control's magenta
// and from the other states' colors (red, yellow, green, muted).
test('agents working skip blue where it is too close to magenta, as in Permafrost', () => {
  const permafrost = { magenta: '#5A7388', red: '#B75D68', yellow: '#A07A52', green: '#4E8A86', muted: '#42657E', blue: '#5B87A0', cyan: '#72D5F4' };
  const bases = [permafrost.magenta, permafrost.red, permafrost.yellow, permafrost.green, permafrost.muted];
  assert.ok(P.difference(permafrost.magenta, permafrost.blue) < 12);
  assert.equal(P.distinct(bases, [permafrost.blue, permafrost.cyan], 12), permafrost.cyan);
});

test('agents working skip a color close to another state, as Everforest cyan is to its green', () => {
  const bases = ['#d699b6', '#e67e80', '#dbbc7f', '#a7c080', '#475258'];
  assert.ok(P.difference('#83c092', '#a7c080') < 12);
  assert.equal(P.distinct(bases, ['#83c092', '#7fbbb3'], 12), '#7fbbb3');
});

test('where no color is clearly apart, agents take the one whose nearest state is farthest', () => {
  // Omarchy's White theme is all grays.
  const bases = ['#2e2e2e', '#2a2a2a', '#4a4a4a', '#3a3a3a', '#808080'];
  assert.equal(P.distinct(bases, ['#1a1a1a', '#3e3e3e', '#6e6e6e', '#000000'], 12), '#000000');
});

test('a value that is not a color is never chosen', () => {
  assert.equal(P.difference('#123456', 'blue'), -1);
  assert.equal(P.distinct(['#2e2e2e'], ['', '#abc', 'blue'], 12), '');
  assert.equal(P.distinct(['#2e2e2e'], ['', '#ff2e2e2e', '#90ee90'], 12), '#90ee90');
});
