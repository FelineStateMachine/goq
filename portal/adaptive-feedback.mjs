export const ADAPTIVE_FEEDBACK_INTERVAL_MS = 1000;
export const ADAPTIVE_FEEDBACK_INTERVAL_MIN_MS = 250;
export const ADAPTIVE_FEEDBACK_INTERVAL_MAX_MS = 5000;
export const ADAPTIVE_QUEUE_DEPTH_MAX = 16;
export const ADAPTIVE_COUNTER_DELTA_MAX = 0xffff_ffff;
export const ADAPTIVE_LATENCY_MS_MAX = 60_000;

function boundedInteger(value, maximum, name) {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new TypeError(`${name} must be a non-negative safe integer`);
  }
  return Math.min(value, maximum);
}

function boundedLatency(value, name) {
  if (value === null || value === undefined) return null;
  if (!Number.isFinite(value) || value < 0) {
    throw new TypeError(`${name} must be finite and non-negative`);
  }
  return Math.min(value, ADAPTIVE_LATENCY_MS_MAX);
}

function boundedQueue(depth, capacity, name) {
  const safeCapacity = boundedInteger(capacity, ADAPTIVE_QUEUE_DEPTH_MAX, `${name} capacity`);
  if (safeCapacity < 1) throw new RangeError(`${name} capacity must be positive`);
  return {
    depth: Math.min(boundedInteger(depth, ADAPTIVE_QUEUE_DEPTH_MAX, `${name} depth`), safeCapacity),
    capacity: safeCapacity,
  };
}

/**
 * Strictly normalize the small, bounded snapshot that crosses the Tauri
 * command boundary. Cumulative counters never cross the wire; the publisher
 * converts them to saturating interval deltas.
 */
export function normalizeAdaptiveFeedbackSnapshot(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') throw new TypeError('feedback snapshot is required');
  const frontend = boundedQueue(
    snapshot.frontendQueueDepth,
    snapshot.frontendQueueCapacity,
    'frontend queue',
  );
  const decoder = boundedQueue(
    snapshot.decoderQueueDepth,
    snapshot.decoderQueueCapacity,
    'decoder queue',
  );
  const presenter = boundedQueue(
    snapshot.presenterQueueDepth,
    snapshot.presenterQueueCapacity,
    'presenter queue',
  );
  if (typeof snapshot.resyncActive !== 'boolean') {
    throw new TypeError('resyncActive must be a boolean');
  }
  return {
    last_sequence: snapshot.lastSequence === null || snapshot.lastSequence === undefined
      ? null
      : boundedInteger(snapshot.lastSequence, Number.MAX_SAFE_INTEGER, 'last sequence'),
    frontend_queue_depth: frontend.depth,
    frontend_queue_capacity: frontend.capacity,
    decode_queue_depth: decoder.depth,
    decode_queue_capacity: decoder.capacity,
    presenter_queue_depth: presenter.depth,
    presenter_queue_capacity: presenter.capacity,
    transport_dropped_total: boundedInteger(
      snapshot.transportDroppedTotal,
      Number.MAX_SAFE_INTEGER,
      'transport dropped total',
    ),
    frontend_dropped_total: boundedInteger(
      snapshot.frontendDroppedTotal,
      Number.MAX_SAFE_INTEGER,
      'frontend dropped total',
    ),
    decoder_dropped_total: boundedInteger(
      snapshot.decoderDroppedTotal,
      Number.MAX_SAFE_INTEGER,
      'decoder dropped total',
    ),
    presenter_dropped_total: boundedInteger(
      snapshot.presenterDroppedTotal,
      Number.MAX_SAFE_INTEGER,
      'presenter dropped total',
    ),
    transport_delivery_p95_ms: boundedLatency(
      snapshot.transportDeliveryP95Ms,
      'transport delivery p95',
    ),
    decode_p95_ms: boundedLatency(snapshot.decodeLatencyP95Ms, 'decode p95'),
    presentation_p95_ms: boundedLatency(
      snapshot.presentationLatencyP95Ms,
      'presentation p95',
    ),
    resync_active: snapshot.resyncActive,
  };
}

function saturatingDelta(current, previous) {
  if (current < previous) return 0;
  return Math.min(current - previous, ADAPTIVE_COUNTER_DELTA_MAX);
}

function counterBaseline(snapshot) {
  return {
    transport: snapshot.transport_dropped_total,
    frontend: snapshot.frontend_dropped_total,
    decoder: snapshot.decoder_dropped_total,
    presenter: snapshot.presenter_dropped_total,
  };
}

function intervalReport(snapshot, baseline, intervalMs) {
  return {
    interval_ms: intervalMs,
    last_sequence: snapshot.last_sequence,
    frontend_queue_depth: snapshot.frontend_queue_depth,
    frontend_queue_capacity: snapshot.frontend_queue_capacity,
    decode_queue_depth: snapshot.decode_queue_depth,
    decode_queue_capacity: snapshot.decode_queue_capacity,
    presenter_queue_depth: snapshot.presenter_queue_depth,
    presenter_queue_capacity: snapshot.presenter_queue_capacity,
    transport_dropped_delta: saturatingDelta(snapshot.transport_dropped_total, baseline.transport),
    frontend_dropped_delta: saturatingDelta(snapshot.frontend_dropped_total, baseline.frontend),
    decoder_dropped_delta: saturatingDelta(snapshot.decoder_dropped_total, baseline.decoder),
    presenter_dropped_delta: saturatingDelta(snapshot.presenter_dropped_total, baseline.presenter),
    transport_delivery_p95_ms: snapshot.transport_delivery_p95_ms,
    decode_p95_ms: snapshot.decode_p95_ms,
    presentation_p95_ms: snapshot.presentation_p95_ms,
    resync_active: snapshot.resync_active,
  };
}

/** Generation-scoped, at-most-1 Hz, single-flight adaptive feedback sender. */
export class AdaptiveFeedbackPublisher {
  #invoke;
  #now;
  #generation = null;
  #available = false;
  #inFlight = false;
  #lastStartedAt = Number.NEGATIVE_INFINITY;
  #baselineAt = null;
  #baseline = { transport: 0, frontend: 0, decoder: 0, presenter: 0 };
  #latest = null;

  constructor({ invokeCommand, now = () => performance.now() }) {
    if (typeof invokeCommand !== 'function') throw new TypeError('invokeCommand must be a function');
    if (typeof now !== 'function') throw new TypeError('now must be a function');
    this.#invoke = invokeCommand;
    this.#now = now;
  }

  start(generation, available) {
    if (!Number.isSafeInteger(generation) || generation < 1) {
      throw new TypeError('feedback generation must be a positive safe integer');
    }
    if (typeof available !== 'boolean') throw new TypeError('feedback availability must be boolean');
    this.#generation = generation;
    this.#available = available;
    this.#inFlight = false;
    this.#lastStartedAt = Number.NEGATIVE_INFINITY;
    this.#baselineAt = null;
    this.#baseline = { transport: 0, frontend: 0, decoder: 0, presenter: 0 };
    this.#latest = null;
  }

  stop() {
    this.#generation = null;
    this.#available = false;
    this.#inFlight = false;
    this.#baselineAt = null;
    this.#latest = null;
  }

  get available() { return this.#available; }
  get inFlight() { return this.#inFlight; }

  publish(rawSnapshot) {
    if (!this.#available || this.#generation === null) return false;
    const snapshot = normalizeAdaptiveFeedbackSnapshot(rawSnapshot);
    this.#latest = snapshot;
    const now = this.#now();
    if (!Number.isFinite(now)) throw new TypeError('feedback clock must be finite');
    if (this.#baselineAt === null) {
      // Counters can advance while the native connect command is still
      // returning. Establish the first baseline after the generation is live
      // so pre-generation pressure is never mislabeled as a short interval.
      this.#baseline = counterBaseline(snapshot);
      this.#baselineAt = now;
      return false;
    }
    const elapsed = now - this.#baselineAt;
    if (!Number.isFinite(elapsed) || elapsed < 0) {
      throw new TypeError('feedback interval must be finite and non-negative');
    }
    if (this.#inFlight || now - this.#lastStartedAt < ADAPTIVE_FEEDBACK_INTERVAL_MS) return false;
    if (elapsed < ADAPTIVE_FEEDBACK_INTERVAL_MIN_MS) return false;

    const generation = this.#generation;
    const intervalMs = Math.min(
      Math.max(Math.round(elapsed), ADAPTIVE_FEEDBACK_INTERVAL_MIN_MS),
      ADAPTIVE_FEEDBACK_INTERVAL_MAX_MS,
    );
    const report = intervalReport(snapshot, this.#baseline, intervalMs);
    this.#inFlight = true;
    let invocation;
    try {
      invocation = this.#invoke('iroh_client_send_media_feedback', { generation, report });
    } catch (error) {
      this.#inFlight = false;
      console.warn('adaptive feedback failed:', error);
      return false;
    }
    this.#lastStartedAt = now;
    Promise.resolve(invocation)
      .then((accepted) => {
        if (generation === this.#generation && accepted === true) {
          this.#baseline = counterBaseline(snapshot);
          this.#baselineAt = now;
        }
      })
      .catch((error) => {
        if (generation === this.#generation) console.warn('adaptive feedback failed:', error);
      })
      .finally(() => {
        if (generation === this.#generation) this.#inFlight = false;
      });
    return true;
  }
}

export function normalizeAdaptiveDecisionEnvelope(payload, generation) {
  if (!payload || typeof payload !== 'object') throw new TypeError('adaptive decision is required');
  if (!Number.isSafeInteger(generation) || generation < 1 || payload.generation !== generation) {
    return null;
  }
  const decision = payload.decision;
  if (!decision || typeof decision !== 'object') throw new TypeError('adaptive decision body is required');
  return {
    decision_id: boundedInteger(decision.decision_id, Number.MAX_SAFE_INTEGER, 'decision ID'),
    report_id: boundedInteger(decision.report_id, Number.MAX_SAFE_INTEGER, 'report ID'),
    target_kbps: boundedInteger(decision.target_kbps, 100_000, 'target bitrate'),
    floor_kbps: boundedInteger(decision.floor_kbps, 100_000, 'bitrate floor'),
    ceiling_kbps: boundedInteger(decision.ceiling_kbps, 100_000, 'bitrate ceiling'),
    state: ['hold', 'decrease', 'increase'].includes(decision.state) ? decision.state : 'unknown',
    reasons: Array.isArray(decision.reasons)
      ? decision.reasons.filter((reason) => typeof reason === 'string').slice(0, 8)
      : [],
    applied: decision.applied === true,
  };
}

export function formatAdaptiveDecision(decision, available) {
  if (!available) return 'unavailable';
  if (!decision) return 'awaiting report · advisory only';
  const reasons = decision.reasons.length ? decision.reasons.join(', ') : 'no pressure signals';
  const disposition = decision.applied ? 'host reports applied' : 'advisory only (not applied)';
  return `${decision.target_kbps} kbps · ${decision.state} · ${reasons} · ${disposition}`;
}
