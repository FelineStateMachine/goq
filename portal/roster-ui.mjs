const MAX_VIEWERS = 8;
const MAX_HANDLE_BYTES = 32;

function validatePresenceId(value) {
  if (typeof value !== 'string'
    || value.length < 1
    || value.length > MAX_HANDLE_BYTES
    || !/^[A-Za-z0-9-]+$/.test(value)) {
    throw new Error('viewer presence id must be a bounded opaque handle');
  }
  return value;
}

function compactHandle(handle) {
  return handle.length <= 18 ? handle : `${handle.slice(0, 8)}...${handle.slice(-6)}`;
}

export function rosterPresentation(snapshot) {
  if (!snapshot || !Array.isArray(snapshot.viewers)
    || snapshot.viewers.length < 1 || snapshot.viewers.length > MAX_VIEWERS) {
    throw new Error('roster must contain 1..=8 viewers');
  }
  const seen = new Set();
  const holder = snapshot.focus?.state === 'held' ? snapshot.focus.holder : null;
  const rows = snapshot.viewers.map((viewer) => {
    const handle = validatePresenceId(viewer?.presence_id);
    if (seen.has(handle)) throw new Error('roster contains a duplicate viewer');
    seen.add(handle);
    const isHolder = holder === handle && snapshot.focus.session_id === viewer.session_id;
    const input = viewer.input_capable === true ? 'input eligible' : 'view only';
    const role = isHolder ? 'holds control' : input;
    return Object.freeze({
      handle: compactHandle(handle),
      fullHandle: handle,
      you: viewer.you === true,
      inputCapable: viewer.input_capable === true,
      focusHolder: isHolder,
      role,
      adaptive: 'shared stream',
    });
  });
  const self = rows.filter((row) => row.you);
  if (self.length !== 1 || self[0].fullHandle !== snapshot.self_presence_id) {
    throw new Error('roster requires one matching you marker');
  }
  if (holder !== null && !rows.some((row) => row.focusHolder)) {
    throw new Error('focus holder is absent from the roster');
  }
  const focus = holder === null
    ? snapshot.focus?.state === 'neutralizing' ? 'control resetting' : 'control available'
    : self[0].focusHolder ? 'you hold control' : `${compactHandle(holder)} holds control`;
  return Object.freeze({
    count: rows.length,
    rows: Object.freeze(rows),
    focus,
    shell: `${rows.length} ${rows.length === 1 ? 'viewer' : 'viewers'} · ${focus}`,
  });
}

function appendText(document, parent, className, text) {
  const element = document.createElement('span');
  element.className = className;
  element.textContent = text;
  parent.appendChild(element);
  return element;
}

export function renderRoster(root, snapshot) {
  if (!root || typeof root.replaceChildren !== 'function' || !root.ownerDocument) {
    throw new TypeError('roster root must be a DOM element');
  }
  const presentation = rosterPresentation(snapshot);
  const { ownerDocument: document } = root;
  const fragment = document.createDocumentFragment();
  for (const row of presentation.rows) {
    const item = document.createElement('li');
    item.className = 'roster-viewer';
    if (row.you) item.classList.add('you');
    if (row.focusHolder) item.classList.add('focus-holder');
    appendText(document, item, 'roster-handle', row.handle);
    if (row.you) appendText(document, item, 'roster-you', 'you');
    appendText(document, item, 'roster-role', row.role);
    appendText(document, item, 'roster-adaptive', row.adaptive);
    item.setAttribute(
      'aria-label',
      `${row.handle}${row.you ? ', you' : ''}, ${row.role}, ${row.adaptive}`,
    );
    fragment.appendChild(item);
  }
  root.replaceChildren(fragment);
  root.setAttribute('aria-label', `${presentation.count} connected viewers. ${presentation.focus}`);
  return presentation;
}
