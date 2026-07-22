function validFormat({ codec, width, height }) {
  if (!['h264', 'h265', 'av1'].includes(codec)) throw new TypeError('invalid video codec');
  if (!Number.isSafeInteger(width) || width <= 0 || !Number.isSafeInteger(height) || height <= 0) {
    throw new TypeError('invalid video dimensions');
  }
  return Object.freeze({ codec, width, height });
}

function sameFormat(left, right) {
  return left !== null
    && left.codec === right.codec
    && left.width === right.width
    && left.height === right.height;
}

export class VideoFormatTransitionGuard {
  #format = null;
  #lastSequence = null;
  #epoch = 0;
  #revision = 0;

  get format() { return this.#format; }
  get epoch() { return this.#epoch; }

  reset() {
    this.#format = null;
    this.#lastSequence = null;
    this.#epoch = 0;
    this.#revision++;
  }

  plan({ sequence, codec, width, height, keyframe, codecConfig, discontinuity }) {
    const format = validFormat({ codec, width, height });
    if (sequence !== null && sequence !== undefined
      && (!Number.isSafeInteger(sequence) || sequence < 0)) {
      throw new TypeError('invalid video sequence');
    }
    if (codecConfig && !keyframe) throw new Error('codec configuration requires a keyframe');
    if (sequence !== null && sequence !== undefined
      && this.#lastSequence !== null && sequence <= this.#lastSequence) {
      return Object.freeze({ action: 'drop-stale', epoch: this.#epoch });
    }

    const reconfigure = !sameFormat(this.#format, format) || discontinuity;
    if (reconfigure && !(keyframe && codecConfig)) {
      return Object.freeze({ action: 'recover', epoch: this.#epoch });
    }
    return Object.freeze({
      action: 'accept',
      reconfigure,
      format,
      sequence: sequence ?? null,
      epoch: this.#epoch + (reconfigure ? 1 : 0),
      revision: this.#revision,
    });
  }

  commit(plan) {
    if (plan?.action !== 'accept' || plan.revision !== this.#revision) {
      throw new Error('stale video format transition');
    }
    if (plan.reconfigure) this.#format = plan.format;
    if (plan.sequence !== null) this.#lastSequence = plan.sequence;
    this.#epoch = plan.epoch;
    this.#revision++;
    return Object.freeze({ format: this.#format, epoch: this.#epoch });
  }
}
