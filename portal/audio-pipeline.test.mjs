import assert from 'node:assert/strict';
import test from 'node:test';

import {
  AUDIO_SAMPLE_RATE,
  createAudioPipelineSession,
} from './audio-pipeline.mjs';
import {
  AUDIO_CHANNEL_PREFIX_LENGTH,
  AUDIO_ENVELOPE_HEADER_LENGTH,
} from './audio-envelope.mjs';

const PACKET_BASE = AUDIO_CHANNEL_PREFIX_LENGTH;

function envelope({
  generation = 7n,
  deliveryId = 11n,
  flags = 0,
  pts = 120000n,
} = {}) {
  const payload = Uint8Array.of(0xf8, 0xff, 0xfe);
  const buffer = new ArrayBuffer(
    AUDIO_CHANNEL_PREFIX_LENGTH + AUDIO_ENVELOPE_HEADER_LENGTH + payload.length,
  );
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);
  bytes.set([0x53, 0x47, 0x41, 0x43]);
  view.setUint16(4, 1, false);
  view.setUint16(6, AUDIO_CHANNEL_PREFIX_LENGTH, false);
  view.setBigUint64(8, generation, false);
  view.setBigUint64(16, deliveryId, false);
  bytes.set([0x53, 0x47, 0x41, 0x31], PACKET_BASE);
  view.setUint16(PACKET_BASE + 4, 1, false);
  bytes[PACKET_BASE + 6] = AUDIO_ENVELOPE_HEADER_LENGTH;
  bytes[PACKET_BASE + 7] = 1;
  bytes[PACKET_BASE + 8] = flags;
  bytes[PACKET_BASE + 9] = 2;
  view.setUint32(PACKET_BASE + 12, AUDIO_SAMPLE_RATE, false);
  view.setUint16(PACKET_BASE + 16, 960, false);
  view.setUint32(PACKET_BASE + 20, payload.length, false);
  view.setBigUint64(PACKET_BASE + 24, 42n, false);
  view.setBigUint64(PACKET_BASE + 32, 123456n, false);
  view.setBigInt64(PACKET_BASE + 40, pts, false);
  bytes.set(payload, PACKET_BASE + AUDIO_ENVELOPE_HEADER_LENGTH);
  return buffer;
}

function harness({
  disconnecting = () => false,
  hasAudioDecoder = () => true,
  hasAudioWorklet = () => true,
} = {}) {
  const invokes = [];
  const updates = [];
  const avResets = [];
  const decoders = [];
  const worklets = [];
  const contexts = [];
  const chunks = [];

  class FakeContext {
    constructor(options) {
      this.options = options;
      this.sampleRate = options.sampleRate;
      this.state = 'suspended';
      this.destination = {};
      this.modules = [];
      this.resumeCalls = 0;
      this.closeCalls = 0;
      this.audioWorklet = {
        addModule: async (url) => { this.modules.push(url); },
      };
      contexts.push(this);
    }

    async resume() {
      this.resumeCalls++;
      this.state = 'running';
    }

    async close() {
      this.closeCalls++;
      this.state = 'closed';
    }

    getOutputTimestamp() {
      return { contextTime: 4, performanceTime: 1000 };
    }
  }

  class FakeChannel {
    constructor(callback) {
      this.callback = callback;
    }
  }

  const pipeline = createAudioPipelineSession({
    invokeCommand: async (command, args) => { invokes.push({ command, args }); },
    onUpdate: (session) => updates.push(session),
    resetAvSync: (...args) => {
      avResets.push(args);
      pipeline.resetSyncTelemetry(...args);
    },
    isDisconnecting: disconnecting,
    hasAudioDecoder,
    hasAudioWorklet,
    getAudioContextConstructor: () => FakeContext,
    probeDecoderSupport: async (config) => ({ supported: config.codec === 'opus' }),
    createDecoder: (callbacks) => {
      const decoder = {
        callbacks,
        state: 'configured',
        decodeQueueSize: 0,
        configured: null,
        decoded: [],
        closeCalls: 0,
        configure(config) { this.configured = config; },
        decode(chunk) { this.decoded.push(chunk); },
        close() { this.closeCalls++; this.state = 'closed'; },
      };
      decoders.push(decoder);
      return decoder;
    },
    createWorkletNode: (context, name, options) => {
      const node = {
        context,
        name,
        options,
        connected: null,
        disconnectCalls: 0,
        port: {
          messages: [],
          postMessage(message, transfers) { this.messages.push({ message, transfers }); },
          onmessage: null,
          onmessageerror: null,
        },
        connect(destination) { this.connected = destination; },
        disconnect() { this.disconnectCalls++; },
        onprocessorerror: null,
      };
      worklets.push(node);
      return node;
    },
    createEncodedChunk: (init) => {
      const chunk = { ...init };
      chunks.push(chunk);
      return chunk;
    },
  });

  return {
    pipeline,
    FakeChannel,
    invokes,
    updates,
    avResets,
    decoders,
    worklets,
    contexts,
    chunks,
  };
}

async function initializeAttempt(fixture) {
  fixture.pipeline.prepareForConnection();
  const primedSession = fixture.pipeline.session;
  fixture.pipeline.beginAttempt();
  const attempt = await fixture.pipeline.createAttemptChannel(fixture.FakeChannel, primedSession);
  return { ...attempt, primedSession };
}

test('connection preparation synchronously primes WebKit audio and initializes bounded Opus resources', async () => {
  const fixture = harness();
  const initial = fixture.pipeline.session;
  initial.muted = true;

  fixture.pipeline.prepareForConnection();
  const session = fixture.pipeline.session;
  assert.notEqual(session, initial);
  assert.equal(session.muted, true);
  assert.equal(session.acceptsNativeEvents, true);
  assert.equal(fixture.contexts.length, 1);
  assert.equal(fixture.contexts[0].resumeCalls, 1);

  fixture.pipeline.beginAttempt();
  const attempt = await fixture.pipeline.createAttemptChannel(fixture.FakeChannel, session);
  assert.equal(attempt.session, session);
  assert.deepEqual(attempt.support, { supported: true, error: null });
  assert.equal(attempt.channel, session.channel);
  assert.equal(fixture.decoders[0].configured.codec, 'opus');
  assert.equal(fixture.decoders[0].configured.sampleRate, AUDIO_SAMPLE_RATE);
  assert.equal(fixture.worklets[0].name, 'sigil-audio-processor');
  assert.deepEqual(fixture.worklets[0].port.messages[0].message, { type: 'mute', muted: true });
  assert.equal(session.state, 'negotiating');
  assert.equal(session.stateDetail, 'Opus output ready');
});

test('capability checks preserve the exact WebCodecs and AudioWorklet fallback errors', async () => {
  const missingDecoder = harness({ hasAudioDecoder: () => false });
  missingDecoder.pipeline.prepareForConnection();
  let session = missingDecoder.pipeline.beginAttempt();
  let attempt = await missingDecoder.pipeline.createAttemptChannel(
    missingDecoder.FakeChannel,
    session,
  );
  assert.deepEqual(attempt.support, {
    supported: false,
    error: 'WebCodecs AudioDecoder is unavailable',
  });

  const missingWorklet = harness({ hasAudioWorklet: () => false });
  missingWorklet.pipeline.prepareForConnection();
  session = missingWorklet.pipeline.beginAttempt();
  attempt = await missingWorklet.pipeline.createAttemptChannel(missingWorklet.FakeChannel, session);
  assert.deepEqual(attempt.support, {
    supported: false,
    error: 'AudioWorklet is unavailable',
  });
});

test('deliveries always acknowledge but only the current generation enters the bounded decoder', async () => {
  const fixture = harness();
  const attempt = await initializeAttempt(fixture);
  await fixture.pipeline.acceptConnectedResult({
    audio_generation: 7,
    audio_available: true,
    audio_error: null,
  }, attempt);

  attempt.channel.callback(envelope({ generation: 8n, deliveryId: 80n }));
  attempt.channel.callback(envelope({ generation: 7n, deliveryId: 70n }));
  await Promise.resolve();
  assert.deepEqual(fixture.invokes.slice(0, 2), [
    { command: 'iroh_client_ack_audio', args: { generation: 8, deliveryId: 80 } },
    { command: 'iroh_client_ack_audio', args: { generation: 7, deliveryId: 70 } },
  ]);
  assert.equal(fixture.decoders[0].decoded.length, 1);
  assert.equal(fixture.chunks[0].timestamp, 120000);
  assert.equal(fixture.chunks[0].duration, 20000);

  let audioDataClosed = false;
  fixture.decoders[0].callbacks.output({
    numberOfChannels: 2,
    numberOfFrames: 2,
    timestamp: 120000,
    copyTo: (channel, { planeIndex }) => channel.fill(planeIndex + 1),
    close: () => { audioDataClosed = true; },
  });
  const samples = fixture.worklets[0].port.messages.at(-1);
  assert.equal(samples.message.type, 'samples');
  assert.deepEqual([...samples.message.channels[0]], [1, 1]);
  assert.deepEqual([...samples.message.channels[1]], [2, 2]);
  assert.equal(samples.transfers.length, 2);
  assert.equal(audioDataClosed, true);

  fixture.decoders[0].decodeQueueSize = 3;
  attempt.channel.callback(envelope({ generation: 7n, deliveryId: 71n }));
  assert.equal(fixture.decoders.length, 2);
  assert.equal(fixture.decoders[0].closeCalls, 1);
  assert.equal(attempt.session.decoderDropped, 3);
  assert.equal(fixture.decoders[1].decoded.length, 1);
  assert.equal(fixture.avResets.at(-1)[1].recoverRing, true);
  assert.deepEqual(fixture.worklets[0].port.messages.at(-1).message, {
    type: 'recover',
    avSyncEpoch: attempt.session.avSyncEpoch,
  });
});

test('worklet telemetry is epoch-gated and projects signed audio/video skew', async () => {
  const fixture = harness();
  const attempt = await initializeAttempt(fixture);
  await fixture.pipeline.acceptConnectedResult({
    audio_generation: 7,
    audio_available: true,
    audio_error: null,
  }, attempt);
  const session = attempt.session;
  const worklet = fixture.worklets[0];

  worklet.port.onmessage({ data: {
    type: 'stats',
    avSyncEpoch: session.avSyncEpoch + 1,
    bufferedFrames: 99,
  } });
  assert.equal(session.bufferedFrames, 0);

  worklet.port.onmessage({ data: {
    type: 'stats',
    avSyncEpoch: session.avSyncEpoch,
    droppedFrames: 11,
    recoveryDiscardedFrames: 12,
    renderedMediaEndPtsMicros: 2_000_000,
    outputContextTimeEndSeconds: 4,
    bufferedFrames: 960,
    underflows: 2,
    underflowDurationMicros: 10_000,
    silentDurationMicros: 20_000,
    started: true,
  } });
  assert.equal(session.state, 'playing');
  assert.equal(session.workletDroppedFrames, 11);
  assert.equal(session.workletRecoveryDiscardedFrames, 12);
  assert.equal(session.bufferedFrames, 960);
  assert.equal(fixture.pipeline.sampleSkew(1_975_000, 1000), 25);

  fixture.pipeline.resetSyncTelemetry(session, { recoverRing: true });
  assert.equal(session.audioTimeline, null);
  assert.deepEqual(worklet.port.messages.at(-1).message, {
    type: 'recover',
    avSyncEpoch: session.avSyncEpoch,
  });
  assert.equal(fixture.pipeline.sampleSkew(1_975_000, 1000), null);
});

test('native terminal state is staged before connect and teardown isolates delayed callbacks', async () => {
  const fixture = harness();
  fixture.pipeline.prepareForConnection();
  fixture.pipeline.handleNativeState({ generation: 9, available: false, error: 'ended early' });
  assert.equal(fixture.pipeline.session.pendingTerminalStates.length, 1);
  const session = fixture.pipeline.beginAttempt();
  const attempt = await fixture.pipeline.createAttemptChannel(fixture.FakeChannel, session);
  const oldSession = attempt.session;
  const oldWorklet = fixture.worklets[0];
  await fixture.pipeline.acceptConnectedResult({
    audio_generation: 9,
    audio_available: true,
    audio_error: null,
  }, attempt);
  await Promise.resolve();
  assert.equal(oldSession.failed, true);
  assert.equal(oldSession.failureDetail, 'ended early');
  assert.deepEqual(fixture.invokes.at(-1), {
    command: 'iroh_client_stop_audio',
    args: { expectedGeneration: 9 },
  });

  oldSession.muted = true;
  await fixture.pipeline.teardown();
  const successor = fixture.pipeline.session;
  assert.notEqual(successor, oldSession);
  assert.equal(successor.muted, true);
  assert.equal(fixture.contexts[0].closeCalls, 1);
  assert.equal(oldWorklet.disconnectCalls, 1);
  const updateCount = fixture.updates.length;
  oldWorklet.port.onmessage({ data: { type: 'accepted', id: 1 } });
  assert.equal(fixture.updates.length, updateCount);
});

test('native stats and mute toggles update only the exact active generation', async () => {
  const fixture = harness();
  const attempt = await initializeAttempt(fixture);
  await fixture.pipeline.acceptConnectedResult({
    audio_generation: 7,
    audio_available: true,
    audio_error: null,
  }, attempt);
  fixture.pipeline.handleNativeStats({
    generation: 6,
    sequence_dropped_total: 99,
    frontend_dropped_total: 98,
  });
  assert.equal(attempt.session.transportDropped, 0);
  fixture.pipeline.handleNativeStats({
    generation: 7,
    sequence_dropped_total: 4,
    frontend_dropped_total: 5,
  });
  assert.equal(attempt.session.transportDropped, 4);
  assert.equal(attempt.session.frontendDropped, 5);

  await fixture.pipeline.toggleMute();
  assert.equal(attempt.session.muted, true);
  assert.deepEqual(fixture.worklets[0].port.messages.at(-1).message, {
    type: 'mute',
    muted: true,
  });
});
