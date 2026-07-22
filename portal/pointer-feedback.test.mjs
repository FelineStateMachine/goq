import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  createPointerFeedbackRuntime,
  newPointerSession,
  parsePointerFeedbackMessage,
} from './pointer-feedback.mjs';

const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');

function runtimeHarness() {
  let activeSession = newPointerSession();
  let capabilitySnapshot = { connected: false, pointerPositionFeedback: true };
  let surface = { width: 1280, height: 800 };
  let renders = 0;
  let disconnects = 0;
  const errors = [];
  const runtime = createPointerFeedbackRuntime({
    getActiveSession: () => activeSession,
    getCapabilities: () => capabilitySnapshot,
    getSurface: () => surface,
    render: () => { renders++; },
    disconnect: () => { disconnects++; },
    logger: { error: (...args) => errors.push(args) },
  });
  return {
    runtime,
    errors,
    get session() { return activeSession; },
    get renders() { return renders; },
    get disconnects() { return disconnects; },
    setActiveSession(session) { activeSession = session; },
    setCapabilities(snapshot) { capabilitySnapshot = snapshot; },
    setSurface(nextSurface) { surface = nextSurface; },
  };
}

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

test('runtime stages pre-connect feedback without applying or rendering it', () => {
  const fixture = runtimeHarness();
  fixture.runtime.handleMessage({
    type: 'position',
    sequence: 1,
    position: { x: 50, y: 60 },
    pointer_visible: true,
  }, fixture.session);

  assert.equal(fixture.session.received, true);
  assert.deepEqual(fixture.session.latest, {
    sequence: 1,
    position: { x: 50, y: 60 },
    pointer_visible: true,
  });
  assert.equal(fixture.session.remotePosition, null);
  assert.equal(fixture.session.remoteVisible, false);
  assert.equal(fixture.renders, 0);
  assert.equal(fixture.disconnects, 0);
});

test('runtime applies active connected feedback only with capability and a surface', () => {
  const fixture = runtimeHarness();
  fixture.setCapabilities({ connected: true, pointerPositionFeedback: true });
  fixture.runtime.handleMessage({
    type: 'position',
    sequence: 2,
    position: { x: 100, y: 200 },
    pointer_visible: false,
  }, fixture.session);
  assert.deepEqual(fixture.session.remotePosition, { x: 100, y: 200 });
  assert.equal(fixture.session.remoteVisible, false);
  assert.equal(fixture.renders, 1);

  fixture.setCapabilities({ connected: true, pointerPositionFeedback: false });
  fixture.runtime.handleMessage({
    type: 'position',
    sequence: 3,
    position: { x: 101, y: 201 },
  }, fixture.session);
  assert.equal(fixture.session.latest.sequence, 3);
  assert.deepEqual(fixture.session.remotePosition, { x: 100, y: 200 });
  assert.equal(fixture.renders, 1);

  fixture.setCapabilities({ connected: true, pointerPositionFeedback: true });
  fixture.setSurface(null);
  fixture.runtime.handleMessage({
    type: 'position',
    sequence: 4,
    position: { x: 102, y: 202 },
  }, fixture.session);
  assert.equal(fixture.session.latest.sequence, 4);
  assert.deepEqual(fixture.session.remotePosition, { x: 100, y: 200 });
  assert.equal(fixture.renders, 1);
});

test('runtime ignores stale, failed, and closing session deliveries', () => {
  const fixture = runtimeHarness();
  const stale = newPointerSession();
  fixture.setCapabilities({ connected: true, pointerPositionFeedback: true });
  fixture.runtime.applyPosition({
    sequence: 0, position: { x: 0, y: 0 }, pointer_visible: true,
  }, stale);
  assert.equal(stale.remotePosition, null);
  assert.equal(fixture.renders, 0);

  fixture.runtime.handleMessage({
    type: 'position', sequence: 1, position: { x: 1, y: 1 },
  }, stale);
  assert.equal(stale.received, false);

  fixture.session.failed = true;
  fixture.runtime.handleMessage({
    type: 'position', sequence: 2, position: { x: 2, y: 2 },
  }, fixture.session);
  assert.equal(fixture.session.received, false);

  const closing = newPointerSession();
  closing.closing = true;
  fixture.setActiveSession(closing);
  fixture.runtime.handleMessage({
    type: 'position', sequence: 3, position: { x: 3, y: 3 },
  }, closing);
  assert.equal(closing.received, false);
  assert.equal(fixture.renders, 0);
});

test('terminal feedback clears render state and disconnects only while connected', () => {
  for (const [reason, detail] of [
    ['eof', 'Pointer feedback ended'],
    ['malformed', 'Pointer feedback was malformed'],
  ]) {
    const fixture = runtimeHarness();
    fixture.session.remotePosition = { x: 4, y: 5 };
    fixture.session.remoteVisible = true;
    fixture.runtime.handleMessage({ type: 'terminal', reason }, fixture.session);
    assert.equal(fixture.session.failed, true);
    assert.equal(fixture.session.failureDetail, detail);
    assert.equal(fixture.session.remotePosition, null);
    assert.equal(fixture.session.remoteVisible, false);
    assert.equal(fixture.renders, 1);
    assert.deepEqual(fixture.errors, [[detail]]);
    assert.equal(fixture.disconnects, 0);

    const connected = runtimeHarness();
    connected.setCapabilities({ connected: true, pointerPositionFeedback: true });
    connected.runtime.handleMessage({ type: 'terminal', reason }, connected.session);
    assert.equal(connected.disconnects, 1);
  }
});

test('invalid and out-of-surface feedback fail, clear state, and preserve error logging', () => {
  const fixture = runtimeHarness();
  fixture.setCapabilities({ connected: true, pointerPositionFeedback: true });
  fixture.session.remotePosition = { x: 3, y: 4 };
  fixture.session.remoteVisible = true;
  fixture.runtime.handleMessage({
    type: 'position',
    sequence: 7,
    position: { x: 1280, y: 0 },
  }, fixture.session);

  assert.equal(fixture.session.received, true);
  assert.equal(fixture.session.latest.sequence, 7);
  assert.equal(fixture.session.failed, true);
  assert.match(fixture.session.failureDetail, /outside the negotiated surface/);
  assert.equal(fixture.session.remotePosition, null);
  assert.equal(fixture.session.remoteVisible, false);
  assert.equal(fixture.renders, 1);
  assert.equal(fixture.errors[0][0], 'invalid pointer-position feedback:');
  assert.match(String(fixture.errors[0][1]), /outside the negotiated surface/);
  assert.equal(fixture.disconnects, 1);
});

test('runtime validates its injected ownership and side-effect boundaries', () => {
  const required = {
    getActiveSession: () => null,
    getCapabilities: () => ({}),
    getSurface: () => null,
    render: () => {},
    disconnect: () => {},
    logger: { error: () => {} },
  };
  for (const name of ['getActiveSession', 'getCapabilities', 'getSurface', 'render', 'disconnect']) {
    assert.throws(
      () => createPointerFeedbackRuntime({ ...required, [name]: null }),
      new RegExp(`${name} must be a function`),
    );
  }
  assert.throws(
    () => createPointerFeedbackRuntime({ ...required, logger: {} }),
    /logger must provide error/,
  );
});

test('Portal delegates pointer feedback while retaining session, surface, and DOM rendering', () => {
  assert.match(main, /const pointerFeedbackRuntime = createPointerFeedbackRuntime\(\{/);
  assert.match(main, /getActiveSession: \(\) => activePointerSession/);
  assert.match(main, /getSurface: currentPointerSurfaceSize/);
  assert.match(main, /render: renderRemotePointer/);
  assert.match(
    main,
    /pointerFeedbackRuntime\.handleMessage\(message, pointerSession\)/,
  );
  assert.match(main, /let activePointerSession = null/);
  assert.match(main, /function currentPointerSurfaceSize\(\)/);
  assert.match(main, /function renderRemotePointer\(\)/);
  assert.doesNotMatch(main, /function applyPointerPositionFeedback\(/);
  assert.doesNotMatch(main, /function handlePointerPositionFeedback\(/);
});
