// Run with node --test test-token-stats.cjs from windows/.
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
vm.runInContext(markedSource(html, 'TOKENS'), context);
const { tokensText, durationText, last30Days, todayTokens } = context;

test('Token figures are written as the Mac writes them', () => {
  assert.equal(tokensText(1_234_567_890), '1.23B');
  assert.equal(tokensText(4_500_000), '4.5M');
  assert.equal(tokensText(450_400), '450K');
  assert.equal(tokensText(999), '999');
  assert.equal(tokensText(null), '—', 'missing is a dash, never zero');
});
test('Durations round to minutes and drop an empty remainder', () => {
  assert.equal(durationText(2712.5), '45m');
  assert.equal(durationText(3900), '1h 5m');
  assert.equal(durationText(7200), '2h');
  assert.equal(durationText(10), '1m');
  assert.equal(durationText(0), '—');
});
test('The chart is the 30 calendar days ending today, gaps drawn as nothing', () => {
  const now = new Date(2026, 9, 6, 15, 0).getTime();
  const days = last30Days([{ date: '2026-10-06', tokens: 5 }, { date: '2026-09-07', tokens: 9 }, { date: '2026-09-06', tokens: 99 }], now);
  assert.equal(days.length, 30);
  assert.equal(days[29].date, '2026-10-06');
  assert.equal(days[29].tokens, 5);
  assert.equal(days[0].date, '2026-09-07');
  assert.equal(days[0].tokens, 9);
  assert.equal(days.reduce((a, d) => a + d.tokens, 0), 14, 'the 31st day back is outside the window');
  assert.equal(new Set(days.map(d => d.date)).size, 30, 'no day repeated or skipped');
});
test('Today is pending until published, and a published zero is a zero', () => {
  const now = new Date(2026, 9, 6, 9, 0).getTime();
  assert.equal(todayTokens([{ date: '2026-10-05', tokens: 3 }], now), null);
  assert.equal(todayTokens([{ date: '2026-10-06', tokens: 0 }], now), 0);
});
