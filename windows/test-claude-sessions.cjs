// Run with node --test test-claude-sessions.cjs from windows/.
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
vm.runInContext(markedSource(html, 'SESSIONS'), context);
const { orderSessions, elapsedParts, sessionDetail, sinceOf } = context;

test('Waiting first, then working, complete, idle; newest first within each', () => {
  const ids = orderSessions([
    { id: 'idle', state: 'idle', started: 9 },
    { id: 'old-run', state: 'running', started: 1 },
    { id: 'done', state: 'done', started: 1, total: 1 },
    { id: 'new-run', state: 'running', started: 5 },
    { id: 'wait', state: 'attention', started: 0 },
  ]).map(s => s.id);
  assert.deepEqual([...ids], ['wait', 'new-run', 'old-run', 'done', 'idle']);
});
test('A finished run is timed from its end', () => {
  assert.equal(sinceOf({ state: 'done', started: 100, total: 50 }), 150);
  assert.equal(sinceOf({ state: 'running', started: 100, total: 50 }), 100);
});
test('Elapsed reads as just now, minutes, hours, as the Mac words it', () => {
  const at = m => 1_000_000 - m * 60_000;
  assert.deepEqual([...elapsedParts(1_000_000 - 30_000, 1_000_000)], [-1, 0]);
  assert.deepEqual([...elapsedParts(at(1), 1_000_000)], [0, 1]);
  assert.deepEqual([...elapsedParts(at(59.4), 1_000_000)], [0, 59]);
  assert.deepEqual([...elapsedParts(at(120), 1_000_000)], [2, 0]);
  assert.deepEqual([...elapsedParts(at(125), 1_000_000)], [2, 5]);
});
test('The detail is what it waits on, the step it is on, or what was asked', () => {
  assert.equal(sessionDetail({ state: 'attention', attn: 'Allow Bash?', last: 'x', prompt: 'p' }), 'Allow Bash?');
  assert.equal(sessionDetail({ state: 'running', last: '🔧 Edit', prompt: 'p' }), '🔧 Edit');
  assert.equal(sessionDetail({ state: 'done', last: '🔧 Edit', prompt: 'fix it' }), 'fix it');
  assert.equal(sessionDetail({ state: 'idle' }), '');
});
