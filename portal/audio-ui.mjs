import { BoundedAudioMessageTracker } from './audio-ring.mjs';

export function newAudioSession({ muted = false } = {}) {
  return {
    expectedGeneration: null,
    acceptsNativeEvents: false,
    pendingTerminalStates: [],
    failed: false,
    failureDetail: null,
    stopRequested: false,
    channel: null,
    context: null,
    decoder: null,
    workletNode: null,
    available: false,
    muted,
    state: 'unavailable',
    stateDetail: 'not connected',
    packetsReceived: 0,
    decoderDropped: 0,
    bufferedFrames: 0,
    underflows: 0,
    underflowDurationMicros: 0,
    silentDurationMicros: 0,
    transportDropped: 0,
    frontendDropped: 0,
    workletDroppedFrames: 0,
    workletRecoveryDiscardedFrames: 0,
    audioTimeline: null,
    avSyncEpoch: 0,
    messageTracker: new BoundedAudioMessageTracker(),
  };
}

export function audioButtonPresentation({ available, muted, state, detail }) {
  const normalizedState = typeof state === 'string' && state.length > 0 ? state : 'unavailable';
  const normalizedDetail = typeof detail === 'string' && detail.length > 0
    ? detail
    : normalizedState;

  let label;
  if (!available || muted || normalizedState === 'error' || normalizedState === 'unavailable') {
    label = 'vol off';
  } else if (normalizedState === 'playing') {
    label = 'vol on';
  } else {
    label = 'vol ...';
  }

  const action = available
    ? muted ? 'Unmute audio' : 'Mute audio'
    : 'Audio unavailable';
  const audibleState = muted && available ? 'muted' : normalizedState;
  return {
    label,
    ariaLabel: `${action}. Audio ${audibleState}. ${normalizedDetail}`,
    title: `Audio ${audibleState}: ${normalizedDetail}`,
  };
}
