export function mapKey(event) {
  const key = event.key;
  const map = {
    'ArrowUp': 'Up', 'ArrowDown': 'Down', 'ArrowLeft': 'Left', 'ArrowRight': 'Right',
    ' ': 'Space', 'Delete': 'Delete', 'Backspace': 'Backspace',
    'Enter': 'Enter', 'Tab': 'Tab', 'Escape': 'Escape',
    'Shift': 'Shift', 'Control': 'Control', 'Alt': 'Alt', 'Meta': 'Meta',
    'Home': 'Home', 'End': 'End', 'PageUp': 'PageUp', 'PageDown': 'PageDown',
  };
  const asciiPrintable = key.length === 1
    && key.codePointAt(0) >= 0x20
    && key.codePointAt(0) <= 0x7e;
  return map[key] || (asciiPrintable ? key : null);
}
