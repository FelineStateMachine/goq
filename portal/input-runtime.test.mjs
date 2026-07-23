import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  INPUT_RETRY_MS,
  REGULAR_RELIABLE_INPUT_LIMIT,
  RELIABLE_INPUT_ENQUEUE_LIMIT,
  createInputRuntime,
} from './input-runtime.mjs';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

const fullCapabilities = Object.freeze({
  control: true,
  absolutePointer: true,
  relativePointer: true,
  keyboard: true,
  text: true,
  gamepad: true,
});

function newHarness({
  capabilities = fullCapabilities,
  invokeCommand = async () => true,
  connected = true,
  scheduleTimeout,
  scheduleMicrotask,
} = {}) {
  const fatal = [];
  const errors = [];
  const warnings = [];
  let activationResets = 0;
  const runtime = createInputRuntime({
    invokeCommand,
    getCapabilities: () => capabilities,
    isConnected: () => connected,
    onFatal: (detail) => fatal.push(detail),
    resetControllerActivation: () => { activationResets += 1; },
    scheduleTimeout,
    scheduleMicrotask,
    logger: {
      error: (...args) => errors.push(args),
      warn: (...args) => warnings.push(args),
    },
  });
  return {
    runtime,
    fatal,
    errors,
    warnings,
    get activationResets() { return activationResets; },
  };
}

test('capability filtering preserves the current event routing contract', () => {
  const capabilities = {
    control: true,
    absolutePointer: false,
    relativePointer: true,
    keyboard: false,
    text: true,
    gamepad: false,
  };
  const { runtime } = newHarness({
    capabilities,
    scheduleTimeout: () => 0,
  });

  assert.equal(runtime.pointerAvailable(), true);
  assert.equal(runtime.inputEventAvailable({ t: 'mm' }), false);
  assert.equal(runtime.inputEventAvailable({ t: 'mr' }), true);
  assert.equal(runtime.inputEventAvailable({ t: 'mp' }), true);
  assert.equal(runtime.inputEventAvailable({ t: 'md' }), true);
  assert.equal(runtime.inputEventAvailable({ t: 'kd' }), false);
  assert.equal(runtime.inputEventAvailable({ t: 'tx' }), true);
  assert.equal(runtime.inputEventAvailable({ t: 'gp' }), false);
  capabilities.control = false;
  assert.equal(runtime.inputEventAvailable({ t: 'tx' }), false);
});

test('reliable transitions run before motion and only the latest gamepad state survives', async () => {
  const delivered = [];
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      return true;
    },
  });

  runtime.send({ t: 'gp', state: { a: true } });
  runtime.send({ t: 'gp', state: { a: false } });
  runtime.send({ t: 'mr', dx: 4, dy: -2 });
  runtime.send({ t: 'mr', dx: 3, dy: 1 });
  runtime.send({ t: 'kd', k: 'a' });
  await runtime.drain(200);

  assert.deepEqual(delivered, [
    { t: 'kd', k: 'a' },
    { t: 'mr', dx: 7, dy: -1 },
    { t: 'gp', state: { a: false } },
  ]);
});

test('absolute pointer state is latest-value-wins', async () => {
  const delivered = [];
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      return true;
    },
  });

  runtime.send({ t: 'mm', x: 10, y: 20 });
  runtime.send({ t: 'mm', x: 30, y: 40 });
  await runtime.drain(200);

  assert.deepEqual(delivered, [{ t: 'mm', x: 30, y: 40 }]);
});

test('pointer transition barriers drain every motion chunk and adjacent scroll coalesces', async () => {
  const delivered = [];
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      return true;
    },
  });

  runtime.send({ t: 'mr', dx: 50_000, dy: -50_000 });
  runtime.send({ t: 'md', b: 1 });
  runtime.send({ t: 'ms', dx: 900_000, dy: -900_000 });
  runtime.send({ t: 'ms', dx: 900_000, dy: -900_000 });
  await runtime.drain(200);

  assert.deepEqual(delivered, [
    { t: 'mr', dx: 32_767, dy: -32_767 },
    { t: 'mr', dx: 17_233, dy: -17_233 },
    { t: 'md', b: 1 },
    { t: 'ms', dx: 1_000_000, dy: -1_000_000 },
  ]);
});

test('rejected in-flight motion is restored before a transition queued behind it', async () => {
  const delivered = [];
  let startFirst;
  const firstStarted = new Promise((resolve) => { startFirst = resolve; });
  let rejectFirst;
  const firstResult = new Promise((resolve) => { rejectFirst = resolve; });
  let attempts = 0;
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      attempts += 1;
      if (attempts === 1) {
        startFirst();
        return firstResult;
      }
      return true;
    },
  });

  runtime.send({ t: 'mr', dx: 50_000, dy: 0 });
  await firstStarted;
  runtime.send({ t: 'md', b: 1 });
  rejectFirst(false);
  await runtime.drain(300);

  assert.deepEqual(delivered, [
    { t: 'mr', dx: 32_767, dy: 0 },
    { t: 'mr', dx: 32_767, dy: 0 },
    { t: 'mr', dx: 17_233, dy: 0 },
    { t: 'md', b: 1 },
  ]);
});

test('a rejected gamepad sample cannot replace newer controller state', async () => {
  const delivered = [];
  let startFirst;
  const firstStarted = new Promise((resolve) => { startFirst = resolve; });
  let rejectFirst;
  const firstResult = new Promise((resolve) => { rejectFirst = resolve; });
  let attempts = 0;
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      attempts += 1;
      if (attempts === 1) {
        startFirst();
        return firstResult;
      }
      return true;
    },
  });

  runtime.send({ t: 'gp', state: { a: true } });
  await firstStarted;
  runtime.send({ t: 'gp', state: { a: false } });
  rejectFirst(false);
  await runtime.drain(300);

  assert.deepEqual(delivered, [
    { t: 'gp', state: { a: true } },
    { t: 'gp', state: { a: false } },
  ]);
});

test('regular and release queue limits retain their reserved capacity and fail asynchronously', async () => {
  const scheduled = [];
  const microtasks = [];
  const harness = newHarness({
    scheduleTimeout: (callback, delay) => {
      scheduled.push({ callback, delay });
      return scheduled.length;
    },
    scheduleMicrotask: (callback) => microtasks.push(callback),
  });
  const { runtime } = harness;

  for (let index = 0; index < REGULAR_RELIABLE_INPUT_LIMIT; index += 1) {
    assert.equal(runtime.send({ t: 'kd', k: `key-${index}` }), true);
  }
  assert.equal(runtime.send({ t: 'kd', k: 'ordinary-overflow' }), false);
  assert.equal(harness.fatal.length, 0);
  microtasks.shift()();
  assert.equal(harness.fatal[0].reason, 'reliable input queue full');

  for (
    let index = REGULAR_RELIABLE_INPUT_LIMIT;
    index < RELIABLE_INPUT_ENQUEUE_LIMIT;
    index += 1
  ) {
    assert.equal(runtime.send({ t: 'ku', k: `release-${index}` }, { release: true }), true);
  }
  assert.equal(runtime.send({ t: 'ku', k: 'release-overflow' }, { release: true }), false);
  microtasks.shift()();
  assert.equal(harness.fatal[1].reason, 'input release reserve exhausted');
  assert.equal(scheduled.length, 1, 'one pending pump owns the queue');
});

test('held transitions release once in key-then-button insertion order', async () => {
  const delivered = [];
  const { runtime } = newHarness({
    invokeCommand: async (_command, { event }) => {
      delivered.push(event);
      return true;
    },
  });

  assert.equal(runtime.trackKey('KeyA', 'a'), 'tracked');
  assert.equal(runtime.trackKey('KeyA', 'a'), 'repeat');
  assert.equal(runtime.trackKey('KeyB', 'b'), 'tracked');
  assert.equal(runtime.trackMouseButton(2), true);
  assert.equal(runtime.trackMouseButton(1), true);
  runtime.releaseHeld();
  runtime.releaseHeld();
  await runtime.drain(200);

  assert.deepEqual(delivered, [
    { t: 'ku', k: 'a' },
    { t: 'ku', k: 'b' },
    { t: 'mu', b: 2 },
    { t: 'mu', b: 1 },
  ]);
});

test('transport exceptions clear all pending state without escalating disconnect', async () => {
  let calls = 0;
  const harness = newHarness({
    invokeCommand: async () => {
      calls += 1;
      throw new Error('transport closed');
    },
  });
  const { runtime } = harness;

  runtime.trackKey('KeyA', 'a');
  runtime.send({ t: 'kd', k: 'a' });
  runtime.send({ t: 'gp', state: { a: true } });
  await runtime.drain(200);

  assert.equal(calls, 1);
  assert.equal(runtime.hasPending(), false);
  assert.equal(runtime.takeKeyRelease('KeyA'), null);
  assert.equal(harness.activationResets, 1);
  assert.equal(harness.fatal.length, 0);
  assert.equal(harness.warnings[0][0], 'input send failed:');
});

test('retry cadence remains eight milliseconds', () => {
  assert.equal(INPUT_RETRY_MS, 8);
});

test('Portal delegates queue ownership while retaining DOM input wiring', () => {
  assert.match(main, /const inputRuntime = createInputRuntime\(\{/);
  assert.match(
    main,
    /resetControllerActivation: \(\) => controlRuntime\?\.resetControllerActivation\(\)/,
  );
  assert.match(main, /window\.addEventListener\('mousedown', handleMouseDown/);
  assert.match(main, /inputRuntime\.trackMouseButton\(btn\)/);
  assert.match(main, /keyboardInputForEvent\(e, connectionState\.inputCapabilities\)/);
  assert.match(main, /inputRuntime\.trackKey\(keyId, input\.key\)/);
  assert.doesNotMatch(main, /const printableText =/);
  assert.doesNotMatch(main, /const reliableInputQueue =/);
  assert.doesNotMatch(main, /function sendInput\(/);
  assert.doesNotMatch(main, /new HeldInputState\(/);
});
