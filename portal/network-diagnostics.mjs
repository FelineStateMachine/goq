const NETWORK_DIAGNOSTICS_VERSION = 1;
const PATH_MODES = new Set(['direct', 'relay', 'custom', 'unknown']);

function record(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`${label} must be an object`);
  }
  return value;
}

function exactUnsigned(value, label) {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new TypeError(`${label} must be an exact unsigned integer`);
  }
  return value;
}

function optionalExactUnsigned(value, label) {
  return value === null ? null : exactUnsigned(value, label);
}

function boolean(value, label) {
  if (typeof value !== 'boolean') throw new TypeError(`${label} must be a boolean`);
  return value;
}

function durationMs(value, label) {
  if (!Number.isFinite(value) || value < 0) {
    throw new TypeError(`${label} must be a finite non-negative duration in milliseconds`);
  }
  return value;
}

function percentileSummary(value, prefix, sampleCount, overflowTotal) {
  const p50 = value[`${prefix}_p50_ms`];
  const p95 = value[`${prefix}_p95_ms`];
  const max = value[`${prefix}_max_ms`];
  if (sampleCount === 0) {
    if (overflowTotal !== 0 || p50 !== null || p95 !== null || max !== null) {
      throw new TypeError(`${prefix} percentiles require at least one retained sample`);
    }
    return { sampleCount, p50Ms: null, p95Ms: null, maxMs: null, overflowTotal };
  }

  if (overflowTotal > sampleCount) {
    throw new RangeError(`${prefix}_overflow_total cannot exceed its sample count`);
  }
  const retainedSamples = sampleCount - overflowTotal;
  const p50Available = retainedSamples >= Math.ceil(sampleCount * 0.5);
  const p95Available = retainedSamples >= Math.ceil(sampleCount * 0.95);
  if ((p50 !== null) !== p50Available || (p95 !== null) !== p95Available) {
    throw new TypeError(`${prefix} percentile availability does not match bounded samples`);
  }
  const p50Ms = p50 === null ? null : durationMs(p50, `${prefix}_p50_ms`);
  const p95Ms = p95 === null ? null : durationMs(p95, `${prefix}_p95_ms`);
  const maxMs = durationMs(max, `${prefix}_max_ms`);
  if ((p50Ms !== null && p50Ms > maxMs)
    || (p95Ms !== null && p95Ms > maxMs)
    || (p50Ms !== null && p95Ms !== null && p50Ms > p95Ms)) {
    throw new RangeError(`${prefix} percentiles must be monotonic`);
  }
  return { sampleCount, p50Ms, p95Ms, maxMs, overflowTotal };
}

function normalizeLeg(value, label) {
  const leg = record(value, label);
  if (!PATH_MODES.has(leg.mode)) throw new TypeError(`${label}.mode is unsupported`);

  const sampleCount = exactUnsigned(leg.sample_count, `${label}.sample_count`);
  const directSamples = exactUnsigned(leg.direct_samples, `${label}.direct_samples`);
  const relaySamples = exactUnsigned(leg.relay_samples, `${label}.relay_samples`);
  const customSamples = exactUnsigned(leg.custom_samples, `${label}.custom_samples`);
  const unknownSamples = exactUnsigned(leg.unknown_samples, `${label}.unknown_samples`);
  const classifiedSamples = directSamples + relaySamples + customSamples + unknownSamples;
  if (!Number.isSafeInteger(classifiedSamples) || classifiedSamples !== sampleCount) {
    throw new RangeError(`${label} path sample buckets must exactly match sample_count`);
  }

  const rttSampleCount = exactUnsigned(leg.rtt_sample_count, `${label}.rtt_sample_count`);
  const rttOverflowTotal = exactUnsigned(leg.rtt_overflow_total, `${label}.rtt_overflow_total`);
  const rtt = percentileSummary(leg, 'rtt', rttSampleCount, rttOverflowTotal);
  const rttCurrentMs = leg.rtt_current_ms === null
    ? null
    : durationMs(leg.rtt_current_ms, `${label}.rtt_current_ms`);
  if ((rttSampleCount === 0 || leg.mode === 'unknown') && rttCurrentMs !== null) {
    throw new RangeError(`${label}.rtt_current_ms requires a selected measured path`);
  }

  const counterRegressionTotal = exactUnsigned(
    leg.counter_regression_total,
    `${label}.counter_regression_total`,
  );
  const complete = boolean(leg.complete, `${label}.complete`);
  if (complete !== (counterRegressionTotal === 0 && rttOverflowTotal === 0)) {
    throw new RangeError(`${label}.complete is inconsistent with bounded telemetry state`);
  }
  if (rttSampleCount > sampleCount) {
    throw new RangeError(`${label}.rtt_sample_count cannot exceed sample_count`);
  }

  return {
    mode: leg.mode,
    sampleCount,
    pathSamples: {
      direct: directSamples,
      relay: relaySamples,
      custom: customSamples,
      unknown: unknownSamples,
    },
    pathEpoch: exactUnsigned(leg.path_epoch, `${label}.path_epoch`),
    pathSwitchTotal: exactUnsigned(leg.path_switch_total, `${label}.path_switch_total`),
    modeTransitionTotal: exactUnsigned(
      leg.mode_transition_total,
      `${label}.mode_transition_total`,
    ),
    rttCurrentMs,
    rtt,
    txDatagrams: exactUnsigned(leg.tx_datagrams, `${label}.tx_datagrams`),
    txBytes: exactUnsigned(leg.tx_bytes, `${label}.tx_bytes`),
    rxDatagrams: exactUnsigned(leg.rx_datagrams, `${label}.rx_datagrams`),
    rxBytes: exactUnsigned(leg.rx_bytes, `${label}.rx_bytes`),
    lostPackets: exactUnsigned(leg.lost_packets, `${label}.lost_packets`),
    lostBytes: exactUnsigned(leg.lost_bytes, `${label}.lost_bytes`),
    cwndBytes: optionalExactUnsigned(leg.cwnd_bytes, `${label}.cwnd_bytes`),
    mtu: optionalExactUnsigned(leg.mtu, `${label}.mtu`),
    congestionEvents: optionalExactUnsigned(leg.congestion_events, `${label}.congestion_events`),
    blackHolesDetected: optionalExactUnsigned(
      leg.black_holes_detected,
      `${label}.black_holes_detected`,
    ),
    counterRegressionTotal,
    complete,
  };
}

function normalizeInputAck(value) {
  const ack = record(value, 'network_diagnostics.input_ack');
  const sentTotal = exactUnsigned(ack.sent_total, 'network_diagnostics.input_ack.sent_total');
  const acknowledgedTotal = exactUnsigned(
    ack.acknowledged_total,
    'network_diagnostics.input_ack.acknowledged_total',
  );
  const pendingCount = exactUnsigned(
    ack.pending_count,
    'network_diagnostics.input_ack.pending_count',
  );
  const pendingCapacity = exactUnsigned(
    ack.pending_capacity,
    'network_diagnostics.input_ack.pending_capacity',
  );
  if (acknowledgedTotal > sentTotal) {
    throw new RangeError('acknowledged_total cannot exceed sent_total');
  }
  if (pendingCount > pendingCapacity) {
    throw new RangeError('pending_count cannot exceed pending_capacity');
  }

  const latencySampleCount = exactUnsigned(
    ack.latency_sample_count,
    'network_diagnostics.input_ack.latency_sample_count',
  );
  const latencyOverflowTotal = exactUnsigned(
    ack.latency_overflow_total,
    'network_diagnostics.input_ack.latency_overflow_total',
  );

  const malformed = boolean(ack.malformed, 'network_diagnostics.input_ack.malformed');
  const closed = boolean(ack.closed, 'network_diagnostics.input_ack.closed');
  const untrackedTotal = exactUnsigned(
    ack.untracked_total,
    'network_diagnostics.input_ack.untracked_total',
  );
  const complete = boolean(ack.complete, 'network_diagnostics.input_ack.complete');
  if (complete !== (!malformed && !closed && untrackedTotal === 0 && latencyOverflowTotal === 0)) {
    throw new RangeError(
      'network_diagnostics.input_ack.complete is inconsistent with bounded telemetry state',
    );
  }

  return {
    negotiated: boolean(ack.negotiated, 'network_diagnostics.input_ack.negotiated'),
    sentTotal,
    acknowledgedTotal,
    pendingCount,
    pendingCapacity,
    duplicateTotal: exactUnsigned(
      ack.duplicate_total,
      'network_diagnostics.input_ack.duplicate_total',
    ),
    untrackedTotal,
    malformed,
    closed,
    latency: percentileSummary(ack, 'latency', latencySampleCount, latencyOverflowTotal),
    complete,
  };
}

/**
 * Strictly validates the versioned network snapshot and returns a presentation-safe
 * allowlist. Peer identities, addresses, and unknown future fields never cross this boundary.
 */
export function normalizeNetworkDiagnostics(value) {
  const diagnostics = record(value, 'network_diagnostics');
  if (diagnostics.version !== NETWORK_DIAGNOSTICS_VERSION) {
    throw new TypeError(`unsupported network_diagnostics version: ${diagnostics.version}`);
  }

  return {
    version: NETWORK_DIAGNOSTICS_VERSION,
    sessionElapsedMs: exactUnsigned(
      diagnostics.session_elapsed_ms,
      'network_diagnostics.session_elapsed_ms',
    ),
    media: normalizeLeg(diagnostics.media, 'network_diagnostics.media'),
    input: normalizeLeg(diagnostics.input, 'network_diagnostics.input'),
    audio: diagnostics.audio === null
      ? null
      : normalizeLeg(diagnostics.audio, 'network_diagnostics.audio'),
    inputAck: normalizeInputAck(diagnostics.input_ack),
  };
}

function formatBytes(value) {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  if (value < 1024 * 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

function formatPercentiles(summary) {
  if (summary.sampleCount === 0) return `— · 0 samples · ${summary.overflowTotal} overflow`;
  const p50 = summary.p50Ms === null ? '—' : summary.p50Ms.toFixed(1);
  const p95 = summary.p95Ms === null ? '—' : summary.p95Ms.toFixed(1);
  return `${p50} / ${p95} / ${summary.maxMs.toFixed(1)} ms · ${summary.sampleCount} samples · ${summary.overflowTotal} overflow`;
}

function formatOptionalBytes(value) {
  return value === null ? '—' : formatBytes(value);
}

function formatOptionalInteger(value, suffix = '') {
  return value === null ? '—' : `${value}${suffix}`;
}

export function formatNetworkLeg(leg) {
  if (leg === null) return 'unavailable';
  const paths = leg.pathSamples;
  const currentRtt = leg.rttCurrentMs === null ? '—' : `${leg.rttCurrentMs.toFixed(1)} ms`;
  return `${leg.mode} · RTT current ${currentRtt} · history ${formatPercentiles(leg.rtt)} · paths d/r/c/u ${paths.direct}/${paths.relay}/${paths.custom}/${paths.unknown} · epoch ${leg.pathEpoch} · switches ${leg.pathSwitchTotal} · transitions ${leg.modeTransitionTotal} · tx ${leg.txDatagrams} datagrams / ${formatBytes(leg.txBytes)} · rx ${leg.rxDatagrams} datagrams / ${formatBytes(leg.rxBytes)} · lost ${leg.lostPackets} packets / ${formatBytes(leg.lostBytes)} · cwnd ${formatOptionalBytes(leg.cwndBytes)} · MTU ${formatOptionalInteger(leg.mtu, ' B')} · congestion ${formatOptionalInteger(leg.congestionEvents)} · black holes ${formatOptionalInteger(leg.blackHolesDetected)} · regressions ${leg.counterRegressionTotal} · ${leg.complete ? 'complete' : 'partial'}`;
}

export function formatInputAck(ack) {
  const negotiation = ack.negotiated ? 'negotiated' : 'not negotiated';
  const health = ack.malformed ? 'malformed' : ack.closed ? 'closed' : 'open';
  return `${negotiation} · ${ack.acknowledgedTotal}/${ack.sentTotal} acknowledged · ${ack.pendingCount}/${ack.pendingCapacity} pending · duplicates ${ack.duplicateTotal} · untracked ${ack.untrackedTotal} · latency ${formatPercentiles(ack.latency)} · ${health} · ${ack.complete ? 'complete' : 'partial'}`;
}

export function networkDiagnosticsPresentation(diagnostics) {
  if (diagnostics === null) {
    return {
      session: 'unavailable',
      media: 'unavailable',
      input: 'unavailable',
      audio: 'unavailable',
      inputAck: 'unavailable',
    };
  }
  return {
    session: `v${diagnostics.version} · ${(diagnostics.sessionElapsedMs / 1000).toFixed(1)} s`,
    media: formatNetworkLeg(diagnostics.media),
    input: formatNetworkLeg(diagnostics.input),
    audio: formatNetworkLeg(diagnostics.audio),
    inputAck: formatInputAck(diagnostics.inputAck),
  };
}
