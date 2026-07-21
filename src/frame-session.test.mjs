import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FRAME_CHANNEL_CAPACITY,
  activateFrameSession,
  isActiveFrameSession,
  newFrameSession,
  stageFrameAcknowledgment,
  stageLegacyFrame,
} from './frame-session.mjs';

test('pre-result acknowledgments stay bounded and flush with the active generation', () => {
  const session = newFrameSession();
  for (let index = 0; index < FRAME_CHANNEL_CAPACITY; index++) {
    assert.equal(stageFrameAcknowledgment(session), null);
  }
  assert.throws(() => stageFrameAcknowledgment(session), /capacity exceeded/);
  assert.deepEqual(activateFrameSession(session, 12), {
    acknowledgments: [12, 12, 12, 12],
    legacyFrames: [],
  });
  assert.equal(stageFrameAcknowledgment(session), 12);
});

test('higher-generation legacy frames evict stale pre-result capacity', () => {
  const session = newFrameSession();
  for (let index = 0; index < FRAME_CHANNEL_CAPACITY; index++) {
    assert.deepEqual(stageLegacyFrame(session, { generation: 8, sequence: index }), {
      acknowledgments: [],
      staged: true,
      accepted: false,
    });
  }
  assert.deepEqual(stageLegacyFrame(session, { generation: 9, sequence: 1 }), {
    acknowledgments: [8, 8, 8, 8],
    staged: true,
    accepted: false,
  });
  assert.deepEqual(stageLegacyFrame(session, { generation: 8, sequence: 99 }), {
    acknowledgments: [8],
    staged: false,
    accepted: false,
  });
  assert.deepEqual(activateFrameSession(session, 9), {
    acknowledgments: [9],
    legacyFrames: [{ generation: 9, sequence: 1 }],
  });
});

test('only the current non-closing frame session owns callbacks', () => {
  const current = newFrameSession();
  const stale = newFrameSession();
  assert.equal(isActiveFrameSession(current, current), true);
  assert.equal(isActiveFrameSession(stale, current), false);
  current.closing = true;
  assert.equal(isActiveFrameSession(current, current), false);
});
