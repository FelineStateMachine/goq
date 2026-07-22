import {
  HeldInputState,
  MAX_HELD_KEYS,
  MAX_HELD_MOUSE_BUTTONS,
  PointerMotionBuffer,
  browserPointerLockLossRequiresControlExit,
  mapCanvasPointToSurface,
  resolvePointerSurfaceSize,
  restoreRejectedPointerMotion,
  scaleRelativePointerDelta,
  validatePointerSurfaceDimensions,
  validatePointerPositionFeedback,
  browserMouseButtonCode,
} from './input-state.mjs';
import { parseFrameEnvelope } from './frame-envelope.mjs';
import {
  formatVideoDiscardTelemetry,
  isCurrentFrameGeneration,
  normalizeFrameStatsPayload,
} from './frame-stats.mjs';
import { networkDiagnosticsPresentation } from './network-diagnostics.mjs';
import {
  FRAME_CHANNEL_CAPACITY,
  activateFrameSession,
  isActiveFrameSession,
  newFrameSession,
  stageFrameAcknowledgment,
  stageLegacyFrame,
} from './frame-session.mjs';
import { newPointerSession, parsePointerFeedbackMessage } from './pointer-feedback.mjs';
import { newConnectionState } from './connection-state.mjs';
import {
  audioMinusVideoSkewMs,
  audioOutputTimelineFromStats,
  exactMediaTimestampMicros,
  formatSignedMilliseconds,
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
import { audioButtonPresentation, newAudioSession } from './audio-ui.mjs';
import {
  ControllerActivationGate,
  ControllerActionRepeater,
  GamepadEscapeHold,
  LatestControllerStatePublisher,
  chooseDirectionalIndex,
  controllerStateSignature,
  disconnectedControllerState,
  maskGamepadEscapeChord,
  neutralGamepadInputState,
  normalizeGamepad,
  selectPreferredController,
  toGamepadInputState,
} from './controller-state.mjs';
import {
  constrainStreamWindowResize,
  fitInitialStreamWindow,
  fitStreamSurface,
  streamAspectKey,
} from './window-geometry.mjs';
import {
  createVideoPipelineSession,
} from './video-pipeline.mjs';
import {
  AdaptiveFeedbackPublisher,
  formatAdaptiveDecision,
  normalizeAdaptiveDecisionEnvelope,
} from './adaptive-feedback.mjs';
import {
  formatInvitationExpiry,
  grantLabel,
  normalizeInvitationSummary,
  shortPeerFingerprint,
} from './enrollment.mjs';
import { mapKey } from './keyboard-map.mjs';

let controlMode = false;
let enrollmentReady = false;
let pendingInvitationSummary = null;
let currentEnrollmentStatus = null;
let fittedStreamAspect = null;
let pendingStreamFit = null;
let lastObservedWindowSize = null;
let streamWindowResizeTimer = null;
let activeFrameChannel = null;
let activeFrameGeneration = null;
let activeFrameSession = null;
let activePointerSession = null;
let activeAudioSession = newAudioSession();
let controllerActivationInProgress = false;
let controlTransitionInProgress = false;
let controlTransitionGeneration = 0;
let nativeCursorCommand = Promise.resolve();
let browserPointerLockRequired = true;
const controllerActivationGate = new ControllerActivationGate();

const AUDIO_DECODER_CONFIG = Object.freeze({
  codec: 'opus',
  sampleRate: 48000,
  numberOfChannels: 2,
});
const MAX_AUDIO_DECODE_QUEUE_SIZE = 3;

function applyPointerPositionFeedback(message, session) {
  if (session !== activePointerSession || !connectionState.inputCapabilities.pointerPositionFeedback) return;
  const surface = currentPointerSurfaceSize();
  if (surface === null) return;
  const feedback = validatePointerPositionFeedback(message, surface);
  session.remotePosition = feedback.position;
  session.remoteVisible = feedback.pointer_visible;
  renderRemotePointer();
}

function handlePointerPositionFeedback(message, session) {
  if (session !== activePointerSession || session.failed || session.closing) return;
  try {
    const envelope = parsePointerFeedbackMessage(message);
    if (envelope.type === 'terminal') {
      session.failed = true;
      session.failureDetail = envelope.reason === 'eof'
        ? 'Pointer feedback ended'
        : 'Pointer feedback was malformed';
      session.remotePosition = null;
      session.remoteVisible = false;
      renderRemotePointer();
      console.error(session.failureDetail);
      if (connectionState.connected) void disconnect();
      return;
    }
    const feedback = envelope.feedback;
    session.received = true;
    session.latest = feedback;
    if (connectionState.connected) applyPointerPositionFeedback(feedback, session);
  } catch (error) {
    session.failed = true;
    session.failureDetail = `Pointer feedback failed: ${error}`;
    session.remotePosition = null;
    session.remoteVisible = false;
    renderRemotePointer();
    console.error('invalid pointer-position feedback:', error);
    if (connectionState.connected) void disconnect();
  }
}

const invoke = (...args) => window.__TAURI__.core.invoke(...args);
const listen = (...args) => window.__TAURI__.event.listen(...args);
const adaptiveFeedbackPublisher = new AdaptiveFeedbackPublisher({ invokeCommand: invoke });
let adaptiveFeedbackAvailable = false;
let adaptiveFeedbackError = null;
let adaptiveDecision = null;

function updatePanelVisibility() {
  const streamSection = document.getElementById('stream-section');
  streamSection.style.display = connectionState.connected ? '' : 'none';
}

// ─── Status ──────────────────────────────────────────────────────────────────
function setStatus(state, text) {
  const line = document.getElementById('status-line');
  const dot = document.getElementById('status-dot');
  const txt = document.getElementById('status-text');
  line.className = 'status-line ' + state;
  dot.className = 'dot ' + state;
  txt.textContent = text;
}

function showIntro() {
  document.getElementById('intro').classList.remove('hidden');
  setTimeout(() => {
    const target = connectionState.developmentMode
      ? document.getElementById('intro-connect')
      : document.getElementById('intro-pin');
    target?.focus();
  }, 50);
}
function hideIntro() { document.getElementById('intro').classList.add('hidden'); }
function showTap() {
  document.getElementById('tap-overlay-title').textContent = 'tap key';
  document.getElementById('tap-overlay-desc').textContent = 'Touch the sensor on your key when it blinks.';
  document.getElementById('tap-status').textContent = 'waiting...';
  document.getElementById('tap-overlay').classList.remove('hidden');
}
function hideTap() { document.getElementById('tap-overlay').classList.add('hidden'); }

function hideEnrollment() { document.getElementById('enrollment-overlay').classList.add('hidden'); }
function showEnrollment() {
  if (!document.getElementById('invitation-overlay').classList.contains('hidden')) return;
  hideIntro();
  document.getElementById('enrollment-overlay').classList.remove('hidden');
  setTimeout(() => document.getElementById('enrollment-pin')?.focus(), 50);
}

function showInvitationSummary(value) {
  const summary = normalizeInvitationSummary(value);
  pendingInvitationSummary = summary;
  hideEnrollment();
  document.getElementById('invitation-host').textContent = summary.hostFingerprint;
  document.getElementById('invitation-peer').textContent = summary.peerFingerprint;
  document.getElementById('invitation-expiry').textContent = formatInvitationExpiry(summary.expiresAtUnix);
  document.getElementById('invitation-grants').textContent = summary.grants.map(grantLabel).join(' · ');
  document.getElementById('invitation-status').textContent = '';
  document.getElementById('invitation-overlay').classList.remove('hidden');
  setTimeout(() => setControllerFocus(document.getElementById('confirm-invitation')), 0);
}

async function refreshEnrollment() {
  if (connectionState.developmentMode) {
    enrollmentReady = true;
    hideEnrollment();
    showIntro();
    return true;
  }
  const status = await invoke('portal_enrollment_status');
  currentEnrollmentStatus = status;
  enrollmentReady = status.enrolled === true;
  document.getElementById('reset-enrollment').classList.toggle('hidden', !enrollmentReady);
  document.getElementById('reset-enrollment-intro').classList.toggle('hidden', !enrollmentReady);
  if (status.pending) {
    showInvitationSummary(status.pending);
    return enrollmentReady;
  }
  if (enrollmentReady) {
    hideEnrollment();
    showIntro();
  } else {
    showEnrollment();
  }
  return enrollmentReady;
}

function openEnrollmentReset() {
  if (!currentEnrollmentStatus?.enrolled || !currentEnrollmentStatus.host_node_id) return;
  togglePanel(false);
  document.getElementById('reset-enrollment-host').textContent =
    shortPeerFingerprint(currentEnrollmentStatus.host_node_id);
  document.getElementById('reset-enrollment-status').textContent = '';
  document.getElementById('reset-enrollment-overlay').classList.remove('hidden');
  setTimeout(() => setControllerFocus(document.getElementById('confirm-reset-enrollment')), 0);
}

function cancelEnrollmentReset() {
  document.getElementById('reset-enrollment-overlay').classList.add('hidden');
}

async function confirmEnrollmentReset() {
  const hostNodeId = currentEnrollmentStatus?.host_node_id;
  if (!hostNodeId) return;
  const status = document.getElementById('reset-enrollment-status');
  status.className = 'overlay-status pending';
  status.textContent = 'erasing enrollment';
  try {
    await invoke('portal_reset_enrollment', { expectedHostNodeId: hostNodeId });
    enrollmentReady = false;
    currentEnrollmentStatus = null;
    document.getElementById('reset-enrollment').classList.add('hidden');
    document.getElementById('reset-enrollment-intro').classList.add('hidden');
    document.getElementById('reset-enrollment-overlay').classList.add('hidden');
    showEnrollment();
  } catch (error) {
    status.className = 'overlay-status err';
    status.textContent = String(error);
  }
}

async function derivePortalIdentity() {
  const pin = document.getElementById('enrollment-pin').value.trim();
  const status = document.getElementById('enrollment-status');
  if (!pin) {
    status.className = 'overlay-status err';
    status.textContent = 'enter pin';
    return;
  }
  status.className = 'overlay-status pending';
  status.textContent = 'tap the security key';
  showTap();
  try {
    const result = await invoke('key_derive_identity', { pin });
    if (result.error || !result.node_id) throw new Error(result.error || 'identity derivation failed');
    document.getElementById('enrollment-peer-id').textContent = result.node_id;
    document.getElementById('enrollment-peer-wrap').classList.remove('hidden');
    status.className = 'overlay-status ok';
    status.textContent = `portal ${shortPeerFingerprint(result.node_id)} ready for an invitation`;
  } catch (error) {
    status.className = 'overlay-status err';
    status.textContent = String(error);
  } finally {
    hideTap();
  }
}

async function chooseInvitationFile() {
  const status = document.getElementById('enrollment-status');
  try {
    const path = await window.__TAURI__.dialog.open({
      multiple: false,
      directory: false,
      filters: [{ name: 'goq invitation', extensions: ['goq-invite'] }],
    });
    if (!path) return;
    const summary = await invoke('portal_import_invitation_file', { path });
    showInvitationSummary(summary);
  } catch (error) {
    status.className = 'overlay-status err';
    status.textContent = String(error);
  }
}

async function confirmInvitation() {
  if (!pendingInvitationSummary) return;
  const status = document.getElementById('invitation-status');
  status.className = 'overlay-status pending';
  status.textContent = 'saving enrollment';
  try {
    await invoke('portal_confirm_invitation');
    pendingInvitationSummary = null;
    document.getElementById('invitation-overlay').classList.add('hidden');
    await refreshEnrollment();
  } catch (error) {
    status.className = 'overlay-status err';
    status.textContent = String(error);
  }
}

async function cancelInvitation() {
  try { await invoke('portal_cancel_invitation'); } catch (error) { console.warn(error); }
  pendingInvitationSummary = null;
  document.getElementById('invitation-overlay').classList.add('hidden');
  showEnrollment();
}

async function checkDevelopmentConnectionMode() {
  const mode = await invoke('development_connection_mode');
  connectionState.setDevelopmentMode(mode.enabled);
  if (!mode.enabled) return;

  const badge = document.getElementById('dev-connect-badge');
  badge.classList.remove('hidden');
  badge.title = mode.warning;
  document.getElementById('dev-connect-warning').classList.remove('hidden');
  document.getElementById('intro-desc').textContent = 'development direct-node routing · passkey lookup skipped · not client authorization';

  for (const id of ['intro-pin', 'pin-input']) {
    const input = document.getElementById(id);
    input.value = '';
    input.disabled = true;
    input.style.display = 'none';
  }
  document.getElementById('controller-pin-intro').style.display = 'none';
  document.getElementById('controller-pin-main').style.display = 'none';
  document.getElementById('pin-label').classList.add('hidden');
}

// ─── Intro actions ────────────────────────────────────────────────────────────
async function introConnect() {
  if (connectionState.connecting || connectionState.connected) return;
  if (!connectionState.developmentMode && !enrollmentReady) {
    showEnrollment();
    return;
  }
  const pin = document.getElementById('intro-pin').value.trim();
  const status = document.getElementById('intro-status');
  if (!connectionState.developmentMode && !pin) {
    status.className = 'overlay-status err';
    status.textContent = 'enter pin';
    return;
  }
  hideIntro();
  if (!connectionState.developmentMode) showTap();
  document.getElementById('pin-input').value = pin;
  await connectHost();
  if (!connectionState.developmentMode) hideTap();
  if (!connectionState.connected) {
    showIntro();
    status.className = 'overlay-status err';
    status.textContent = 'connection failed';
  }
}

// ─── Client ───────────────────────────────────────────────────────────────────
function updateAudioUI() {
  const session = activeAudioSession;
  const toggle = document.getElementById('audio-toggle');
  const status = document.getElementById('stream-audio-state');
  const webviewReceived = document.getElementById('stream-audio-webview-received-packets');
  const transportMissing = document.getElementById('stream-audio-transport-missing-packets');
  const ipcDropped = document.getElementById('stream-audio-ipc-dropped-packets');
  const decoderDropped = document.getElementById('stream-audio-decoder-dropped-packets');
  const pcmHandoffDropped = document.getElementById('stream-audio-pcm-handoff-dropped-blocks');
  const ringOverflow = document.getElementById('stream-audio-ring-overflow-ms');
  const ringRecoveryDiscarded = document.getElementById('stream-audio-ring-recovery-discarded-ms');
  const ringBuffered = document.getElementById('stream-audio-ring-buffer-ms');
  const underruns = document.getElementById('stream-audio-underruns');
  const silence = document.getElementById('stream-audio-silence-ms');
  if (toggle) {
    const presentation = audioButtonPresentation({
      available: session.available,
      muted: session.muted,
      state: session.state,
      detail: session.stateDetail,
    });
    toggle.textContent = presentation.glyph;
    toggle.classList.toggle('active', session.available && !session.muted);
    toggle.classList.toggle('disabled', !session.available);
    toggle.setAttribute('aria-disabled', session.available ? 'false' : 'true');
    toggle.setAttribute('aria-label', presentation.ariaLabel);
    toggle.title = presentation.title;
  }
  if (status) {
    status.textContent = session.muted && session.available ? 'muted' : session.state;
    status.title = session.stateDetail;
  }
  if (webviewReceived) webviewReceived.textContent = `${session.packetsReceived} packets`;
  if (transportMissing) transportMissing.textContent = `${session.transportDropped} packets`;
  if (ipcDropped) ipcDropped.textContent = `${session.frontendDropped} packets`;
  if (decoderDropped) decoderDropped.textContent = `${session.decoderDropped} packets`;
  if (pcmHandoffDropped) {
    pcmHandoffDropped.textContent = `${session.messageTracker.droppedMessages} blocks`;
  }
  if (ringOverflow) {
    ringOverflow.textContent = `${(session.workletDroppedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (ringRecoveryDiscarded) {
    ringRecoveryDiscarded.textContent = `${(session.workletRecoveryDiscardedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (ringBuffered) {
    ringBuffered.textContent = `${(session.bufferedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (underruns) {
    underruns.textContent = `${session.underflows} events / ${(session.underflowDurationMicros / 1000).toFixed(1)} ms`;
  }
  if (silence) silence.textContent = `${(session.silentDurationMicros / 1000).toFixed(1)} ms`;
}

function setAudioState(state, detail = state) {
  activeAudioSession.state = state;
  activeAudioSession.stateDetail = detail;
  updateAudioUI();
}

function resetAudioTelemetry(session = activeAudioSession) {
  session.packetsReceived = 0;
  session.decoderDropped = 0;
  session.bufferedFrames = 0;
  session.underflows = 0;
  session.underflowDurationMicros = 0;
  session.silentDurationMicros = 0;
  session.transportDropped = 0;
  session.frontendDropped = 0;
  videoPipeline.resetAvSync(session);
  updateAudioUI();
}

function configureAudioDecoder(session = activeAudioSession) {
  if (session.decoder) {
    try { session.decoder.close(); } catch (_) {}
  }
  session.decoder = new AudioDecoder({
    output: (audioData) => {
      try {
        if (session !== activeAudioSession || session.failed) return;
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
          updateAudioUI();
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
        disableAudioForSession(`Decoded audio output failed: ${error}`, session);
      } finally {
        audioData.close();
      }
    },
    error: (error) => {
      disableAudioForSession(`Opus decoder failed: ${error}`, session);
    },
  });
  session.decoder.configure(AUDIO_DECODER_CONFIG);
}

function primeAudioOutputForActivation() {
  const session = activeAudioSession;
  const AudioContextConstructor = window.AudioContext || window.webkitAudioContext;
  if (typeof AudioContextConstructor !== 'function') return;
  if (!session.context || session.context.state === 'closed') {
    try {
      session.context = new AudioContextConstructor({ latencyHint: 'interactive', sampleRate: 48000 });
    } catch (_) {
      return;
    }
  }
  // This function is deliberately synchronous and is called before the first
  // await in a click/keyboard connect handler, preserving WebKit user activation.
  void session.context.resume().catch(() => {});
}

async function initializeAudioPipeline(session) {
  if (session.decoder) {
    try { session.decoder.close(); } catch (_) {}
    session.decoder = null;
  }
  if (session.workletNode) {
    try { session.workletNode.disconnect(); } catch (_) {}
    session.workletNode = null;
  }
  session.messageTracker.clear();
  if (typeof AudioDecoder !== 'function') {
    return { supported: false, error: 'WebCodecs AudioDecoder is unavailable' };
  }
  if (typeof AudioWorkletNode !== 'function') {
    return { supported: false, error: 'AudioWorklet is unavailable' };
  }
  let support;
  try {
    support = await AudioDecoder.isConfigSupported(AUDIO_DECODER_CONFIG);
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
    if (session.context.sampleRate !== 48000) {
      throw new Error(`audio output opened at ${session.context.sampleRate} Hz instead of 48000 Hz`);
    }
    await session.context.audioWorklet.addModule(new URL('./audio-worklet.js', import.meta.url));
    session.workletNode = new AudioWorkletNode(session.context, 'sigil-audio-processor', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [2],
    });
    session.workletNode.port.onmessage = (event) => {
      if (event.data?.type === 'accepted') {
        session.messageTracker.accept(event.data.id);
        if (session === activeAudioSession) updateAudioUI();
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
        if (session !== activeAudioSession) return;
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
          setAudioState(event.data.started ? 'playing' : 'priming', 'bounded Opus playback');
        } else {
          updateAudioUI();
        }
      } else if (event.data?.type === 'error') {
        disableAudioForSession(`AudioWorklet failed: ${event.data.error}`, session);
      }
    };
    session.workletNode.port.onmessageerror = () => {
      disableAudioForSession('AudioWorklet returned an unreadable message', session);
    };
    session.workletNode.onprocessorerror = () => {
      disableAudioForSession('AudioWorklet processor stopped unexpectedly', session);
    };
    session.workletNode.connect(session.context.destination);
    session.workletNode.port.postMessage({ type: 'mute', muted: session.muted });
    configureAudioDecoder(session);
    await session.context.resume();
    setAudioState(
      session.context.state === 'running' ? 'negotiating' : 'blocked',
      session.context.state === 'running'
        ? 'Opus output ready'
        : 'WebKit suspended audio output; activate the audio button to retry',
    );
    return { supported: true, error: null };
  } catch (error) {
    await teardownAudioPipeline(false);
    return { supported: false, error: `Audio initialization failed: ${error}` };
  }
}

async function teardownAudioPipeline(resetStatus = true) {
  const session = activeAudioSession;
  videoPipeline.resetAvSync(session);
  const decoder = session.decoder;
  const workletNode = session.workletNode;
  const context = session.context;
  // Replace the active owner before closing resources so delayed callbacks
  // cannot mutate the disconnected UI or a successor session. Seed the next
  // dormant session with the user's mute preference across reconnects.
  activeAudioSession = newAudioSession({ muted: session.muted });
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
  if (resetStatus) updateAudioUI();
}

async function toggleAudioMute() {
  const session = activeAudioSession;
  if (!session.available) return;
  session.muted = !session.muted;
  try {
    if (!session.muted && session.context?.state !== 'running') await session.context?.resume();
    session.workletNode?.port.postMessage({ type: 'mute', muted: session.muted });
    if (!session.muted && session.context?.state !== 'running') {
      setAudioState('blocked', 'WebKit suspended audio output; click the audio button to retry');
    } else {
      updateAudioUI();
    }
  } catch (error) {
    setAudioState('error', `Audio output activation failed: ${error}`);
  }
}

function prepareAudioForConnection() {
  activeAudioSession = newAudioSession({ muted: activeAudioSession.muted });
  activeAudioSession.acceptsNativeEvents = true;
  // Keep this synchronous and before createConnectionAttempt's first await so
  // WebKit observes the originating click/keyboard user activation.
  primeAudioOutputForActivation();
}

async function createConnectionAttempt({ createChannel: Channel }) {
  // Reset before opening the channel so early frames cannot race a
  // post-connect reset and disappear from the live diagnostics.
  if (activeFrameSession) activeFrameSession.closing = true;
  activeFrameSession = null;
  activeFrameGeneration = null;
  adaptiveFeedbackPublisher.stop();
  adaptiveFeedbackAvailable = false;
  adaptiveFeedbackError = null;
  adaptiveDecision = null;
  resetStreamTelemetry();
  if (typeof Channel !== 'function') throw new Error('Tauri binary channels are unavailable');
  const audioSession = activeAudioSession;
  resetAudioTelemetry(audioSession);
  const frameSession = newFrameSession();
  const pointerSession = newPointerSession();
  activeFrameSession = frameSession;
  activePointerSession = pointerSession;
  const audioSupport = await initializeAudioPipeline(audioSession);
  if (!audioSupport.supported) setAudioState('unavailable', audioSupport.error);
  activeFrameChannel = new Channel((message) => handleBinaryFrameMessage(message, frameSession));
  audioSession.channel = new Channel((message) => handleBinaryAudioMessage(message, audioSession));
  pointerSession.channel = new Channel(
    (message) => handlePointerPositionFeedback(message, pointerSession),
  );
  return {
    audioSession,
    frameSession,
    pointerSession,
    audioSupport,
    connectionArgs: {
      frameChannel: activeFrameChannel,
      audioChannel: audioSession.channel,
      pointerChannel: pointerSession.channel,
      audioSupported: audioSupport.supported,
    },
  };
}

function validateConnectedAttempt(result, attempt) {
  if (attempt.pointerSession.failed) {
    throw new Error(attempt.pointerSession.failureDetail || 'host returned invalid pointer-position feedback');
  }
  if (attempt.frameSession.failed) {
    throw new Error(attempt.frameSession.failureDetail || 'host returned invalid frame data');
  }
  attempt.pointerSession.surfaceDimensions = result.connected
    ? validatePointerSurfaceDimensions(result.pointer_surface_dimensions)
    : null;
  attempt.pointerFeedbackAvailable = result.connected
    && result.pointer_position_feedback_available === true;
  if (attempt.pointerFeedbackAvailable && attempt.pointerSession.surfaceDimensions === null) {
    throw new Error('host offered pointer feedback without a native pointer surface');
  }
  attempt.initialPointerFeedback = attempt.pointerFeedbackAvailable && attempt.pointerSession.received
    ? validatePointerPositionFeedback(
      attempt.pointerSession.latest,
      attempt.pointerSession.surfaceDimensions,
    )
    : null;
}

function activateConnectedAttempt(result, attempt) {
  activateConnectedFrameSession(attempt.frameSession, result.media_generation);
  adaptiveFeedbackAvailable = result.adaptive_feedback_available === true;
  adaptiveFeedbackError = typeof result.adaptive_feedback_error === 'string'
    ? result.adaptive_feedback_error : null;
  adaptiveFeedbackPublisher.start(result.media_generation, adaptiveFeedbackAvailable);
}

function closeConnectionAttempt(attempt, { committed = false } = {}) {
  if (attempt?.frameSession) attempt.frameSession.closing = true;
  if (activeFrameSession === attempt?.frameSession) activeFrameSession = null;
  activeFrameGeneration = null;
  adaptiveFeedbackPublisher.stop();
  adaptiveFeedbackAvailable = false;
  adaptiveDecision = null;
  if (committed && attempt?.pointerSession) attempt.pointerSession.closing = true;
}

async function teardownConnectionAttempt(attempt) {
  activeFrameChannel = null;
  const pointerSession = attempt?.pointerSession ?? activePointerSession;
  if (activePointerSession === pointerSession) activePointerSession = null;
  if (pointerSession) pointerSession.channel = null;
  if (attempt?.audioSession && attempt.audioSession !== activeAudioSession) {
    attempt.audioSession.channel = null;
  }
  await teardownAudioPipeline();
}

function updateAcceptedConnectionState({ attempt, state }) {
  const session = attempt.pointerSession;
  if (!state.inputCapabilities.pointerPositionFeedback) {
    session.remotePosition = null;
    session.remoteVisible = false;
  } else if (attempt.initialPointerFeedback !== null) {
    session.remotePosition = attempt.initialPointerFeedback.position;
    session.remoteVisible = attempt.initialPointerFeedback.pointer_visible;
  }
  controlMode = false;
  clearPendingInput();
}

async function showConnectedAttempt({ result, attempt }) {
  const { audioSession, audioSupport } = attempt;
  audioSession.expectedGeneration = Number.isSafeInteger(result.audio_generation)
    && result.audio_generation > 0
    ? result.audio_generation : null;
  const pendingAudioTerminal = takeAudioTerminalState(
    audioSession.pendingTerminalStates,
    audioSession.expectedGeneration,
  );
  if (pendingAudioTerminal !== null) {
    audioSession.failed = true;
    audioSession.failureDetail = pendingAudioTerminal.error || 'Audio connection ended';
  }
  audioSession.available = result.audio_available === true;
  if (audioSession.available && audioSession.failed) {
    disableAudioForSession(audioSession.failureDetail || 'Audio output failed', audioSession);
  } else if (audioSession.available) {
    setAudioState(
      audioSession.context?.state === 'running' ? 'priming' : 'blocked',
      audioSession.context?.state === 'running'
        ? 'Waiting for bounded Opus prebuffer'
        : 'WebKit suspended audio output; activate the audio button to retry',
    );
  } else {
    await teardownAudioPipeline(false);
    setAudioState('unavailable', result.audio_error || audioSupport.error || 'host audio unavailable');
  }
  updatePanelVisibility();
  document.getElementById('node-id-text').textContent = result.host_node_id.substring(0, 16) + '...';
  document.getElementById('frame-canvas').style.display = 'block';
  document.getElementById('placeholder').classList.add('hidden');
  document.getElementById('control-bar').classList.add('visible');
  void fitWindowToIncomingStream();
  updateControlUI();
  // Land controller users on the deliberate view-only/control boundary.
  // The connect button is hidden at this point, so retaining its focus
  // would make the next A press fall back to an unrelated top-bar action.
  setTimeout(() => setControllerFocus(document.getElementById('control-toggle')), 0);
  document.getElementById('action-connect').style.display = 'none';
  document.getElementById('action-disconnect').textContent = 'disconnect';
  document.getElementById('action-disconnect').classList.remove('hidden');
  document.getElementById('pin-label').style.display = 'none';
  document.getElementById('pin-input').style.display = 'none';
  document.getElementById('controller-pin-main').style.display = 'none';
}

async function prepareDisconnect() {
  if (activeFrameSession) activeFrameSession.closing = true;
  activeFrameSession = null;
  activeFrameGeneration = null;
  adaptiveFeedbackPublisher.stop();
  adaptiveFeedbackAvailable = false;
  adaptiveFeedbackError = null;
  adaptiveDecision = null;
  controlTransitionGeneration += 1;
  controlTransitionInProgress = false;
  controllerActivationGate.reset();
  queueNeutralControllerState();
  releaseHeldInputs();
  controlMode = false;
  releasePointerLock();
  updateControlUI();
  // Give release commands a short, finite opportunity to cross the bounded
  // Tauri queue before closing it. A broken stream cannot stall disconnect.
  await waitForInputDrain(250);
}

async function teardownConnectedResources() {
  activeFrameChannel = null;
  const pointerSession = activePointerSession;
  activePointerSession = null;
  if (pointerSession) pointerSession.channel = null;
  await teardownAudioPipeline();
  videoPipeline.teardown();
}

async function showDisconnectedState() {
  controlMode = false;
  fittedStreamAspect = null;
  pendingStreamFit = null;
  lastObservedWindowSize = null;
  if (streamWindowResizeTimer !== null) clearTimeout(streamWindowResizeTimer);
  streamWindowResizeTimer = null;
  clearPendingInput();
  updateControlUI();
  updatePanelVisibility();
  setStatus('', 'idle');
  document.getElementById('frame-canvas').style.display = 'none';
  document.getElementById('placeholder').classList.remove('hidden');
  document.getElementById('placeholder').textContent = 'no stream';
  document.getElementById('control-bar').classList.remove('visible');
  document.getElementById('action-connect').style.display = '';
  document.getElementById('action-disconnect').classList.add('hidden');
  if (!connectionState.developmentMode) {
    document.getElementById('pin-label').style.display = '';
    document.getElementById('pin-input').style.display = '';
    document.getElementById('controller-pin-main').style.display = '';
  }
  document.getElementById('intro-status').textContent = '';
}

const connectionState = newConnectionState({
  invokeCommand: invoke,
  createChannel: window.__TAURI__.core.Channel,
  beforeConnect: prepareAudioForConnection,
  createAttempt: createConnectionAttempt,
  validateConnectedResult: validateConnectedAttempt,
  activateAttempt: activateConnectedAttempt,
  closeAttempt: closeConnectionAttempt,
  teardownAttempt: teardownConnectionAttempt,
  beforeDisconnect: prepareDisconnect,
  teardownConnection: teardownConnectedResources,
  onStatus: (state, detail, context) => {
    if (state === 'ok') updateAcceptedConnectionState(context);
    setStatus(state, detail);
  },
  onConnected: showConnectedAttempt,
  onDisconnected: showDisconnectedState,
  onFailure: ({ error }) => console.error('connection failed:', error),
  onDisconnectError: (error) => console.error(error),
  onNativeDisconnectError: (error) => console.error('failed to close rejected connection:', error),
});

async function connectHost() {
  await connectionState.connect({ pin: document.getElementById('pin-input').value });
}

async function disconnect() {
  await connectionState.disconnect();
}

// ─── FIDO scan ────────────────────────────────────────────────────────────────
async function scanFido() {
  try {
    const info = await invoke('fido_device_info');
    const el = document.getElementById('fido-status');
    if (!info.found) {
      el.textContent = 'no device';
      el.style.color = 'var(--muted-fg)';
      return;
    }
    el.textContent = info.error ? `found (${info.error})` : 'connected';
    el.style.color = info.error ? 'var(--yellow)' : 'var(--green)';
    document.getElementById('fido-product').textContent = info.product || '—';
    document.getElementById('fido-vidpid').textContent =
      `${info.vid.toString(16).padStart(4,'0')}:${info.pid.toString(16).padStart(4,'0')}`;
    document.getElementById('fido-versions').textContent = info.versions.join(', ') || '—';
    document.getElementById('fido-extensions').textContent = info.extensions.join(', ') || '—';
    document.getElementById('fido-pin').textContent = info.pin_retries ?? '—';
  } catch (e) {
    console.error('scanFido failed:', e);
    document.getElementById('fido-status').textContent = 'error';
  }
}

// ─── Panel ────────────────────────────────────────────────────────────────────
function togglePanel(force) {
  const panel = document.getElementById('panel');
  const open = typeof force === 'boolean' ? force : !panel.classList.contains('visible');
  panel.classList.toggle('visible', open);
  document.getElementById('panel-overlay').classList.toggle('visible', open);
  if (open) setTimeout(() => document.getElementById('panel-close').focus(), 0);
}

// ─── WebCodecs detection ──────────────────────────────────────────────────────
let hasWebCodecs = ('VideoDecoder' in window);

(async function detectWebCodecs() {
  await invoke('set_webcodecs_available', { available: hasWebCodecs });
})();

// ─── Frame decoding ───────────────────────────────────────────────────────────
let transportDroppedFrames = 0;
let transportObjectDroppedFrames = null;
let transportLateObjectDroppedFrames = null;
let frontendDroppedFrames = 0;
let frontendQueueDroppedFrames = null;
let frontendResyncDroppedFrames = null;
let frontendQueueStats = null;
let frontendResyncStats = null;
let transportIntervalStats = null;
let frontendIpcSendDurationStats = null;
let rustTimingWindow = null;
let networkDiagnostics = null;
let lastTransportFps = 0;
let lastFrontendSendFps = 0;
let streamPathMode = 'unknown';
let streamRttMs = null;
let lastMediaSequence = null;

function requestDecoderKeyframe(reason) {
  if (!Number.isSafeInteger(activeFrameGeneration) || activeFrameGeneration < 1) return;
  void invoke('iroh_client_request_keyframe', {
    generation: activeFrameGeneration,
    reason,
  }).catch((error) => {
    console.warn(`keyframe request failed (${reason}):`, error);
  });
}

function resetAudioSyncTelemetry(session = activeAudioSession, { recoverRing = false } = {}) {
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

function sampleAvSkew(videoPtsMicros, presentationTimeMs) {
  const session = activeAudioSession;
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

const canvas = document.getElementById('frame-canvas');
const remotePointer = document.getElementById('remote-pointer');
const ctx = canvas.getContext('2d');
const videoPipeline = createVideoPipelineSession({
  hasWebCodecs,
  canvas,
  context: ctx,
  requestKeyframe: requestDecoderKeyframe,
  resetAudioSync: resetAudioSyncTelemetry,
  sampleAudioSkew: sampleAvSkew,
  onFormatChanged: () => {
    sizeCanvasToIncomingStream();
    if (connectionState.connected) void fitWindowToIncomingStream();
  },
});

function resetStreamTelemetry() {
  videoPipeline.reset();
  transportDroppedFrames = 0;
  transportObjectDroppedFrames = null;
  transportLateObjectDroppedFrames = null;
  frontendDroppedFrames = 0;
  frontendQueueDroppedFrames = null;
  frontendResyncDroppedFrames = null;
  frontendQueueStats = null;
  frontendResyncStats = null;
  transportIntervalStats = null;
  frontendIpcSendDurationStats = null;
  rustTimingWindow = null;
  networkDiagnostics = null;
  lastTransportFps = 0;
  lastFrontendSendFps = 0;
  streamPathMode = 'unknown';
  streamRttMs = null;
  lastMediaSequence = null;
  adaptiveDecision = null;
}

function formatCadence(summary) {
  if (!summary.count) return '—';
  return `${summary.p50.toFixed(1)} / ${summary.p95.toFixed(1)} / ${summary.p99.toFixed(1)} / ${summary.max.toFixed(1)} ms · >25/>33 ${summary.over25Ms}/${summary.over33Ms}`;
}

function formatRustDurationSummary(summary) {
  if (!summary) return '—';
  if (!summary.count) return '— · 0 samples';
  return `${summary.p50Ms.toFixed(2)} / ${summary.p95Ms.toFixed(2)} / ${summary.maxMs.toFixed(2)} ms · ${summary.count} samples`;
}

function updateStreamStats() {
  const now = performance.now();
  const video = videoPipeline.snapshot(now);
  const networkPresentation = networkDiagnosticsPresentation(networkDiagnostics);
  document.getElementById('stream-received').textContent = video.receivedFrames;
  document.getElementById('stream-decoder-input').textContent = video.decoderInputFrames;
  document.getElementById('stream-decoder-output').textContent = video.decoderOutputFrames;
  document.getElementById('stream-presented').textContent = video.presentedFrames;
  document.getElementById('stream-decode-queue').textContent = video.decoderQueueDepth;
  document.getElementById('stream-present-queue').textContent = video.presenterQueueDepth;
  const discardTelemetry = formatVideoDiscardTelemetry({
    transportDroppedFrames,
    frontendDroppedFrames,
    decoderDroppedFrames: video.droppedFrames,
    presenterOverwrittenFrames: video.presentationDroppedFrames,
  });
  document.getElementById('stream-dropped').textContent = discardTelemetry.total;
  document.getElementById('stream-transport-dropped').textContent = discardTelemetry.transport;
  document.getElementById('stream-transport-object-dropped').textContent =
    transportObjectDroppedFrames === null ? '—' : `${transportObjectDroppedFrames} frames`;
  document.getElementById('stream-transport-late-object-dropped').textContent =
    transportLateObjectDroppedFrames === null
      ? '—' : `${transportLateObjectDroppedFrames} frames`;
  document.getElementById('stream-frontend-dropped').textContent = discardTelemetry.frontend;
  document.getElementById('stream-frontend-queue-dropped').textContent = frontendQueueDroppedFrames === null
    ? '—' : `${frontendQueueDroppedFrames} frames`;
  document.getElementById('stream-frontend-resync-dropped').textContent = frontendResyncDroppedFrames === null
    ? '—' : `${frontendResyncDroppedFrames} frames`;
  document.getElementById('stream-frontend-queue').textContent = frontendQueueStats === null
    ? '—'
    : `${frontendQueueStats.depth} current / ${frontendQueueStats.peak} peak / ${frontendQueueStats.capacity} frames capacity`;
  document.getElementById('stream-frontend-resync').textContent = frontendResyncStats === null
    ? '—'
    : `${frontendResyncStats.episodes} ${frontendResyncStats.episodes === 1 ? 'episode' : 'episodes'} · ${frontendResyncStats.active ? 'active' : 'idle'} · total ${frontendResyncStats.totalMs.toFixed(1)} ms · current ${frontendResyncStats.active ? `${frontendResyncStats.currentMs.toFixed(1)} ms` : '—'} · max ${frontendResyncStats.maxMs.toFixed(1)} ms`;
  document.getElementById('stream-transport-interval').textContent = formatRustDurationSummary(transportIntervalStats);
  document.getElementById('stream-ipc-send-duration').textContent = formatRustDurationSummary(frontendIpcSendDurationStats);
  document.getElementById('stream-rust-timing-window').textContent = rustTimingWindow === null
    ? '—'
    : `${rustTimingWindow.windowMs.toFixed(1)} ms / ${rustTimingWindow.sampleCapacity} samples`;
  document.getElementById('stream-decoder-dropped').textContent = discardTelemetry.decoder;
  document.getElementById('stream-present-dropped').textContent = discardTelemetry.presenterOverwrite;
  document.getElementById('stream-transport-fps').textContent = lastTransportFps.toFixed(1);
  document.getElementById('stream-ipc-send-fps').textContent = lastFrontendSendFps.toFixed(1);
  document.getElementById('stream-frontend-fps').textContent = video.frontendDeliveryFps.toFixed(1);
  document.getElementById('stream-decode-fps').textContent = video.decodeFps.toFixed(1);
  document.getElementById('stream-decoder-output-fps').textContent = video.decoderOutputFps.toFixed(1);
  document.getElementById('stream-present-fps').textContent = video.presentFps.toFixed(1);
  document.getElementById('stream-av-skew').textContent = video.avSkewPercentiles.count
    ? `${formatSignedMilliseconds(video.avSkewPercentiles.p50)} / ${formatSignedMilliseconds(video.avSkewPercentiles.p95)} / ${video.avSkewPercentiles.maxAbsolute.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-decode-latency').textContent = video.decodePercentiles.count
    ? `${video.decodePercentiles.p50.toFixed(1)} / ${video.decodePercentiles.p95.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-present-latency').textContent = video.presentPercentiles.count
    ? `${video.presentPercentiles.p50.toFixed(1)} / ${video.presentPercentiles.p95.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-draw-latency').textContent = video.drawPercentiles.count
    ? `${video.drawPercentiles.p50.toFixed(2)} / ${video.drawPercentiles.p95.toFixed(2)} / ${video.drawPercentiles.p99.toFixed(2)} / ${video.drawPercentiles.max.toFixed(2)} ms`
    : '—';
  document.getElementById('stream-delivery-cadence').textContent = formatCadence(video.deliveryCadence);
  document.getElementById('stream-decoder-input-cadence').textContent = formatCadence(video.decoderInputIntervals);
  document.getElementById('stream-decoder-output-cadence').textContent = formatCadence(video.decoderOutputIntervals);
  document.getElementById('stream-present-cadence').textContent = formatCadence(video.presentationIntervals);
  document.getElementById('stream-codec').textContent = video.activeCodec;
  document.getElementById('stream-transport').textContent = connectionState.mediaTransport;
  document.getElementById('stream-path').textContent = streamPathMode;
  document.getElementById('stream-rtt').textContent = Number.isFinite(streamRttMs)
    ? `${streamRttMs.toFixed(1)} ms`
    : '— ms';
  document.getElementById('stream-network-session').textContent = networkPresentation.session;
  document.getElementById('stream-network-media').textContent = networkPresentation.media;
  document.getElementById('stream-network-input').textContent = networkPresentation.input;
  document.getElementById('stream-network-audio').textContent = networkPresentation.audio;
  document.getElementById('stream-network-input-ack').textContent = networkPresentation.inputAck;
  document.getElementById('stream-adaptive-feedback').textContent = adaptiveFeedbackAvailable
    ? 'authenticated · 1 Hz bounded reports'
    : adaptiveFeedbackError || 'unavailable';
  document.getElementById('stream-adaptive-decision').textContent = formatAdaptiveDecision(
    adaptiveDecision,
    adaptiveFeedbackAvailable,
  );

  adaptiveFeedbackPublisher.publish({
    lastSequence: lastMediaSequence,
    frontendQueueDepth: frontendQueueStats?.depth ?? 0,
    frontendQueueCapacity: frontendQueueStats?.capacity ?? 4,
    decoderQueueDepth: video.decoderQueueDepth,
    decoderQueueCapacity: video.decoderQueueCapacity,
    presenterQueueDepth: video.presenterQueueDepth,
    presenterQueueCapacity: video.presenterQueueCapacity,
    transportDroppedTotal: transportDroppedFrames,
    frontendDroppedTotal: frontendDroppedFrames,
    decoderDroppedTotal: video.droppedFrames,
    presenterDroppedTotal: video.presentationDroppedFrames,
    // Portal has no clock-synchronized capture-to-delivery measurement yet.
    // IPC send duration is a different boundary and must not be mislabeled.
    transportDeliveryP95Ms: null,
    decodeLatencyP95Ms: video.decodePercentiles.p95,
    presentationLatencyP95Ms: video.presentPercentiles.p95,
    resyncActive: video.recovering || frontendResyncStats?.active === true,
  });
}

setInterval(() => {
  if (connectionState.connected) updateStreamStats();
}, 250);

function clientChromeHeight() {
  const topbar = document.querySelector('.topbar');
  const bottombar = document.querySelector('.bottombar');
  return Math.max(0, (topbar?.offsetHeight ?? 0) + (bottombar?.offsetHeight ?? 0));
}

function sizeCanvasToIncomingStream() {
  const { width: frameWidth, height: frameHeight } = videoPipeline.format;
  if (frameWidth < 1 || frameHeight < 1) return;
  const main = canvas.parentElement;
  if (!main) return;
  const bounds = main.getBoundingClientRect();
  if (bounds.width <= 0 || bounds.height <= 0) return;
  const surface = fitStreamSurface({
    frameWidth,
    frameHeight,
    availableWidth: bounds.width,
    availableHeight: bounds.height,
  });
  canvas.style.width = `${surface.width}px`;
  canvas.style.height = `${surface.height}px`;
}

async function applyClientWindowGeometry(geometry, unmaximize) {
  try {
    const applied = await invoke('set_client_window_size', {
      logicalWidth: geometry.width,
      logicalHeight: geometry.height,
      unmaximize,
    });
    if (applied) lastObservedWindowSize = { ...geometry };
    return applied;
  } catch (error) {
    console.warn('could not apply stream window geometry:', error);
    return false;
  }
}

async function fitWindowToIncomingStream() {
  const {
    width: frameWidth,
    height: frameHeight,
    epoch: activeVideoFormatEpoch,
  } = videoPipeline.format;
  if (!connectionState.connected || frameWidth < 1 || frameHeight < 1) return false;
  const aspect = streamAspectKey(frameWidth, frameHeight);
  if (fittedStreamAspect === aspect || pendingStreamFit?.aspect === aspect) return false;
  const request = { aspect, generation: activeFrameGeneration, epoch: activeVideoFormatEpoch };
  pendingStreamFit = request;
  const geometry = fitInitialStreamWindow({
    frameWidth,
    frameHeight,
    chromeHeight: clientChromeHeight(),
    availableWidth: window.screen.availWidth,
    availableHeight: window.screen.availHeight,
  });
  const applied = await applyClientWindowGeometry(geometry, true);
  const currentFormat = videoPipeline.format;
  const currentAspect = currentFormat.width > 0 && currentFormat.height > 0
    ? streamAspectKey(currentFormat.width, currentFormat.height)
    : null;
  const current = pendingStreamFit === request
    && connectionState.connected
    && activeFrameGeneration === request.generation
    && currentFormat.epoch === request.epoch
    && currentAspect === request.aspect;
  if (pendingStreamFit === request) pendingStreamFit = null;
  if (applied && current) fittedStreamAspect = request.aspect;
  return applied && current;
}

function scheduleStreamWindowAspectCorrection() {
  const observed = { width: window.innerWidth, height: window.innerHeight };
  const previous = lastObservedWindowSize ?? observed;
  lastObservedWindowSize = observed;
  if (streamWindowResizeTimer !== null) clearTimeout(streamWindowResizeTimer);
  streamWindowResizeTimer = setTimeout(() => {
    streamWindowResizeTimer = null;
    const { width: frameWidth, height: frameHeight } = videoPipeline.format;
    if (!connectionState.connected || frameWidth < 1 || frameHeight < 1) return;
    const current = { width: window.innerWidth, height: window.innerHeight };
    let geometry;
    try {
      geometry = constrainStreamWindowResize({
        frameWidth,
        frameHeight,
        chromeHeight: clientChromeHeight(),
        width: current.width,
        height: current.height,
        previousWidth: previous.width,
        previousHeight: previous.height,
      });
    } catch (error) {
      console.warn('could not constrain stream window geometry:', error);
      return;
    }
    if (geometry !== null) void applyClientWindowGeometry(geometry, false);
  }, 80);
}

function sendFrameAcknowledgment(generation) {
  void invoke('iroh_client_ack_frame', { generation }).catch((error) => {
    console.warn('frame acknowledgment failed:', error);
  });
}

function acknowledgeFrame(session, generation = null) {
  const readyGeneration = stageFrameAcknowledgment(session, generation);
  if (readyGeneration !== null) sendFrameAcknowledgment(readyGeneration);
}

function activateConnectedFrameSession(session, generation) {
  if (!isActiveFrameSession(session, activeFrameSession)) {
    throw new Error('frame session was superseded during connect');
  }
  const activation = activateFrameSession(session, generation);
  activeFrameGeneration = generation;
  for (const pendingGeneration of activation.acknowledgments) {
    sendFrameAcknowledgment(pendingGeneration);
  }
  const pendingError = session.pendingFrameErrors.splice(0).find(
    (payload) => isCurrentFrameGeneration(payload?.generation, generation),
  );
  if (pendingError) throw new Error(pendingError.error || 'Media connection failed');
  for (const payload of activation.legacyFrames) {
    if (isCurrentFrameGeneration(payload?.generation, generation)) {
      videoPipeline.processFramePayload(payload);
    }
  }
}

function failFrameSession(session, error) {
  if (!isActiveFrameSession(session, activeFrameSession)) return;
  session.failed = true;
  session.failureDetail = `Frame delivery failed: ${error}`;
  console.error(session.failureDetail);
  if (connectionState.connected) void disconnect();
}

function handleBinaryFrameMessage(message, session) {
  if (!isActiveFrameSession(session, activeFrameSession)) return;
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
      data: null,
      codec: frame.codec,
      keyframe: frame.keyframe,
      codecConfig: frame.codecConfig,
      sequence: frame.sequence,
      pts_micros: frame.ptsMicros,
      discontinuity: frame.discontinuity,
    }, frame.data);
  } catch (error) {
    failFrameSession(session, error);
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

function acknowledgeAudio(generation, deliveryId) {
  void invoke('iroh_client_ack_audio', { generation, deliveryId }).catch((error) => {
    console.warn('audio acknowledgment failed:', error);
  });
}

function requestNativeAudioStop(session) {
  const generation = session?.expectedGeneration;
  if (session !== activeAudioSession
    || !Number.isSafeInteger(generation)
    || generation < 0
    || session.stopRequested) return;
  session.stopRequested = true;
  void invoke('iroh_client_stop_audio', { expectedGeneration: generation }).catch((error) => {
    console.warn('audio stop failed:', error);
  });
}

function disableAudioForSession(detail, session = activeAudioSession) {
  if (!session || session !== activeAudioSession) return;
  videoPipeline.resetAvSync(session);
  session.failed = true;
  session.failureDetail = detail;
  session.available = false;
  if (session.decoder) {
    try { session.decoder.close(); } catch (_) {}
    session.decoder = null;
  }
  session.messageTracker.clear();
  try { session.workletNode?.port.postMessage({ type: 'clear' }); } catch (_) {}
  requestNativeAudioStop(session);
  setAudioState('error', detail);
}

function handleBinaryAudioMessage(message, session) {
  let packet;
  try {
    packet = parseAudioEnvelope(message);
  } catch (error) {
    console.error('invalid audio packet:', error);
    disableAudioForSession(`Audio packet failed: ${error}`, session);
    return;
  }

  // Every valid delivery is released using the token embedded in that exact
  // message. A delayed delivery from an old connection is acknowledged but
  // cannot enter the current decoder.
  acknowledgeAudio(packet.generation, packet.deliveryId);
  if (session !== activeAudioSession
    || session.failed
    || !isCurrentAudioDelivery(packet, session.expectedGeneration)) return;

  try {
    session.packetsReceived++;
    if (!session.decoder || session.decoder.state === 'closed') {
      session.decoderDropped++;
      updateAudioUI();
      return;
    }
    const queuedAudioPackets = session.decoder.decodeQueueSize;
    if (packet.discontinuity || queuedAudioPackets >= MAX_AUDIO_DECODE_QUEUE_SIZE) {
      videoPipeline.resetAvSync(session, { recoverRing: true });
      session.decoderDropped += queuedAudioPackets;
      configureAudioDecoder(session);
    }
    session.decoder.decode(new EncodedAudioChunk({
      type: 'key',
      timestamp: packet.ptsMicros,
      duration: (packet.frameSamples * 1_000_000) / packet.sampleRate,
      data: packet.data,
    }));
    updateAudioUI();
  } catch (error) {
    console.error('undecodable audio packet:', error);
    disableAudioForSession(`Audio packet failed: ${error}`, session);
  }
}

// The software decoder/JPEG compatibility path intentionally remains an event:
// it is only selected when WebCodecs is unavailable and is not latency-critical.
listen('frame', (event) => {
  const session = activeFrameSession;
  if (!isActiveFrameSession(session, activeFrameSession)) return;
  try {
    const delivery = stageLegacyFrame(session, event.payload);
    for (const generation of delivery.acknowledgments) {
      sendFrameAcknowledgment(generation);
    }
    if (delivery.accepted) videoPipeline.processFramePayload(event.payload);
  } catch (error) {
    failFrameSession(session, error);
  }
});

listen('frame-stats', (event) => {
  if (!isCurrentFrameGeneration(event.payload?.generation, activeFrameGeneration)) return;
  const diagnostics = normalizeFrameStatsPayload(event.payload);
  transportDroppedFrames = diagnostics.transportDroppedFrames ?? transportDroppedFrames;
  transportObjectDroppedFrames = diagnostics.objectDroppedFrames;
  transportLateObjectDroppedFrames = diagnostics.lateObjectDroppedFrames;
  frontendDroppedFrames = diagnostics.frontendDroppedFrames ?? frontendDroppedFrames;
  frontendQueueDroppedFrames = diagnostics.queueDroppedFrames;
  frontendResyncDroppedFrames = diagnostics.resyncDroppedFrames;
  frontendQueueStats = diagnostics.queue;
  frontendResyncStats = diagnostics.resync;
  transportIntervalStats = diagnostics.transportIntervals;
  frontendIpcSendDurationStats = diagnostics.ipcSendDurations;
  rustTimingWindow = diagnostics.timingWindow;
  networkDiagnostics = diagnostics.networkDiagnostics;
  streamPathMode = typeof event.payload.path_mode === 'string'
    ? event.payload.path_mode
    : streamPathMode;
  streamRttMs = Number.isFinite(event.payload.path_rtt_ms)
    ? event.payload.path_rtt_ms
    : streamRttMs;
  lastMediaSequence = Number.isSafeInteger(event.payload.sequence) && event.payload.sequence >= 0
    ? event.payload.sequence : lastMediaSequence;
  lastTransportFps = Number.isFinite(event.payload.transport_receive_fps)
    ? event.payload.transport_receive_fps
    : Number.isFinite(event.payload.transport_fps)
      ? event.payload.transport_fps
      : event.payload.fps;
  lastFrontendSendFps = Number.isFinite(event.payload.frontend_send_fps)
    ? event.payload.frontend_send_fps
    : Number.isFinite(event.payload.frontend_fps)
      ? event.payload.frontend_fps
      : event.payload.fps;
  updateStreamStats();
});

listen('adaptive-bitrate-decision', (event) => {
  try {
    const decision = normalizeAdaptiveDecisionEnvelope(event.payload, activeFrameGeneration);
    if (decision === null) return;
    adaptiveDecision = decision;
    updateStreamStats();
  } catch (error) {
    console.warn('invalid adaptive bitrate diagnostic:', error);
  }
});

listen('adaptive-feedback-state', (event) => {
  if (!isCurrentFrameGeneration(event.payload?.generation, activeFrameGeneration)) return;
  if (event.payload?.available !== false) return;
  adaptiveFeedbackAvailable = false;
  adaptiveFeedbackError = typeof event.payload.error === 'string'
    ? event.payload.error : 'feedback stream closed';
  adaptiveFeedbackPublisher.stop();
  updateStreamStats();
});

listen('frame-error', (event) => {
  const session = activeFrameSession;
  if (!isActiveFrameSession(session, activeFrameSession)) return;
  if (session.expectedGeneration === null) {
    if (session.pendingFrameErrors.length < FRAME_CHANNEL_CAPACITY) {
      session.pendingFrameErrors.push(event.payload);
    } else {
      failFrameSession(session, 'pre-connect frame error capacity exceeded');
    }
    return;
  }
  if (!isCurrentFrameGeneration(event.payload?.generation, session.expectedGeneration)) return;
  failFrameSession(session, event.payload?.error || 'Media connection failed');
});

listen('audio-state', (event) => {
  const session = activeAudioSession;
  if (connectionState.disconnecting
    || !session.acceptsNativeEvents
    || event.payload?.available !== false) return;
  if (session.expectedGeneration === null) {
    try {
      stageAudioTerminalState(session.pendingTerminalStates, event.payload);
    } catch (error) {
      console.error('invalid pre-connect audio terminal state:', error);
    }
    return;
  }
  if (!isCurrentAudioGeneration(event.payload?.generation, session.expectedGeneration)) return;
  disableAudioForSession(event.payload.error || 'Audio connection ended', session);
});

listen('audio-stats', (event) => {
  const session = activeAudioSession;
  if (!isCurrentAudioGeneration(event.payload?.generation, session.expectedGeneration)) return;
  session.transportDropped = Number.isSafeInteger(event.payload?.sequence_dropped_total)
    ? event.payload.sequence_dropped_total : session.transportDropped;
  session.frontendDropped = Number.isSafeInteger(event.payload?.frontend_dropped_total)
    ? event.payload.frontend_dropped_total : session.frontendDropped;
  updateAudioUI();
});

// ─── Input ────────────────────────────────────────────────────────────────────
async function toggleControl() {
  if (!connectionState.connected
    || !connectionState.inputCapabilities.control
    || controlTransitionInProgress) return;
  // Capture synchronous controller provenance before cursor acquisition yields.
  // The DOM click dispatcher does not await this async handler.
  const controllerInitiated = controllerActivationInProgress;
  if (controlMode) {
    controlTransitionGeneration += 1;
    controllerActivationGate.reset();
    queueNeutralControllerState();
    releaseHeldInputs();
    controlMode = false;
    releasePointerLock();
  } else {
    const transitionGeneration = controlTransitionGeneration + 1;
    controlTransitionGeneration = transitionGeneration;
    controlTransitionInProgress = true;
    if (controllerInitiated) controllerActivationGate.arm();
    const acquired = await requestRelativePointerLock();
    const ownsTransition = transitionGeneration === controlTransitionGeneration;
    if (!ownsTransition) {
      // A cancellation may have released before a late browser request took
      // ownership. Do not disturb a newer transition, but do not leave an
      // orphaned lock behind when control is otherwise idle.
      if (!controlMode
        && !controlTransitionInProgress
        && document.pointerLockElement === canvas) {
        releasePointerLock();
      }
      return;
    }
    if (!acquired
      || connectionState.disconnecting
      || !connectionState.connected
      || !connectionState.inputCapabilities.control) {
      exitControlAfterBrowserPointerLockFailure();
      return;
    }
    controlTransitionInProgress = false;
    controlMode = true;
    // A used to cross the local control boundary must not also become an
    // accidental remote A press. Its release forwards the current snapshot,
    // without requiring noisy analog axes to be exactly zero.
    if (controllerInitiated) {
      queueNeutralControllerState();
      // A may have been released while native cursor acquisition was pending,
      // when controller publications were still local-only. Re-submit the
      // latest snapshot through the gate so that release takes effect now;
      // a still-held A remains suppressed and leaves the queued neutral state.
      publishCurrentControllerState();
    } else {
      publishCurrentControllerState();
    }
  }
  updateControlUI();
}

function queueNeutralControllerState() {
  if (!connectionState.connected || !connectionState.inputCapabilities.gamepad) return false;
  return sendInput({ t: 'gp', state: neutralGamepadInputState() });
}

function describeInputCapabilities() {
  const accepted = [];
  const capabilities = connectionState.inputCapabilities;
  if (capabilities.relativePointer) accepted.push('relative pointer');
  else if (capabilities.absolutePointer) accepted.push('pointer');
  if (capabilities.keyboard) accepted.push('keyboard');
  if (capabilities.text) accepted.push('text');
  if (capabilities.gamepad) accepted.push('gamepad');
  return accepted.length > 0 ? accepted.join(' + ') : 'view only';
}

function pointerInputAvailable() {
  const capabilities = connectionState.inputCapabilities;
  return capabilities.relativePointer || capabilities.absolutePointer;
}

async function requestRelativePointerLock() {
  if (!connectionState.inputCapabilities.relativePointer) return true;
  try {
    const nativeResult = await setNativeCursorGrab(true);
    // Older commands returned no value, so only an explicit false disables
    // the browser fallback. Current macOS builds return false because
    // CoreGraphics owns relative capture and cursor visibility there.
    browserPointerLockRequired = nativeResult !== false;
    await requestBrowserPointerLock();
    return true;
  } catch (error) {
    console.warn('relative pointer lock unavailable:', error);
    return false;
  }
}

async function requestBrowserPointerLock() {
  if (!browserPointerLockRequired) return;
  try {
    if (typeof canvas.requestPointerLock !== 'function') {
      throw new Error('browser Pointer Lock is unavailable');
    }
    const request = canvas.requestPointerLock();
    if (request && typeof request.then === 'function') await request;
    await waitForBrowserPointerLockOwnership();
    // Pointer Lock and the native window grab are separate on Linux. Reassert
    // the native state only after the browser proves canvas ownership.
    await setNativeCursorGrab(true);
  } catch (error) {
    exitControlAfterBrowserPointerLockFailure();
    throw error;
  }
}

function waitForBrowserPointerLockOwnership(timeoutMs = 500) {
  if (document.pointerLockElement === canvas) return Promise.resolve();
  return new Promise((resolve, reject) => {
    let timeoutId;
    const cleanup = () => {
      clearTimeout(timeoutId);
      document.removeEventListener('pointerlockchange', handleChange);
      document.removeEventListener('pointerlockerror', handleError);
    };
    const settle = (callback, value) => {
      cleanup();
      callback(value);
    };
    const handleChange = () => {
      if (document.pointerLockElement === canvas) {
        settle(resolve);
      } else {
        settle(reject, new Error('browser Pointer Lock ownership was lost'));
      }
    };
    const handleError = () => {
      settle(reject, new Error('browser Pointer Lock request was rejected'));
    };
    document.addEventListener('pointerlockchange', handleChange);
    document.addEventListener('pointerlockerror', handleError);
    timeoutId = setTimeout(() => {
      settle(reject, new Error('browser Pointer Lock ownership timed out'));
    }, timeoutMs);
  });
}

function exitControlAfterBrowserPointerLockFailure() {
  if (!browserPointerLockRequired || (!controlMode && !controlTransitionInProgress)) {
    return false;
  }
  controlTransitionGeneration += 1;
  controlTransitionInProgress = false;
  controllerActivationGate.reset();
  controllerEscape.reset();
  queueNeutralControllerState();
  releaseHeldInputs();
  controlMode = false;
  releasePointerLock();
  updateControlUI();
  return true;
}

function handleBrowserPointerLockChange() {
  if (!browserPointerLockLossRequiresControlExit({
    browserPointerLockRequired,
    pointerLockElement: document.pointerLockElement,
    expectedElement: canvas,
    controlMode,
    controlTransitionInProgress,
  })) return;
  exitControlAfterBrowserPointerLockFailure();
}

document.addEventListener('pointerlockchange', handleBrowserPointerLockChange);

function setNativeCursorGrab(grab) {
  const command = nativeCursorCommand.then(
    () => invoke('set_client_cursor_grab', { grab }),
  );
  nativeCursorCommand = command.catch(() => {});
  return command;
}

async function releaseNativeCursorGrab(releaseGeneration) {
  const retryDelaysMs = [0, 16, 50];
  let lastError = null;
  for (const delayMs of retryDelaysMs) {
    if (releaseGeneration !== controlTransitionGeneration || controlMode) return false;
    if (delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, delayMs));
      if (releaseGeneration !== controlTransitionGeneration || controlMode) return false;
    }
    try {
      await setNativeCursorGrab(false);
      return true;
    } catch (error) {
      lastError = error;
    }
  }
  console.error('native cursor release failed after bounded retries:', lastError);
  if (!controlMode && releaseGeneration === controlTransitionGeneration) {
    setStatus('err', 'cursor release failed · quit app');
  }
  return false;
}

function currentPointerSurfaceSize() {
  const { width: frameWidth, height: frameHeight } = videoPipeline.format;
  return resolvePointerSurfaceSize(
    activePointerSession?.surfaceDimensions ?? null,
    frameWidth,
    frameHeight,
    connectionState.inputCapabilities.relativePointer,
  );
}

function renderRemotePointer() {
  const session = activePointerSession;
  const surface = currentPointerSurfaceSize();
  const visible = controlMode
    && connectionState.inputCapabilities.relativePointer
    && connectionState.inputCapabilities.pointerPositionFeedback
    && session?.remotePosition !== null
    && session?.remoteVisible
    && surface !== null;
  remotePointer.classList.toggle('visible', visible);
  if (!visible) return;
  const canvasRect = canvas.getBoundingClientRect();
  const mainRect = canvas.parentElement.getBoundingClientRect();
  remotePointer.style.left = `${canvasRect.left - mainRect.left
    + (session.remotePosition.x / surface.width) * canvasRect.width}px`;
  remotePointer.style.top = `${canvasRect.top - mainRect.top
    + (session.remotePosition.y / surface.height) * canvasRect.height}px`;
}

function releasePointerLock() {
  const releaseGeneration = controlTransitionGeneration;
  void releaseNativeCursorGrab(releaseGeneration);
  if (document.pointerLockElement !== canvas || typeof document.exitPointerLock !== 'function') return;
  try { document.exitPointerLock(); } catch (_) {}
}

function updateControlUI() {
  const el = document.getElementById('control-toggle');
  const capabilities = connectionState.inputCapabilities;
  const available = connectionState.connected && capabilities.control;
  if (!available) controlMode = false;
  el.textContent = available
    ? `${controlMode ? 'controlling' : 'take control'} · ${describeInputCapabilities()}${controlMode && capabilities.relativePointer ? ' · Ctrl+Alt+Esc to exit' : controlMode && capabilities.gamepad ? ' · hold Back+Start to exit' : ''}`
    : 'view only · input unavailable';
  el.classList.toggle('active', controlMode);
  el.classList.toggle('disabled', !available);
  el.setAttribute('aria-disabled', available ? 'false' : 'true');
  document.body.classList.toggle(
    'native-pointer-control',
    controlMode && capabilities.relativePointer,
  );
  canvas.classList.toggle(
    'relative-control',
    controlMode && capabilities.relativePointer,
  );
  renderRemotePointer();
  const streamControl = document.getElementById('stream-control');
  if (streamControl) {
    streamControl.textContent = available ? describeInputCapabilities() : 'view only · unavailable';
  }
}

function scaleCoords(clientX, clientY) {
  const rect = canvas.getBoundingClientRect();
  return mapCanvasPointToSurface({
    clientX,
    clientY,
    rect,
    surface: currentPointerSurfaceSize(),
  }) ?? { x: 0, y: 0 };
}

const RELIABLE_INPUT_QUEUE_LIMIT = 128;
const IN_FLIGHT_INPUT_RESTORE_RESERVE = 1;
const RELIABLE_INPUT_ENQUEUE_LIMIT = RELIABLE_INPUT_QUEUE_LIMIT
  - IN_FLIGHT_INPUT_RESTORE_RESERVE;
const POINTER_MOTION_BARRIER_RESERVE = 2;
const RELEASE_INPUT_RESERVE = MAX_HELD_KEYS
  + MAX_HELD_MOUSE_BUTTONS
  + POINTER_MOTION_BARRIER_RESERVE;
const REGULAR_RELIABLE_INPUT_LIMIT = RELIABLE_INPUT_ENQUEUE_LIMIT - RELEASE_INPUT_RESERVE;
const INPUT_RETRY_MS = 8;
const reliableInputQueue = [];
const heldInputs = new HeldInputState();
const pendingPointerMotion = new PointerMotionBuffer();
let pendingGamepadInput = null;
let inputPumpRunning = false;
let inputPumpScheduled = false;

function clearPendingInput() {
  reliableInputQueue.length = 0;
  heldInputs.clear();
  controllerActivationGate.reset();
  pendingPointerMotion.clear();
  pendingGamepadInput = null;
}

function inputEventAvailable(event) {
  const capabilities = connectionState.inputCapabilities;
  if (!capabilities.control) return false;
  if (event.t === 'mm') return capabilities.absolutePointer;
  if (event.t === 'mr') return capabilities.relativePointer;
  if (event.t === 'mp') return capabilities.relativePointer;
  if (['mc', 'md', 'mu', 'ms'].includes(event.t)) return pointerInputAvailable();
  if (['kd', 'ku', 'kt'].includes(event.t)) return capabilities.keyboard;
  if (event.t === 'tx') return capabilities.text;
  if (event.t === 'gp') return capabilities.gamepad;
  return false;
}

function hasPendingInput() {
  return reliableInputQueue.length > 0
    || pendingPointerMotion.pending
    || pendingGamepadInput !== null;
}

function scheduleInputPump() {
  if (inputPumpRunning || inputPumpScheduled) return;
  inputPumpScheduled = true;
  setTimeout(() => {
    inputPumpScheduled = false;
    pumpInputQueue();
  }, 0);
}

function sendInput(event, { release = false } = {}) {
  if (!inputEventAvailable(event)) return false;
  if (event.t === 'gp') {
    // Controller samples are state, not transitions. A single latest-value slot
    // bounds latency and guarantees a neutral snapshot can replace stale input.
    pendingGamepadInput = event;
  } else if (event.t === 'mm') {
    // Absolute pointer motion is latest-value data; stale coordinates are not
    // useful and must not create an invocation backlog.
    pendingPointerMotion.setAbsolute(event);
  } else if (event.t === 'mr') {
    // Relative motion is displacement. Coalesce every sample into one
    // constant-size total; the pump emits protocol-bounded chunks so
    // throttling preserves distance instead of dropping motion.
    if (!pendingPointerMotion.addRelative(event.dx, event.dy)) return false;
  } else {
    const queueLimit = release ? RELIABLE_INPUT_ENQUEUE_LIMIT : REGULAR_RELIABLE_INPUT_LIMIT;
    const pointerTransition = ['mp', 'mc', 'md', 'mu', 'ms'].includes(event.t);
    const barrierLength = pointerTransition ? pendingPointerMotion.barrierLength : 0;
    const tail = reliableInputQueue.at(-1);
    const coalesceScroll = event.t === 'ms' && barrierLength === 0 && tail?.t === 'ms';
    const requiredSlots = coalesceScroll ? 0 : barrierLength + 1;
    if (reliableInputQueue.length + requiredSlots > queueLimit) {
      console.error(release
        ? 'input release reserve exhausted; closing control session'
        : 'reliable input queue full; closing control session');
      // Defer teardown until the caller has removed any transition it could
      // not enqueue from held-state tracking.
      queueMicrotask(() => {
        if (connectionState.connected) void disconnect();
      });
      return false;
    }
    if (coalesceScroll) {
      tail.dx = Math.max(-1000000, Math.min(1000000, tail.dx + event.dx));
      tail.dy = Math.max(-1000000, Math.min(1000000, tail.dy + event.dy));
    } else if (pointerTransition) {
      reliableInputQueue.push(...pendingPointerMotion.takeBarrierBefore(event));
    } else {
      reliableInputQueue.push(event);
    }
  }
  scheduleInputPump();
  return true;
}

function releaseHeldInputs() {
  for (const release of heldInputs.releaseEvents()) {
    // Ordinary reliable events cannot consume this reserved capacity.
    sendInput(release, { release: true });
  }
  heldInputs.clear();
}

async function waitForInputDrain(timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  while ((hasPendingInput() || inputPumpRunning) && performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, INPUT_RETRY_MS));
  }
}

function takePendingInput() {
  if (reliableInputQueue.length > 0) {
    return { event: reliableInputQueue.shift(), reliable: true };
  }
  const pointerMotionEvent = pendingPointerMotion.take();
  if (pointerMotionEvent !== null) {
    return { event: pointerMotionEvent, reliable: false };
  }
  if (pendingGamepadInput !== null) {
    const event = pendingGamepadInput;
    pendingGamepadInput = null;
    return { event, reliable: false };
  }
  return null;
}

function restorePendingInput(item) {
  if (item.reliable) {
    if (reliableInputQueue.length >= RELIABLE_INPUT_QUEUE_LIMIT) {
      throw new Error('reliable input restore reserve exhausted');
    }
    reliableInputQueue.unshift(item.event);
  } else if (item.event.t === 'mm' || item.event.t === 'mr') {
    // A transition queued during the invoke must remain after the rejected
    // motion. With no transition, relative displacement merges while stale
    // absolute state yields to any newer latest position.
    restoreRejectedPointerMotion(
      reliableInputQueue,
      pendingPointerMotion,
      item.event,
      RELIABLE_INPUT_QUEUE_LIMIT,
    );
  } else if (item.event.t === 'gp') {
    // Keep a newer controller sample, especially a neutral release state.
    if (pendingGamepadInput === null) pendingGamepadInput = item.event;
  }
}

async function pumpInputQueue() {
  if (inputPumpRunning) return;
  inputPumpRunning = true;
  try {
    while (hasPendingInput()) {
      const item = takePendingInput();
      if (!item) break;
      let accepted;
      try {
        accepted = await invoke('iroh_client_send_input', { event: item.event });
      } catch (e) {
        console.warn('input send failed:', e);
        clearPendingInput();
        return;
      }
      if (!accepted) {
        try {
          restorePendingInput(item);
        } catch (error) {
          console.error('input restore failed:', error);
          clearPendingInput();
          if (connectionState.connected) void disconnect();
          return;
        }
        await new Promise((resolve) => setTimeout(resolve, INPUT_RETRY_MS));
      }
    }
  } finally {
    inputPumpRunning = false;
    if (hasPendingInput()) scheduleInputPump();
  }
}

window.addEventListener('mousemove', (e) => {
  if (!controlMode || !pointerInputAvailable()) return;
  if (connectionState.inputCapabilities.relativePointer) {
    const movement = scaleRelativePointerDelta(
      e.movementX,
      e.movementY,
    );
    sendInput({ t: 'mr', ...movement });
    return;
  }
}, { capture: true });

canvas.addEventListener('mousemove', (e) => {
  if (!controlMode
    || !connectionState.inputCapabilities.absolutePointer
    || connectionState.inputCapabilities.relativePointer) return;
  const { x, y } = scaleCoords(e.clientX, e.clientY);
  sendInput({ t: 'mm', x, y });
});

function handleMouseDown(e) {
  if (!controlMode || !pointerInputAvailable()) return;
  if (connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  else if (e.target !== canvas) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  e.preventDefault();
  if (connectionState.inputCapabilities.relativePointer && document.pointerLockElement !== canvas) {
    void requestBrowserPointerLock().catch((error) => {
      console.warn('browser pointer lock retry failed:', error);
    });
  }
  if (!heldInputs.trackMouseButton(btn)) return;
  if (!sendInput({ t: 'md', b: btn })) heldInputs.takeMouseButtonRelease(btn);
}

window.addEventListener('mousedown', handleMouseDown, { capture: true });

function releaseMouseButton(e) {
  if (!connectionState.connected || !pointerInputAvailable()) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  const release = heldInputs.takeMouseButtonRelease(btn);
  if (!release) return;
  e.preventDefault();
  sendInput(release, { release: true });
}

window.addEventListener('mouseup', (e) => {
  if (controlMode && connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  releaseMouseButton(e);
}, { capture: true });

window.addEventListener('contextmenu', (e) => {
  if (!controlMode || !pointerInputAvailable()) return;
  if (!connectionState.inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
}, { capture: true });

window.addEventListener('wheel', (e) => {
  if (!controlMode || !pointerInputAvailable()) return;
  if (!connectionState.inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
  if (connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  sendInput({ t: 'ms', dx: Math.round(e.deltaX), dy: Math.round(e.deltaY) });
}, { passive: false, capture: true });

window.addEventListener('keydown', (e) => {
  if (!controlMode) return;
  if (e.key === 'Escape' && e.ctrlKey && e.altKey) {
    e.preventDefault();
    controllerActivationGate.reset();
    queueNeutralControllerState();
    releaseHeldInputs();
    controlMode = false;
    releasePointerLock();
    updateControlUI();
    return;
  }
  const printableText = e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey;
  if (printableText && connectionState.inputCapabilities.text) {
    e.preventDefault();
    sendInput({ t: 'tx', s: e.key });
    return;
  }
  if (!connectionState.inputCapabilities.keyboard) return;
  const k = mapKey(e);
  if (k) {
    e.preventDefault();
    const keyId = e.code || e.key;
    const tracking = heldInputs.trackKey(keyId, k);
    if (tracking === 'repeat') return;
    if (tracking === 'full') {
      console.error('held key limit reached; refusing additional key transition');
      return;
    }
    if (!sendInput({ t: 'kd', k })) heldInputs.takeKeyRelease(keyId);
  }
});

window.addEventListener('keyup', (e) => {
  const keyId = e.code || e.key;
  const release = heldInputs.takeKeyRelease(keyId);
  if (!release) return;
  e.preventDefault();
  sendInput(release, { release: true });
});

window.addEventListener('blur', () => {
  if (!controlMode && !controlTransitionInProgress) return;
  controlTransitionGeneration += 1;
  controlTransitionInProgress = false;
  controllerActivationGate.reset();
  queueNeutralControllerState();
  releaseHeldInputs();
  controlMode = false;
  releasePointerLock();
  updateControlUI();
});

// ─── Controller PIN entry ─────────────────────────────────────────────────────
let activePinInput = null;
let pinPadReturnFocus = null;

function renderPinPad() {
  const display = document.getElementById('pin-pad-display');
  const length = activePinInput?.value.length || 0;
  display.textContent = length === 0 ? 'empty' : '•'.repeat(length);
  display.setAttribute('aria-label', length === 0 ? 'PIN empty' : `PIN has ${length} digits`);
}

function openPinPad(targetId, returnFocus = document.activeElement) {
  if (connectionState.developmentMode) return;
  const target = document.getElementById(targetId);
  if (!(target instanceof HTMLInputElement) || target.disabled) return;
  activePinInput = target;
  pinPadReturnFocus = returnFocus;
  renderPinPad();
  document.getElementById('pin-pad-overlay').classList.remove('hidden');
  setTimeout(() => document.querySelector('[data-pin-digit="1"]').focus(), 0);
}

function closePinPad() {
  document.getElementById('pin-pad-overlay').classList.add('hidden');
  const returnFocus = pinPadReturnFocus;
  activePinInput = null;
  pinPadReturnFocus = null;
  returnFocus?.focus();
}

function pinPadDigit(digit) {
  if (!activePinInput || !/^\d$/.test(digit)) return;
  const limit = Number(activePinInput.maxLength) > 0 ? activePinInput.maxLength : 64;
  if (activePinInput.value.length >= limit) return;
  activePinInput.value += digit;
  activePinInput.dispatchEvent(new Event('input', { bubbles: true }));
  renderPinPad();
}

function pinPadBackspace() {
  if (!activePinInput || activePinInput.value.length === 0) return;
  activePinInput.value = activePinInput.value.slice(0, -1);
  activePinInput.dispatchEvent(new Event('input', { bubbles: true }));
  renderPinPad();
}

function pinPadClear() {
  if (!activePinInput) return;
  activePinInput.value = '';
  activePinInput.dispatchEvent(new Event('input', { bubbles: true }));
  renderPinPad();
}

for (const id of ['enrollment-pin', 'intro-pin', 'pin-input']) {
  document.getElementById(id).addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    if (id === 'enrollment-pin') void derivePortalIdentity();
    else if (id === 'intro-pin') void introConnect();
    else void connectHost();
  });
}

// ─── Gamepad polling, UI navigation, and latest-state routing ─────────────────
const controllerPublisher = new LatestControllerStatePublisher();
const controllerActions = new ControllerActionRepeater();
const controllerEscape = new GamepadEscapeHold(1000);
let controllerSequence = 0;
let selectedControllerIndex = null;
let currentControllerState = disconnectedControllerState();
let lastControllerSignature = controllerStateSignature(currentControllerState);
let externalControllerObserver = null;

controllerPublisher.setHandler((state) => {
  if (externalControllerObserver) {
    try { externalControllerObserver(state); } catch (error) { console.warn('controller observer failed:', error); }
  }
  if (!connectionState.connected
    || !controlMode
    || !connectionState.inputCapabilities.gamepad) return;
  const inputState = maskGamepadEscapeChord(toGamepadInputState(state));
  if (!controllerActivationGate.accepts(inputState)) return;
  sendInput({ t: 'gp', state: inputState });
});

function publishCurrentControllerState() {
  controllerPublisher.publish(currentControllerState);
}

function controllerScope() {
  const tap = document.getElementById('tap-overlay');
  if (!tap.classList.contains('hidden')) return tap;
  const pinPad = document.getElementById('pin-pad-overlay');
  if (!pinPad.classList.contains('hidden')) return pinPad;
  const resetEnrollment = document.getElementById('reset-enrollment-overlay');
  if (!resetEnrollment.classList.contains('hidden')) return resetEnrollment;
  const invitation = document.getElementById('invitation-overlay');
  if (!invitation.classList.contains('hidden')) return invitation;
  const enrollment = document.getElementById('enrollment-overlay');
  if (!enrollment.classList.contains('hidden')) return enrollment;
  const intro = document.getElementById('intro');
  if (!intro.classList.contains('hidden')) return intro;
  const panel = document.getElementById('panel');
  if (panel.classList.contains('visible')) return panel;
  return document;
}

function controllerElements() {
  return [...controllerScope().querySelectorAll('[data-controller-focus]')].filter((element) => {
    if (element.disabled || element.getAttribute('aria-disabled') === 'true') return false;
    if (element.closest('.overlay-screen.hidden') || element.closest('.panel:not(.visible)')) return false;
    const style = getComputedStyle(element);
    return style.display !== 'none' && style.visibility !== 'hidden' && element.getClientRects().length > 0;
  });
}

function setControllerFocus(element) {
  document.querySelectorAll('.controller-focus').forEach((item) => item.classList.remove('controller-focus'));
  if (!element) return;
  element.classList.add('controller-focus');
  element.focus({ preventScroll: false });
}

function navigateControllerFocus(direction) {
  const elements = controllerElements();
  if (elements.length === 0) return;
  const currentIndex = elements.indexOf(document.activeElement);
  const rects = elements.map((element) => element.getBoundingClientRect());
  setControllerFocus(elements[chooseDirectionalIndex(rects, currentIndex, direction)]);
}

function activateControllerFocus() {
  const elements = controllerElements();
  let target = elements.includes(document.activeElement) ? document.activeElement : elements[0];
  if (!target) return;
  setControllerFocus(target);
  if (target.dataset.pinTarget) {
    openPinPad(target.dataset.pinTarget, target);
  } else {
    controllerActivationInProgress = true;
    try {
      target.click();
    } finally {
      controllerActivationInProgress = false;
    }
  }
}

function controllerBack() {
  if (!document.getElementById('pin-pad-overlay').classList.contains('hidden')) {
    closePinPad();
  } else if (!document.getElementById('reset-enrollment-overlay').classList.contains('hidden')) {
    cancelEnrollmentReset();
  } else if (document.getElementById('panel').classList.contains('visible')) {
    togglePanel(false);
  }
}

function updateControllerStatus(state) {
  const status = document.getElementById('controller-status');
  if (!state.connected) {
    status.textContent = 'controller: none';
    status.classList.remove('connected', 'warning');
    return;
  }
  const standard = state.mapping === 'standard';
  status.textContent = standard ? 'controller: ready' : 'controller: non-standard';
  status.classList.toggle('connected', standard);
  status.classList.toggle('warning', !standard);
}

function pollControllers(nowMs) {
  let gamepads = [];
  try {
    gamepads = typeof navigator.getGamepads === 'function'
      ? Array.from(navigator.getGamepads()).filter(Boolean)
      : [];
  } catch (error) {
    console.warn('gamepad poll failed:', error);
  }
  const gamepad = selectPreferredController(gamepads, selectedControllerIndex);
  const nextIndex = gamepad?.index ?? null;
  if (nextIndex !== selectedControllerIndex) {
    selectedControllerIndex = nextIndex;
    controllerActions.reset();
    controllerEscape.reset();
    lastControllerSignature = '';
  }

  currentControllerState = normalizeGamepad(gamepad, controllerSequence + 1);
  const signature = controllerStateSignature(currentControllerState);
  if (signature !== lastControllerSignature) {
    controllerSequence += 1;
    currentControllerState = normalizeGamepad(gamepad, controllerSequence);
    lastControllerSignature = signature;
    controllerPublisher.publish(currentControllerState);
    updateControllerStatus(currentControllerState);
  }

  const remoteRoute = connectionState.connected
    && controlMode
    && connectionState.inputCapabilities.gamepad
    && currentControllerState.connected
    && currentControllerState.mapping === 'standard';
  if (remoteRoute) {
    if (controllerEscape.update(currentControllerState, nowMs)) {
      controllerActivationGate.reset();
      queueNeutralControllerState();
      releaseHeldInputs();
      controlMode = false;
      releasePointerLock();
      updateControlUI();
      controllerActions.reset();
      setControllerFocus(document.getElementById('control-toggle'));
    }
  } else {
    controllerEscape.reset();
    for (const action of controllerActions.update(currentControllerState, nowMs)) {
      if (action.type === 'navigate') navigateControllerFocus(action.direction);
      else if (action.type === 'activate') activateControllerFocus();
      else if (action.type === 'back') controllerBack();
    }
  }
  requestAnimationFrame(pollControllers);
}

document.addEventListener('pointerdown', () => {
  document.querySelectorAll('.controller-focus').forEach((item) => item.classList.remove('controller-focus'));
}, { capture: true });

window.addEventListener('resize', () => {
  sizeCanvasToIncomingStream();
  renderRemotePointer();
  scheduleStreamWindowAspectCorrection();
});

window.addEventListener('focus', () => {
  if (!controlMode || !connectionState.inputCapabilities.relativePointer) return;
  // WebKit rebuilds its native cursor rectangles after the window-level focus
  // event. Reassert from the webview on the next frame so the final rectangle
  // is the transparent one used during relative control.
  requestAnimationFrame(() => {
    if (!controlMode || !connectionState.inputCapabilities.relativePointer) return;
    void setNativeCursorGrab(true).catch((error) => {
      console.warn('native cursor re-hide after focus failed:', error);
    });
  });
});

document.addEventListener('keydown', () => {
  document.querySelectorAll('.controller-focus').forEach((item) => item.classList.remove('controller-focus'));
}, { capture: true });

listen('fido-done', () => {
  document.getElementById('tap-overlay-title').textContent = 'connecting';
  document.getElementById('tap-overlay-desc').textContent = 'Key recognised. Waiting for host to respond.';
  document.getElementById('tap-status').textContent = 'please wait...';
});

listen('dev-connect-routing', (event) => {
  console.warn('[development direct-node routing]', event.payload.warning, event.payload.host_node_id);
});

listen('invitation-pending', (event) => {
  try { showInvitationSummary(event.payload); }
  catch (error) { console.error('invalid native invitation summary:', error); }
});

// ─── Expose to HTML onclick handlers ─────────────────────────────────────────
Object.assign(window, {
  introConnect, connectHost, disconnect,
  scanFido, togglePanel, toggleControl, toggleAudioMute,
  openPinPad, closePinPad, pinPadDigit, pinPadBackspace, pinPadClear,
  openEnrollmentReset, cancelEnrollmentReset, confirmEnrollmentReset,
  derivePortalIdentity, chooseInvitationFile, confirmInvitation, cancelInvitation,
  sigilController: Object.freeze({
    getLatestState: () => controllerPublisher.latest,
    setObserver: (observer) => {
      if (observer !== null && typeof observer !== 'function') throw new TypeError('observer must be a function or null');
      externalControllerObserver = observer;
    },
  }),
});

// ─── Init ─────────────────────────────────────────────────────────────────────
async function initialize() {
  try {
    await checkDevelopmentConnectionMode();
  } catch (e) {
    console.error('development connection mode check failed:', e);
  }
  updatePanelVisibility();
  updateControlUI();
  try {
    await refreshEnrollment();
  } catch (error) {
    console.error('enrollment status check failed:', error);
    showEnrollment();
    const status = document.getElementById('enrollment-status');
    status.className = 'overlay-status err';
    status.textContent = 'could not load enrollment';
  }
  if (!connectionState.developmentMode) scanFido();
  requestAnimationFrame(pollControllers);
}

initialize();
