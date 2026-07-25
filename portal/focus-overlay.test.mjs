import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';

import {
  focusOverlayStillCurrent,
  handoffProposalForSelf,
  preemptionCandidateForSelf,
} from './focus-overlay.mjs';

const html = fs.readFileSync(new URL('./index.html', import.meta.url), 'utf8');
const main = fs.readFileSync(new URL('./main.js', import.meta.url), 'utf8');

function session() {
  return {
    eligible: true,
    focused: true,
    snapshot: {
      revision: 4,
      self_presence_id: 'viewer-a',
      self_is_focus_owner: false,
      viewers: [
        { presence_id: 'viewer-a', session_id: 1, you: true, input_capable: true },
        { presence_id: 'viewer-b', session_id: 2, you: false, input_capable: true },
      ],
      focus: { state: 'held', holder: 'viewer-a', session_id: 1, focus_generation: 8 },
      focus_proposal: {
        proposal_id: 3,
        holder: 'viewer-a',
        holder_session_id: 1,
        requester: 'viewer-b',
        requester_session_id: 2,
      },
    },
  };
}

test('holder sees one opaque handoff proposal and stale proposals dismiss immediately', () => {
  const current = session();
  const overlay = handoffProposalForSelf(current);
  assert.deepEqual(overlay, {
    type: 'handoff', proposalId: 3, requester: 'viewer-b', revision: 4,
  });
  assert.equal(focusOverlayStillCurrent(overlay, current), true);
  current.snapshot = { ...current.snapshot, revision: 5, focus_proposal: null };
  assert.equal(focusOverlayStillCurrent(overlay, current), false);
});

test('only configured owner receives an exact-holder preemption candidate', () => {
  const current = session();
  current.focused = false;
  current.snapshot = {
    ...current.snapshot,
    self_is_focus_owner: true,
    focus_proposal: null,
    focus: { state: 'held', holder: 'viewer-b', session_id: 2, focus_generation: 9 },
  };
  const overlay = preemptionCandidateForSelf(current);
  assert.equal(overlay.holder, 'viewer-b');
  assert.equal(focusOverlayStillCurrent(overlay, current), true);
  current.snapshot = { ...current.snapshot, revision: 5, focus: { state: 'vacant', slot: 0 } };
  assert.equal(focusOverlayStillCurrent(overlay, current), false);
});

test('handoff and preemption dialogs are controller-scoped with safe defaults and Back behavior', () => {
  assert.match(html, /id="deny-focus-handoff" data-controller-focus/);
  assert.match(html, /id="cancel-focus-preempt" data-controller-focus/);
  assert.match(main, /if \(!handoff\.classList\.contains\('hidden'\)\) return handoff/);
  assert.match(main, /if \(!preempt\.classList\.contains\('hidden'\)\) return preempt/);
  assert.match(main, /void denyFocusHandoff\(\)/);
  assert.match(main, /cancelFocusPreempt\(\)/);
});
