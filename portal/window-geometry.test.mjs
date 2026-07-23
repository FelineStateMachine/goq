import assert from 'node:assert/strict';
import test from 'node:test';

import { fitStreamSurface } from './window-geometry.mjs';

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

test('arbitrary portrait and ultrawide host resolutions letterbox inside user bounds', () => {
  assert.deepEqual(fitStreamSurface({
    frameWidth: 800,
    frameHeight: 1280,
    availableWidth: 1920,
    availableHeight: 1080,
  }), { width: 675, height: 1080 });

  assert.deepEqual(fitStreamSurface({
    frameWidth: 3440,
    frameHeight: 1440,
    availableWidth: 1200,
    availableHeight: 900,
  }), { width: 1200, height: 502 });
});

test('malformed dimensions fail closed before changing the surface', () => {
  assert.throws(() => fitStreamSurface({
    frameWidth: 0,
    frameHeight: 800,
    availableWidth: 1920,
    availableHeight: 1080,
  }), /frame width/);
  assert.throws(() => fitStreamSurface({
    frameWidth: 1280,
    frameHeight: 800,
    availableWidth: Number.NaN,
    availableHeight: 1080,
  }), /available surface width/);
});
