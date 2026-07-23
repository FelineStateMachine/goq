import { parseFrameEnvelope } from './frame-envelope.mjs';
import {
  isCurrentFrameGeneration,
  normalizeFrameStatsPayload,
} from './frame-stats.mjs';
import {
  FRAME_CHANNEL_CAPACITY,
  activateFrameSession as activatePendingFrameSession,
  isActiveFrameSession,
  newFrameSession,
  stageFrameAcknowledgment,
} from './frame-session.mjs';
import {
  AdaptiveFeedbackPublisher,
  normalizeAdaptiveDecisionEnvelope,
} from './adaptive-feedback.mjs';

function newTransportTelemetry() {
  return {
    transportDroppedFrames: 0,
    transportObjectDroppedFrames: null,
    transportLateObjectDroppedFrames: null,
    frontendDroppedFrames: 0,
    frontendQueueDroppedFrames: null,
    frontendResyncDroppedFrames: null,
    frontendQueueStats: null,
    frontendResyncStats: null,
    transportIntervalStats: null,
    frontendIpcSendDurationStats: null,
    rustTimingWindow: null,
    networkDiagnostics: null,
    lastTransportFps: 0,
    lastFrontendSendFps: 0,
    streamPathMode: 'unknown',
    streamRttMs: null,
    lastMediaSequence: null,
  };
}

function assertDependency(value, name) {
  if (typeof value !== 'function') throw new TypeError(`${name} must be a function`);
}

export function createStreamRuntime({
  invokeCommand,
  videoPipeline,
  isConnected = () => false,
  disconnect = () => {},
  adaptiveFeedbackPublisher = null,
  logger = console,
} = {}) {
  assertDependency(invokeCommand, 'invokeCommand');
  assertDependency(isConnected, 'isConnected');
  assertDependency(disconnect, 'disconnect');
  if (!videoPipeline
    || typeof videoPipeline.processFramePayload !== 'function'
    || typeof videoPipeline.reset !== 'function'
    || typeof videoPipeline.teardown !== 'function'
    || typeof videoPipeline.snapshot !== 'function') {
    throw new TypeError('videoPipeline must implement the stream pipeline interface');
  }
  const feedbackPublisher = adaptiveFeedbackPublisher
    ?? new AdaptiveFeedbackPublisher({ invokeCommand });
  if (!feedbackPublisher
    || typeof feedbackPublisher.start !== 'function'
    || typeof feedbackPublisher.stop !== 'function'
    || typeof feedbackPublisher.publish !== 'function') {
    throw new TypeError('adaptiveFeedbackPublisher must implement start, stop, and publish');
  }

  let activeFrameSession = null;
  let activeFrameGeneration = null;
  let adaptiveFeedbackAvailable = false;
  let adaptiveFeedbackError = null;
  let adaptiveDecision = null;
  let transport = newTransportTelemetry();

  function sendFrameAcknowledgment(generation) {
    void invokeCommand('iroh_client_ack_frame', { generation }).catch((error) => {
      logger.warn('frame acknowledgment failed:', error);
    });
  }

  function acknowledgeFrame(session, generation = null) {
    const readyGeneration = stageFrameAcknowledgment(session, generation);
    if (readyGeneration !== null) sendFrameAcknowledgment(readyGeneration);
  }

  function failFrameSession(session, error) {
    if (!isActiveFrameSession(session, activeFrameSession)) return false;
    session.failed = true;
    session.failureDetail = `Frame delivery failed: ${error}`;
    logger.error(session.failureDetail);
    if (isConnected()) void disconnect();
    return true;
  }

  function prepareConnection() {
    if (activeFrameSession) activeFrameSession.closing = true;
    activeFrameSession = null;
    activeFrameGeneration = null;
    feedbackPublisher.stop();
    adaptiveFeedbackAvailable = false;
    adaptiveFeedbackError = null;
    adaptiveDecision = null;
    videoPipeline.reset();
    transport = newTransportTelemetry();
  }

  function openFrameSession() {
    const session = newFrameSession();
    activeFrameSession = session;
    return session;
  }

  function activateFrameSession(session, {
    generation,
    adaptiveFeedbackAvailable: feedbackAvailable = false,
    adaptiveFeedbackError: feedbackError = null,
  }) {
    if (!isActiveFrameSession(session, activeFrameSession)) {
      throw new Error('frame session was superseded during connect');
    }
    const activation = activatePendingFrameSession(session, generation);
    activeFrameGeneration = generation;
    for (const pendingGeneration of activation.acknowledgments) {
      sendFrameAcknowledgment(pendingGeneration);
    }
    const pendingError = session.pendingFrameErrors.splice(0).find(
      (payload) => isCurrentFrameGeneration(payload?.generation, generation),
    );
    if (pendingError) throw new Error(pendingError.error || 'Media connection failed');
    adaptiveFeedbackAvailable = feedbackAvailable === true;
    adaptiveFeedbackError = typeof feedbackError === 'string' ? feedbackError : null;
    feedbackPublisher.start(generation, adaptiveFeedbackAvailable);
  }

  function closeFrameSession(session) {
    const closingSession = session ?? activeFrameSession;
    if (closingSession) closingSession.closing = true;
    if (activeFrameSession === closingSession) activeFrameSession = null;
    activeFrameGeneration = null;
    feedbackPublisher.stop();
    adaptiveFeedbackAvailable = false;
    adaptiveDecision = null;
  }

  function prepareDisconnect() {
    if (activeFrameSession) activeFrameSession.closing = true;
    activeFrameSession = null;
    activeFrameGeneration = null;
    feedbackPublisher.stop();
    adaptiveFeedbackAvailable = false;
    adaptiveFeedbackError = null;
    adaptiveDecision = null;
  }

  function teardown() {
    videoPipeline.teardown();
  }

  function requestKeyframe(reason) {
    if (!Number.isSafeInteger(activeFrameGeneration) || activeFrameGeneration < 1) return false;
    void invokeCommand('iroh_client_request_keyframe', {
      generation: activeFrameGeneration,
      reason,
    }).catch((error) => {
      logger.warn(`keyframe request failed (${reason}):`, error);
    });
    return true;
  }

  function handleBinaryFrame(message, session) {
    if (!isActiveFrameSession(session, activeFrameSession)) return false;
    let acknowledged = false;
    try {
      const frame = parseFrameEnvelope(message);
      // Parsing establishes the exact envelope and payload bounds. Release the
      // Rust-side delivery permit before codec parsing/decode enqueue so a slow
      // invoke round trip cannot turn one scheduling hiccup into a full-GOP
      // discard. WebCodecs remains independently bounded by decodeQueueSize.
      acknowledgeFrame(session);
      acknowledged = true;
      videoPipeline.processFramePayload({
        width: frame.width,
        height: frame.height,
        codec: frame.codec,
        keyframe: frame.keyframe,
        codecConfig: frame.codecConfig,
        sequence: frame.sequence,
        pts_micros: frame.ptsMicros,
        discontinuity: frame.discontinuity,
      }, frame.data);
      return true;
    } catch (error) {
      failFrameSession(session, error);
      return false;
    } finally {
      if (!acknowledged) {
        // A malformed envelope disconnects above, but must never deadlock the
        // bounded sender while teardown reaches Rust.
        try {
          acknowledgeFrame(session);
        } catch (error) {
          failFrameSession(session, error);
        }
      }
    }
  }

  function handleFrameStats(payload) {
    if (!isCurrentFrameGeneration(payload?.generation, activeFrameGeneration)) return false;
    const diagnostics = normalizeFrameStatsPayload(payload);
    transport.transportDroppedFrames = diagnostics.transportDroppedFrames
      ?? transport.transportDroppedFrames;
    transport.transportObjectDroppedFrames = diagnostics.objectDroppedFrames;
    transport.transportLateObjectDroppedFrames = diagnostics.lateObjectDroppedFrames;
    transport.frontendDroppedFrames = diagnostics.frontendDroppedFrames
      ?? transport.frontendDroppedFrames;
    transport.frontendQueueDroppedFrames = diagnostics.queueDroppedFrames;
    transport.frontendResyncDroppedFrames = diagnostics.resyncDroppedFrames;
    transport.frontendQueueStats = diagnostics.queue;
    transport.frontendResyncStats = diagnostics.resync;
    transport.transportIntervalStats = diagnostics.transportIntervals;
    transport.frontendIpcSendDurationStats = diagnostics.ipcSendDurations;
    transport.rustTimingWindow = diagnostics.timingWindow;
    transport.networkDiagnostics = diagnostics.networkDiagnostics;
    transport.streamPathMode = typeof payload.path_mode === 'string'
      ? payload.path_mode
      : transport.streamPathMode;
    transport.streamRttMs = Number.isFinite(payload.path_rtt_ms)
      ? payload.path_rtt_ms
      : transport.streamRttMs;
    transport.lastMediaSequence = Number.isSafeInteger(payload.sequence) && payload.sequence >= 0
      ? payload.sequence : transport.lastMediaSequence;
    transport.lastTransportFps = Number.isFinite(payload.transport_receive_fps)
      ? payload.transport_receive_fps
      : Number.isFinite(payload.transport_fps)
        ? payload.transport_fps
        : payload.fps;
    transport.lastFrontendSendFps = Number.isFinite(payload.frontend_send_fps)
      ? payload.frontend_send_fps
      : Number.isFinite(payload.frontend_fps)
        ? payload.frontend_fps
        : payload.fps;
    return true;
  }

  function handleAdaptiveDecision(payload) {
    try {
      const decision = normalizeAdaptiveDecisionEnvelope(payload, activeFrameGeneration);
      if (decision === null) return false;
      adaptiveDecision = decision;
      return true;
    } catch (error) {
      logger.warn('invalid adaptive bitrate diagnostic:', error);
      return false;
    }
  }

  function handleAdaptiveFeedbackState(payload) {
    if (!isCurrentFrameGeneration(payload?.generation, activeFrameGeneration)) return false;
    if (payload?.available !== false) return false;
    adaptiveFeedbackAvailable = false;
    adaptiveFeedbackError = typeof payload.error === 'string'
      ? payload.error : 'feedback stream closed';
    feedbackPublisher.stop();
    return true;
  }

  function handleFrameError(payload) {
    const session = activeFrameSession;
    if (!isActiveFrameSession(session, activeFrameSession)) return false;
    if (session.expectedGeneration === null) {
      if (session.pendingFrameErrors.length < FRAME_CHANNEL_CAPACITY) {
        session.pendingFrameErrors.push(payload);
        return true;
      }
      failFrameSession(session, 'pre-connect frame error capacity exceeded');
      return false;
    }
    if (!isCurrentFrameGeneration(payload?.generation, session.expectedGeneration)) return false;
    return failFrameSession(session, payload?.error || 'Media connection failed');
  }

  function snapshot(at) {
    return {
      video: at === undefined ? videoPipeline.snapshot() : videoPipeline.snapshot(at),
      generation: activeFrameGeneration,
      ...transport,
      adaptiveFeedbackAvailable,
      adaptiveFeedbackError,
      adaptiveDecision,
    };
  }

  function publishAdaptiveFeedback(video) {
    return feedbackPublisher.publish({
      lastSequence: transport.lastMediaSequence,
      frontendQueueDepth: transport.frontendQueueStats?.depth ?? 0,
      frontendQueueCapacity: transport.frontendQueueStats?.capacity ?? 4,
      decoderQueueDepth: video.decoderQueueDepth,
      decoderQueueCapacity: video.decoderQueueCapacity,
      presenterQueueDepth: video.presenterQueueDepth,
      presenterQueueCapacity: video.presenterQueueCapacity,
      transportDroppedTotal: transport.transportDroppedFrames,
      frontendDroppedTotal: transport.frontendDroppedFrames,
      decoderDroppedTotal: video.droppedFrames,
      presenterDroppedTotal: video.presentationDroppedFrames,
      // Portal has no clock-synchronized capture-to-delivery measurement yet.
      // IPC send duration is a different boundary and must not be mislabeled.
      transportDeliveryP95Ms: null,
      decodeLatencyP95Ms: video.decodePercentiles.p95,
      presentationLatencyP95Ms: video.presentPercentiles.p95,
      resyncActive: video.recovering || transport.frontendResyncStats?.active === true,
    });
  }

  return Object.freeze({
    prepareConnection,
    openFrameSession,
    activateFrameSession,
    closeFrameSession,
    prepareDisconnect,
    teardown,
    requestKeyframe,
    handleBinaryFrame,
    handleFrameStats,
    handleAdaptiveDecision,
    handleAdaptiveFeedbackState,
    handleFrameError,
    snapshot,
    publishAdaptiveFeedback,
    get generation() { return activeFrameGeneration; },
    get activeSession() { return activeFrameSession; },
  });
}
