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
