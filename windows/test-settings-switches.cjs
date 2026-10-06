// Run with node --test test-settings-switches.cjs from windows/.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const assert = require('node:assert/strict');
const { test } = require('node:test');

const html = readFileSync(join(__dirname, 'codenotch/ui/settings.html'), 'utf8');

test('Every saved switch is read back when Settings opens', () => {
  // A switch left out of the first read opens showing off whatever was saved
  const switches = [...html.matchAll(/const (\w+) = remote\(/g)].map(m => m[1]);
  assert.ok(switches.length >= 5, 'found the switches');
  for (const name of switches) {
    assert.ok(html.includes(`${name}.refresh(),`) || html.includes(`${name}.refresh()\n`),
      `${name} is refreshed when Settings opens`);
  }
});
