import {
  audioMinusVideoSkewMs,
  audioOutputTimelineFromStats,
  exactMediaTimestampMicros,
  isCurrentAvSyncEpoch,
  nextAvSyncEpoch,
  projectAudioMediaPtsMicros,
} from './av-sync.mjs';
import {
  isCurrentAudioDelivery,
  isCurrentAudioGeneration,
  parseAudioEnvelope,
  stageAudioTerminalState,
  takeAudioTerminalState,
} from './audio-envelope.mjs';
import { newAudioSession } from './audio-ui.mjs';

export const AUDIO_SAMPLE_RATE = 48000;

const AUDIO_DECODER_CONFIG = Object.freeze({
  codec: 'opus',
  sampleRate: AUDIO_SAMPLE_RATE,
  numberOfChannels: 2,
});
const MAX_AUDIO_DECODE_QUEUE_SIZE = 3;

export function createAudioPipelineSession({
  invokeCommand,
  onUpdate = () => {},
  resetAvSync = () => {},
  isDisconnecting = () => false,
  hasAudioDecoder = () => typeof globalThis.AudioDecoder === 'function',
  hasAudioWorklet = () => typeof globalThis.AudioWorkletNode === 'function',
  getAudioContextConstructor = () => globalThis.AudioContext || globalThis.webkitAudioContext,
  probeDecoderSupport = (config) => globalThis.AudioDecoder.isConfigSupported(config),
  createDecoder = (callbacks) => new globalThis.AudioDecoder(callbacks),
  createWorkletNode = (context, name, options) => new globalThis.AudioWorkletNode(
    context,
    name,
    options,
  ),
  createEncodedChunk = (init) => new globalThis.EncodedAudioChunk(init),
} = {}) {
  if (typeof invokeCommand !== 'function') throw new TypeError('invokeCommand must be a function');

  let activeSession = newAudioSession();

  function update() {
    onUpdate(activeSession);
  }

  function setState(state, detail = state) {
    activeSession.state = state;
    activeSession.stateDetail = detail;
    update();
  }

  function resetSyncTelemetry(session = activeSession, { recoverRing = false } = {}) {
    if (!session) return;
    session.audioTimeline = null;
    session.avSyncEpoch = nextAvSyncEpoch(session.avSyncEpoch);
    try {
      session.workletNode?.port.postMessage({
        type: recoverRing ? 'recover' : 'av-sync-epoch',
        avSyncEpoch: session.avSyncEpoch,
      });
    } catch (_) {}
  }

  function sampleSkew(videoPtsMicros, presentationTimeMs) {
    const session = activeSession;
    if (
      exactMediaTimestampMicros(videoPtsMicros) === null
      || !session
      || session.failed
      || !session.audioTimeline
      || !session.available
      || session.context?.state !== 'running'
      || typeof session.context.getOutputTimestamp !== 'function'
    ) return null;

    let outputTimestamp;
    try {
      outputTimestamp = session.context.getOutputTimestamp();
    } catch (_) {
      return null;
    }
    const audioPtsMicros = projectAudioMediaPtsMicros(
      session.audioTimeline,
      outputTimestamp,
      presentationTimeMs,
    );
    return audioMinusVideoSkewMs(audioPtsMicros, videoPtsMicros);
  }

  function resetTelemetry(session = activeSession) {
    session.packetsReceived = 0;
    session.decoderDropped = 0;
    session.bufferedFrames = 0;
    session.underflows = 0;
    session.underflowDurationMicros = 0;
    session.silentDurationMicros = 0;
    session.transportDropped = 0;
    session.frontendDropped = 0;
    resetAvSync(session);
    update();
  }

  function acknowledge(generation, deliveryId) {
    void invokeCommand('iroh_client_ack_audio', { generation, deliveryId }).catch((error) => {
      console.warn('audio acknowledgment failed:', error);
    });
  }

  function requestNativeStop(session) {
    const generation = session?.expectedGeneration;
    if (session !== activeSession
      || !Number.isSafeInteger(generation)
      || generation < 0
      || session.stopRequested) return;
    session.stopRequested = true;
    void invokeCommand('iroh_client_stop_audio', { expectedGeneration: generation }).catch((error) => {
      console.warn('audio stop failed:', error);
    });
  }

  function disable(detail, session = activeSession) {
    if (!session || session !== activeSession) return;
    resetAvSync(session);
    session.failed = true;
    session.failureDetail = detail;
    session.available = false;
    if (session.decoder) {
      try { session.decoder.close(); } catch (_) {}
      session.decoder = null;
    }
    session.messageTracker.clear();
    try { session.workletNode?.port.postMessage({ type: 'clear' }); } catch (_) {}
    requestNativeStop(session);
    setState('error', detail);
  }

  function configureDecoder(session = activeSession) {
    if (session.decoder) {
      try { session.decoder.close(); } catch (_) {}
    }
    session.decoder = createDecoder({
      output: (audioData) => {
        try {
          if (session !== activeSession || session.failed) return;
          const workletNode = session?.workletNode;
          if (!workletNode || audioData.numberOfChannels !== 2) {
            throw new Error(`unexpected decoded audio channel count: ${audioData.numberOfChannels}`);
          }
          const timestampMicros = exactMediaTimestampMicros(audioData.timestamp);
          if (timestampMicros === null) throw new Error('decoded audio has an invalid media timestamp');
          // Reserve ownership before allocating/copying PCM. The MessagePort has
          // no useful queue-depth API, so this makes overload latest-frame-wins
          // at a hard ceiling of three decoded messages.
          const messageId = session.messageTracker.reserve();
          if (messageId === null) {
            update();
            return;
          }
          const channels = [];
          const transfers = [];
          try {
            for (let planeIndex = 0; planeIndex < 2; planeIndex++) {
              const channel = new Float32Array(audioData.numberOfFrames);
              audioData.copyTo(channel, { planeIndex, format: 'f32-planar' });
              channels.push(channel);
              transfers.push(channel.buffer);
            }
            workletNode.port.postMessage({
              type: 'samples',
              id: messageId,
              channels,
              timestampMicros,
            }, transfers);
          } catch (error) {
            session.messageTracker.accept(messageId);
            throw error;
          }
        } catch (error) {
          disable(`Decoded audio output failed: ${error}`, session);
        } finally {
          audioData.close();
        }
      },
      error: (error) => {
        disable(`Opus decoder failed: ${error}`, session);
      },
    });
    session.decoder.configure(AUDIO_DECODER_CONFIG);
  }

  function primeOutputForActivation() {
    const session = activeSession;
    const AudioContextConstructor = getAudioContextConstructor();
    if (typeof AudioContextConstructor !== 'function') return;
    if (!session.context || session.context.state === 'closed') {
      try {
        session.context = new AudioContextConstructor({
          latencyHint: 'interactive',
          sampleRate: AUDIO_SAMPLE_RATE,
        });
      } catch (_) {
        return;
      }
    }
    // This function is deliberately synchronous and is called before the first
    // await in a click/keyboard connect handler, preserving WebKit user activation.
    void session.context.resume().catch(() => {});
  }

  async function initialize(session) {
    if (session.decoder) {
      try { session.decoder.close(); } catch (_) {}
      session.decoder = null;
    }
    if (session.workletNode) {
      try { session.workletNode.disconnect(); } catch (_) {}
      session.workletNode = null;
    }
    session.messageTracker.clear();
    if (!hasAudioDecoder()) {
      return { supported: false, error: 'WebCodecs AudioDecoder is unavailable' };
    }
    if (!hasAudioWorklet()) {
      return { supported: false, error: 'AudioWorklet is unavailable' };
    }
    let support;
    try {
      support = await probeDecoderSupport(AUDIO_DECODER_CONFIG);
    } catch (error) {
      return { supported: false, error: `Opus capability probe failed: ${error}` };
    }
    if (!support.supported) {
      return { supported: false, error: 'WebCodecs Opus decoding is unsupported' };
    }
    if (!session.context) {
      return { supported: false, error: 'Web Audio output is unavailable' };
    }
    try {
      if (session.context.sampleRate !== AUDIO_SAMPLE_RATE) {
        throw new Error(`audio output opened at ${session.context.sampleRate} Hz instead of 48000 Hz`);
      }
      await session.context.audioWorklet.addModule(new URL('./audio-worklet.js', import.meta.url));
      session.workletNode = createWorkletNode(session.context, 'sigil-audio-processor', {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [2],
      });
      session.workletNode.port.onmessage = (event) => {
        if (event.data?.type === 'accepted') {
          session.messageTracker.accept(event.data.id);
          if (session === activeSession) update();
        } else if (event.data?.type === 'stats') {
          if (!isCurrentAvSyncEpoch(event.data.avSyncEpoch, session.avSyncEpoch)) return;
          session.workletDroppedFrames = Number.isSafeInteger(event.data.droppedFrames)
            && event.data.droppedFrames >= 0
            ? event.data.droppedFrames : session.workletDroppedFrames;
          session.workletRecoveryDiscardedFrames = Number.isSafeInteger(
            event.data.recoveryDiscardedFrames,
          ) && event.data.recoveryDiscardedFrames >= 0
            ? event.data.recoveryDiscardedFrames : session.workletRecoveryDiscardedFrames;
          session.audioTimeline = audioOutputTimelineFromStats(event.data);
          if (session !== activeSession) return;
          session.bufferedFrames = Number.isSafeInteger(event.data.bufferedFrames)
            ? event.data.bufferedFrames : session.bufferedFrames;
          session.underflows = Number.isSafeInteger(event.data.underflows)
            ? event.data.underflows : session.underflows;
          session.underflowDurationMicros = Number.isFinite(event.data.underflowDurationMicros)
            && event.data.underflowDurationMicros >= 0
            ? event.data.underflowDurationMicros : session.underflowDurationMicros;
          session.silentDurationMicros = Number.isFinite(event.data.silentDurationMicros)
            && event.data.silentDurationMicros >= 0
            ? event.data.silentDurationMicros : session.silentDurationMicros;
          if (session.available && !session.muted) {
            setState(event.data.started ? 'playing' : 'priming', 'bounded Opus playback');
          } else {
            update();
          }
        } else if (event.data?.type === 'error') {
          disable(`AudioWorklet failed: ${event.data.error}`, session);
        }
      };
      session.workletNode.port.onmessageerror = () => {
        disable('AudioWorklet returned an unreadable message', session);
      };
      session.workletNode.onprocessorerror = () => {
        disable('AudioWorklet processor stopped unexpectedly', session);
      };
      session.workletNode.connect(session.context.destination);
      session.workletNode.port.postMessage({ type: 'mute', muted: session.muted });
      configureDecoder(session);
      await session.context.resume();
      setState(
        session.context.state === 'running' ? 'negotiating' : 'blocked',
        session.context.state === 'running'
          ? 'Opus output ready'
          : 'WebKit suspended audio output; activate the audio button to retry',
      );
      return { supported: true, error: null };
    } catch (error) {
      await teardown(false);
      return { supported: false, error: `Audio initialization failed: ${error}` };
    }
  }

  async function teardown(resetStatus = true) {
    const session = activeSession;
    resetAvSync(session);
    const decoder = session.decoder;
    const workletNode = session.workletNode;
    const context = session.context;
    // Replace the active owner before closing resources so delayed callbacks
    // cannot mutate the disconnected UI or a successor session. Seed the next
    // dormant session with the user's mute preference across reconnects.
    activeSession = newAudioSession({ muted: session.muted });
    session.expectedGeneration = null;
    session.channel = null;
    session.available = false;
    session.messageTracker.clear();
    session.decoder = null;
    session.workletNode = null;
    session.context = null;
    if (decoder) {
      try { decoder.close(); } catch (_) {}
    }
    if (workletNode) {
      try { workletNode.port.postMessage({ type: 'clear' }); } catch (_) {}
      try { workletNode.disconnect(); } catch (_) {}
    }
    if (context && context.state !== 'closed') {
      try { await context.close(); } catch (_) {}
    }
    if (resetStatus) update();
  }

  async function toggleMute() {
    const session = activeSession;
    if (!session.available) return;
    session.muted = !session.muted;
    try {
      if (!session.muted && session.context?.state !== 'running') await session.context?.resume();
      session.workletNode?.port.postMessage({ type: 'mute', muted: session.muted });
      if (!session.muted && session.context?.state !== 'running') {
        setState('blocked', 'WebKit suspended audio output; click the audio button to retry');
      } else {
        update();
      }
    } catch (error) {
      setState('error', `Audio output activation failed: ${error}`);
    }
  }

  function prepareForConnection() {
    activeSession = newAudioSession({ muted: activeSession.muted });
    activeSession.acceptsNativeEvents = true;
    // Keep this synchronous and before createConnectionAttempt's first await so
    // WebKit observes the originating click/keyboard user activation.
    primeOutputForActivation();
  }

  function beginAttempt() {
    const session = activeSession;
    resetTelemetry(session);
    return session;
  }

  async function createAttemptChannel(Channel, session = activeSession) {
    const support = await initialize(session);
    if (!support.supported) setState('unavailable', support.error);
    session.channel = new Channel((message) => handleBinaryMessage(message, session));
    return { session, support, channel: session.channel };
  }

  function releaseAttempt(session) {
    if (session && session !== activeSession) session.channel = null;
  }

  async function acceptConnectedResult(result, { session, support }) {
    session.expectedGeneration = Number.isSafeInteger(result.audio_generation)
      && result.audio_generation > 0
      ? result.audio_generation : null;
    const pendingTerminal = takeAudioTerminalState(
      session.pendingTerminalStates,
      session.expectedGeneration,
    );
    if (pendingTerminal !== null) {
      session.failed = true;
      session.failureDetail = pendingTerminal.error || 'Audio connection ended';
    }
    session.available = result.audio_available === true;
    if (session.available && session.failed) {
      disable(session.failureDetail || 'Audio output failed', session);
    } else if (session.available) {
      setState(
        session.context?.state === 'running' ? 'priming' : 'blocked',
        session.context?.state === 'running'
          ? 'Waiting for bounded Opus prebuffer'
          : 'WebKit suspended audio output; activate the audio button to retry',
      );
    } else {
      await teardown(false);
      setState('unavailable', result.audio_error || support.error || 'host audio unavailable');
    }
  }

  function handleBinaryMessage(message, session) {
    let packet;
    try {
      packet = parseAudioEnvelope(message);
    } catch (error) {
      console.error('invalid audio packet:', error);
      disable(`Audio packet failed: ${error}`, session);
      return;
    }

    // Every valid delivery is released using the token embedded in that exact
    // message. A delayed delivery from an old connection is acknowledged but
    // cannot enter the current decoder.
    acknowledge(packet.generation, packet.deliveryId);
    if (session !== activeSession
      || session.failed
      || !isCurrentAudioDelivery(packet, session.expectedGeneration)) return;

    try {
      session.packetsReceived++;
      if (!session.decoder || session.decoder.state === 'closed') {
        session.decoderDropped++;
        update();
        return;
      }
      const queuedAudioPackets = session.decoder.decodeQueueSize;
      if (packet.discontinuity || queuedAudioPackets >= MAX_AUDIO_DECODE_QUEUE_SIZE) {
        resetAvSync(session, { recoverRing: true });
        session.decoderDropped += queuedAudioPackets;
        configureDecoder(session);
      }
      session.decoder.decode(createEncodedChunk({
        type: 'key',
        timestamp: packet.ptsMicros,
        duration: (packet.frameSamples * 1_000_000) / packet.sampleRate,
        data: packet.data,
      }));
      update();
    } catch (error) {
      console.error('undecodable audio packet:', error);
      disable(`Audio packet failed: ${error}`, session);
    }
  }

  function handleNativeState(payload) {
    const session = activeSession;
    if (isDisconnecting()
      || !session.acceptsNativeEvents
      || payload?.available !== false) return;
    if (session.expectedGeneration === null) {
      try {
        stageAudioTerminalState(session.pendingTerminalStates, payload);
      } catch (error) {
        console.error('invalid pre-connect audio terminal state:', error);
      }
      return;
    }
    if (!isCurrentAudioGeneration(payload?.generation, session.expectedGeneration)) return;
    disable(payload.error || 'Audio connection ended', session);
  }

  function handleNativeStats(payload) {
    const session = activeSession;
    if (!isCurrentAudioGeneration(payload?.generation, session.expectedGeneration)) return;
    session.transportDropped = Number.isSafeInteger(payload?.sequence_dropped_total)
      ? payload.sequence_dropped_total : session.transportDropped;
    session.frontendDropped = Number.isSafeInteger(payload?.frontend_dropped_total)
      ? payload.frontend_dropped_total : session.frontendDropped;
    update();
  }

  return Object.freeze({
    acceptConnectedResult,
    beginAttempt,
    createAttemptChannel,
    handleNativeState,
    handleNativeStats,
    prepareForConnection,
    releaseAttempt,
    resetSyncTelemetry,
    sampleSkew,
    teardown,
    toggleMute,
    get session() {
      return activeSession;
    },
  });
}
