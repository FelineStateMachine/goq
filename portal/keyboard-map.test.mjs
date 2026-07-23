import assert from 'node:assert/strict';
import test from 'node:test';

import {
  keyboardInputForEvent,
  mapKey,
} from './keyboard-map.mjs';

test('forwards the bounded physical code contract including extended keys', () => {
  for (const code of [
    'KeyA',
    'Digit0',
    'Minus',
    'BracketLeft',
    'IntlBackslash',
    'IntlRo',
    'IntlYen',
    'ShiftLeft',
    'ShiftRight',
    'ControlLeft',
    'ControlRight',
    'AltLeft',
    'AltRight',
    'MetaLeft',
    'MetaRight',
    'ArrowUp',
    'Insert',
    'PrintScreen',
    ...Array.from({ length: 12 }, (_, index) => `F${index + 1}`),
  ]) {
    assert.equal(mapKey({ code, key: 'layout-dependent' }), code, code);
  }
});

test('physical codes preserve client key positions across keyboard layouts', () => {
  assert.equal(mapKey({ code: 'KeyE', key: 'é' }), 'KeyE');
  assert.equal(mapKey({ code: 'Digit2', key: 'é' }), 'Digit2');
  assert.equal(mapKey({ code: 'KeyQ', key: 'a' }), 'KeyQ');
  assert.equal(mapKey({ code: 'AltRight', key: 'AltGraph' }), 'AltRight');
});

test('legacy missing-code events retain the compatible named and ASCII map', () => {
  for (const [key, expected] of [
    ['ArrowUp', 'Up'],
    [' ', 'Space'],
    ['Control', 'Control'],
    ['PageDown', 'PageDown'],
    ['Insert', 'Insert'],
    ['PrintScreen', 'PrintScreen'],
    ['F1', 'F1'],
    ['F12', 'F12'],
    ['!', '!'],
    ['a', 'a'],
  ]) {
    assert.equal(mapKey({ code: '', key }), expected, key);
  }
});

test('rejects unsupported physical and logical keys without guessing', () => {
  for (const event of [
    { code: 'F13', key: 'F13' },
    { code: 'CapsLock', key: 'CapsLock' },
    { code: 'Numpad1', key: '1' },
    { code: 'AudioVolumeUp', key: 'AudioVolumeUp' },
    { code: '', key: 'é' },
    { code: '', key: '😀' },
    { code: '', key: '' },
    null,
  ]) {
    assert.equal(mapKey(event), null);
  }
});

test('production keyboard capability forwards a physical code without Text', () => {
  assert.deepEqual(
    keyboardInputForEvent(
      { code: 'KeyE', key: 'é' },
      { keyboard: true, text: false },
    ),
    { type: 'key', key: 'KeyE' },
  );
});

test('hypothetical operational Text capability remains exclusive with keyboard', () => {
  const both = { keyboard: true, text: true };
  assert.deepEqual(
    keyboardInputForEvent({ code: 'F5', key: 'F5' }, both),
    { type: 'key', key: 'F5' },
  );
  assert.deepEqual(
    keyboardInputForEvent({ code: 'Unknown', key: 'é' }, both),
    { type: 'text', text: 'é' },
  );
});

test('hypothetical Text-only capability accepts one scalar and rejects unsafe input', () => {
  const textOnly = { keyboard: false, text: true };
  assert.deepEqual(
    keyboardInputForEvent({ code: '', key: 'é' }, textOnly),
    { type: 'text', text: 'é' },
  );
  assert.deepEqual(
    keyboardInputForEvent({ code: '', key: '😀' }, textOnly),
    { type: 'text', text: '😀' },
  );
  for (const event of [
    { code: '', key: 'a', ctrlKey: true },
    { code: '', key: 'a', altKey: true },
    { code: '', key: 'a', metaKey: true },
    { code: '', key: 'a', isComposing: true },
    { code: '', key: 'Enter' },
    { code: '', key: '\u007f' },
    { code: '', key: '\ud800' },
  ]) {
    assert.equal(keyboardInputForEvent(event, textOnly), null);
  }
  assert.equal(
    keyboardInputForEvent({ code: 'KeyA', key: 'a' }, {}),
    null,
  );
});
