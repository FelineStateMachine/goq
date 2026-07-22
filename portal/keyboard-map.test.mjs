import assert from 'node:assert/strict';
import test from 'node:test';

import { mapKey } from './keyboard-map.mjs';

test('maps the current named keyboard controls exactly', () => {
  for (const [key, expected] of [
    ['ArrowUp', 'Up'],
    ['ArrowDown', 'Down'],
    ['ArrowLeft', 'Left'],
    ['ArrowRight', 'Right'],
    [' ', 'Space'],
    ['Delete', 'Delete'],
    ['Backspace', 'Backspace'],
    ['Enter', 'Enter'],
    ['Tab', 'Tab'],
    ['Escape', 'Escape'],
    ['Shift', 'Shift'],
    ['Control', 'Control'],
    ['Alt', 'Alt'],
    ['Meta', 'Meta'],
    ['Home', 'Home'],
    ['End', 'End'],
    ['PageUp', 'PageUp'],
    ['PageDown', 'PageDown'],
  ]) {
    assert.equal(mapKey({ key }), expected, key);
  }
});

test('preserves only single ASCII-printable keys', () => {
  for (const key of ['!', 'a', 'A', '0', '~']) {
    assert.equal(mapKey({ key }), key, key);
  }
  for (const key of ['F1', 'F12', 'é', '😀', 'CapsLock', 'Insert', '']) {
    assert.equal(mapKey({ key }), null, key);
  }
});
