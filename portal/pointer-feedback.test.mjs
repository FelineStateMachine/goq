import assert from 'node:assert/strict';
import test from 'node:test';

import { newPointerSession, parsePointerFeedbackMessage } from './pointer-feedback.mjs';

test('new pointer sessions pin feedback, channel, surface, and render defaults', () => {
  assert.deepEqual(newPointerSession(), {
    received: false,
    latest: null,
    failed: false,
    failureDetail: null,
    closing: false,
    channel: null,
    surfaceDimensions: null,
    remotePosition: null,
    remoteVisible: false,
  });
});

test('new pointer sessions isolate staged and rendered feedback state', () => {
  const staged = newPointerSession();
  const fresh = newPointerSession();
  staged.received = true;
  staged.latest = { sequence: 3, position: { x: 4, y: 5 }, pointer_visible: true };
  staged.surfaceDimensions = { width: 1280, height: 800 };
  staged.remotePosition = { x: 4, y: 5 };
  staged.remoteVisible = true;

  assert.deepEqual(fresh, newPointerSession());
  assert.equal(staged.received, true);
  assert.equal(staged.remoteVisible, true);
});

test('pointer feedback position envelopes preserve validated coordinates', () => {
  assert.deepEqual(
    parsePointerFeedbackMessage(
      { type: 'position', sequence: 7, position: { x: 1280, y: 800 } },
      { width: 2560, height: 1600 },
    ),
    {
      type: 'position',
      feedback: { sequence: 7, position: { x: 1280, y: 800 }, pointer_visible: true },
    },
  );
  assert.deepEqual(
    parsePointerFeedbackMessage(
      {
        type: 'position',
        sequence: 8,
        position: { x: 1280, y: 800 },
        pointer_visible: false,
      },
      { width: 2560, height: 1600 },
    ),
    {
      type: 'position',
      feedback: { sequence: 8, position: { x: 1280, y: 800 }, pointer_visible: false },
    },
  );
});

test('pointer feedback terminal envelopes are finite and explicit', () => {
  assert.deepEqual(
    parsePointerFeedbackMessage({ type: 'terminal', reason: 'eof' }),
    { type: 'terminal', reason: 'eof' },
  );
  assert.deepEqual(
    parsePointerFeedbackMessage({ type: 'terminal', reason: 'malformed' }),
    { type: 'terminal', reason: 'malformed' },
  );
  assert.throws(
    () => parsePointerFeedbackMessage({ type: 'terminal', reason: 'unknown' }),
    /invalid pointer feedback message/,
  );
});

test('pointer feedback envelopes reject malformed or out-of-surface positions', () => {
  assert.throws(
    () => parsePointerFeedbackMessage({ sequence: 1, position: null }),
    /invalid pointer feedback message/,
  );
  assert.throws(
    () => parsePointerFeedbackMessage(
      { type: 'position', sequence: 2, position: { x: 2560, y: 0 } },
      { width: 2560, height: 1600 },
    ),
    /outside the negotiated surface/,
  );
});
