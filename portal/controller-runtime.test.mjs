import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { createControllerRuntime } from './controller-runtime.mjs';
import { neutralGamepadInputState } from './controller-state.mjs';

const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');

function gamepad({
  index = 0,
  id = 'test-pad',
  mapping = 'standard',
  timestamp = 1,
  axes = [0, 0, 0, 0],
  pressed = [],
  values = {},
} = {}) {
  const buttons = Array(17).fill(0);
  for (const button of pressed) buttons[button] = 1;
  for (const [button, value] of Object.entries(values)) buttons[Number(button)] = value;
  return {
    connected: true,
    index,
    id,
    mapping,
    timestamp,
    axes,
    buttons,
  };
}

function harness() {
  let pads = [];
  let remoteRoute = false;
  const scheduled = [];
  const sent = [];
  const navigated = [];
  const statuses = [];
  const warnings = [];
  let activations = 0;
  let backs = 0;
  let exits = 0;
  const runtime = createControllerRuntime({
    getGamepads: () => pads,
    schedulePoll: (callback) => scheduled.push(callback),
    isRemoteRoute: () => remoteRoute,
    sendRemoteState: (state) => sent.push(state),
    onNavigate: (direction) => navigated.push(direction),
    onActivate: () => { activations++; },
    onBack: () => { backs++; },
    onStatus: (state) => statuses.push(state),
    onExit: () => { exits++; },
    warn: (...args) => warnings.push(args),
  });
  return {
    runtime,
    scheduled,
    sent,
    navigated,
    statuses,
    warnings,
    get activations() { return activations; },
    get backs() { return backs; },
    get exits() { return exits; },
    setPads(next) { pads = next; },
    setRemoteRoute(next) { remoteRoute = next; },
  };
}

function flushPublisher() {
  return new Promise((resolve) => setImmediate(resolve));
}

test('polling selects a standard controller and publishes only signature changes', async () => {
  const fixture = harness();
  const observed = [];
  fixture.runtime.setObserver((state) => observed.push(state));
  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad({ timestamp: 10, axes: [0.25, 0, 0, 0] })]);

  fixture.runtime.poll(0);
  await flushPublisher();
  assert.equal(fixture.statuses.length, 1);
  assert.equal(fixture.statuses[0].sequence, 1);
  assert.equal(fixture.runtime.latest.sequence, 1);
  assert.equal(observed.length, 1);
  assert.equal(fixture.sent.length, 1);
  assert.equal(fixture.sent[0].left_x, 8192);
  assert.equal(fixture.scheduled.length, 1);

  fixture.setPads([gamepad({ timestamp: 999, axes: [0.25, 0, 0, 0] })]);
  fixture.runtime.poll(16);
  await flushPublisher();
  assert.equal(fixture.statuses.length, 1);
  assert.equal(observed.length, 1);
  assert.equal(fixture.runtime.latest.sequence, 1);

  fixture.setPads([gamepad({ timestamp: 1000, axes: [0.5, 0, 0, 0] })]);
  fixture.runtime.poll(32);
  await flushPublisher();
  assert.equal(fixture.statuses.length, 2);
  assert.equal(fixture.statuses[1].sequence, 2);
  assert.equal(fixture.runtime.latest.sequence, 2);
  assert.equal(observed.length, 2);
});

test('local routing preserves navigation repeat and A/B edge actions', () => {
  const fixture = harness();
  fixture.setPads([gamepad({ pressed: [0, 1, 12] })]);

  fixture.runtime.poll(0);
  assert.deepEqual(fixture.navigated, ['up']);
  assert.equal(fixture.activations, 1);
  assert.equal(fixture.backs, 1);

  fixture.runtime.poll(100);
  assert.deepEqual(fixture.navigated, ['up']);
  assert.equal(fixture.activations, 1);
  assert.equal(fixture.backs, 1);

  fixture.runtime.poll(360);
  assert.deepEqual(fixture.navigated, ['up', 'up']);
  assert.equal(fixture.activations, 1);
  assert.equal(fixture.backs, 1);
});

test('remote routing reserves Back+Start and exits after one uninterrupted second', async () => {
  const fixture = harness();
  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad({ pressed: [8, 9, 12] })]);

  fixture.runtime.poll(0);
  fixture.runtime.poll(999);
  assert.equal(fixture.exits, 0);
  fixture.runtime.poll(1000);
  assert.equal(fixture.exits, 1);
  fixture.runtime.poll(2000);
  assert.equal(fixture.exits, 1);
  assert.deepEqual(fixture.navigated, []);
  await flushPublisher();
  assert.equal(fixture.sent.length, 1);
  assert.equal(fixture.sent[0].back, false);
  assert.equal(fixture.sent[0].start, false);
  assert.equal(fixture.sent[0].dpad_up, true);

  fixture.setPads([gamepad()]);
  fixture.runtime.poll(2001);
  fixture.setPads([gamepad({ pressed: [8, 9] })]);
  fixture.runtime.poll(2100);
  fixture.runtime.poll(3100);
  assert.equal(fixture.exits, 2);
});

test('controller loss publishes a neutral remote state while the control route stays active', async () => {
  const fixture = harness();
  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad({ pressed: [0], axes: [1, 0, 0, 0] })]);
  fixture.runtime.poll(0);
  await flushPublisher();
  assert.equal(fixture.sent[0].a, true);

  fixture.setPads([]);
  fixture.runtime.poll(16);
  await flushPublisher();
  assert.deepEqual(fixture.sent[1], neutralGamepadInputState());
  assert.equal(fixture.statuses.at(-1).connected, false);
});

test('manual publication preserves observer and remote routing without changing sequence', async () => {
  const fixture = harness();
  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad({ pressed: [0] })]);
  fixture.runtime.poll(0);
  await flushPublisher();
  const sequence = fixture.runtime.latest.sequence;
  const observed = [];
  fixture.runtime.setObserver((state) => observed.push(state));

  fixture.runtime.publishCurrentState();
  await flushPublisher();
  assert.equal(fixture.runtime.latest.sequence, sequence);
  assert.equal(observed.length, 1);
  assert.equal(fixture.sent.length, 2);
  assert.throws(() => fixture.runtime.setObserver('invalid'), /observer must be a function or null/);
});

test('observer and gamepad polling failures are contained without stopping the loop', async () => {
  const fixture = harness();
  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad()]);
  fixture.runtime.setObserver(() => { throw new Error('observer boom'); });
  fixture.runtime.poll(0);
  await flushPublisher();
  assert.equal(fixture.sent.length, 1);
  assert.match(String(fixture.warnings[0][1]), /observer boom/);

  const scheduled = [];
  const warnings = [];
  const runtime = createControllerRuntime({
    getGamepads: () => { throw new Error('poll boom'); },
    schedulePoll: (callback) => scheduled.push(callback),
    warn: (...args) => warnings.push(args),
  });
  runtime.poll(16);
  assert.equal(scheduled.length, 1);
  assert.equal(warnings[0][0], 'gamepad poll failed:');
  assert.match(String(warnings[0][1]), /poll boom/);
});

test('start schedules the first poll and resetEscape cancels an in-progress hold', () => {
  const fixture = harness();
  fixture.runtime.start();
  assert.equal(fixture.scheduled.length, 1);

  fixture.setRemoteRoute(true);
  fixture.setPads([gamepad({ pressed: [8, 9] })]);
  fixture.runtime.poll(0);
  fixture.runtime.resetEscape();
  fixture.runtime.poll(1000);
  assert.equal(fixture.exits, 0);
  fixture.runtime.poll(2000);
  assert.equal(fixture.exits, 1);
});

test('Portal delegates controller state while retaining DOM navigation and polling start', () => {
  assert.match(main, /import \{ createControllerRuntime \} from '\.\/controller-runtime\.mjs';/);
  assert.match(main, /controllerRuntime = createControllerRuntime\(\{/);
  assert.match(main, /onNavigate: navigateControllerFocus/);
  assert.match(main, /onActivate: activateControllerFocus/);
  assert.match(main, /onBack: controllerBack/);
  assert.match(main, /onStatus: updateControllerStatus/);
  assert.match(main, /requestAnimationFrame\(controllerRuntime\.poll\)/);
  assert.match(main, /function controllerScope\(\)/);
  assert.match(main, /function setControllerFocus\(element\)/);
  assert.doesNotMatch(main, /const controllerPublisher =/);
  assert.doesNotMatch(main, /let controllerSequence =/);
  assert.doesNotMatch(main, /function pollControllers\(/);
});
