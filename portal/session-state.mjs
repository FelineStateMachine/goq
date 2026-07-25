function emptySnapshotState() {
  return {
    mode: 'disconnected',
    nativeGeneration: 0,
    revision: 0,
    snapshot: null,
    eligible: false,
    focused: false,
    focusGeneration: null,
    locallyActive: false,
  };
}

function normalizeSnapshot(snapshot) {
  if (!snapshot || !Number.isSafeInteger(snapshot.revision) || snapshot.revision <= 0) {
    throw new Error('session snapshot revision must be a positive safe integer');
  }
  if (!Array.isArray(snapshot.viewers) || snapshot.viewers.length < 1 || snapshot.viewers.length > 8) {
    throw new Error('session snapshot roster must contain 1..=8 viewers');
  }
  const self = snapshot.viewers.find((viewer) => viewer.you === true);
  if (!self || self.presence_id !== snapshot.self_presence_id) {
    throw new Error('session snapshot does not contain the self viewer');
  }
  const focus = snapshot.focus;
  const focused = focus?.state === 'held'
    && focus.holder === self.presence_id
    && focus.session_id === self.session_id
    && Number.isSafeInteger(focus.focus_generation)
    && focus.focus_generation > 0;
  return {
    snapshot,
    revision: snapshot.revision,
    eligible: self.input_capable === true,
    focused,
    focusGeneration: focused ? focus.focus_generation : null,
  };
}

export function createSessionState({ onFocusLoss = () => {}, onChange = () => {} } = {}) {
  if (typeof onFocusLoss !== 'function' || typeof onChange !== 'function') {
    throw new TypeError('session state callbacks must be functions');
  }
  let state = emptySnapshotState();

  function publish(next) {
    const lostFocus = state.focused && !next.focused;
    if (lostFocus) onFocusLoss({ previous: snapshot(), next: { ...next } });
    state = next;
    onChange(snapshot());
    return true;
  }

  function acceptConnection({ nativeGeneration, controlProtocol, initialSnapshot, eligible }) {
    if (!Number.isSafeInteger(nativeGeneration) || nativeGeneration <= 0) {
      throw new Error('native generation must be a positive safe integer');
    }
    if (controlProtocol !== 'control-v2') {
      return publish({
        ...emptySnapshotState(),
        mode: 'legacy-v1',
        nativeGeneration,
        eligible: eligible === true,
        focused: eligible === true,
      });
    }
    const normalized = normalizeSnapshot(initialSnapshot);
    return publish({
      ...emptySnapshotState(),
      ...normalized,
      mode: 'control-v2',
      nativeGeneration,
    });
  }

  function applyNativeSnapshot(payload) {
    if (state.mode !== 'control-v2') return false;
    if (payload?.native_generation !== state.nativeGeneration) return false;
    const normalized = normalizeSnapshot(payload.snapshot);
    if (normalized.revision <= state.revision) return false;
    return publish({
      ...state,
      ...normalized,
      locallyActive: normalized.focused ? state.locallyActive : false,
    });
  }

  function setLocallyActive(active) {
    const next = active === true && state.focused;
    if (next === state.locallyActive) return false;
    state = { ...state, locallyActive: next };
    onChange(snapshot());
    return true;
  }

  function reset() {
    if (state.focused) onFocusLoss({ previous: snapshot(), next: emptySnapshotState() });
    state = emptySnapshotState();
    onChange(snapshot());
  }

  function snapshot() {
    return { ...state, snapshot: state.snapshot };
  }

  return Object.freeze({
    acceptConnection,
    applyNativeSnapshot,
    reset,
    setLocallyActive,
    snapshot,
    get mode() { return state.mode; },
    get eligible() { return state.eligible; },
    get focused() { return state.focused; },
    get focusGeneration() { return state.focusGeneration; },
    get locallyActive() { return state.locallyActive; },
    get revision() { return state.revision; },
    get nativeGeneration() { return state.nativeGeneration; },
  });
}
