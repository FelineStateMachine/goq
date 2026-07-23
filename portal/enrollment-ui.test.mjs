import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

test('onboarding and confirmation actions are controller reachable', () => {
  for (const id of ['enrollment-pin', 'derive-portal-id', 'choose-invitation', 'confirm-invitation', 'reset-enrollment', 'reset-enrollment-intro', 'confirm-reset-enrollment']) {
    assert.match(html, new RegExp(`id="${id}"[^>]*data-controller-focus`));
  }
  assert.match(main, /const invitation = document\.getElementById\('invitation-overlay'\)/);
  assert.match(main, /const enrollment = document\.getElementById\('enrollment-overlay'\)/);
  assert.match(main, /const resetEnrollment = document\.getElementById\('reset-enrollment-overlay'\)/);
});

test('disconnected intro keeps enrollment reset inside the active controller scope', () => {
  const introStart = html.indexOf('<div class="overlay-screen hidden" id="intro">');
  const topbarStart = html.indexOf('<div class="topbar">');
  assert.notEqual(introStart, -1);
  assert.ok(topbarStart > introStart);
  const intro = html.slice(introStart, topbarStart);
  assert.match(intro, /id="reset-enrollment-intro"[^>]*data-controller-focus/);
  assert.match(main, /getElementById\('reset-enrollment-intro'\)\.classList\.toggle\('hidden', !enrollmentReady\)/);
});

test('enrollment reset is explicit and native', () => {
  assert.match(html, /Only continue after revoking enrollment on Sigil/);
  assert.match(main, /portal_reset_enrollment/);
  assert.match(main, /expectedHostNodeId: hostNodeId/);
});

test('raw invitation bytes stay in native commands instead of the DOM', () => {
  assert.doesNotMatch(html, /invitation-token|goq-invite-v1/);
  assert.match(main, /portal_import_invitation_file/);
  assert.doesNotMatch(main, /readAsText|FileReader/);
});

test('forced JPEG development mode is visibly labeled without bypassing enrollment', () => {
  assert.match(html, /id="dev-jpeg-badge"[^>]*>dev JPEG forced</);
  assert.match(main, /if \(mode\.force_jpeg\)/);
  assert.match(main, /getElementById\('dev-jpeg-badge'\)/);
  assert.match(main, /if \(!mode\.enabled\) return;/);
});
