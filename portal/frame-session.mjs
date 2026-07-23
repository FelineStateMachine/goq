export const FRAME_CHANNEL_CAPACITY = 4;

function validGeneration(value) {
  return Number.isSafeInteger(value) && value > 0;
}

export function newFrameSession() {
  return {
    expectedGeneration: null,
    pendingAcknowledgments: [],
    pendingFrameErrors: [],
    failed: false,
    failureDetail: null,
    closing: false,
  };
}

// Before connect returns, a binary channel delivery has no generation in its
// envelope. Keep only the Rust channel's maximum outstanding acknowledgments;
// this is enough to release every permit once the connect result identifies
// the session, without allowing an unbounded callback backlog.
export function stageFrameAcknowledgment(session, generation = null) {
  if (!session || session.closing) return null;
  if (generation !== null && !validGeneration(generation)) {
    throw new TypeError('invalid frame generation');
  }
  if (session.expectedGeneration !== null) {
    return generation ?? session.expectedGeneration;
  }
  if (session.pendingAcknowledgments.length >= FRAME_CHANNEL_CAPACITY) {
    throw new Error('pre-connect frame acknowledgment capacity exceeded');
  }
  session.pendingAcknowledgments.push(generation);
  return null;
}

export function activateFrameSession(session, generation) {
  if (!session || session.closing || !validGeneration(generation)) {
    throw new TypeError('invalid active frame generation');
  }
  if (session.expectedGeneration !== null) {
    throw new Error('frame session is already active');
  }
  session.expectedGeneration = generation;
  const acknowledgments = session.pendingAcknowledgments.map(
    (pendingGeneration) => pendingGeneration ?? generation,
  );
  session.pendingAcknowledgments.length = 0;
  return { acknowledgments };
}

export function isActiveFrameSession(session, activeSession) {
  return session !== null && session === activeSession && !session.closing;
}
