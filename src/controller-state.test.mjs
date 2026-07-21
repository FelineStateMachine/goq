import assert from 'node:assert/strict';
import test from 'node:test';

import {
  ControllerActivationGate,
  ControllerActionRepeater,
  GamepadEscapeHold,
  LatestControllerStatePublisher,
  STANDARD_AXIS_COUNT,
  STANDARD_BUTTON_COUNT,
  chooseDirectionalIndex,
  controllerStateSignature,
  disconnectedControllerState,
  maskGamepadEscapeChord,
  navigationDirection,
  neutralGamepadInputState,
  normalizeGamepad,
  selectPreferredController,
  toGamepadInputState,
} from './controller-state.mjs';

function gamepad({ axes = [], pressed = [], values = [], index = 2 } = {}) {
  const buttons = Array.from({ length: 24 }, (_, buttonIndex) => ({
    pressed: pressed.includes(buttonIndex),
    value: values[buttonIndex] || 0,
  }));
  return { connected: true, index, id: 'test pad', mapping: 'standard', timestamp: 42, axes, buttons };
}

test('normalization is immutable, clamped, and fixed-size', () => {
  const state = normalizeGamepad(gamepad({ axes: [-2, 0.25, 4, NaN, 0.8], values: { 0: 2 } }), 7);
  assert.equal(state.sequence, 7);
  assert.equal(state.axes.length, STANDARD_AXIS_COUNT);
  assert.equal(state.buttons.length, STANDARD_BUTTON_COUNT);
  assert.deepEqual(state.axes, [-1, 0.25, 1, 0]);
  assert.equal(state.buttons[0], 1);
  assert.ok(Object.isFrozen(state));
  assert.ok(Object.isFrozen(state.axes));
});

test('navigation prefers dpad then the dominant left-stick axis', () => {
  assert.equal(navigationDirection(normalizeGamepad(gamepad({ pressed: [15], axes: [0, -1] }))), 'right');
  assert.equal(navigationDirection(normalizeGamepad(gamepad({ axes: [-0.8, 0.6] }))), 'left');
  assert.equal(navigationDirection(normalizeGamepad(gamepad({ axes: [0.2, 0.54] }))), null);
});

test('actions are edge-triggered with deterministic directional repeat', () => {
  const repeater = new ControllerActionRepeater({ repeatDelayMs: 300, repeatIntervalMs: 100 });
  const held = normalizeGamepad(gamepad({ pressed: [0, 12] }));
  assert.deepEqual(repeater.update(held, 0), [
    { type: 'navigate', direction: 'up' },
    { type: 'activate' },
  ]);
  assert.deepEqual(repeater.update(held, 299), []);
  assert.deepEqual(repeater.update(held, 300), [{ type: 'navigate', direction: 'up' }]);
  assert.deepEqual(repeater.update(held, 399), []);
  assert.deepEqual(repeater.update(held, 400), [{ type: 'navigate', direction: 'up' }]);
  const released = normalizeGamepad(gamepad());
  assert.deepEqual(repeater.update(released, 401), []);
  assert.deepEqual(repeater.update(normalizeGamepad(gamepad({ pressed: [0, 1] })), 402), [
    { type: 'activate' },
    { type: 'back' },
  ]);
});

test('directional selection follows geometry and never wraps unexpectedly', () => {
  const rects = [
    { left: 0, top: 0, width: 20, height: 20 },
    { left: 100, top: 0, width: 20, height: 20 },
    { left: 0, top: 100, width: 20, height: 20 },
    { left: 100, top: 100, width: 20, height: 20 },
  ];
  assert.equal(chooseDirectionalIndex(rects, 0, 'right'), 1);
  assert.equal(chooseDirectionalIndex(rects, 0, 'down'), 2);
  assert.equal(chooseDirectionalIndex(rects, 0, 'left'), 0);
  assert.equal(chooseDirectionalIndex(rects, -1, 'right'), 0);
});

test('state signature ignores sequence and browser timestamp', () => {
  const one = normalizeGamepad(gamepad(), 1);
  const two = normalizeGamepad({ ...gamepad(), timestamp: 999 }, 2);
  assert.equal(controllerStateSignature(one), controllerStateSignature(two));
  assert.notEqual(controllerStateSignature(one), controllerStateSignature(disconnectedControllerState()));
});

test('standard gamepad maps exactly to bounded protocol state', () => {
  const state = normalizeGamepad(gamepad({
    axes: [-1, 0.5, 2, -2],
    pressed: [0, 4, 8, 12, 13, 14],
    values: { 6: 0.5, 7: 1 },
  }));
  const mapped = toGamepadInputState(state);
  assert.deepEqual(mapped, {
    ...neutralGamepadInputState(),
    a: true,
    left_shoulder: true,
    back: true,
    dpad_left: true,
    left_x: -32767,
    left_y: 16384,
    right_x: 32767,
    right_y: -32767,
    left_trigger: 16384,
    right_trigger: 32767,
  });
  assert.equal(mapped.dpad_up, false);
  assert.equal(mapped.dpad_down, false);
});

test('controller activation gate suppresses only the crossing A press', () => {
  const gate = new ControllerActivationGate();
  const held = { ...neutralGamepadInputState(), a: true };
  const releasedWithDrift = { ...neutralGamepadInputState(), left_x: 33 };

  assert.equal(gate.accepts(held), true);
  gate.arm();
  assert.equal(gate.active, true);
  assert.equal(gate.accepts(held), false);
  assert.equal(gate.accepts(releasedWithDrift), true);
  assert.equal(gate.active, false);
  assert.equal(gate.accepts(held), true);
});

test('controller activation gate reset cannot leak into another control session', () => {
  const gate = new ControllerActivationGate();
  const held = { ...neutralGamepadInputState(), a: true };

  gate.arm();
  assert.equal(gate.accepts(held), false);
  gate.reset();

  assert.equal(gate.active, false);
  assert.equal(gate.accepts(held), true);
});

test('controller activation gate accepts an A release already current after acquisition', () => {
  const gate = new ControllerActivationGate();
  const releasedWhileAcquiring = { ...neutralGamepadInputState(), left_x: 33 };

  gate.arm();

  assert.equal(gate.accepts(releasedWhileAcquiring), true);
  assert.equal(gate.active, false);
});

test('disconnected and non-standard pads map to neutral', () => {
  assert.deepEqual(toGamepadInputState(disconnectedControllerState()), neutralGamepadInputState());
  assert.deepEqual(
    toGamepadInputState(normalizeGamepad({ ...gamepad({ pressed: [0] }), mapping: '' })),
    neutralGamepadInputState(),
  );
});

test('Back+Start escape chord is reserved without consuming unrelated controls', () => {
  const input = {
    ...neutralGamepadInputState(),
    a: true,
    back: true,
    start: true,
    left_x: 1234,
  };
  const masked = maskGamepadEscapeChord(input);

  assert.deepEqual(masked, {
    ...input,
    back: false,
    start: false,
  });
  assert.equal(input.back, true);
  assert.equal(input.start, true);
  assert.equal(maskGamepadEscapeChord({ ...input, start: false }).back, true);
  assert.equal(maskGamepadEscapeChord({ ...input, back: false }).start, true);
});

test('controller selection prefers standard mappings and retains a selected standard pad', () => {
  const nonStandard = { connected: true, index: 4, mapping: '' };
  const firstStandard = { connected: true, index: 1, mapping: 'standard' };
  const selectedStandard = { connected: true, index: 2, mapping: 'standard' };

  assert.equal(
    selectPreferredController([nonStandard, firstStandard, selectedStandard], nonStandard.index),
    firstStandard,
  );
  assert.equal(
    selectPreferredController(
      [nonStandard, firstStandard, selectedStandard],
      selectedStandard.index,
    ),
    selectedStandard,
  );
});

test('controller selection falls back deterministically without a standard mapping', () => {
  const first = { connected: true, index: 3, mapping: '' };
  const selected = { connected: true, index: 7, mapping: 'custom' };
  const disconnected = { connected: false, index: 9, mapping: 'standard' };

  assert.equal(selectPreferredController([first, selected], selected.index), selected);
  assert.equal(selectPreferredController([first, selected], 99), first);
  assert.equal(selectPreferredController([disconnected, first], disconnected.index), first);
  assert.equal(selectPreferredController([], null), null);
  assert.equal(selectPreferredController(null, null), null);
});

test('escape chord requires an uninterrupted one-second hold and fires once', () => {
  const escape = new GamepadEscapeHold(1000);
  const held = normalizeGamepad(gamepad({ pressed: [8, 9] }));
  assert.equal(escape.update(held, 100), false);
  assert.equal(escape.update(held, 1099), false);
  assert.equal(escape.update(held, 1100), true);
  assert.equal(escape.update(held, 2000), false);
  assert.equal(escape.update(normalizeGamepad(gamepad()), 2001), false);
  assert.equal(escape.update(held, 2002), false);
});

test('slow publisher drops intermediate states and delivers the latest one', async () => {
  const publisher = new LatestControllerStatePublisher();
  const delivered = [];
  let releaseFirst;
  publisher.setHandler(async (state) => {
    delivered.push(state.sequence);
    if (state.sequence === 1) await new Promise((resolve) => { releaseFirst = resolve; });
  });
  publisher.publish(normalizeGamepad(gamepad(), 1));
  await Promise.resolve();
  publisher.publish(normalizeGamepad(gamepad({ axes: [0.1] }), 2));
  publisher.publish(normalizeGamepad(gamepad({ axes: [0.2] }), 3));
  assert.deepEqual(delivered, [1]);
  releaseFirst();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(delivered, [1, 3]);
  assert.equal(publisher.latest.sequence, 3);
});
