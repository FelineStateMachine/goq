import assert from 'node:assert/strict';
import test from 'node:test';

import {
  MEDIA_TRANSPORT_ALLOWLIST,
  newConnectionState,
  normalizeMediaTransport,
} from './connection-state.mjs';

function connectedResult(overrides = {}) {
  return {
    connected: true,
    media_generation: 1,
    audio_available: false,
    audio_generation: null,
    relative_pointer_available: false,
    pointer_position_feedback_available: false,
    absolute_pointer_available: false,
    keyboard_available: true,
    text_available: false,
    gamepad_available: false,
    control_available: true,
    media_transport: 'iroh-moq',
    development_mode: false,
    ...overrides,
  };
}

function harness(results = [connectedResult()]) {
  const commands = [];
  const attempts = [];
  const statuses = [];
  const events = [];
  let nextResult = 0;
  const state = newConnectionState({
    invokeCommand: async (command, args) => {
      commands.push({ command, args });
      if (command === 'iroh_client_connect') return results[nextResult++];
      return undefined;
    },
    createChannel: class FakeChannel {},
    createAttempt: async ({ createChannel }) => {
      const attempt = {
        id: attempts.length + 1,
        connectionArgs: { frameChannel: new createChannel() },
      };
      attempts.push(attempt);
      events.push(`create:${attempt.id}`);
      return attempt;
    },
    validateConnectedResult: (_result, attempt) => events.push(`validate:${attempt.id}`),
    activateAttempt: (_result, attempt) => events.push(`activate:${attempt.id}`),
    closeAttempt: (attempt, context) => events.push(
      `close:${attempt?.id ?? 'none'}:${context.committed ? 'committed' : 'uncommitted'}`,
    ),
    teardownAttempt: async (attempt) => events.push(`teardown:${attempt?.id ?? 'none'}`),
    beforeDisconnect: async () => events.push('before-disconnect'),
    teardownConnection: async () => events.push('teardown-connection'),
    onStatus: (kind, detail) => statuses.push([kind, detail]),
    onConnected: async ({ attempt }) => events.push(`connected:${attempt.id}`),
    onDisconnected: async () => events.push('disconnected'),
    onFailure: ({ error }) => events.push(`failure:${error.message}`),
  });
  return { state, commands, attempts, statuses, events };
}

test('pins every Rust diagnostic transport name including preferred iroh-moq', () => {
  assert.deepEqual([...MEDIA_TRANSPORT_ALLOWLIST], [
    'iroh-moq',
    'grouped-v3',
  ]);
  for (const transport of MEDIA_TRANSPORT_ALLOWLIST) {
    assert.equal(normalizeMediaTransport(transport), transport);
  }
  assert.equal(normalizeMediaTransport('iroh-moq'), 'iroh-moq');
  assert.equal(normalizeMediaTransport(null), 'unknown');
});

test('production mode rejects an empty pin while development mode permits it', async () => {
  const production = harness();
  assert.equal(await production.state.connect({ pin: '   ' }), false);
  assert.deepEqual(production.commands, []);
  assert.deepEqual(production.statuses, [['err', 'enter pin']]);
  assert.equal(production.state.connecting, false);

  const development = harness([connectedResult({ development_mode: true })]);
  development.state.setDevelopmentMode(true);
  assert.equal(await development.state.connect({ pin: '   ' }), true);
  assert.equal(development.commands[0].args.pin, '');
  assert.deepEqual(development.statuses, [
    ['pending', 'connecting...'],
    ['ok', 'connected · dev direct-node'],
  ]);
});

test('connect is single-flight', async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  let connectCalls = 0;
  const state = newConnectionState({
    invokeCommand: async (command) => {
      if (command === 'iroh_client_connect') {
        connectCalls += 1;
        await pending;
        return connectedResult();
      }
      return undefined;
    },
  });
  const first = state.connect({ pin: '1234' });
  assert.equal(state.connecting, true);
  assert.equal(await state.connect({ pin: '1234' }), false);
  release();
  assert.equal(await first, true);
  assert.equal(connectCalls, 1);
});

test('invalid connected generations close committed native state exactly once', async () => {
  for (const invalid of [
    connectedResult({ media_generation: 0 }),
    connectedResult({ audio_available: true, audio_generation: 0 }),
  ]) {
    const current = harness([invalid]);
    assert.equal(await current.state.connect({ pin: '1234' }), false);
    assert.deepEqual(current.commands.map(({ command }) => command), [
      'iroh_client_connect',
      'iroh_client_disconnect',
    ]);
    assert.ok(current.events.includes('close:1:committed'));
    assert.match(current.events.at(-1), /^failure:host returned an invalid/);
  }
});

test('a rejected result tears down without disconnecting uncommitted native state', async () => {
  const current = harness([{ connected: false }]);
  assert.equal(await current.state.connect({ pin: '1234' }), false);
  assert.deepEqual(current.commands.map(({ command }) => command), ['iroh_client_connect']);
  assert.deepEqual(current.events, [
    'create:1',
    'validate:1',
    'close:1:uncommitted',
    'teardown:1',
  ]);
  assert.deepEqual(current.statuses.at(-1), ['err', 'failed']);
});

test('successful connect normalizes capabilities and forces inconsistent control view-only', async () => {
  const current = harness([connectedResult({
    keyboard_available: false,
    control_available: true,
  })]);
  assert.equal(await current.state.connect({ pin: ' 1234 ' }), true);
  assert.deepEqual(current.state.inputCapabilities, {
    relativePointer: false,
    pointerPositionFeedback: false,
    absolutePointer: false,
    keyboard: false,
    text: false,
    gamepad: false,
    control: false,
  });
  assert.equal(current.state.mediaTransport, 'iroh-moq');
  assert.deepEqual(current.statuses, [
    ['pending', 'connecting...'],
    ['ok', 'connected · view only'],
  ]);
  assert.deepEqual(current.commands[0].args.pin, '1234');
});

test('disconnect is single-flight and resets capabilities and transport', async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const current = harness();
  assert.equal(await current.state.connect({ pin: '1234' }), true);
  current.events.length = 0;

  const state = newConnectionState({
    invokeCommand: async (command) => {
      if (command === 'iroh_client_connect') return connectedResult();
      await pending;
    },
  });
  await state.connect({ pin: '1234' });
  const first = state.disconnect();
  assert.equal(state.disconnecting, true);
  assert.equal(await state.disconnect(), false);
  release();
  assert.equal(await first, true);
  assert.equal(state.connected, false);
  assert.equal(state.mediaTransport, 'unknown');
  assert.equal(state.inputCapabilities.control, false);
});

test('connect disconnect connect creates fresh attempts and channels', async () => {
  const current = harness([connectedResult(), connectedResult({ media_generation: 2 })]);
  assert.equal(await current.state.connect({ pin: '1234' }), true);
  assert.equal(await current.state.disconnect(), true);
  assert.equal(await current.state.connect({ pin: '1234' }), true);
  assert.equal(current.attempts.length, 2);
  assert.notEqual(
    current.attempts[0].connectionArgs.frameChannel,
    current.attempts[1].connectionArgs.frameChannel,
  );
  assert.deepEqual(current.commands.map(({ command }) => command), [
    'iroh_client_connect',
    'iroh_client_disconnect',
    'iroh_client_connect',
  ]);
});

test('connect failures clear connecting and report lifecycle order', async () => {
  const statuses = [];
  const events = [];
  const state = newConnectionState({
    invokeCommand: async () => { throw new Error('offline'); },
    createAttempt: async () => {
      events.push('create');
      return { connectionArgs: {} };
    },
    closeAttempt: () => events.push('close'),
    teardownAttempt: async () => events.push('teardown'),
    onFailure: ({ error }) => events.push(`failure:${error.message}`),
    onStatus: (kind, detail) => statuses.push(`${kind}:${detail}`),
  });
  assert.equal(await state.connect({ pin: '1234' }), false);
  assert.equal(state.connecting, false);
  assert.deepEqual(events, ['create', 'close', 'teardown', 'failure:offline']);
  assert.deepEqual(statuses, ['pending:connecting...', 'err:error']);
});

test('connected and disconnected callbacks follow status and teardown boundaries', async () => {
  const current = harness();
  assert.equal(await current.state.connect({ pin: '1234' }), true);
  assert.deepEqual(current.events, ['create:1', 'validate:1', 'activate:1', 'connected:1']);
  current.events.length = 0;
  assert.equal(await current.state.disconnect(), true);
  assert.deepEqual(current.events, ['before-disconnect', 'teardown-connection', 'disconnected']);
  assert.deepEqual(current.statuses.at(-1), ['ok', 'connected']);
});
