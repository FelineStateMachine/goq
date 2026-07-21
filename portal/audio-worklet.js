import { BoundedAudioRing } from './audio-ring.mjs';

class SigilAudioProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = new BoundedAudioRing();
    this.muted = false;
    this.processCount = 0;
    this.avSyncEpoch = 0;
    this.port.onmessage = (event) => {
      try {
        if (event.data?.type === 'samples') {
          const id = event.data.id;
          if (!Number.isSafeInteger(id) || id < 1) throw new Error('invalid audio message ID');
          this.ring.write(event.data.channels, event.data.timestampMicros);
          this.port.postMessage({ type: 'accepted', id });
        } else if (event.data?.type === 'av-sync-epoch' || event.data?.type === 'recover') {
          const avSyncEpoch = event.data.avSyncEpoch;
          if (!Number.isSafeInteger(avSyncEpoch) || avSyncEpoch < 0) {
            throw new Error('invalid A/V sync epoch');
          }
          this.avSyncEpoch = avSyncEpoch;
          if (event.data.type === 'recover') this.ring.clear({ recovery: true });
        } else if (event.data?.type === 'mute') this.muted = event.data.muted === true;
        else if (event.data?.type === 'clear') this.ring.clear();
      } catch (error) {
        this.ring.clear();
        this.port.postMessage({ type: 'error', error: String(error) });
      }
    };
  }

  process(_inputs, outputs) {
    const output = outputs[0];
    const underflowsBefore = this.ring.underflows;
    if (output?.length === 2) this.ring.read(output, this.muted);
    const underflowStarted = this.ring.underflows !== underflowsBefore;
    const outputQuantumFrames = output?.[0]?.length ?? 0;
    const outputContextSampleRate = typeof sampleRate === 'number' ? sampleRate : null;
    const outputContextFrameEnd = typeof currentFrame === 'number'
      ? currentFrame + outputQuantumFrames
      : null;
    const outputContextTimeEndSeconds = outputContextFrameEnd !== null
      && outputContextSampleRate !== null
      ? outputContextFrameEnd / outputContextSampleRate
      : null;
    this.processCount++;
    // Invalidate the projected A/V timeline on the exact quantum that starts
    // an underrun. Routine telemetry remains rate-limited to every 16 quanta.
    if (underflowStarted || this.processCount % 16 === 0) {
      this.port.postMessage({
        type: 'stats',
        ...this.ring.snapshot(),
        muted: this.muted,
        outputQuantumFrames,
        outputContextFrameEnd,
        outputContextTimeEndSeconds,
        outputContextSampleRate,
        avSyncEpoch: this.avSyncEpoch,
      });
    }
    return true;
  }
}

registerProcessor('sigil-audio-processor', SigilAudioProcessor);
