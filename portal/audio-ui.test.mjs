import assert from 'node:assert/strict';
import test from 'node:test';

import { audioButtonPresentation, newAudioSession } from './audio-ui.mjs';

test('new audio sessions pin all resource, channel, status, and telemetry defaults', () => {
  const session = newAudioSession();

  assert.deepEqual({
    ...session,
    messageTracker: undefined,
  }, {
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
    muted: false,
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
    messageTracker: undefined,
  });
  assert.equal(session.messageTracker.size, 0);
  assert.equal(session.messageTracker.droppedMessages, 0);
});

test('new audio sessions preserve seeded mute state and isolate mutable state', () => {
  const muted = newAudioSession({ muted: true });
  const fresh = newAudioSession();

  muted.pendingTerminalStates.push({ generation: 7 });
  assert.equal(muted.muted, true);
  assert.equal(fresh.muted, false);
  assert.deepEqual(fresh.pendingTerminalStates, []);
  assert.notEqual(muted.pendingTerminalStates, fresh.pendingTerminalStates);
  assert.notEqual(muted.messageTracker, fresh.messageTracker);
});

test('audio button uses a speaker glyph with a mute action while playing', () => {
  assert.deepEqual(audioButtonPresentation({
    available: true,
    muted: false,
    state: 'playing',
    detail: 'bounded Opus playback',
  }), {
    glyph: '🔊',
    ariaLabel: 'Mute audio. Audio playing. bounded Opus playback',
    title: 'Audio playing: bounded Opus playback',
  });
});

test('audio button uses a muted glyph with an unmute action while muted', () => {
  const presentation = audioButtonPresentation({
    available: true,
    muted: true,
    state: 'playing',
    detail: 'bounded Opus playback',
  });
  assert.equal(presentation.glyph, '🔇');
  assert.match(presentation.ariaLabel, /^Unmute audio\. Audio muted\./);
});

test('audio button distinguishes priming from unavailable', () => {
  assert.equal(audioButtonPresentation({
    available: true,
    muted: false,
    state: 'priming',
    detail: 'Waiting for bounded Opus prebuffer',
  }).glyph, 'audio ...');
  const unavailable = audioButtonPresentation({
    available: false,
    muted: false,
    state: 'unavailable',
    detail: 'not connected',
  });
  assert.equal(unavailable.glyph, '🔇');
  assert.match(unavailable.ariaLabel, /^Audio unavailable\./);
});
