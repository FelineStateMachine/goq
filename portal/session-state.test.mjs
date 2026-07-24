import test from 'node:test';
import assert from 'node:assert/strict';

import { createSessionState } from './session-state.mjs';

function snapshot(revision, focus = { state: 'vacant', slot: 0 }) {
  return {
    revision,
    self_presence_id: 'viewer-1',
    viewers: [{ presence_id: 'viewer-1', session_id: 7, input_capable: true, you: true }],
    focus,
    transition_reason: 'initial',
    media: { generation_id: 7, broadcast_name: 'sigil/session/7/video' },
  };
}

test('accepts revision gaps and ignores stale or native-generation-mismatched snapshots', () => {
  const runtime = createSessionState();
  runtime.acceptConnection({
    nativeGeneration: 3,
    controlProtocol: 'control-v2',
    initialSnapshot: snapshot(1),
  });
  assert.equal(runtime.applyNativeSnapshot({ native_generation: 4, snapshot: snapshot(2) }), false);
  assert.equal(runtime.applyNativeSnapshot({ native_generation: 3, snapshot: snapshot(8) }), true);
  assert.equal(runtime.revision, 8);
  assert.equal(runtime.applyNativeSnapshot({ native_generation: 3, snapshot: snapshot(7) }), false);
});

test('publishes focus teardown before exposing self-focus loss', () => {
  const order = [];
  const runtime = createSessionState({
    onFocusLoss: () => order.push(`teardown:${runtime.focused}`),
    onChange: (state) => order.push(`publish:${state.focused}`),
  });
  runtime.acceptConnection({
    nativeGeneration: 2,
    controlProtocol: 'control-v2',
    initialSnapshot: snapshot(1, {
      state: 'held', slot: 0, holder: 'viewer-1', session_id: 7, focus_generation: 4,
    }),
  });
  order.length = 0;
  runtime.applyNativeSnapshot({ native_generation: 2, snapshot: snapshot(2) });
  assert.deepEqual(order, ['teardown:true', 'publish:false']);
});

test('separates eligibility, host focus possession, and local activation', () => {
  const runtime = createSessionState();
  runtime.acceptConnection({
    nativeGeneration: 1,
    controlProtocol: 'control-v2',
    initialSnapshot: snapshot(1),
  });
  assert.equal(runtime.eligible, true);
  assert.equal(runtime.focused, false);
  assert.equal(runtime.setLocallyActive(true), false);
  runtime.applyNativeSnapshot({
    native_generation: 1,
    snapshot: snapshot(2, {
      state: 'held', slot: 0, holder: 'viewer-1', session_id: 7, focus_generation: 9,
    }),
  });
  assert.equal(runtime.focused, true);
  assert.equal(runtime.locallyActive, false);
  assert.equal(runtime.setLocallyActive(true), true);
  assert.equal(runtime.locallyActive, true);
});

test('legacy mode retains explicit implicit-focus behavior', () => {
  const runtime = createSessionState();
  runtime.acceptConnection({
    nativeGeneration: 5,
    controlProtocol: 'legacy-v1',
    initialSnapshot: null,
    eligible: true,
  });
  assert.equal(runtime.mode, 'legacy-v1');
  assert.equal(runtime.focused, true);
});
