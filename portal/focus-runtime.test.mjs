import test from 'node:test';
import assert from 'node:assert/strict';

import { createFocusRuntime } from './focus-runtime.mjs';

test('focus request is non-optimistic and enforces one in-flight command', async () => {
  const calls = [];
  const session = {
    mode: 'control-v2', eligible: true, focused: false, nativeGeneration: 3, revision: 4,
  };
  const runtime = createFocusRuntime({
    invokeCommand: async (command, args) => { calls.push([command, args]); return true; },
    getSession: () => ({ ...session }),
    startFocusedInput: async () => {},
    activateLocalControl: async () => {},
  });
  assert.equal(await runtime.request({ controllerInitiated: true }), true);
  assert.equal(await runtime.request(), false);
  assert.equal(session.focused, false);
  assert.equal(calls[0][1].expectedRevision, 4);
});

test('authoritative grant starts input before activation and preserves controller provenance', async () => {
  const order = [];
  const session = {
    mode: 'control-v2', eligible: true, focused: false, nativeGeneration: 3, revision: 4,
  };
  const runtime = createFocusRuntime({
    invokeCommand: async () => true,
    getSession: () => ({ ...session }),
    startFocusedInput: async () => order.push('input'),
    activateLocalControl: async ({ controllerInitiated }) => order.push(`activate:${controllerInitiated}`),
  });
  await runtime.request({ controllerInitiated: true });
  session.focused = true;
  session.focusGeneration = 8;
  session.revision = 5;
  assert.equal(await runtime.observeSession(), true);
  assert.deepEqual(order, ['input', 'activate:true']);
});

test('release carries authoritative revision and focus generation', async () => {
  let call;
  const runtime = createFocusRuntime({
    invokeCommand: async (command, args) => { call = [command, args]; return true; },
    getSession: () => ({
      mode: 'control-v2', focused: true, nativeGeneration: 6, revision: 9, focusGeneration: 12,
    }),
    startFocusedInput: async () => {},
    activateLocalControl: async () => {},
  });
  assert.equal(await runtime.release(), true);
  assert.equal(call[0], 'iroh_client_release_focus');
  assert.equal(call[1].expectedRevision, 9);
  assert.equal(call[1].expectedFocusGeneration, 12);
});
