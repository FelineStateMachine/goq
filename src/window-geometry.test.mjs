import assert from 'node:assert/strict';
import test from 'node:test';

import {
  constrainStreamWindowResize,
  fitInitialStreamWindow,
  fitStreamSurface,
} from './window-geometry.mjs';

test('initial stream window fits the display while preserving the drawable aspect', () => {
  const geometry = fitInitialStreamWindow({
    frameWidth: 1280,
    frameHeight: 800,
    chromeHeight: 64,
    availableWidth: 1440,
    availableHeight: 900,
  });

  assert.deepEqual(geometry, { width: 1165, height: 792 });
  assert.ok(Math.abs((geometry.height - 64) - geometry.width / 1.6) <= 1);
});

test('width-led resize corrects height and height-led resize corrects width', () => {
  assert.deepEqual(constrainStreamWindowResize({
    frameWidth: 1280,
    frameHeight: 800,
    chromeHeight: 64,
    width: 1000,
    height: 700,
    previousWidth: 900,
    previousHeight: 700,
  }), { width: 1000, height: 689 });

  assert.deepEqual(constrainStreamWindowResize({
    frameWidth: 1280,
    frameHeight: 800,
    chromeHeight: 64,
    width: 1000,
    height: 800,
    previousWidth: 1000,
    previousHeight: 700,
  }), { width: 1178, height: 800 });
});

test('already-correct geometry is stable and malformed dimensions fail closed', () => {
  assert.equal(constrainStreamWindowResize({
    frameWidth: 16,
    frameHeight: 10,
    chromeHeight: 64,
    width: 1280,
    height: 864,
    previousWidth: 1200,
    previousHeight: 814,
  }), null);

  assert.throws(() => fitInitialStreamWindow({
    frameWidth: 0,
    frameHeight: 800,
    chromeHeight: 64,
    availableWidth: 1440,
    availableHeight: 900,
  }), /frame width/);
  assert.throws(() => fitInitialStreamWindow({
    frameWidth: 1280,
    frameHeight: 800,
    chromeHeight: 900,
    availableWidth: 1440,
    availableHeight: 900,
  }), /chrome/);
});

test('stream surface scales above native resolution without changing aspect ratio', () => {
  assert.deepEqual(fitStreamSurface({
    frameWidth: 1280,
    frameHeight: 800,
    availableWidth: 1920,
    availableHeight: 1200,
  }), { width: 1920, height: 1200 });

  assert.deepEqual(fitStreamSurface({
    frameWidth: 1280,
    frameHeight: 800,
    availableWidth: 1920,
    availableHeight: 1080,
  }), { width: 1728, height: 1080 });
});
