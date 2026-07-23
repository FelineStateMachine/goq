// This bounded subset of W3C KeyboardEvent.code values is the Portal/Sigil
// physical-key contract. Codes identify key position rather than the active
// client layout, which is what games, Steam, and Linux evdev consume.
const PHYSICAL_KEY_CODES = new Set([
  ...Array.from({ length: 26 }, (_, index) => `Key${String.fromCharCode(65 + index)}`),
  ...Array.from({ length: 10 }, (_, index) => `Digit${index}`),
  'Minus',
  'Equal',
  'BracketLeft',
  'BracketRight',
  'Backslash',
  'IntlBackslash',
  'IntlRo',
  'IntlYen',
  'Semicolon',
  'Quote',
  'Backquote',
  'Comma',
  'Period',
  'Slash',
  'Enter',
  'Tab',
  'Space',
  'Backspace',
  'Escape',
  'ShiftLeft',
  'ShiftRight',
  'ControlLeft',
  'ControlRight',
  'AltLeft',
  'AltRight',
  'MetaLeft',
  'MetaRight',
  'ArrowUp',
  'ArrowDown',
  'ArrowLeft',
  'ArrowRight',
  'Home',
  'End',
  'PageUp',
  'PageDown',
  'Insert',
  'Delete',
  'PrintScreen',
  ...Array.from({ length: 12 }, (_, index) => `F${index + 1}`),
]);

const LEGACY_NAMED_KEYS = Object.freeze({
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  ' ': 'Space',
  Delete: 'Delete',
  Backspace: 'Backspace',
  Enter: 'Enter',
  Tab: 'Tab',
  Escape: 'Escape',
  Shift: 'Shift',
  Control: 'Control',
  Alt: 'Alt',
  Meta: 'Meta',
  Home: 'Home',
  End: 'End',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
  Insert: 'Insert',
  PrintScreen: 'PrintScreen',
});

function isAsciiPrintable(value) {
  return value.length === 1
    && value.codePointAt(0) >= 0x20
    && value.codePointAt(0) <= 0x7e;
}

function textValue(event) {
  if (event.ctrlKey || event.altKey || event.metaKey || event.isComposing) return null;
  if (typeof event.key !== 'string') return null;
  const codePoints = Array.from(event.key);
  if (codePoints.length !== 1) return null;
  const codePoint = codePoints[0].codePointAt(0);
  if (codePoint <= 0x1f
    || (codePoint >= 0x7f && codePoint <= 0x9f)
    || (codePoint >= 0xd800 && codePoint <= 0xdfff)) return null;
  return event.key;
}

export function mapKey(event) {
  if (!event || typeof event !== 'object') return null;
  if (typeof event.code === 'string' && event.code.length > 0) {
    return PHYSICAL_KEY_CODES.has(event.code) ? event.code : null;
  }
  if (typeof event.key !== 'string') return null;
  if (/^F(?:[1-9]|1[0-2])$/.test(event.key)) return event.key;
  return LEGACY_NAMED_KEYS[event.key]
    ?? (isAsciiPrintable(event.key) ? event.key : null);
}

// Resolve each keydown to exactly one protocol class. Physical keyboard input
// wins whenever it is available; text is a fallback for virtual keyboards or
// unsupported physical codes and is never emitted alongside a key transition.
export function keyboardInputForEvent(event, capabilities = {}) {
  if (capabilities.keyboard === true) {
    const key = mapKey(event);
    if (key !== null) return { type: 'key', key };
  }
  if (capabilities.text === true) {
    const text = textValue(event);
    if (text !== null) return { type: 'text', text };
  }
  return null;
}
