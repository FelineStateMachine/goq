import { validatePointerPositionFeedback } from './input-state.mjs';

const TERMINAL_REASONS = new Set(['eof', 'malformed']);

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
