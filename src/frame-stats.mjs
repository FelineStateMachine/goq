function exactUnsigned(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function formatFrameCount(value) {
  return `${value} ${value === 1 ? 'frame' : 'frames'}`;
}

/**
 * Names and formats each browser-side video discard counter without conflating
 * latest-frame-wins presenter overwrites with transport or decoder loss.
 */
export function formatVideoDiscardTelemetry({
  transportDroppedFrames,
  frontendDroppedFrames,
  decoderDroppedFrames,
  presenterOverwrittenFrames,
}) {
  const counters = [
    transportDroppedFrames,
    frontendDroppedFrames,
    decoderDroppedFrames,
    presenterOverwrittenFrames,
  ].map(exactUnsigned);
  if (counters.some((value) => value === null)) {
    throw new TypeError('video discard counters must be exact unsigned integers');
  }

  const totalFrames = counters.reduce((total, value) => total + value, 0);
  if (!Number.isSafeInteger(totalFrames)) {
    throw new RangeError('video discard total exceeds the exact integer range');
  }

  return {
    total: formatFrameCount(totalFrames),
    transport: formatFrameCount(transportDroppedFrames),
    frontend: formatFrameCount(frontendDroppedFrames),
    decoder: formatFrameCount(decoderDroppedFrames),
    presenterOverwrite: formatFrameCount(presenterOverwrittenFrames),
  };
}

export function isCurrentFrameGeneration(generation, activeGeneration) {
  return Number.isSafeInteger(generation)
    && generation > 0
    && Number.isSafeInteger(activeGeneration)
    && activeGeneration > 0
    && generation === activeGeneration;
}

function exactDurationMs(value) {
  return Number.isFinite(value) && value >= 0 ? value : null;
}

function firstExactUnsigned(...values) {
  for (const value of values) {
    const exact = exactUnsigned(value);
    if (exact !== null) return exact;
  }
  return null;
}

function durationSummary(payload, prefix) {
  const count = exactUnsigned(payload[`${prefix}_sample_count`]);
  if (count === null) return null;
  if (count === 0) return { count: 0, p50Ms: null, p95Ms: null, maxMs: null };
  const p50Ms = exactDurationMs(payload[`${prefix}_p50_ms`]);
  const p95Ms = exactDurationMs(payload[`${prefix}_p95_ms`]);
  const maxMs = exactDurationMs(payload[`${prefix}_max_ms`]);
  if (p50Ms === null || p95Ms === null || maxMs === null) return null;
  return { count, p50Ms, p95Ms, maxMs };
}

/**
 * Validates the exact v2 frame-stat fields while retaining the aggregate
 * aliases emitted by older clients. A null value means that metric must be
 * presented as unavailable rather than inferred from a differently named unit.
 */
export function normalizeFrameStatsPayload(payload) {
  if (!payload || typeof payload !== 'object') throw new TypeError('frame stats must be an object');

  const statsVersion = exactUnsigned(payload.stats_version) ?? 1;
  const v2 = statsVersion >= 2;
  const queueDroppedFrames = v2 ? exactUnsigned(payload.frontend_queue_dropped_total) : null;
  const resyncDroppedFrames = v2 ? exactUnsigned(payload.frontend_resync_dropped_total) : null;
  const splitFrontendTotal = queueDroppedFrames !== null && resyncDroppedFrames !== null
    && Number.isSafeInteger(queueDroppedFrames + resyncDroppedFrames)
    ? queueDroppedFrames + resyncDroppedFrames
    : null;

  let queue = null;
  let resync = null;
  let transportIntervals = null;
  let ipcSendDurations = null;
  let timingWindow = null;
  if (v2) {
    const depth = exactUnsigned(payload.frontend_queue_depth);
    const peak = exactUnsigned(payload.frontend_queue_peak);
    const capacity = exactUnsigned(payload.frontend_queue_capacity);
    if (depth !== null && peak !== null && capacity !== null) {
      queue = { depth, peak, capacity };
    }

    const episodes = exactUnsigned(payload.frontend_resync_episode_total);
    const totalMs = exactDurationMs(payload.frontend_resync_duration_ms_total);
    const maxMs = exactDurationMs(payload.frontend_resync_duration_ms_max);
    const active = typeof payload.frontend_resync_active === 'boolean'
      ? payload.frontend_resync_active : null;
    const currentMs = payload.frontend_resync_duration_ms_current === null
      ? null : exactDurationMs(payload.frontend_resync_duration_ms_current);
    if (episodes !== null && totalMs !== null && maxMs !== null && active !== null
      && ((!active && currentMs === null) || (active && currentMs !== null))) {
      resync = { episodes, active, totalMs, currentMs, maxMs };
    }

    transportIntervals = durationSummary(payload, 'transport_interval');
    ipcSendDurations = durationSummary(payload, 'frontend_ipc_send_duration');
    const windowMs = exactDurationMs(payload.timing_window_ms);
    const sampleCapacity = exactUnsigned(payload.timing_sample_capacity);
    if (windowMs !== null && sampleCapacity !== null) {
      timingWindow = { windowMs, sampleCapacity };
    }
  }

  return {
    statsVersion,
    transportDroppedFrames: firstExactUnsigned(
      payload.sequence_dropped_total,
      payload.host_dropped_frames,
    ),
    frontendDroppedFrames: firstExactUnsigned(
      payload.frontend_dropped_total,
      payload.frontend_dropped_frames,
      splitFrontendTotal,
    ),
    queueDroppedFrames,
    resyncDroppedFrames,
    queue,
    resync,
    transportIntervals,
    ipcSendDurations,
    timingWindow,
  };
}
