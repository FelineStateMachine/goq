import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  STREAM_WINDOW_CORRECTION_DELAY_MS,
  createWindowRuntime,
} from './window-runtime.mjs';

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

function newHarness({ applyNativeGeometry } = {}) {
  let connected = true;
  let generation = 7;
  let format = { width: 1280, height: 800, epoch: 3 };
  let chromeHeight = 64;
  let screenBounds = { width: 1440, height: 900 };
  let windowSize = { width: 1000, height: 700 };
  let surfaceBounds = { width: 1920, height: 1080 };
  const surfaceSizes = [];
  const nativeApplications = [];
  const warnings = [];
  const timers = new Map();
  const cancelled = [];
  let nextTimerId = 1;
  const runtime = createWindowRuntime({
    isConnected: () => connected,
    getFormat: () => format,
    getGeneration: () => generation,
    getChromeHeight: () => chromeHeight,
    getScreenBounds: () => screenBounds,
    getWindowSize: () => windowSize,
    getSurfaceBounds: () => surfaceBounds,
    setSurfaceSize: (geometry) => surfaceSizes.push(geometry),
    applyNativeGeometry: applyNativeGeometry ?? (async (geometry, unmaximize) => {
      nativeApplications.push({ geometry, unmaximize });
      return true;
    }),
    scheduleTimeout: (callback, delay) => {
      const id = nextTimerId;
      nextTimerId += 1;
      timers.set(id, { callback, delay });
      return id;
    },
    cancelTimeout: (id) => {
      cancelled.push(id);
      timers.delete(id);
    },
    logger: { warn: (...args) => warnings.push(args) },
  });
  return {
    runtime,
    surfaceSizes,
    nativeApplications,
    warnings,
    timers,
    cancelled,
    set connected(value) { connected = value; },
    set generation(value) { generation = value; },
    set format(value) { format = value; },
    set chromeHeight(value) { chromeHeight = value; },
    set screenBounds(value) { screenBounds = value; },
    set windowSize(value) { windowSize = value; },
    set surfaceBounds(value) { surfaceBounds = value; },
  };
}

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
}

test('surface sizing scales to available bounds without owning DOM style', () => {
  const harness = newHarness();

  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), true);
  assert.deepEqual(harness.surfaceSizes, [{ width: 1728, height: 1080 }]);

  harness.surfaceBounds = null;
  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), false);
  harness.format = { width: 0, height: 800, epoch: 4 };
  harness.surfaceBounds = { width: 800, height: 600 };
  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), false);
  assert.equal(harness.surfaceSizes.length, 1);
});

test('initial fit applies once per stable aspect and records the observed native result', async () => {
  const harness = newHarness();

  assert.equal(await harness.runtime.fitWindowToIncomingStream(), true);
  assert.deepEqual(harness.nativeApplications, [{
    geometry: { width: 1165, height: 792 },
    unmaximize: true,
  }]);
  assert.deepEqual(harness.runtime.snapshot(), {
    fittedStreamAspect: '8:5',
    pendingStreamFit: null,
    lastObservedWindowSize: { width: 1165, height: 792 },
    correctionScheduled: false,
  });
  assert.equal(await harness.runtime.fitWindowToIncomingStream(), false);
  assert.equal(harness.nativeApplications.length, 1);
});

test('same-aspect fits are single-flight while native sizing is pending', async () => {
  const native = deferred();
  let applications = 0;
  const harness = newHarness({
    applyNativeGeometry: async () => {
      applications += 1;
      return native.promise;
    },
  });

  const first = harness.runtime.fitWindowToIncomingStream();
  assert.equal(await harness.runtime.fitWindowToIncomingStream(), false);
  assert.equal(applications, 1);
  native.resolve(true);
  assert.equal(await first, true);
});

test('generation and format epoch protect against stale async fit completion', async () => {
  const native = deferred();
  const harness = newHarness({ applyNativeGeometry: async () => native.promise });
  const fitting = harness.runtime.fitWindowToIncomingStream();
  harness.generation = 8;
  harness.format = { width: 1280, height: 800, epoch: 4 };
  native.resolve(true);

  assert.equal(await fitting, false);
  assert.deepEqual(harness.runtime.snapshot(), {
    fittedStreamAspect: null,
    pendingStreamFit: null,
    lastObservedWindowSize: { width: 1165, height: 792 },
    correctionScheduled: false,
  });
});

test('an older completion cannot clear or commit a newer aspect request', async () => {
  const firstNative = deferred();
  const secondNative = deferred();
  let applications = 0;
  const harness = newHarness({
    applyNativeGeometry: async () => {
      applications += 1;
      return applications === 1 ? firstNative.promise : secondNative.promise;
    },
  });
  const first = harness.runtime.fitWindowToIncomingStream();
  harness.format = { width: 1280, height: 720, epoch: 4 };
  const second = harness.runtime.fitWindowToIncomingStream();

  firstNative.resolve(true);
  assert.equal(await first, false);
  assert.deepEqual(harness.runtime.snapshot().pendingStreamFit, {
    aspect: '16:9',
    generation: 7,
    epoch: 4,
  });
  secondNative.resolve(true);
  assert.equal(await second, true);
  assert.equal(harness.runtime.snapshot().fittedStreamAspect, '16:9');
});

test('native geometry errors fail closed and retain the exact warning boundary', async () => {
  const harness = newHarness({
    applyNativeGeometry: async () => { throw new Error('native rejected'); },
  });

  assert.equal(await harness.runtime.fitWindowToIncomingStream(), false);
  assert.equal(harness.warnings[0][0], 'could not apply stream window geometry:');
  assert.match(harness.warnings[0][1].message, /native rejected/);
});

test('resize correction debounces for 80ms and uses only the latest observed delta', async () => {
  const harness = newHarness();
  harness.runtime.scheduleAspectCorrection();
  const firstTimer = [...harness.timers.keys()][0];
  harness.windowSize = { width: 1100, height: 700 };
  harness.runtime.scheduleAspectCorrection();

  assert.deepEqual(harness.cancelled, [firstTimer]);
  assert.equal(harness.timers.size, 1);
  const [{ callback, delay }] = [...harness.timers.values()];
  assert.equal(delay, STREAM_WINDOW_CORRECTION_DELAY_MS);
  callback();
  await flushMicrotasks();

  assert.deepEqual(harness.nativeApplications, [{
    geometry: { width: 1100, height: 752 },
    unmaximize: false,
  }]);
  assert.equal(harness.runtime.snapshot().correctionScheduled, false);
});

test('already-correct and disconnected resize callbacks do not touch native geometry', async () => {
  const harness = newHarness();
  harness.windowSize = { width: 1280, height: 864 };
  harness.runtime.scheduleAspectCorrection();
  [...harness.timers.values()][0].callback();
  await flushMicrotasks();
  assert.equal(harness.nativeApplications.length, 0);

  harness.windowSize = { width: 1000, height: 700 };
  harness.runtime.scheduleAspectCorrection();
  harness.connected = false;
  [...harness.timers.values()][0].callback();
  await flushMicrotasks();
  assert.equal(harness.nativeApplications.length, 0);
});

test('invalid correction geometry is contained and reset cancels pending correction state', () => {
  const harness = newHarness();
  harness.chromeHeight = 900;
  harness.runtime.scheduleAspectCorrection();
  const timerId = [...harness.timers.keys()][0];
  harness.timers.get(timerId).callback();
  harness.timers.delete(timerId);
  assert.equal(harness.warnings[0][0], 'could not constrain stream window geometry:');

  harness.runtime.scheduleAspectCorrection();
  const pendingTimer = [...harness.timers.keys()][0];
  harness.runtime.reset();
  assert.ok(harness.cancelled.includes(pendingTimer));
  assert.deepEqual(harness.runtime.snapshot(), {
    fittedStreamAspect: null,
    pendingStreamFit: null,
    lastObservedWindowSize: null,
    correctionScheduled: false,
  });
});

test('window correction delay remains pinned to the existing behavior', () => {
  assert.equal(STREAM_WINDOW_CORRECTION_DELAY_MS, 80);
});

test('Portal injects DOM geometry boundaries and delegates orchestration state', () => {
  assert.match(main, /const windowRuntime = createWindowRuntime\(\{/);
  assert.match(main, /getSurfaceBounds: \(\) => \{/);
  assert.match(main, /setSurfaceSize: \(\{ width, height \}\) => \{/);
  assert.match(main, /applyNativeGeometry: \(geometry, unmaximize\) => invoke\('set_client_window_size'/);
  assert.match(main, /windowRuntime\.reset\(\)/);
  assert.match(main, /windowRuntime\.scheduleAspectCorrection\(\)/);
  assert.doesNotMatch(main, /let fittedStreamAspect =/);
  assert.doesNotMatch(main, /let streamWindowResizeTimer =/);
  assert.doesNotMatch(main, /function fitWindowToIncomingStream\(/);
  assert.doesNotMatch(main, /function scheduleStreamWindowAspectCorrection\(/);
});
