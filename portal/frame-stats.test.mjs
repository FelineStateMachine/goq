import test from 'node:test';
import assert from 'node:assert/strict';
import {
  formatVideoDiscardTelemetry,
  isCurrentFrameGeneration,
  normalizeFrameStatsPayload,
} from './frame-stats.mjs';

test('formats each video discard owner in exact frame units', () => {
  assert.deepEqual(formatVideoDiscardTelemetry({
    transportDroppedFrames: 3,
    frontendDroppedFrames: 4,
    decoderDroppedFrames: 5,
    presenterOverwrittenFrames: 6,
  }), {
    total: '18 frames',
    transport: '3 frames',
    frontend: '4 frames',
    decoder: '5 frames',
    presenterOverwrite: '6 frames',
  });

  assert.equal(formatVideoDiscardTelemetry({
    transportDroppedFrames: 0,
    frontendDroppedFrames: 0,
    decoderDroppedFrames: 0,
    presenterOverwrittenFrames: 1,
  }).presenterOverwrite, '1 frame');
});

test('rejects inexact video discard counters and totals', () => {
  assert.throws(() => formatVideoDiscardTelemetry({
    transportDroppedFrames: -1,
    frontendDroppedFrames: 0,
    decoderDroppedFrames: 0,
    presenterOverwrittenFrames: 0,
  }), /exact unsigned integers/);

  assert.throws(() => formatVideoDiscardTelemetry({
    transportDroppedFrames: Number.MAX_SAFE_INTEGER,
    frontendDroppedFrames: 1,
    decoderDroppedFrames: 0,
    presenterOverwrittenFrames: 0,
  }), /exact integer range/);
});

test('accepts frame stats only for the exact active nonzero generation', () => {
  assert.equal(isCurrentFrameGeneration(7, 7), true);
  assert.equal(isCurrentFrameGeneration(6, 7), false);
  assert.equal(isCurrentFrameGeneration(0, 0), false);
  assert.equal(isCurrentFrameGeneration(undefined, undefined), false);
  assert.equal(isCurrentFrameGeneration(Number.MAX_SAFE_INTEGER + 1, 7), false);
});

test('normalizes exact v2 drop, queue, timing, and resync units', () => {
  assert.deepEqual(normalizeFrameStatsPayload({
    stats_version: 2,
    sequence_dropped_total: 3,
    frontend_queue_dropped_total: 4,
    frontend_resync_dropped_total: 5,
    frontend_dropped_total: 9,
    frontend_queue_depth: 1,
    frontend_queue_peak: 3,
    frontend_queue_capacity: 4,
    frontend_resync_episode_total: 2,
    frontend_resync_active: true,
    frontend_resync_duration_ms_total: 125.5,
    frontend_resync_duration_ms_current: 25.25,
    frontend_resync_duration_ms_max: 100.25,
    timing_window_ms: 5000,
    timing_sample_capacity: 512,
    transport_interval_sample_count: 3,
    transport_interval_p50_ms: 16.6,
    transport_interval_p95_ms: 25.1,
    transport_interval_max_ms: 40.2,
    frontend_ipc_send_duration_sample_count: 2,
    frontend_ipc_send_duration_p50_ms: 0.1,
    frontend_ipc_send_duration_p95_ms: 0.4,
    frontend_ipc_send_duration_max_ms: 0.4,
  }), {
    statsVersion: 2,
    transportDroppedFrames: 3,
    objectDroppedFrames: null,
    lateObjectDroppedFrames: null,
    frontendDroppedFrames: 9,
    queueDroppedFrames: 4,
    resyncDroppedFrames: 5,
    queue: { depth: 1, peak: 3, capacity: 4 },
    resync: { episodes: 2, active: true, totalMs: 125.5, currentMs: 25.25, maxMs: 100.25 },
    transportIntervals: { count: 3, p50Ms: 16.6, p95Ms: 25.1, maxMs: 40.2 },
    ipcSendDurations: { count: 2, p50Ms: 0.1, p95Ms: 0.4, maxMs: 0.4 },
    timingWindow: { windowMs: 5000, sampleCapacity: 512 },
    networkDiagnostics: null,
  });
});

test('uses legacy aggregate drop aliases without inventing v2 splits', () => {
  const stats = normalizeFrameStatsPayload({
    host_dropped_frames: 7,
    frontend_dropped_frames: 11,
  });
  assert.equal(stats.statsVersion, 1);
  assert.equal(stats.transportDroppedFrames, 7);
  assert.equal(stats.objectDroppedFrames, null);
  assert.equal(stats.lateObjectDroppedFrames, null);
  assert.equal(stats.frontendDroppedFrames, 11);
  assert.equal(stats.queueDroppedFrames, null);
  assert.equal(stats.resyncDroppedFrames, null);
  assert.equal(stats.queue, null);
  assert.equal(stats.resync, null);
  assert.equal(stats.transportIntervals, null);
  assert.equal(stats.ipcSendDurations, null);
  assert.equal(stats.timingWindow, null);
  assert.equal(stats.networkDiagnostics, null);
});

test('normalizes v4 network diagnostics without changing v1-v3 behavior', () => {
  const leg = {
    mode: 'direct',
    sample_count: 1,
    direct_samples: 1,
    relay_samples: 0,
    custom_samples: 0,
    unknown_samples: 0,
    path_epoch: 1,
    path_switch_total: 0,
    mode_transition_total: 0,
    rtt_sample_count: 1,
    rtt_current_ms: 0.8,
    rtt_p50_ms: 5,
    rtt_p95_ms: 5,
    rtt_max_ms: 5,
    rtt_overflow_total: 0,
    tx_datagrams: 1,
    tx_bytes: 100,
    rx_datagrams: 1,
    rx_bytes: 100,
    lost_packets: 0,
    lost_bytes: 0,
    cwnd_bytes: 12000,
    mtu: 1200,
    congestion_events: 0,
    black_holes_detected: 0,
    counter_regression_total: 0,
    complete: true,
  };
  const inputAck = {
    negotiated: true,
    sent_total: 1,
    acknowledged_total: 1,
    pending_count: 0,
    pending_capacity: 64,
    duplicate_total: 0,
    untracked_total: 0,
    malformed: false,
    closed: false,
    latency_sample_count: 1,
    latency_p50_ms: 2,
    latency_p95_ms: 2,
    latency_max_ms: 2,
    latency_overflow_total: 0,
    complete: true,
  };
  const raw = {
    version: 1,
    session_elapsed_ms: 1000,
    media: leg,
    input: leg,
    audio: null,
    input_ack: inputAck,
  };

  const v4 = normalizeFrameStatsPayload({ stats_version: 4, network_diagnostics: raw });
  assert.equal(v4.networkDiagnostics.media.mode, 'direct');
  assert.equal(v4.networkDiagnostics.inputAck.acknowledgedTotal, 1);

  const v3 = normalizeFrameStatsPayload({ stats_version: 3, network_diagnostics: raw });
  assert.equal(v3.networkDiagnostics, null);
  const malformed = normalizeFrameStatsPayload({
    stats_version: 4,
    network_diagnostics: { ...raw, media: { ...leg, complete: 'yes' } },
  });
  assert.equal(malformed.networkDiagnostics, null);
});

test('normalizes bounded v3 independent-object discard counters', () => {
  const stats = normalizeFrameStatsPayload({
    stats_version: 3,
    transport_object_dropped_total: 8,
    transport_late_object_dropped_total: 3,
  });
  assert.equal(stats.objectDroppedFrames, 8);
  assert.equal(stats.lateObjectDroppedFrames, 3);

  const malformed = normalizeFrameStatsPayload({
    stats_version: 3,
    transport_object_dropped_total: 2,
    transport_late_object_dropped_total: 3,
  });
  assert.equal(malformed.objectDroppedFrames, null);
  assert.equal(malformed.lateObjectDroppedFrames, null);
});

test('does not display malformed or unit-ambiguous v2 diagnostics', () => {
  const stats = normalizeFrameStatsPayload({
    stats_version: 2,
    frontend_queue_dropped_total: 2,
    frontend_resync_dropped_total: 3,
    transport_interval_sample_count: 1,
    transport_interval_p50_ms: 10,
    transport_interval_p95_ms: Number.NaN,
    transport_interval_max_ms: 20,
    frontend_resync_episode_total: 1,
    frontend_resync_active: false,
    frontend_resync_duration_ms_total: 20,
    frontend_resync_duration_ms_current: 4,
    frontend_resync_duration_ms_max: 20,
  });
  assert.equal(stats.frontendDroppedFrames, 5);
  assert.equal(stats.transportIntervals, null);
  assert.equal(stats.resync, null);
});

test('represents empty timing windows without fabricated percentiles', () => {
  const stats = normalizeFrameStatsPayload({
    stats_version: 2,
    transport_interval_sample_count: 0,
    frontend_ipc_send_duration_sample_count: 0,
  });
  assert.deepEqual(stats.transportIntervals, {
    count: 0, p50Ms: null, p95Ms: null, maxMs: null,
  });
  assert.deepEqual(stats.ipcSendDurations, {
    count: 0, p50Ms: null, p95Ms: null, maxMs: null,
  });
});
