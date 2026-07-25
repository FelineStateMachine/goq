import test from 'node:test';
import assert from 'node:assert/strict';

import { renderRoster, rosterPresentation } from './roster-ui.mjs';

function snapshot({ focus = { state: 'vacant', slot: 0 } } = {}) {
  return {
    revision: 4,
    self_presence_id: 'viewer-two',
    viewers: [
      { presence_id: 'viewer-one', session_id: 10, input_capable: true, you: false },
      { presence_id: 'viewer-two', session_id: 11, input_capable: false, you: true },
      { presence_id: 'viewer-three', session_id: 12, input_capable: true, you: false },
    ],
    focus,
  };
}

test('presentation exposes only opaque roster, coarse eligibility, and focus state', () => {
  const presentation = rosterPresentation(snapshot({
    focus: {
      state: 'held',
      slot: 0,
      holder: 'viewer-one',
      session_id: 10,
      focus_generation: 3,
    },
  }));
  assert.equal(presentation.count, 3);
  assert.equal(presentation.shell, '3 viewers · viewer-one holds control');
  assert.deepEqual(
    presentation.rows.map(({ fullHandle, role, you, adaptive }) => ({ fullHandle, role, you, adaptive })),
    [
      { fullHandle: 'viewer-one', role: 'holds control', you: false, adaptive: 'shared stream' },
      { fullHandle: 'viewer-two', role: 'view only', you: true, adaptive: 'shared stream' },
      { fullHandle: 'viewer-three', role: 'input eligible', you: false, adaptive: 'shared stream' },
    ],
  );
});

test('presentation rejects duplicate, unbounded, and inconsistent identities', () => {
  const duplicate = snapshot();
  duplicate.viewers[2].presence_id = 'viewer-one';
  assert.throws(() => rosterPresentation(duplicate), /duplicate/);
  const rawPeer = snapshot();
  rawPeer.viewers[0].presence_id = 'a'.repeat(64);
  assert.throws(() => rosterPresentation(rawPeer), /bounded opaque/);
  const missingHolder = snapshot({
    focus: { state: 'held', slot: 0, holder: 'viewer-gone', session_id: 90, focus_generation: 1 },
  });
  assert.throws(() => rosterPresentation(missingHolder), /absent/);
});

class FakeElement {
  constructor(tagName, document) {
    this.tagName = tagName;
    this.ownerDocument = document;
    this.children = [];
    this.attributes = new Map();
    this.className = '';
    this.classList = { add: (...names) => { this.className += ` ${names.join(' ')}`; } };
    this.textContent = '';
  }
  appendChild(child) { this.children.push(child); return child; }
  replaceChildren(...children) { this.children = children; }
  setAttribute(name, value) { this.attributes.set(name, value); }
}

test('renderer uses text nodes and emits a non-color-only accessible summary', () => {
  const document = {
    createElement: (tag) => new FakeElement(tag, document),
    createDocumentFragment: () => new FakeElement('#fragment', document),
  };
  const root = new FakeElement('ul', document);
  const presentation = renderRoster(root, snapshot());
  assert.equal(presentation.shell, '3 viewers · control available');
  assert.equal(root.attributes.get('aria-label'), '3 connected viewers. control available');
  const rows = root.children[0].children;
  assert.equal(rows.length, 3);
  assert.match(rows[1].attributes.get('aria-label'), /you, view only/);
  assert.equal('innerHTML' in rows[0], false);
});
