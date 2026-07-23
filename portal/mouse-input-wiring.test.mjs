import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

test('mouse buttons use the same WebKit event family as motion and scrolling', () => {
  assert.match(
    main,
    /window\.addEventListener\('mousedown', handleMouseDown, \{ capture: true \}\);/,
  );
  assert.match(
    main,
    /window\.addEventListener\('mouseup', \(e\) => \{[\s\S]*?releaseMouseButton\(e\);[\s\S]*?\}, \{ capture: true \}\);/,
  );

  const buttonHandlers = main.slice(
    main.indexOf('function handleMouseDown'),
    main.indexOf("window.addEventListener('contextmenu'"),
  );
  assert.doesNotMatch(buttonHandlers, /pointer(?:down|up|cancel)|pointerType|PointerCapture/);
});

test('relative control consumes the synthetic local click before UI toggles see it', () => {
  assert.match(
    main,
    /window\.addEventListener\('click', suppressLocalClickDuringRelativeControl, \{ capture: true \}\);/,
  );

  const clickGuard = main.slice(
    main.indexOf('function suppressLocalClickDuringRelativeControl'),
    main.indexOf("window.addEventListener('contextmenu'"),
  );
  assert.match(clickGuard, /controlRuntime\.active/);
  assert.match(clickGuard, /connectionState\.inputCapabilities\.relativePointer/);
  assert.match(clickGuard, /e\.preventDefault\(\)/);
  assert.match(clickGuard, /e\.stopImmediatePropagation\(\)/);
});
