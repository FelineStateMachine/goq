export const STREAM_RATE_WINDOW_MS = 1000;
export const MAX_STREAM_RATE_SAMPLES = 240;
export const CADENCE_HITCH_25_MS = 25;
export const CADENCE_HITCH_33_333_MS = 33.333;

/**
 * A bounded, monotonic rolling-rate estimator. Callers supply their own clock
 * so the same implementation is deterministic in tests and uses
 * performance.now() in the webview.
 */
export class RollingRateWindow {
  constructor(windowMs = STREAM_RATE_WINDOW_MS, maxSamples = MAX_STREAM_RATE_SAMPLES) {
    if (!Number.isFinite(windowMs) || windowMs <= 0) throw new RangeError('windowMs must be positive');
    if (!Number.isInteger(maxSamples) || maxSamples < 2) throw new RangeError('maxSamples must be at least two');
    this.windowMs = windowMs;
    this.maxSamples = maxSamples;
    this.samples = [];
  }

  record(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('sample time must be finite');
    const previous = this.samples.at(-1);
    if (previous !== undefined && nowMs < previous) throw new RangeError('sample time must be monotonic');
    this.samples.push(nowMs);
    if (this.samples.length > this.maxSamples) {
      this.samples.splice(0, this.samples.length - this.maxSamples);
    }
    this.#prune(nowMs);
  }

  rate(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('query time must be finite');
    this.#prune(nowMs);
    if (this.samples.length < 2) return 0;
    const first = this.samples[0];
    const last = this.samples.at(-1);
    const elapsedMs = last - first;
    return elapsedMs > 0 ? ((this.samples.length - 1) * 1000) / elapsedMs : 0;
  }

  reset() {
    this.samples.length = 0;
  }

  #prune(nowMs) {
    const cutoff = nowMs - this.windowMs;
    let remove = 0;
    while (remove < this.samples.length && this.samples[remove] < cutoff) remove++;
    if (remove > 0) this.samples.splice(0, remove);
  }
}

export class BoundedLatencyWindow {
  constructor(windowMs = 2000, maxSamples = 512) {
    if (!Number.isFinite(windowMs) || windowMs <= 0) throw new RangeError('windowMs must be positive');
    if (!Number.isInteger(maxSamples) || maxSamples < 1) throw new RangeError('maxSamples must be positive');
    this.windowMs = windowMs;
    this.maxSamples = maxSamples;
    this.samples = [];
  }

  record(valueMs, nowMs) {
    if (!Number.isFinite(valueMs) || valueMs < 0) throw new RangeError('latency must be finite and non-negative');
    if (!Number.isFinite(nowMs)) throw new TypeError('sample time must be finite');
    const previous = this.samples.at(-1);
    if (previous && nowMs < previous.time) throw new RangeError('sample time must be monotonic');
    this.samples.push({ time: nowMs, value: valueMs });
    if (this.samples.length > this.maxSamples) {
      this.samples.splice(0, this.samples.length - this.maxSamples);
    }
    this.#prune(nowMs);
  }

  summary(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('query time must be finite');
    this.#prune(nowMs);
    if (this.samples.length === 0) {
      return { p50: null, p95: null, p99: null, max: null, count: 0 };
    }
    const values = this.samples.map((sample) => sample.value).sort((a, b) => a - b);
    return {
      p50: values[Math.ceil(values.length * 0.50) - 1],
      p95: values[Math.ceil(values.length * 0.95) - 1],
      p99: values[Math.ceil(values.length * 0.99) - 1],
      max: values.at(-1),
      count: values.length,
    };
  }

  reset() {
    this.samples.length = 0;
  }

  #prune(nowMs) {
    const cutoff = nowMs - this.windowMs;
    let remove = 0;
    while (remove < this.samples.length && this.samples[remove].time < cutoff) remove++;
    if (remove > 0) this.samples.splice(0, remove);
  }
}

export class BoundedValueWindow {
  constructor(windowMs = 5000, maxSamples = 512) {
    if (!Number.isFinite(windowMs) || windowMs <= 0) throw new RangeError('windowMs must be positive');
    if (!Number.isInteger(maxSamples) || maxSamples < 1) throw new RangeError('maxSamples must be positive');
    this.windowMs = windowMs;
    this.maxSamples = maxSamples;
    this.samples = [];
  }

  record(value, nowMs) {
    if (!Number.isFinite(value)) throw new TypeError('value must be finite');
    if (!Number.isFinite(nowMs)) throw new TypeError('sample time must be finite');
    const previous = this.samples.at(-1);
    if (previous && nowMs < previous.time) throw new RangeError('sample time must be monotonic');
    this.samples.push({ time: nowMs, value });
    if (this.samples.length > this.maxSamples) {
      this.samples.splice(0, this.samples.length - this.maxSamples);
    }
    this.#prune(nowMs);
  }

  summary(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('query time must be finite');
    this.#prune(nowMs);
    if (this.samples.length === 0) {
      return { p50: null, p95: null, maxAbsolute: null, count: 0 };
    }
    const values = this.samples.map((sample) => sample.value).sort((a, b) => a - b);
    return {
      p50: values[Math.ceil(values.length * 0.50) - 1],
      p95: values[Math.ceil(values.length * 0.95) - 1],
      maxAbsolute: Math.max(...values.map(Math.abs)),
      count: values.length,
    };
  }

  reset() {
    this.samples.length = 0;
  }

  #prune(nowMs) {
    const cutoff = nowMs - this.windowMs;
    let remove = 0;
    while (remove < this.samples.length && this.samples[remove].time < cutoff) remove++;
    if (remove > 0) this.samples.splice(0, remove);
  }
}

/**
 * A bounded rolling distribution of presentation intervals. The first
 * timestamp establishes the cadence anchor; each later timestamp contributes
 * one interval ending at that timestamp.
 */
export class BoundedCadenceWindow {
  constructor(windowMs = 5000, maxSamples = 512) {
    if (!Number.isFinite(windowMs) || windowMs <= 0) throw new RangeError('windowMs must be positive');
    if (!Number.isInteger(maxSamples) || maxSamples < 1) throw new RangeError('maxSamples must be positive');
    this.windowMs = windowMs;
    this.maxSamples = maxSamples;
    this.samples = [];
    this.lastSampleTimeMs = null;
  }

  record(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('sample time must be finite');
    if (this.lastSampleTimeMs !== null && nowMs < this.lastSampleTimeMs) {
      throw new RangeError('sample time must be monotonic');
    }
    if (this.lastSampleTimeMs !== null
      && nowMs - this.lastSampleTimeMs <= this.windowMs) {
      this.samples.push({ time: nowMs, value: nowMs - this.lastSampleTimeMs });
      if (this.samples.length > this.maxSamples) {
        this.samples.splice(0, this.samples.length - this.maxSamples);
      }
    }
    this.lastSampleTimeMs = nowMs;
    this.#prune(nowMs);
  }

  summary(nowMs) {
    if (!Number.isFinite(nowMs)) throw new TypeError('query time must be finite');
    if (this.lastSampleTimeMs !== null && nowMs < this.lastSampleTimeMs) {
      throw new RangeError('query time must be monotonic');
    }
    this.#prune(nowMs);
    if (this.samples.length === 0) {
      return {
        p50: null,
        p95: null,
        p99: null,
        max: null,
        count: 0,
        over25Ms: 0,
        over33Ms: 0,
      };
    }
    const values = this.samples.map((sample) => sample.value).sort((a, b) => a - b);
    return {
      p50: values[Math.ceil(values.length * 0.50) - 1],
      p95: values[Math.ceil(values.length * 0.95) - 1],
      p99: values[Math.ceil(values.length * 0.99) - 1],
      max: values.at(-1),
      count: values.length,
      over25Ms: values.filter((value) => value > CADENCE_HITCH_25_MS).length,
      over33Ms: values.filter((value) => value > CADENCE_HITCH_33_333_MS).length,
    };
  }

  reset() {
    this.samples.length = 0;
    this.lastSampleTimeMs = null;
  }

  #prune(nowMs) {
    const cutoff = nowMs - this.windowMs;
    let remove = 0;
    while (remove < this.samples.length && this.samples[remove].time < cutoff) remove++;
    if (remove > 0) this.samples.splice(0, remove);
  }
}

export class LatestFramePresenter {
  constructor({ requestFrame, cancelFrame, draw, onPresent = () => {}, onDrop = () => {} }) {
    this.requestFrame = requestFrame;
    this.cancelFrame = cancelFrame;
    this.draw = draw;
    this.onPresent = onPresent;
    this.onDrop = onDrop;
    // Keep one frame for the next display refresh and one frame of jitter
    // tolerance. Decoder output commonly arrives in pairs even when its
    // average rate matches the display; a single latest-frame slot turns
    // those pairs into an avoidable 30–40 fps presentation cadence. The hard
    // two-frame ceiling still bounds latency, and overload evicts the oldest
    // stale frame first.
    this.pending = [];
    this.frameRequest = null;
  }

  enqueue(frame, metadata = null) {
    if (!frame || typeof frame.close !== 'function') throw new TypeError('frame must be closeable');
    if (this.pending.length >= 2) {
      const stale = this.pending.shift();
      stale.frame.close();
      this.onDrop(stale.metadata);
    }
    this.pending.push({ frame, metadata });
    this.#schedule();
  }

  #schedule() {
    if (this.frameRequest !== null || this.pending.length === 0) return;
    this.frameRequest = this.requestFrame((nowMs) => {
      this.frameRequest = null;
      const pending = this.pending.shift();
      if (!pending) return;
      try {
        this.draw(pending.frame);
        this.onPresent(pending.metadata, nowMs);
      } finally {
        pending.frame.close();
        this.#schedule();
      }
    });
  }

  clear() {
    if (this.frameRequest !== null) {
      this.cancelFrame(this.frameRequest);
      this.frameRequest = null;
    }
    for (const pending of this.pending.splice(0)) pending.frame.close();
  }

  get depth() {
    return this.pending.length;
  }
}
