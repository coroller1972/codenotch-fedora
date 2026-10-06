// Run with node --test test-usage-pace.cjs from windows/.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const html = readFileSync(join(__dirname, 'codenotch/ui/notch.html'), 'utf8');
function markedSource(document, name) {
  const begin = `// BEGIN TESTABLE ${name} SELECTOR`;
  const end = `// END TESTABLE ${name} SELECTOR`;
  assert.equal(document.split(begin).length, 2, `exactly one ${name} begin marker`);
  assert.equal(document.split(end).length, 2, `exactly one ${name} end marker`);
  return document.slice(document.indexOf(begin) + begin.length, document.indexOf(end));
}
const context = vm.createContext({});
vm.runInContext(markedSource(html, 'PACE'), context);
vm.runInContext(markedSource(html, 'HEADLINE'), context);
const { paceOf, paceFigure } = context;

const HOUR = 3600, NOW = 1_000_000_000_000;
const session = (used, hoursLeft) => ({ used, duration: 5 * HOUR, resets_at: NOW + hoursLeft * HOUR * 1000 });

test('Used ahead of time gone is a deficit, behind it a reserve', () => {
  // 2.5 of 5 hours gone: half the window
  assert.ok(Math.abs(paceOf(session(0.6, 2.5), NOW) - 10) < 1e-9);
  assert.ok(Math.abs(paceOf(session(0.4, 2.5), NOW) + 10) < 1e-9);
});
test('No length or no reset is no pace, never a guess', () => {
  assert.equal(paceOf({ used: 0.5, resets_at: NOW + 1000 }, NOW), null);
  assert.equal(paceOf({ used: 0.5, duration: 5 * HOUR }, NOW), null);
  assert.equal(paceOf(session(0.5, 0), NOW), null, 'a window already resetting');
  assert.equal(paceOf(null, NOW), null);
});
test('A reset just beyond one cycle counts as nothing gone, an exhausted window as full', () => {
  assert.ok(Math.abs(paceOf(session(0.2, 6), NOW) - 20) < 1e-9);
  assert.ok(Math.abs(paceOf(session(1.5, 2.5), NOW) - 50) < 1e-9);
});
test('The figure is one decimal, whole numbers bare, a sliver never zero', () => {
  assert.equal(paceFigure(12.04), '12');
  assert.equal(paceFigure(-3.26), '3.3');
  assert.equal(paceFigure(0.04), '<0.1');
  assert.equal(paceFigure(0), '0');
});
test('A Claude credit seat reads its balance; a missing session otherwise stays a dash', () => {
  const pick = windows => context.headlineOf({ windows }, 'claude')?.id ?? null;
  assert.equal(pick([{ id: 'spend', money: { spent: 1, remaining: 9 } }]), 'spend');
  assert.equal(pick([{ id: 'session' }, { id: 'spend', money: {} }]), 'session');
  assert.equal(pick([{ id: 'weekly_all' }]), null);
});
