import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

test('onboarding and confirmation actions are controller reachable', () => {
  for (const id of ['enrollment-pin', 'derive-portal-id', 'choose-invitation', 'confirm-invitation', 'reset-enrollment', 'confirm-reset-enrollment']) {
    assert.match(html, new RegExp(`id="${id}"[^>]*data-controller-focus`));
  }
  assert.match(main, /const invitation = document\.getElementById\('invitation-overlay'\)/);
  assert.match(main, /const enrollment = document\.getElementById\('enrollment-overlay'\)/);
  assert.match(main, /const resetEnrollment = document\.getElementById\('reset-enrollment-overlay'\)/);
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
