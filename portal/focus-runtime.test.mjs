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

test('holder approve and deny are bound to the exact proposal and focus generation', async () => {
  const calls = [];
  const session = {
    mode: 'control-v2', eligible: true, focused: true, nativeGeneration: 6,
    revision: 9, focusGeneration: 12,
    snapshot: {
      self_presence_id: 'viewer-a',
      viewers: [{ presence_id: 'viewer-a', session_id: 1, you: true }],
      focus: { state: 'held', holder: 'viewer-a', session_id: 1, focus_generation: 12 },
      focus_proposal: {
        proposal_id: 4, holder: 'viewer-a', holder_session_id: 1,
        requester: 'viewer-b', requester_session_id: 2,
      },
    },
  };
  const runtime = createFocusRuntime({
    invokeCommand: async (command, args) => { calls.push([command, args]); return true; },
    getSession: () => structuredClone(session),
    startFocusedInput: async () => {},
    activateLocalControl: async () => {},
  });
  assert.equal(await runtime.approve(), true);
  assert.equal(calls[0][0], 'iroh_client_approve_focus');
  assert.equal(calls[0][1].expectedProposalId, 4);
  assert.equal(calls[0][1].expectedFocusGeneration, 12);
  runtime.observeResult({ request_id: calls[0][1].requestId, accepted: true });
  assert.equal(await runtime.deny(), true);
  assert.equal(calls[1][0], 'iroh_client_deny_focus');
});

test('configured owner preemption remains non-optimistic and suppresses granting A on activation', async () => {
  const order = [];
  const session = {
    mode: 'control-v2', eligible: true, focused: false, nativeGeneration: 8,
    revision: 10, focusGeneration: null,
    snapshot: {
      self_is_focus_owner: true,
      focus: { state: 'held', holder: 'viewer-b', session_id: 2, focus_generation: 20 },
    },
  };
  const runtime = createFocusRuntime({
    invokeCommand: async (command, args) => { order.push([command, args]); return true; },
    getSession: () => structuredClone(session),
    startFocusedInput: async () => order.push('input'),
    activateLocalControl: async ({ controllerInitiated }) => order.push(`activate:${controllerInitiated}`),
  });
  assert.equal(await runtime.preempt({ controllerInitiated: true }), true);
  assert.equal(session.focused, false);
  session.focused = true;
  session.focusGeneration = 21;
  session.revision = 12;
  assert.equal(await runtime.observeSession(), true);
  assert.deepEqual(order.slice(1), ['input', 'activate:true']);
});

test('denied and expired handoff requests clear without optimistic possession', async () => {
  const session = {
    mode: 'control-v2', eligible: true, focused: false, nativeGeneration: 3, revision: 4,
    snapshot: { self_presence_id: 'viewer-b', transition_reason: 'handoff_requested' },
  };
  const runtime = createFocusRuntime({
    invokeCommand: async () => true,
    getSession: () => structuredClone(session),
    startFocusedInput: async () => {},
    activateLocalControl: async () => {},
  });
  await runtime.request();
  session.revision = 5;
  session.snapshot.transition_reason = 'proposal_expired';
  await runtime.observeSession();
  assert.equal(runtime.snapshot().pending, false);
  assert.equal(session.focused, false);
});
