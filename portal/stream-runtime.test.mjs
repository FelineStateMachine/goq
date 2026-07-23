import test from 'node:test';
import assert from 'node:assert/strict';

import { FRAME_CHANNEL_CAPACITY } from './frame-session.mjs';
import { createStreamRuntime } from './stream-runtime.mjs';

function frameEnvelope({ sequence = 11, ptsMicros = 22, payload = [0x65] } = {}) {
  const buffer = new ArrayBuffer(40 + payload.length);
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);
  bytes.set([0x53, 0x47, 0x46, 0x52]);
  bytes[4] = 1;
  bytes[5] = 1;
  bytes[6] = 0b00000101;
  view.setUint16(8, 1280, false);
  view.setUint16(10, 800, false);
  view.setUint32(12, payload.length, false);
  view.setBigUint64(16, BigInt(sequence), false);
  view.setBigUint64(24, 0xffffffffffffffffn, false);
  view.setBigInt64(32, BigInt(ptsMicros), false);
  bytes.set(payload, 40);
  return buffer;
}

function videoSnapshot(overrides = {}) {
  return {
    decoderQueueDepth: 1,
    decoderQueueCapacity: 2,
    presenterQueueDepth: 1,
    presenterQueueCapacity: 2,
    droppedFrames: 3,
    presentationDroppedFrames: 4,
    decodePercentiles: { p95: 5 },
    presentPercentiles: { p95: 6 },
    recovering: false,
    ...overrides,
  };
}

function subject({ connected = false } = {}) {
  const events = [];
  const video = {
    processFramePayload(payload, data = null) {
      events.push({ type: 'process', payload, data });
    },
    reset() { events.push({ type: 'video-reset' }); },
    teardown() { events.push({ type: 'video-teardown' }); },
    snapshot(at) {
      events.push({ type: 'video-snapshot', at });
      return videoSnapshot();
    },
  };
  const publisher = {
    start(generation, available) {
      events.push({ type: 'publisher-start', generation, available });
    },
    stop() { events.push({ type: 'publisher-stop' }); },
    publish(snapshot) {
      events.push({ type: 'publisher-publish', snapshot });
      return true;
    },
  };
  const logger = {
    warn(...args) { events.push({ type: 'warn', args }); },
    error(...args) { events.push({ type: 'error', args }); },
  };
  const runtime = createStreamRuntime({
    invokeCommand(command, args) {
      events.push({ type: 'invoke', command, args });
      return Promise.resolve(true);
    },
    videoPipeline: video,
    isConnected: () => connected,
    disconnect: async () => { events.push({ type: 'disconnect' }); },
    adaptiveFeedbackPublisher: publisher,
    logger,
  });
  return { runtime, events, video, publisher };
}

function activate(runtime, session, generation = 7, overrides = {}) {
  runtime.activateFrameSession(session, {
    generation,
    adaptiveFeedbackAvailable: true,
    adaptiveFeedbackError: null,
    ...overrides,
  });
}

test('connection lifecycle resets stream state while close preserves the adaptive error quirk', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 7, {
    adaptiveFeedbackAvailable: false,
    adaptiveFeedbackError: 'not supported',
  });
  assert.equal(runtime.snapshot().adaptiveFeedbackError, 'not supported');

  runtime.closeFrameSession(session);
  assert.equal(session.closing, true);
  assert.equal(runtime.generation, null);
  assert.equal(runtime.snapshot().adaptiveFeedbackAvailable, false);
  assert.equal(runtime.snapshot().adaptiveFeedbackError, 'not supported');

  runtime.prepareConnection();
  const reset = runtime.snapshot(123);
  assert.equal(reset.adaptiveFeedbackError, null);
  assert.equal(reset.transportDroppedFrames, 0);
  assert.equal(reset.streamPathMode, 'unknown');
  assert.ok(events.some((event) => event.type === 'video-reset'));

  runtime.prepareDisconnect();
  assert.equal(runtime.snapshot().adaptiveFeedbackError, null);
  runtime.teardown();
  assert.ok(events.some((event) => event.type === 'video-teardown'));
});

test('closing a partially constructed attempt falls back to the active frame session', () => {
  const { runtime } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();

  runtime.closeFrameSession();

  assert.equal(session.closing, true);
  assert.equal(runtime.activeSession, null);
  assert.equal(runtime.generation, null);
});

test('activation flushes pre-connect binary acknowledgments', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  assert.equal(runtime.handleBinaryFrame(frameEnvelope(), session), true);

  events.length = 0;
  activate(runtime, session, 7);
  assert.deepEqual(events.map((event) => event.type), [
    'invoke',
    'publisher-start',
  ]);
  assert.equal(events[0].args.generation, 7);
});

test('a current pending frame error aborts activation after acknowledgments', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  runtime.handleBinaryFrame(frameEnvelope(), session);
  runtime.handleFrameError({ generation: 9, error: 'native failed' });
  events.length = 0;

  assert.throws(() => activate(runtime, session, 9), /native failed/);
  assert.deepEqual(events.map((event) => event.type), ['invoke']);
  assert.equal(runtime.generation, 9);
});

test('active binary delivery acknowledges before decode and malformed delivery still acknowledges', () => {
  const { runtime, events } = subject({ connected: true });
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 5);
  events.length = 0;

  assert.equal(runtime.handleBinaryFrame(frameEnvelope({ sequence: 31 }), session), true);
  assert.deepEqual(events.slice(0, 2).map((event) => event.type), ['invoke', 'process']);
  assert.equal(events[0].args.generation, 5);
  assert.equal(events[1].payload.sequence, 31);
  assert.ok(events[1].data instanceof Uint8Array);

  events.length = 0;
  assert.equal(runtime.handleBinaryFrame(new ArrayBuffer(1), session), false);
  assert.equal(session.failed, true);
  assert.match(session.failureDetail, /shorter than its header/);
  assert.deepEqual(events.map((event) => event.type), ['error', 'disconnect', 'invoke']);
  assert.equal(events[2].args.generation, 5);
});

test('pre-connect frame errors are bounded and stale active errors are ignored', () => {
  const { runtime, events } = subject({ connected: true });
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  for (let index = 0; index < FRAME_CHANNEL_CAPACITY; index++) {
    assert.equal(runtime.handleFrameError({ generation: 20 + index, error: `${index}` }), true);
  }
  assert.equal(runtime.handleFrameError({ generation: 99, error: 'overflow' }), false);
  assert.equal(session.failed, true);
  assert.ok(events.some((event) => event.type === 'disconnect'));

  runtime.prepareConnection();
  const next = runtime.openFrameSession();
  activate(runtime, next, 30);
  assert.equal(runtime.handleFrameError({ generation: 29, error: 'stale' }), false);
  assert.equal(next.failed, false);
});

test('frame stats keep legacy FPS fallbacks and reset to exact defaults', () => {
  const { runtime } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 4);
  assert.equal(runtime.handleFrameStats({ generation: 3, fps: 1 }), false);
  assert.equal(runtime.handleFrameStats({
    generation: 4,
    stats_version: 3,
    sequence_dropped_total: 6,
    frontend_dropped_total: 7,
    transport_object_dropped_total: 5,
    transport_late_object_dropped_total: 2,
    frontend_queue_dropped_total: 4,
    frontend_resync_dropped_total: 3,
    frontend_queue_depth: 1,
    frontend_queue_peak: 2,
    frontend_queue_capacity: 4,
    frontend_resync_episode_total: 1,
    frontend_resync_duration_ms_total: 12,
    frontend_resync_duration_ms_max: 12,
    frontend_resync_active: true,
    frontend_resync_duration_ms_current: 2,
    transport_interval_sample_count: 0,
    frontend_ipc_send_duration_sample_count: 0,
    timing_window_ms: 1000,
    timing_sample_capacity: 120,
    path_mode: 'future-mode-is-kept-verbatim',
    path_rtt_ms: 8.5,
    sequence: 41,
    fps: 44,
  }), true);
  const snapshot = runtime.snapshot();
  assert.equal(snapshot.transportDroppedFrames, 6);
  assert.equal(snapshot.transportObjectDroppedFrames, 5);
  assert.equal(snapshot.transportLateObjectDroppedFrames, 2);
  assert.equal(snapshot.frontendDroppedFrames, 7);
  assert.deepEqual(snapshot.frontendQueueStats, { depth: 1, peak: 2, capacity: 4 });
  assert.equal(snapshot.frontendResyncStats.active, true);
  assert.equal(snapshot.streamPathMode, 'future-mode-is-kept-verbatim');
  assert.equal(snapshot.streamRttMs, 8.5);
  assert.equal(snapshot.lastMediaSequence, 41);
  assert.equal(snapshot.lastTransportFps, 44);
  assert.equal(snapshot.lastFrontendSendFps, 44);

  runtime.prepareConnection();
  const reset = runtime.snapshot();
  assert.equal(reset.lastTransportFps, 0);
  assert.equal(reset.lastFrontendSendFps, 0);
  assert.equal(reset.frontendQueueStats, null);
});

test('adaptive diagnostics reject stale generations and stop failed feedback', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 12);
  const decision = {
    generation: 12,
    decision: {
      decision_id: 1,
      report_id: 2,
      target_kbps: 8000,
      floor_kbps: 2000,
      ceiling_kbps: 12000,
      state: 'decrease',
      reasons: ['queue'],
      applied: false,
    },
  };
  assert.equal(runtime.handleAdaptiveDecision({ ...decision, generation: 11 }), false);
  assert.equal(runtime.handleAdaptiveDecision(decision), true);
  assert.equal(runtime.snapshot().adaptiveDecision.target_kbps, 8000);
  assert.equal(runtime.handleAdaptiveFeedbackState({ generation: 11, available: false }), false);
  assert.equal(runtime.handleAdaptiveFeedbackState({ generation: 12, available: false }), true);
  assert.equal(runtime.snapshot().adaptiveFeedbackAvailable, false);
  assert.equal(runtime.snapshot().adaptiveFeedbackError, 'feedback stream closed');
  assert.ok(events.some((event) => event.type === 'publisher-stop'));

  assert.equal(runtime.handleAdaptiveDecision({ generation: 12, decision: null }), false);
  assert.equal(events.at(-1).type, 'warn');
});

test('adaptive feedback preserves bounded queue fallbacks and pressure signals', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 6);
  runtime.handleFrameStats({
    generation: 6,
    stats_version: 2,
    sequence_dropped_total: 10,
    frontend_dropped_total: 11,
    frontend_resync_active: true,
    frontend_resync_episode_total: 1,
    frontend_resync_duration_ms_total: 3,
    frontend_resync_duration_ms_max: 3,
    frontend_resync_duration_ms_current: 1,
    fps: 60,
    sequence: 99,
  });
  events.length = 0;
  const video = videoSnapshot({ recovering: false });
  assert.equal(runtime.publishAdaptiveFeedback(video), true);
  const published = events[0].snapshot;
  assert.deepEqual(published, {
    lastSequence: 99,
    frontendQueueDepth: 0,
    frontendQueueCapacity: 4,
    decoderQueueDepth: 1,
    decoderQueueCapacity: 2,
    presenterQueueDepth: 1,
    presenterQueueCapacity: 2,
    transportDroppedTotal: 10,
    frontendDroppedTotal: 11,
    decoderDroppedTotal: 3,
    presenterDroppedTotal: 4,
    transportDeliveryP95Ms: null,
    decodeLatencyP95Ms: 5,
    presentationLatencyP95Ms: 6,
    resyncActive: true,
  });
});

test('adaptive feedback projects the bounded presenter queue and overwrite counter', () => {
  const { runtime, events } = subject();
  runtime.prepareConnection();
  const session = runtime.openFrameSession();
  activate(runtime, session, 6);
  events.length = 0;

  assert.equal(runtime.publishAdaptiveFeedback(videoSnapshot({
    presenterQueueDepth: 2,
    presenterQueueCapacity: 2,
    presentationDroppedFrames: 3,
  })), true);
  assert.equal(events[0].snapshot.presenterQueueDepth, 2);
  assert.equal(events[0].snapshot.presenterQueueCapacity, 2);
  assert.equal(events[0].snapshot.presenterDroppedTotal, 3);

  events.length = 0;
  assert.equal(runtime.publishAdaptiveFeedback(videoSnapshot({
    presenterQueueDepth: 0,
    presenterQueueCapacity: 2,
    presentationDroppedFrames: 3,
  })), true);
  assert.equal(events[0].snapshot.presenterQueueDepth, 0);
  assert.equal(events[0].snapshot.presenterQueueCapacity, 2);
  assert.equal(events[0].snapshot.presenterDroppedTotal, 3);
});
