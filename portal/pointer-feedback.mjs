import { validatePointerPositionFeedback } from './input-state.mjs';

const TERMINAL_REASONS = new Set(['eof', 'malformed']);

export function newPointerSession() {
  return {
    received: false,
    latest: null,
    failed: false,
    failureDetail: null,
    closing: false,
    channel: null,
    surfaceDimensions: null,
    remotePosition: null,
    remoteVisible: false,
  };
}

export function parsePointerFeedbackMessage(value, surface = null) {
  if (value?.type === 'position') {
    return {
      type: 'position',
      feedback: validatePointerPositionFeedback(value, surface),
    };
  }
  if (value?.type === 'terminal' && TERMINAL_REASONS.has(value.reason)) {
    return { type: 'terminal', reason: value.reason };
  }
  throw new TypeError('invalid pointer feedback message');
}

export function createPointerFeedbackRuntime({
  getActiveSession,
  getCapabilities,
  getSurface,
  render,
  disconnect,
  logger = console,
} = {}) {
  if (typeof getActiveSession !== 'function') {
    throw new TypeError('getActiveSession must be a function');
  }
  if (typeof getCapabilities !== 'function') {
    throw new TypeError('getCapabilities must be a function');
  }
  if (typeof getSurface !== 'function') throw new TypeError('getSurface must be a function');
  if (typeof render !== 'function') throw new TypeError('render must be a function');
  if (typeof disconnect !== 'function') throw new TypeError('disconnect must be a function');
  if (typeof logger?.error !== 'function') throw new TypeError('logger must provide error');

  function capabilities() {
    return getCapabilities() ?? {};
  }

  function disconnectIfConnected(snapshot) {
    if (snapshot.connected === true) void disconnect();
  }

  function applyPosition(message, session) {
    const snapshot = capabilities();
    if (session !== getActiveSession() || snapshot.pointerPositionFeedback !== true) return;
    const surface = getSurface();
    if (surface === null) return;
    const feedback = validatePointerPositionFeedback(message, surface);
    session.remotePosition = feedback.position;
    session.remoteVisible = feedback.pointer_visible;
    render();
  }

  function handleMessage(message, session) {
    if (session !== getActiveSession() || session.failed || session.closing) return;
    try {
      const envelope = parsePointerFeedbackMessage(message);
      if (envelope.type === 'terminal') {
        session.failed = true;
        session.failureDetail = envelope.reason === 'eof'
          ? 'Pointer feedback ended'
          : 'Pointer feedback was malformed';
        session.remotePosition = null;
        session.remoteVisible = false;
        render();
        logger.error(session.failureDetail);
        disconnectIfConnected(capabilities());
        return;
      }
      const feedback = envelope.feedback;
      session.received = true;
      session.latest = feedback;
      const snapshot = capabilities();
      if (snapshot.connected === true) applyPosition(feedback, session);
    } catch (error) {
      session.failed = true;
      session.failureDetail = `Pointer feedback failed: ${error}`;
      session.remotePosition = null;
      session.remoteVisible = false;
      render();
      logger.error('invalid pointer-position feedback:', error);
      disconnectIfConnected(capabilities());
    }
  }

  return Object.freeze({
    applyPosition,
    handleMessage,
  });
}
