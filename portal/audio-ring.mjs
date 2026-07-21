export const AUDIO_RING_CAPACITY_FRAMES = 2880;
export const AUDIO_RING_START_FRAMES = 1920;
export const AUDIO_SAMPLE_RATE = 48_000;
export const AUDIO_FRAME_DURATION_MICROS = 1_000_000 / AUDIO_SAMPLE_RATE;
export const MAX_AUDIO_WORKLET_MESSAGES_IN_FLIGHT = 3;

/** Bounded ownership tracker for decoded PCM transferred to an AudioWorklet. */
export class BoundedAudioMessageTracker {
  constructor(capacity = MAX_AUDIO_WORKLET_MESSAGES_IN_FLIGHT) {
    if (!Number.isSafeInteger(capacity) || capacity < 1) {
      throw new RangeError('message capacity must be a positive safe integer');
    }
    this.capacity = capacity;
    this.pending = new Set();
    this.nextId = 1;
    this.droppedMessages = 0;
  }

  reserve() {
    if (this.pending.size >= this.capacity) {
      this.droppedMessages++;
      return null;
    }
    for (let attempt = 0; attempt <= this.capacity; attempt++) {
      const id = this.nextId;
      this.nextId = id === Number.MAX_SAFE_INTEGER ? 1 : id + 1;
      if (!this.pending.has(id)) {
        this.pending.add(id);
        return id;
      }
    }
    throw new Error('unable to allocate a unique audio message ID');
  }

  accept(id) {
    if (!Number.isSafeInteger(id) || id < 1) return false;
    return this.pending.delete(id);
  }

  clear() {
    this.pending.clear();
  }

  reset() {
    this.clear();
    this.nextId = 1;
    this.droppedMessages = 0;
  }

  get size() {
    return this.pending.size;
  }
}

/** Fixed stereo planar ring used by the AudioWorklet. */
export class BoundedAudioRing {
  constructor({
    channels = 2,
    capacityFrames = AUDIO_RING_CAPACITY_FRAMES,
    startFrames = AUDIO_RING_START_FRAMES,
  } = {}) {
    if (!Number.isInteger(channels) || channels < 1) throw new RangeError('channels must be positive');
    if (!Number.isInteger(capacityFrames) || capacityFrames < 1) {
      throw new RangeError('capacityFrames must be positive');
    }
    if (!Number.isInteger(startFrames) || startFrames < 1 || startFrames > capacityFrames) {
      throw new RangeError('startFrames must fit within capacityFrames');
    }
    this.channels = channels;
    this.capacityFrames = capacityFrames;
    this.startFrames = startFrames;
    this.storage = Array.from({ length: channels }, () => new Float32Array(capacityFrames));
    // PTS storage is fixed alongside PCM storage so timestamp tracking cannot
    // allocate or grow on the AudioWorklet render path. NaN means that the
    // corresponding frame arrived through the backwards-compatible untimed API.
    this.frameEndPtsMicros = new Float64Array(capacityFrames);
    this.frameEndPtsMicros.fill(Number.NaN);
    this.readIndex = 0;
    this.writeIndex = 0;
    this.length = 0;
    this.started = false;
    this.droppedFrames = 0;
    this.recoveryDiscardedFrames = 0;
    this.underflows = 0;
    this.renderedFrames = 0;
    this.silentFrames = 0;
    this.underflowFrames = 0;
    this.underflowActive = false;
    this.renderedMediaEndPtsMicros = null;
  }

  write(channelData, startPtsMicros = null) {
    if (!Array.isArray(channelData) || channelData.length !== this.channels) {
      throw new TypeError(`expected ${this.channels} planar audio channels`);
    }
    const frames = channelData[0]?.length;
    if (!Number.isInteger(frames) || frames < 1) throw new RangeError('audio packet is empty');
    for (const channel of channelData) {
      if (!(channel instanceof Float32Array) || channel.length !== frames) {
        throw new TypeError('audio channels must be equal-length Float32Array values');
      }
    }
    const timestamped = startPtsMicros !== null && startPtsMicros !== undefined;
    if (timestamped && !Number.isSafeInteger(startPtsMicros)) {
      throw new TypeError('audio packet timestamp must be a safe integer');
    }

    let sourceOffset = 0;
    let writeFrames = frames;
    if (writeFrames > this.capacityFrames) {
      sourceOffset = writeFrames - this.capacityFrames;
      this.droppedFrames += sourceOffset;
      writeFrames = this.capacityFrames;
    }
    const overflow = Math.max(0, this.length + writeFrames - this.capacityFrames);
    if (overflow > 0) {
      this.readIndex = (this.readIndex + overflow) % this.capacityFrames;
      this.length -= overflow;
      this.droppedFrames += overflow;
    }
    const first = Math.min(writeFrames, this.capacityFrames - this.writeIndex);
    for (let channel = 0; channel < this.channels; channel++) {
      this.storage[channel].set(
        channelData[channel].subarray(sourceOffset, sourceOffset + first),
        this.writeIndex,
      );
      if (first < writeFrames) {
        this.storage[channel].set(
          channelData[channel].subarray(sourceOffset + first, sourceOffset + writeFrames),
          0,
        );
      }
    }
    for (let frame = 0; frame < writeFrames; frame++) {
      const destination = (this.writeIndex + frame) % this.capacityFrames;
      this.frameEndPtsMicros[destination] = timestamped
        ? startPtsMicros + (sourceOffset + frame + 1) * AUDIO_FRAME_DURATION_MICROS
        : Number.NaN;
    }
    this.writeIndex = (this.writeIndex + writeFrames) % this.capacityFrames;
    this.length += writeFrames;
    if (!this.started && this.length >= this.startFrames) {
      this.started = true;
      this.underflowActive = false;
    }
    return overflow + sourceOffset;
  }

  read(outputs, muted = false) {
    if (!Array.isArray(outputs) || outputs.length !== this.channels) {
      throw new TypeError(`expected ${this.channels} output channels`);
    }
    const frames = outputs[0]?.length;
    for (const output of outputs) {
      if (!(output instanceof Float32Array) || output.length !== frames) {
        throw new TypeError('output channels must be equal-length Float32Array values');
      }
      output.fill(0);
    }
    if (frames === 0) return 0;
    if (!this.started) {
      this.renderedMediaEndPtsMicros = null;
      this.silentFrames += frames;
      if (this.underflowActive) this.underflowFrames += frames;
      return 0;
    }

    const rendered = Math.min(frames, this.length);
    const first = Math.min(rendered, this.capacityFrames - this.readIndex);
    if (!muted) {
      for (let channel = 0; channel < this.channels; channel++) {
        outputs[channel].set(this.storage[channel].subarray(this.readIndex, this.readIndex + first));
        if (first < rendered) {
          outputs[channel].set(this.storage[channel].subarray(0, rendered - first), first);
        }
      }
    }
    const finalMediaFrame = (this.readIndex + rendered - 1) % this.capacityFrames;
    const finalMediaEndPtsMicros = this.frameEndPtsMicros[finalMediaFrame];
    this.renderedMediaEndPtsMicros = rendered === frames && Number.isFinite(finalMediaEndPtsMicros)
      ? finalMediaEndPtsMicros
      : null;
    this.readIndex = (this.readIndex + rendered) % this.capacityFrames;
    this.length -= rendered;
    this.renderedFrames += rendered;
    if (rendered < frames) {
      this.underflows++;
      const missingFrames = frames - rendered;
      this.silentFrames += missingFrames;
      this.underflowFrames += missingFrames;
      this.underflowActive = true;
      this.started = false;
    }
    return rendered;
  }

  clear({ recovery = false } = {}) {
    if (recovery) this.recoveryDiscardedFrames += this.length;
    this.readIndex = 0;
    this.writeIndex = 0;
    this.length = 0;
    this.started = false;
    this.underflowActive = false;
    this.renderedMediaEndPtsMicros = null;
    this.frameEndPtsMicros.fill(Number.NaN);
  }

  snapshot() {
    return {
      bufferedFrames: this.length,
      capacityFrames: this.capacityFrames,
      started: this.started,
      droppedFrames: this.droppedFrames,
      recoveryDiscardedFrames: this.recoveryDiscardedFrames,
      underflows: this.underflows,
      renderedFrames: this.renderedFrames,
      silentFrames: this.silentFrames,
      silentDurationMicros: this.silentFrames * AUDIO_FRAME_DURATION_MICROS,
      underflowFrames: this.underflowFrames,
      underflowDurationMicros: this.underflowFrames * AUDIO_FRAME_DURATION_MICROS,
      renderedMediaEndPtsMicros: this.renderedMediaEndPtsMicros,
    };
  }
}
