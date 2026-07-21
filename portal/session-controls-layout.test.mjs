import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
const css = await readFile(new URL('./style.css', import.meta.url), 'utf8');

test('session controls live in the bottom bar instead of over the stream', () => {
  const mainEnd = html.indexOf('</main>');
  const bottomBarStart = html.indexOf('<div class="bottombar">');
  const bottomBarEnd = html.indexOf('<div class="panel-overlay"');
  const bottomBar = html.slice(bottomBarStart, bottomBarEnd);
  const controlStart = html.indexOf('<div class="control-bar" id="control-bar">');
  const disconnectButton = html.indexOf('id="action-disconnect"');
  const controlButton = html.indexOf('id="control-toggle"');
  const audioButton = html.indexOf('id="audio-toggle"');

  assert.ok(mainEnd >= 0 && mainEnd < bottomBarStart);
  assert.ok(disconnectButton > bottomBarStart && disconnectButton < bottomBarEnd);
  assert.ok(controlStart > bottomBarStart && controlStart < bottomBarEnd);
  assert.ok(controlButton > controlStart && controlButton < bottomBarEnd);
  assert.ok(audioButton > controlStart && audioButton < bottomBarEnd);
  assert.doesNotMatch(bottomBar, /id="stream-/);
  assert.doesNotMatch(css, /\.control-bar\s*\{[^}]*position:\s*absolute/s);
  assert.match(css, /\.control-bar\.visible \+ \.bottombar-end \.sep\s*\{\s*display:\s*none;/);
});

test('stream diagnostics remain exclusive to the Info panel', () => {
  const panelStart = html.indexOf('<aside class="panel" id="panel"');
  const panelEnd = html.indexOf('</aside>', panelStart);
  const panel = html.slice(panelStart, panelEnd);
  const streamIds = [...html.matchAll(/id="(stream-[^"]+)"/g)].map((match) => match[1]);

  assert.doesNotMatch(html, /id="stats"/);
  assert.ok(panelStart >= 0 && panelEnd > panelStart);
  assert.ok(streamIds.length > 0);
  for (const id of streamIds) assert.match(panel, new RegExp(`id="${id}"`));
  assert.match(html, /id="audio-toggle"[^>]*aria-label="Audio unavailable\. Not connected"/);
  assert.match(html, /type="button"[^>]*id="audio-toggle"/);
  assert.match(css, /#audio-toggle\s*\{[^}]*min-width:\s*40px;[^}]*min-height:\s*32px;/s);
});
