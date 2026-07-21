export const DECODER_RECOVERY_REASONS = Object.freeze({
  TRANSPORT_GAP: 'transport-gap',
  DISCONTINUITY: 'discontinuity',
  DELIVERY_TIMEOUT: 'delivery-timeout',
  DECODER_RESET: 'decoder-reset',
  DECODER_ERROR: 'decoder-error',
  FRONTEND_BACKPRESSURE: 'frontend-backpressure',
});

const VALID_REASONS = new Set(Object.values(DECODER_RECOVERY_REASONS));

function validReason(reason) {
  if (!VALID_REASONS.has(reason)) {
    throw new TypeError(`unsupported decoder recovery reason: ${reason}`);
  }
  return reason;
}

/**
 * Owns the small state machine between partial media delivery and WebCodecs.
 *
 * Entering recovery issues at most one keyframe request for that episode.
 * Receiving a keyframe does not itself end the episode: the caller must first
 * configure the decoder and enqueue the keyframe successfully. This keeps a
 * synchronous decode failure from admitting dependent delta frames.
 */
export class DecoderRecoveryState {
  #recovering;
  #requestIssued = false;
  #reason = null;
  #onKeyframeRequest;

  constructor({ initiallyRecovering = true, onKeyframeRequest = () => {} } = {}) {
    if (typeof initiallyRecovering !== 'boolean') {
      throw new TypeError('initiallyRecovering must be a boolean');
    }
    if (typeof onKeyframeRequest !== 'function') {
      throw new TypeError('onKeyframeRequest must be a function');
    }
    this.#recovering = initiallyRecovering;
    this.#onKeyframeRequest = onKeyframeRequest;
  }

  get recovering() {
    return this.#recovering;
  }

  get reason() {
    return this.#reason;
  }

  get requestIssued() {
    return this.#requestIssued;
  }

  /** Enter recovery, coalescing with an already active recovery episode. */
  enter(reason) {
    validReason(reason);
    const entered = !this.#recovering;
    if (entered) {
      this.#recovering = true;
      this.#requestIssued = false;
      this.#reason = reason;
    } else if (this.#reason === null) {
      this.#reason = reason;
    }

    let requested = false;
    if (!this.#requestIssued) {
      // Mark the request before invoking user code. Even if that code throws,
      // the same recovery episode must not create an unbounded retry loop.
      this.#requestIssued = true;
      requested = true;
      this.#onKeyframeRequest(reason);
    }
    return { entered, requested };
  }

  /**
   * Start an explicitly new recovery episode, even if the previous episode
   * has not recovered. This is the deliberate retry boundary after an
   * external reset; ordinary repeated loss signals should use enter().
   */
  restart(reason) {
    validReason(reason);
    this.#recovering = true;
    this.#requestIssued = false;
    this.#reason = reason;
    const result = this.enter(reason);
    return { ...result, entered: true };
  }

  /** Reset session-local state without requesting from a not-yet-live host. */
  reset({ initiallyRecovering = true } = {}) {
    if (typeof initiallyRecovering !== 'boolean') {
      throw new TypeError('initiallyRecovering must be a boolean');
    }
    this.#recovering = initiallyRecovering;
    this.#requestIssued = false;
    this.#reason = null;
  }

  shouldDropFrame({ keyframe }) {
    if (typeof keyframe !== 'boolean') throw new TypeError('keyframe must be a boolean');
    return this.#recovering && !keyframe;
  }

  /**
   * Confirm the result of configuring/enqueuing a candidate recovery
   * keyframe. Failure intentionally stays in the same coalesced episode.
   */
  confirmKeyframeEnqueued(succeeded) {
    if (typeof succeeded !== 'boolean') throw new TypeError('succeeded must be a boolean');
    if (!this.#recovering || !succeeded) return false;
    this.#recovering = false;
    this.#requestIssued = false;
    this.#reason = null;
    return true;
  }
}
