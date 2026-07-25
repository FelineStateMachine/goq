import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
const css = await readFile(new URL('./style.css', import.meta.url), 'utf8');
const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');
const roster = await readFile(new URL('./roster-ui.mjs', import.meta.url), 'utf8');

test('viewer and focus status is textual, live, and not color-only', () => {
  assert.match(html, /id="viewer-status"[^>]*aria-live="polite"/);
  assert.match(html, /id="roster-summary"[^>]*aria-live="polite"/);
  assert.match(html, /id="roster-list"[^>]*aria-label="Connected viewers"/);
  assert.match(roster, /you hold control/);
  assert.match(roster, /control available/);
  assert.match(roster, /view only/);
  assert.match(roster, /input eligible/);
});

test('view-only v2 spectators have no focus action but retain audio and drawer access', () => {
  assert.match(main, /classList\.toggle\('hidden', v2 && !sessionStateRuntime\.eligible\)/);
  assert.match(html, /id="audio-toggle"[^>]*data-controller-focus/);
  assert.match(html, /onclick="togglePanel\(true\)"/);
  assert.match(main, /spectating · request control/);
});

test('roster rendering is text-safe and responsive controller focus remains visible', () => {
  assert.doesNotMatch(roster, /innerHTML|insertAdjacentHTML|outerHTML/);
  assert.match(roster, /textContent = text/);
  assert.match(css, /\.link-btn:focus-visible,[\s\S]*\.link-btn\.controller-focus[\s\S]*outline:\s*2px solid var\(--moss\)/);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.topbar-right[\s\S]*min-width:\s*0/);
});
