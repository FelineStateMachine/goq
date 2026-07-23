import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { createWindowRuntime } from './window-runtime.mjs';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');
const nativeMain = await readFile(
  new URL('../src-tauri/src/main.rs', import.meta.url),
  'utf8',
);
const nativeState = await readFile(
  new URL('../src-tauri/src/commands/state.rs', import.meta.url),
  'utf8',
);
const nativeCargo = await readFile(
  new URL('../src-tauri/Cargo.toml', import.meta.url),
  'utf8',
);

function newHarness() {
  let format = { width: 1280, height: 800, epoch: 3 };
  let surfaceBounds = { width: 1920, height: 1080 };
  const surfaceSizes = [];
  const runtime = createWindowRuntime({
    getFormat: () => format,
    getSurfaceBounds: () => surfaceBounds,
    setSurfaceSize: (geometry) => surfaceSizes.push(geometry),
  });
  return {
    runtime,
    surfaceSizes,
    set format(value) { format = value; },
    set surfaceBounds(value) { surfaceBounds = value; },
  };
}

test('surface sizing scales to available bounds without changing native window geometry', () => {
  const harness = newHarness();

  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), true);
  assert.deepEqual(harness.surfaceSizes, [{ width: 1728, height: 1080 }]);
  assert.deepEqual(Object.keys(harness.runtime), ['sizeSurfaceToIncomingStream']);

  harness.surfaceBounds = null;
  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), false);
  harness.format = { width: 0, height: 800, epoch: 4 };
  harness.surfaceBounds = { width: 800, height: 600 };
  assert.equal(harness.runtime.sizeSurfaceToIncomingStream(), false);
  assert.equal(harness.surfaceSizes.length, 1);
});

test('Portal persists the user window and never derives native geometry from a stream', () => {
  assert.match(nativeCargo, /tauri-plugin-window-state = "2"/);
  assert.match(
    nativeMain,
    /\.plugin\(tauri_plugin_window_state::Builder::default\(\)\.build\(\)\)/,
  );
  assert.doesNotMatch(nativeMain, /set_client_window_size/);
  assert.doesNotMatch(nativeState, /set_client_window_size/);
  assert.doesNotMatch(main, /set_client_window_size/);
  assert.doesNotMatch(main, /fitWindowToIncomingStream/);
  assert.doesNotMatch(main, /scheduleAspectCorrection/);
  assert.match(main, /windowRuntime\.sizeSurfaceToIncomingStream\(\)/);
});
