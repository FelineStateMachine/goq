import assert from 'node:assert/strict';
import test from 'node:test';

import {
  HeldInputState,
  MAX_HELD_KEYS,
  MAX_HELD_MOUSE_BUTTONS,
  MAX_RELATIVE_POINTER_DELTA,
  PointerMotionBuffer,
  RelativePointerAccumulator,
  advanceRemotePointerPosition,
  browserPointerLockLossRequiresControlExit,
  mapCanvasPointToSurface,
  resolvePointerSurfaceSize,
  restoreRejectedPointerMotion,
  scaleRelativePointerDelta,
  validatePointerSurfaceDimensions,
  validatePointerPositionFeedback,
  browserMouseButtonCode,
} from './input-state.mjs';

test('relative motion forwards finite safe deltas without surface scaling', () => {
  assert.deepEqual(
    scaleRelativePointerDelta(10, -5, 1280, 800, 640, 400),
    { dx: 10, dy: -5 },
  );
  assert.deepEqual(
    scaleRelativePointerDelta(1, 1, 1280, 800, 0, 400),
    { dx: 1, dy: 1 },
  );
  assert.deepEqual(
    scaleRelativePointerDelta(Number.POSITIVE_INFINITY, 1, 1280, 800, 640, 400),
    { dx: 0, dy: 1 },
  );
  assert.deepEqual(
    scaleRelativePointerDelta(10.4, -5.6),
    { dx: 10, dy: -6 },
  );
  assert.deepEqual(
    scaleRelativePointerDelta(
      MAX_RELATIVE_POINTER_DELTA + 1_000,
      -MAX_RELATIVE_POINTER_DELTA - 1_000,
    ),
    {
      dx: MAX_RELATIVE_POINTER_DELTA + 1_000,
      dy: -MAX_RELATIVE_POINTER_DELTA - 1_000,
    },
  );
  assert.deepEqual(scaleRelativePointerDelta(Number.MAX_VALUE, 1), { dx: 0, dy: 1 });
});

test('pointer surface metadata is bounded and old hosts fall back to frame size', () => {
  const native = validatePointerSurfaceDimensions({ width: 2560, height: 1600 });
  assert.deepEqual(native, { width: 2560, height: 1600 });
  assert.deepEqual(
    resolvePointerSurfaceSize(native, 1280, 800, true),
    { width: 2560, height: 1600 },
  );
  assert.deepEqual(
    resolvePointerSurfaceSize(null, 1280, 800, true),
    { width: 1280, height: 800 },
  );
  assert.deepEqual(
    resolvePointerSurfaceSize(native, 1280, 800, false),
    { width: 2560, height: 1600 },
  );
  assert.equal(resolvePointerSurfaceSize(null, 0, 800, true), null);
  assert.throws(
    () => validatePointerSurfaceDimensions({ width: 7681, height: 1600 }),
    /invalid pointer surface dimensions/,
  );
  assert.throws(
    () => validatePointerSurfaceDimensions({ width: 2560, height: 63 }),
    /invalid pointer surface dimensions/,
  );
});

test('absolute pointer mapping always targets the negotiated native surface', () => {
  const rect = { left: 20, top: 10, width: 640, height: 400 };
  const surface = { width: 1280, height: 800 };
  assert.deepEqual(mapCanvasPointToSurface({ clientX: 340, clientY: 210, rect, surface }), {
    x: 640,
    y: 400,
  });
  assert.deepEqual(mapCanvasPointToSurface({ clientX: 700, clientY: -10, rect, surface }), {
    x: 1279,
    y: 0,
  });
});

test('host pointer feedback is bounded and validated against the native surface', () => {
  const surface = { width: 2560, height: 1600 };
  assert.deepEqual(
    validatePointerPositionFeedback({ sequence: 7, position: { x: 1280, y: 800 } }, surface),
    { sequence: 7, position: { x: 1280, y: 800 }, pointer_visible: true },
  );
  assert.deepEqual(
    validatePointerPositionFeedback({ sequence: 8, position: null }, surface),
    { sequence: 8, position: null, pointer_visible: false },
  );
  assert.deepEqual(
    validatePointerPositionFeedback({
      sequence: 9,
      position: { x: 1280, y: 800 },
      pointer_visible: false,
    }, surface),
    { sequence: 9, position: { x: 1280, y: 800 }, pointer_visible: false },
  );
  const normalizedHidden = validatePointerPositionFeedback({
    sequence: 10,
    position: { x: 1280, y: 800 },
    pointer_visible: false,
  }, surface);
  assert.deepEqual(
    validatePointerPositionFeedback(normalizedHidden, surface),
    normalizedHidden,
  );
  assert.throws(
    () => validatePointerPositionFeedback({ sequence: -1, position: null }, surface),
    /invalid pointer feedback sequence/,
  );
  assert.throws(
    () => validatePointerPositionFeedback({ sequence: 11, position: { x: 2560, y: 0 } }, surface),
    /outside the negotiated surface/,
  );
  assert.throws(
    () => validatePointerPositionFeedback({
      sequence: 12,
      position: { x: 1, y: 1 },
      pointer_visible: 'yes',
    }, surface),
    /invalid pointer feedback visibility/,
  );
  assert.throws(
    () => validatePointerPositionFeedback({
      sequence: 13,
      position: null,
      pointer_visible: true,
    }, surface),
    /visible pointer feedback requires a position/,
  );
});

test('software cursor follows remote deltas and clamps to the streamed surface', () => {
  assert.deepEqual(
    advanceRemotePointerPosition({ x: 640, y: 400 }, { dx: 20, dy: -10 }, 1280, 800),
    { x: 660, y: 390 },
  );
  assert.deepEqual(
    advanceRemotePointerPosition({ x: 2, y: 798 }, { dx: -20, dy: 10 }, 1280, 800),
    { x: 0, y: 799 },
  );
  assert.equal(advanceRemotePointerPosition(null, { dx: 1, dy: 1 }, 1280, 800), null);
});

test('browser pointer lock loss only exits a browser-owned control session', () => {
  const canvas = {};
  const state = {
    browserPointerLockRequired: true,
    pointerLockElement: canvas,
    expectedElement: canvas,
    controlMode: true,
    controlTransitionInProgress: false,
  };
  assert.equal(browserPointerLockLossRequiresControlExit(state), false);
  assert.equal(browserPointerLockLossRequiresControlExit({
    ...state,
    pointerLockElement: null,
  }), true);
  assert.equal(browserPointerLockLossRequiresControlExit({
    ...state,
    pointerLockElement: null,
    controlMode: false,
    controlTransitionInProgress: true,
  }), true);
  assert.equal(browserPointerLockLossRequiresControlExit({
    ...state,
    browserPointerLockRequired: false,
    pointerLockElement: null,
  }), false);
  assert.equal(browserPointerLockLossRequiresControlExit({
    ...state,
    pointerLockElement: null,
    controlMode: false,
  }), false);
});

test('browser mouse buttons map exactly and reject auxiliary buttons', () => {
  assert.equal(browserMouseButtonCode(0), 1);
  assert.equal(browserMouseButtonCode(2), 2);
  assert.equal(browserMouseButtonCode(1), 3);
  assert.equal(browserMouseButtonCode(3), null);
  assert.equal(browserMouseButtonCode(-1), null);
});

test('held transitions produce one matching release and then clear', () => {
  const state = new HeldInputState();
  assert.equal(state.trackKey('KeyA', 'a'), 'tracked');
  assert.equal(state.trackKey('KeyA', 'a'), 'repeat');
  assert.equal(state.trackMouseButton(1), true);
  assert.equal(state.trackMouseButton(1), false);
  assert.deepEqual(state.releaseEvents(), [
    { t: 'ku', k: 'a' },
    { t: 'mu', b: 1 },
  ]);
  assert.equal(state.size, 2);
  state.clear();
  assert.equal(state.size, 0);
  assert.deepEqual(state.releaseEvents(), []);
});

test('held transition tracking is strictly bounded', () => {
  const state = new HeldInputState();
  assert.equal(state.trackMouseButton(0), false);
  assert.equal(state.trackMouseButton(4), false);
  for (let index = 0; index < MAX_HELD_KEYS; index++) {
    assert.equal(state.trackKey(`Code${index}`, `key-${index}`), 'tracked');
  }
  assert.equal(state.trackKey('overflow', 'overflow'), 'full');
  for (let button = 1; button <= MAX_HELD_MOUSE_BUTTONS; button++) {
    assert.equal(state.trackMouseButton(button), true);
  }
  assert.equal(state.releaseEvents().length, MAX_HELD_KEYS + MAX_HELD_MOUSE_BUTTONS);
});

test('ordinary releases retain the original forwarded key value', () => {
  const state = new HeldInputState();
  state.trackKey('Digit1', '!');
  assert.deepEqual(state.takeKeyRelease('Digit1'), { t: 'ku', k: '!' });
  assert.equal(state.takeKeyRelease('Digit1'), null);
});

test('extended physical keys suppress repeats and retain exact neutralization tokens', () => {
  const state = new HeldInputState();
  assert.equal(state.trackKey('F12', 'F12'), 'tracked');
  assert.equal(state.trackKey('F12', 'F12'), 'repeat');
  assert.equal(state.trackKey('IntlBackslash', 'IntlBackslash'), 'tracked');
  assert.equal(state.trackKey('AltRight', 'AltRight'), 'tracked');
  assert.deepEqual(state.releaseEvents(), [
    { t: 'ku', k: 'F12' },
    { t: 'ku', k: 'IntlBackslash' },
    { t: 'ku', k: 'AltRight' },
  ]);
  assert.deepEqual(state.takeKeyRelease('F12'), { t: 'ku', k: 'F12' });
  assert.equal(state.takeKeyRelease('F12'), null);
});

test('relative motion coalesces small displacement into one bounded event', () => {
  const motion = new RelativePointerAccumulator();
  assert.equal(motion.add(2.4, -3.6), true);
  assert.equal(motion.add(5, 2), true);
  assert.equal(motion.pending, true);
  assert.deepEqual(motion.take(), { t: 'mr', dx: 7, dy: -2 });
  assert.equal(motion.pending, false);
  assert.equal(motion.take(), null);
});

test('relative motion emits bounded chunks without losing accumulated displacement', () => {
  const motion = new RelativePointerAccumulator();
  assert.equal(motion.add(Number.NaN, Number.POSITIVE_INFINITY), false);
  assert.equal(motion.add(MAX_RELATIVE_POINTER_DELTA, -MAX_RELATIVE_POINTER_DELTA), true);
  assert.equal(motion.add(500, -500), true);
  assert.equal(motion.chunkCount, 2);
  assert.deepEqual(motion.take(), {
    t: 'mr',
    dx: MAX_RELATIVE_POINTER_DELTA,
    dy: -MAX_RELATIVE_POINTER_DELTA,
  });
  assert.deepEqual(motion.take(), { t: 'mr', dx: 500, dy: -500 });
  assert.equal(motion.take(), null);
});

test('one wide browser sample is preserved across protocol-sized chunks', () => {
  const motion = new RelativePointerAccumulator();
  assert.equal(motion.add(50_000, -50_000), true);
  assert.equal(motion.chunkCount, 2);
  assert.deepEqual(motion.take(), {
    t: 'mr',
    dx: MAX_RELATIVE_POINTER_DELTA,
    dy: -MAX_RELATIVE_POINTER_DELTA,
  });
  assert.deepEqual(motion.take(), { t: 'mr', dx: 17_233, dy: -17_233 });
  assert.equal(motion.take(), null);
});

test('relative motion restore preserves a rejected in-flight displacement', () => {
  const motion = new RelativePointerAccumulator();
  assert.equal(motion.restore({ t: 'mm', x: 1, y: 2 }), false);
  assert.equal(motion.restore({ t: 'mr', dx: -9, dy: 4 }), true);
  motion.add(3, 2);
  assert.deepEqual(motion.take(), { t: 'mr', dx: -6, dy: 6 });
});

test('rejected wide motion stays before a click queued during its invoke', () => {
  const motion = new PointerMotionBuffer();
  const reliable = [];
  motion.addRelative(50_000, 0);
  const rejected = motion.take();
  reliable.push(...motion.takeBarrierBefore({ t: 'md', b: 1 }));

  assert.equal(
    restoreRejectedPointerMotion(reliable, motion, rejected, 128),
    'queued',
  );
  assert.deepEqual(reliable, [
    { t: 'mr', dx: MAX_RELATIVE_POINTER_DELTA, dy: 0 },
    { t: 'mr', dx: 17_233, dy: 0 },
    { t: 'md', b: 1 },
  ]);
});

test('rejected older absolute motion cannot replace a newer latest position', () => {
  const motion = new PointerMotionBuffer();
  motion.setAbsolute({ t: 'mm', x: 30, y: 40 });
  assert.equal(
    restoreRejectedPointerMotion(
      [],
      motion,
      { t: 'mm', x: 10, y: 20 },
      128,
    ),
    'superseded',
  );
  assert.deepEqual(motion.take(), { t: 'mm', x: 30, y: 40 });
});

test('pointer motion barrier stays immediately ahead of a button under a busy pump', () => {
  const motion = new PointerMotionBuffer();
  motion.addRelative(4, -2);
  motion.addRelative(3, 1);
  assert.equal(motion.barrierLength, 1);
  assert.deepEqual(motion.takeBarrierBefore({ t: 'md', b: 1 }), [
    { t: 'mr', dx: 7, dy: -1 },
    { t: 'md', b: 1 },
  ]);
  assert.equal(motion.pending, false);
});

test('pointer motion barrier drains every bounded relative chunk before a button', () => {
  const motion = new PointerMotionBuffer();
  motion.addRelative(MAX_RELATIVE_POINTER_DELTA, -MAX_RELATIVE_POINTER_DELTA);
  motion.addRelative(9, -11);
  assert.equal(motion.barrierLength, 2);
  assert.deepEqual(motion.takeBarrierBefore({ t: 'md', b: 1 }), [
    {
      t: 'mr',
      dx: MAX_RELATIVE_POINTER_DELTA,
      dy: -MAX_RELATIVE_POINTER_DELTA,
    },
    { t: 'mr', dx: 9, dy: -11 },
    { t: 'md', b: 1 },
  ]);
  assert.equal(motion.pending, false);
});

test('absolute fallback also forms a latest-position barrier before click', () => {
  const motion = new PointerMotionBuffer();
  motion.setAbsolute({ t: 'mm', x: 10, y: 20 });
  motion.setAbsolute({ t: 'mm', x: 30, y: 40 });
  assert.deepEqual(motion.takeBarrierBefore({ t: 'mc', b: 1 }), [
    { t: 'mm', x: 30, y: 40 },
    { t: 'mc', b: 1 },
  ]);
});
