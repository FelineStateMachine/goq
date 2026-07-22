import test from 'node:test';
import assert from 'node:assert/strict';
import {
  networkDiagnosticsPresentation,
  normalizeNetworkDiagnostics,
} from './network-diagnostics.mjs';

function leg(overrides = {}) {
  return {
    mode: 'direct',
    sample_count: 3,
    direct_samples: 3,
    relay_samples: 0,
    custom_samples: 0,
    unknown_samples: 0,
    path_epoch: 1,
    path_switch_total: 0,
    mode_transition_total: 0,
    rtt_sample_count: 3,
    rtt_current_ms: 9.25,
    rtt_p50_ms: 8,
    rtt_p95_ms: 12.5,
    rtt_max_ms: 13,
    rtt_overflow_total: 0,
    tx_datagrams: 20,
    tx_bytes: 2048,
    rx_datagrams: 18,
    rx_bytes: 1024,
    lost_packets: 1,
    lost_bytes: 64,
    cwnd_bytes: 65536,
    mtu: 1200,
    congestion_events: 2,
    black_holes_detected: 0,
    counter_regression_total: 0,
    complete: true,
    ...overrides,
  };
}

function snapshot(overrides = {}) {
  return {
    version: 1,
    session_elapsed_ms: 12345,
    media: leg(),
    input: leg({ mode: 'relay', direct_samples: 0, relay_samples: 3 }),
    audio: null,
    input_ack: {
      negotiated: true,
      sent_total: 10,
      acknowledged_total: 9,
      pending_count: 1,
      pending_capacity: 64,
      duplicate_total: 1,
      untracked_total: 0,
      malformed: false,
      closed: false,
      latency_sample_count: 3,
      latency_p50_ms: 4,
      latency_p95_ms: 9,
      latency_max_ms: 10,
      latency_overflow_total: 0,
      complete: true,
    },
    ...overrides,
  };
}

test('normalizes a bounded v1 snapshot and keeps transport legs separate', () => {
  const diagnostics = normalizeNetworkDiagnostics(snapshot());
  assert.equal(diagnostics.version, 1);
  assert.equal(diagnostics.sessionElapsedMs, 12345);
  assert.equal(diagnostics.media.mode, 'direct');
  assert.equal(diagnostics.input.mode, 'relay');
  assert.equal(diagnostics.audio, null);
  assert.equal(diagnostics.media.rttCurrentMs, 9.25);
  assert.deepEqual(diagnostics.media.rtt, {
    sampleCount: 3,
    p50Ms: 8,
    p95Ms: 12.5,
    maxMs: 13,
    overflowTotal: 0,
  });
  assert.equal(diagnostics.inputAck.pendingCount, 1);
});

test('validates exact counters, classified samples, percentiles, and completeness', () => {
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    media: leg({ tx_bytes: Number.MAX_SAFE_INTEGER + 1 }),
  })), /exact unsigned integer/);
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    media: leg({ direct_samples: 2 }),
  })), /exactly match sample_count/);
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    media: leg({ rtt_p50_ms: 14 }),
  })), /percentiles must be monotonic/);
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    media: leg({ complete: 'yes' }),
  })), /must be a boolean/);
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    input_ack: { ...snapshot().input_ack, pending_count: 65 },
  })), /pending_count cannot exceed pending_capacity/);
});

test('requires null percentiles for an empty bounded window', () => {
  const empty = leg({
    rtt_sample_count: 0,
    rtt_current_ms: null,
    rtt_p50_ms: null,
    rtt_p95_ms: null,
    rtt_max_ms: null,
  });
  assert.equal(normalizeNetworkDiagnostics(snapshot({ media: empty })).media.rtt.sampleCount, 0);
  assert.throws(() => normalizeNetworkDiagnostics(snapshot({
    media: { ...empty, rtt_p50_ms: 0 },
  })), /require at least one retained sample/);
});

test('accepts nullable path gauges and represents bounded histogram overflow', () => {
  const overflowed = leg({
    sample_count: 2,
    direct_samples: 0,
    unknown_samples: 2,
    mode: 'unknown',
    rtt_sample_count: 2,
    rtt_current_ms: null,
    rtt_p50_ms: 12,
    rtt_p95_ms: null,
    rtt_max_ms: 6000,
    rtt_overflow_total: 1,
    cwnd_bytes: null,
    mtu: null,
    congestion_events: null,
    black_holes_detected: null,
    complete: false,
  });
  const diagnostics = normalizeNetworkDiagnostics(snapshot({ media: overflowed }));
  assert.equal(diagnostics.media.rtt.p50Ms, 12);
  assert.equal(diagnostics.media.rtt.p95Ms, null);
  assert.equal(diagnostics.media.cwndBytes, null);
  assert.match(
    networkDiagnosticsPresentation(diagnostics).media,
    /RTT current — · history 12\.0 \/ — \/ 6000\.0 ms/,
  );
});

test('renders legs and ACK health without exposing identifiers or addresses', () => {
  const raw = snapshot({
    peer_id: 'secret-peer',
    remote_address: '192.0.2.1:443',
    media: leg({ endpoint_id: 'secret-endpoint' }),
  });
  const presentation = networkDiagnosticsPresentation(normalizeNetworkDiagnostics(raw));
  assert.match(presentation.media, /^direct · RTT/);
  assert.match(presentation.input, /^relay · RTT/);
  assert.equal(presentation.audio, 'unavailable');
  assert.match(presentation.inputAck, /^negotiated · 9\/10 acknowledged/);
  assert.doesNotMatch(JSON.stringify(presentation), /secret|192\.0\.2\.1|endpoint/i);
});

test('formats a missing v4 snapshot as explicitly unavailable', () => {
  assert.deepEqual(networkDiagnosticsPresentation(null), {
    session: 'unavailable',
    media: 'unavailable',
    input: 'unavailable',
    audio: 'unavailable',
    inputAck: 'unavailable',
  });
});
