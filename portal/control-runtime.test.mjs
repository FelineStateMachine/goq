import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  BROWSER_POINTER_LOCK_TIMEOUT_MS,
  NATIVE_CURSOR_RELEASE_RETRY_DELAYS_MS,
  createControlRuntime,
} from './control-runtime.mjs';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function eventually(assertion, timeoutMs = 300) {
  const deadline = performance.now() + timeoutMs;
  let lastError;
  while (performance.now() < deadline) {
    try {
      assertion();
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 1));
    }
  }
  throw lastError;
}

class FakeEventTarget {
  constructor() {
    this.listeners = new Map();
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.listeners.get(type)?.delete(listener);
  }

  dispatch(type) {
    for (const listener of [...(this.listeners.get(type) ?? [])]) listener();
  }
}

function newHarness({
  invokeCursorGrab = async () => false,
  pointerRequest,
  pointerLockTimeoutMs,
  scheduleTimeout,
} = {}) {
  const target = {};
  const eventTarget = new FakeEventTarget();
  let owner = null;
  let connected = true;
  let disconnecting = false;
  const capabilities = {
    control: true,
    relativePointer: true,
    absolutePointer: false,
    gamepad: true,
  };
  const sent = [];
  const calls = {
    clear: 0,
    releaseHeld: 0,
    drains: [],
    publish: 0,
    resetEscape: 0,
    change: 0,
    releaseFailures: [],
    exits: 0,
  };
  const inputRuntime = {
    send: (event) => { sent.push(event); return true; },
    clear: () => { calls.clear += 1; },
    releaseHeld: () => { calls.releaseHeld += 1; },
    drain: async (timeoutMs) => { calls.drains.push(timeoutMs); },
  };
  const pointerLock = {
    target,
    eventTarget,
    getOwner: () => owner,
    request: pointerRequest ?? (() => {
      owner = target;
      eventTarget.dispatch('pointerlockchange');
    }),
    exit: () => {
      calls.exits += 1;
      owner = null;
      eventTarget.dispatch('pointerlockchange');
    },
  };
  const warnings = [];
  const errors = [];
  const runtime = createControlRuntime({
    getConnection: () => ({ connected, disconnecting, capabilities }),
    inputRuntime,
    invokeCursorGrab,
    pointerLock,
    publishController: () => { calls.publish += 1; },
    resetControllerEscape: () => { calls.resetEscape += 1; },
    onChange: () => { calls.change += 1; },
    onReleaseFailure: (error) => calls.releaseFailures.push(error),
    pointerLockTimeoutMs,
    scheduleTimeout,
    logger: {
      warn: (...args) => warnings.push(args),
      error: (...args) => errors.push(args),
    },
  });
  return {
    runtime,
    calls,
    capabilities,
    sent,
    warnings,
    errors,
    eventTarget,
    target,
    pointerLock,
    get owner() { return owner; },
    set owner(value) { owner = value; },
    set connected(value) { connected = value; },
    set disconnecting(value) { disconnecting = value; },
  };
}

test('explicit native false bypasses browser lock and controller activation stays gated until A release', async () => {
  let pointerRequests = 0;
  const harness = newHarness({
    invokeCursorGrab: async () => false,
    pointerRequest: () => { pointerRequests += 1; },
  });

  await harness.runtime.toggle({ controllerInitiated: true });

  assert.equal(harness.runtime.active, true);
  assert.equal(harness.runtime.browserPointerLockRequired, false);
  assert.equal(pointerRequests, 0);
  assert.equal(harness.calls.publish, 1);
  assert.equal(harness.sent.length, 1);
  assert.equal(harness.sent[0].t, 'gp');
  assert.equal(harness.runtime.activationGateActive, true);
  assert.equal(harness.runtime.acceptsControllerInput({ a: true }), false);
  assert.equal(harness.runtime.acceptsControllerInput({ a: false }), true);
  assert.equal(harness.runtime.activationGateActive, false);
});

test('absolute-pointer control never enters the gated relative-capture path', async () => {
  let nativeGrabCalls = 0;
  let pointerLockRequests = 0;
  const harness = newHarness({
    invokeCursorGrab: async () => {
      nativeGrabCalls += 1;
      return true;
    },
    pointerRequest: () => {
      pointerLockRequests += 1;
    },
  });
  harness.capabilities.relativePointer = false;
  harness.capabilities.absolutePointer = true;

  await harness.runtime.toggle();

  assert.equal(harness.runtime.active, true);
  assert.equal(nativeGrabCalls, 0);
  assert.equal(pointerLockRequests, 0);
  assert.equal(harness.calls.publish, 1);
});

test('ordinary toggle exit queues neutral and releases held input without resetting controller escape', async () => {
  const harness = newHarness({ invokeCursorGrab: async () => false });
  await harness.runtime.toggle();
  await harness.runtime.toggle();
  await eventually(() => assert.equal(harness.sent.length, 1));

  assert.equal(harness.runtime.active, false);
  assert.equal(harness.sent[0].t, 'gp');
  assert.equal(harness.calls.releaseHeld, 1);
  assert.equal(harness.calls.resetEscape, 0);
  assert.equal(harness.calls.change, 2);
});

test('browser acquisition reasserts native grab after ownership and publishes non-controller state', async () => {
  const grabs = [];
  const harness = newHarness({
    invokeCursorGrab: async (grab) => { grabs.push(grab); return true; },
  });

  await harness.runtime.toggle();

  assert.deepEqual(grabs, [true, true]);
  assert.equal(harness.owner, harness.target);
  assert.equal(harness.runtime.active, true);
  assert.equal(harness.calls.publish, 1);
  assert.equal(harness.sent.length, 0);
  assert.equal(harness.calls.change, 1);
});

test('native cursor commands remain serialized', async () => {
  const first = deferred();
  const invocations = [];
  const harness = newHarness({
    invokeCursorGrab: (grab) => {
      invocations.push(grab);
      return invocations.length === 1 ? first.promise : Promise.resolve(true);
    },
  });

  const one = harness.runtime.reassertNativeGrab();
  const two = harness.runtime.reassertNativeGrab();
  await Promise.resolve();
  assert.deepEqual(invocations, [true]);
  first.resolve(true);
  await Promise.all([one, two]);
  assert.deepEqual(invocations, [true, true]);
});

test('browser ownership timeout exits the transition and resets controller escape', async () => {
  const harness = newHarness({
    invokeCursorGrab: async () => true,
    pointerRequest: () => {},
    pointerLockTimeoutMs: 5,
  });

  await harness.runtime.toggle();

  assert.equal(harness.runtime.active, false);
  assert.equal(harness.runtime.transitioning, false);
  assert.equal(harness.calls.resetEscape, 1);
  assert.equal(harness.calls.releaseHeld, 1);
  assert.equal(harness.calls.change, 1);
  assert.match(harness.warnings[0][1].message, /timed out/);
});

test('native-only post-acquisition rejection retains the existing transition-state quirk', async () => {
  const nativeGrab = deferred();
  const harness = newHarness({ invokeCursorGrab: async () => nativeGrab.promise });
  const entering = harness.runtime.toggle();
  await Promise.resolve();
  harness.disconnecting = true;
  nativeGrab.resolve(false);
  await entering;

  assert.equal(harness.runtime.active, false);
  assert.equal(harness.runtime.transitioning, true);
  assert.equal(harness.calls.change, 0);
  assert.equal(harness.calls.releaseHeld, 0);
});

test('losing a browser-owned lock exits active control exactly once', async () => {
  const harness = newHarness({ invokeCursorGrab: async () => true });
  await harness.runtime.toggle();
  harness.owner = null;

  assert.equal(harness.runtime.handleBrowserPointerLockChange(), true);
  assert.equal(harness.runtime.active, false);
  assert.equal(harness.calls.resetEscape, 1);
  assert.equal(harness.calls.releaseHeld, 1);
  assert.equal(harness.runtime.handleBrowserPointerLockChange(), false);
});

test('a native-only session ignores browser ownership changes', async () => {
  const harness = newHarness({ invokeCursorGrab: async () => false });
  await harness.runtime.toggle();

  assert.equal(harness.runtime.handleBrowserPointerLockChange(), false);
  assert.equal(harness.runtime.active, true);
  assert.equal(harness.calls.resetEscape, 0);
});

test('late acquisition cannot reactivate a cancelled transition or leave browser ownership behind', async () => {
  const firstGrab = deferred();
  let grabCalls = 0;
  const harness = newHarness({
    invokeCursorGrab: async () => {
      grabCalls += 1;
      if (grabCalls === 1) return firstGrab.promise;
      return true;
    },
  });

  const entering = harness.runtime.toggle();
  await Promise.resolve();
  harness.runtime.exit();
  firstGrab.resolve(true);
  await entering;
  await eventually(() => assert.equal(harness.owner, null));

  assert.equal(harness.runtime.active, false);
  assert.equal(harness.runtime.transitioning, false);
  assert.ok(harness.calls.exits >= 1);
  assert.equal(harness.calls.publish, 0);
});

test('native cursor release uses exactly three bounded retries before reporting failure', async () => {
  let acquisition = true;
  let releaseAttempts = 0;
  const delays = [];
  const harness = newHarness({
    invokeCursorGrab: async (grab) => {
      if (grab) return acquisition ? false : true;
      releaseAttempts += 1;
      throw new Error(`release-${releaseAttempts}`);
    },
    scheduleTimeout: (callback, delay) => {
      delays.push(delay);
      return setTimeout(callback, 0);
    },
  });
  await harness.runtime.toggle();
  acquisition = false;

  harness.runtime.exit();
  await eventually(() => assert.equal(harness.calls.releaseFailures.length, 1));

  assert.equal(releaseAttempts, 3);
  assert.deepEqual(delays, [16, 50]);
  assert.deepEqual(NATIVE_CURSOR_RELEASE_RETRY_DELAYS_MS, [0, 16, 50]);
  assert.equal(harness.errors[0][0], 'native cursor release failed after bounded retries:');
});

test('disconnect exits control and awaits the requested finite input drain', async () => {
  const harness = newHarness({ invokeCursorGrab: async () => false });
  await harness.runtime.toggle();
  await harness.runtime.prepareDisconnect(250);

  assert.equal(harness.runtime.active, false);
  assert.equal(harness.calls.releaseHeld, 1);
  assert.deepEqual(harness.calls.drains, [250]);
  assert.equal(harness.calls.change, 2);
});

test('accepted and disconnected resets preserve their distinct UI notification behavior', () => {
  const harness = newHarness();

  harness.runtime.resetAcceptedConnection();
  assert.equal(harness.calls.clear, 1);
  assert.equal(harness.calls.change, 0);

  harness.runtime.resetDisconnected();
  assert.equal(harness.calls.clear, 2);
  assert.equal(harness.calls.change, 1);
});

test('runtime constants pin existing browser timeout and disconnect retry policy', () => {
  assert.equal(BROWSER_POINTER_LOCK_TIMEOUT_MS, 500);
  assert.deepEqual(NATIVE_CURSOR_RELEASE_RETRY_DELAYS_MS, [0, 16, 50]);
});

test('Portal delegates control state while retaining DOM presentation and registration', () => {
  assert.match(main, /controlRuntime = createControlRuntime\(\{/);
  assert.match(
    main,
    /document\.addEventListener\('pointerlockchange', handleBrowserPointerLockChange\)/,
  );
  assert.match(main, /const controlling = controlRuntime\.setInactiveIfUnavailable\(available\)/);
  assert.match(main, /controlRuntime\.requestBrowserPointerLock\(\)/);
  assert.match(main, /controlRuntime\.reassertNativeGrab\(\)/);
  assert.doesNotMatch(main, /let controlMode =/);
  assert.doesNotMatch(main, /let nativeCursorCommand =/);
  assert.doesNotMatch(main, /function releaseNativeCursorGrab\(/);
});
