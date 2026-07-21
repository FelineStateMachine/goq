import assert from 'node:assert/strict';
import test from 'node:test';

import { parsePointerFeedbackMessage } from './pointer-feedback.mjs';

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
