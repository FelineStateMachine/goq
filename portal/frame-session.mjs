export const FRAME_CHANNEL_CAPACITY = 4;

function validGeneration(value) {
  return Number.isSafeInteger(value) && value > 0;
}

export function newFrameSession() {
  return {
    expectedGeneration: null,
    pendingAcknowledgments: [],
    pendingLegacyFrames: [],
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
  const legacyFrames = session.pendingLegacyFrames.splice(0);
  acknowledgments.push(...legacyFrames.map((frame) => frame.generation));
  return { acknowledgments, legacyFrames };
}

export function isActiveFrameSession(session, activeSession) {
  return session !== null && session === activeSession && !session.closing;
}

export function stageLegacyFrame(session, payload) {
  if (!session || session.closing || !validGeneration(payload?.generation)) {
    throw new TypeError('invalid legacy frame delivery');
  }
  const generation = payload.generation;
  if (session.expectedGeneration !== null) {
    return {
      acknowledgments: [generation],
      staged: false,
      accepted: generation === session.expectedGeneration,
    };
  }

  const highestPendingGeneration = session.pendingLegacyFrames.reduce(
    (highest, frame) => Math.max(highest, frame.generation),
    0,
  );
  if (generation < highestPendingGeneration) {
    return { acknowledgments: [generation], staged: false, accepted: false };
  }

  const acknowledgments = [];
  if (generation > highestPendingGeneration && highestPendingGeneration > 0) {
    acknowledgments.push(...session.pendingLegacyFrames.map((frame) => frame.generation));
    session.pendingLegacyFrames.length = 0;
  }
  if (session.pendingLegacyFrames.length >= FRAME_CHANNEL_CAPACITY) {
    throw new Error('pre-connect legacy frame capacity exceeded');
  }
  session.pendingLegacyFrames.push(payload);
  return { acknowledgments, staged: true, accepted: false };
}
