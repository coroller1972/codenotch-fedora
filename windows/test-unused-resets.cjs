// Run with node --test test-unused-resets.cjs from windows/.
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
vm.runInContext(markedSource(html, 'RESETS'), context);
const { availableResets } = context;
const NOW = 1_000_000;

test('The reported total stands, less what has expired since', () => {
  const rc = { available_count: 3, credits: [
    { status: 'available', expires_at: NOW - 1 },
    { status: 'available', expires_at: NOW + 500 },
    { status: 'used', expires_at: NOW - 1 },
  ] };
  const r = availableResets(rc, NOW);
  assert.equal(r.count, 2, 'a truncated list does not lower the total; only an expired available one does');
  assert.equal(r.next, NOW + 500);
});
test('A grant standing for several resets takes them all when it ends', () => {
  const rc = { available_count: 4, credits: [{ status: 'available', expires_at: NOW - 1, count: 3 }, { status: 'available', expires_at: null }] };
  const r = availableResets(rc, NOW);
  assert.equal(r.count, 1);
  assert.equal(r.next, null, 'no expiry known, none invented');
});
test('None left, or nothing reported, draws no section', () => {
  assert.equal(availableResets({ available_count: 0, credits: [] }, NOW), null);
  assert.equal(availableResets({ available_count: 1, credits: [{ status: 'available', expires_at: NOW }] }, NOW), null);
  assert.equal(availableResets(null, NOW), null);
});
