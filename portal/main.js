import {
  parseAnnexBNals,
  nalsToLengthPrefixed,
  buildAvcDescription,
  avcCodecStr,
  h264NalType,
  buildHvcDescription,
  hevcCodecStr,
  hevcNalType,
  parseAv1Obus,
  av1CodecStr,
  buildAv1Description,
} from './codecs.js';
import {
  HeldInputState,
  MAX_HELD_KEYS,
  MAX_HELD_MOUSE_BUTTONS,
  PointerMotionBuffer,
  browserPointerLockLossRequiresControlExit,
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
import {
  FRAME_CHANNEL_CAPACITY,
  activateFrameSession,
  isActiveFrameSession,
  newFrameSession,
  stageFrameAcknowledgment,
  stageLegacyFrame,
} from './frame-session.mjs';
import { parsePointerFeedbackMessage } from './pointer-feedback.mjs';
import {
  committedRustConnection,
  disconnectRejectedRustConnection,
} from './connection-attempt.mjs';
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
import { BoundedAudioMessageTracker } from './audio-ring.mjs';
import { audioButtonPresentation } from './audio-ui.mjs';
import {
  BoundedCadenceWindow,
  BoundedLatencyWindow,
  BoundedValueWindow,
  LatestFramePresenter,
  RollingRateWindow,
} from './stream-metrics.mjs';
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
} from './window-geometry.mjs';
import { buildVideoDecoderConfig } from './video-decoder-config.mjs';
import {
  DECODER_RECOVERY_REASONS,
  DecoderRecoveryState,
} from './decoder-recovery.mjs';
import {
  formatInvitationExpiry,
  grantLabel,
  normalizeInvitationSummary,
  shortPeerFingerprint,
} from './enrollment.mjs';

let controlMode = false;
let connected = false;
let connecting = false;
let developmentConnectionMode = false;
let enrollmentReady = false;
let pendingInvitationSummary = null;
let currentEnrollmentStatus = null;
let disconnecting = false;
let inputCapabilities = {
  relativePointer: false,
  pointerPositionFeedback: false,
  absolutePointer: false,
  keyboard: false,
  text: false,
  gamepad: false,
  control: false,
};
let frameWidth = 0;
let frameHeight = 0;
let fittedStreamDimensions = null;
let lastObservedWindowSize = null;
let streamWindowResizeTimer = null;
let pointerSurfaceDimensions = null;
let remotePointerPosition = null;
let remotePointerVisible = false;
let activeFrameChannel = null;
let activeFrameGeneration = null;
let activeFrameSession = null;
let activeAudioChannel = null;
let activePointerChannel = null;
let activePointerSession = null;
let activeAudioSession = null;
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
let audioContext = null;
let audioWorkletNode = null;
let audioDecoder = null;
let audioAvailable = false;
let audioMuted = false;
let audioState = 'unavailable';
let audioStateDetail = 'not connected';
let audioPacketsReceived = 0;
let audioDecoderDropped = 0;
let audioBufferedFrames = 0;
let audioUnderflows = 0;
let audioUnderflowDurationMicros = 0;
let audioSilentDurationMicros = 0;
let audioTransportDropped = 0;
let audioFrontendDropped = 0;

function newAudioSession() {
  return {
    expectedGeneration: null,
    pendingTerminalStates: [],
    failed: false,
    failureDetail: null,
    stopRequested: false,
    workletNode: null,
    workletDroppedFrames: 0,
    workletRecoveryDiscardedFrames: 0,
    audioTimeline: null,
    avSyncEpoch: 0,
    messageTracker: new BoundedAudioMessageTracker(),
  };
}

function newPointerSession() {
  return {
    received: false,
    latest: null,
    failed: false,
    failureDetail: null,
    closing: false,
  };
}

function applyPointerPositionFeedback(message, session) {
  if (session !== activePointerSession || !inputCapabilities.pointerPositionFeedback) return;
  const surface = currentPointerSurfaceSize();
  if (surface === null) return;
  const feedback = validatePointerPositionFeedback(message, surface);
  remotePointerPosition = feedback.position;
  remotePointerVisible = feedback.pointer_visible;
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
      remotePointerPosition = null;
      remotePointerVisible = false;
      renderRemotePointer();
      console.error(session.failureDetail);
      if (connected) void disconnect();
      return;
    }
    const feedback = envelope.feedback;
    session.received = true;
    session.latest = feedback;
    if (connected) applyPointerPositionFeedback(feedback, session);
  } catch (error) {
    session.failed = true;
    session.failureDetail = `Pointer feedback failed: ${error}`;
    remotePointerPosition = null;
    remotePointerVisible = false;
    renderRemotePointer();
    console.error('invalid pointer-position feedback:', error);
    if (connected) void disconnect();
  }
}

const invoke = (...args) => window.__TAURI__.core.invoke(...args);
const listen = (...args) => window.__TAURI__.event.listen(...args);

function updatePanelVisibility() {
  const streamSection = document.getElementById('stream-section');
  streamSection.style.display = connected ? '' : 'none';
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
    const target = developmentConnectionMode
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
  if (developmentConnectionMode) {
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
  developmentConnectionMode = mode.enabled;
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
  if (connecting || connected) return;
  if (!developmentConnectionMode && !enrollmentReady) {
    showEnrollment();
    return;
  }
  const pin = document.getElementById('intro-pin').value.trim();
  const status = document.getElementById('intro-status');
  if (!developmentConnectionMode && !pin) {
    status.className = 'overlay-status err';
    status.textContent = 'enter pin';
    return;
  }
  hideIntro();
  if (!developmentConnectionMode) showTap();
  document.getElementById('pin-input').value = pin;
  await connectHost();
  if (!developmentConnectionMode) hideTap();
  if (!connected) {
    showIntro();
    status.className = 'overlay-status err';
    status.textContent = 'connection failed';
  }
}

// ─── Client ───────────────────────────────────────────────────────────────────
function updateAudioUI() {
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
      available: audioAvailable,
      muted: audioMuted,
      state: audioState,
      detail: audioStateDetail,
    });
    toggle.textContent = presentation.glyph;
    toggle.classList.toggle('active', audioAvailable && !audioMuted);
    toggle.classList.toggle('disabled', !audioAvailable);
    toggle.setAttribute('aria-disabled', audioAvailable ? 'false' : 'true');
    toggle.setAttribute('aria-label', presentation.ariaLabel);
    toggle.title = presentation.title;
  }
  if (status) {
    status.textContent = audioMuted && audioAvailable ? 'muted' : audioState;
    status.title = audioStateDetail;
  }
  if (webviewReceived) webviewReceived.textContent = `${audioPacketsReceived} packets`;
  if (transportMissing) transportMissing.textContent = `${audioTransportDropped} packets`;
  if (ipcDropped) ipcDropped.textContent = `${audioFrontendDropped} packets`;
  if (decoderDropped) decoderDropped.textContent = `${audioDecoderDropped} packets`;
  if (pcmHandoffDropped) {
    const droppedBlocks = activeAudioSession?.messageTracker.droppedMessages ?? 0;
    pcmHandoffDropped.textContent = `${droppedBlocks} blocks`;
  }
  if (ringOverflow) {
    const droppedFrames = activeAudioSession?.workletDroppedFrames ?? 0;
    ringOverflow.textContent = `${(droppedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (ringRecoveryDiscarded) {
    const droppedFrames = activeAudioSession?.workletRecoveryDiscardedFrames ?? 0;
    ringRecoveryDiscarded.textContent = `${(droppedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (ringBuffered) {
    ringBuffered.textContent = `${(audioBufferedFrames * 1000 / AUDIO_DECODER_CONFIG.sampleRate).toFixed(1)} ms`;
  }
  if (underruns) {
    underruns.textContent = `${audioUnderflows} events / ${(audioUnderflowDurationMicros / 1000).toFixed(1)} ms`;
  }
  if (silence) silence.textContent = `${(audioSilentDurationMicros / 1000).toFixed(1)} ms`;
}

function setAudioState(state, detail = state) {
  audioState = state;
  audioStateDetail = detail;
  updateAudioUI();
}

function resetAudioTelemetry() {
  audioPacketsReceived = 0;
  audioDecoderDropped = 0;
  audioBufferedFrames = 0;
  audioUnderflows = 0;
  audioUnderflowDurationMicros = 0;
  audioSilentDurationMicros = 0;
  audioTransportDropped = 0;
  audioFrontendDropped = 0;
  resetAvSyncTelemetry(activeAudioSession);
  updateAudioUI();
}

function configureAudioDecoder(session = activeAudioSession) {
  if (audioDecoder) {
    try { audioDecoder.close(); } catch (_) {}
  }
  audioDecoder = new AudioDecoder({
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
  audioDecoder.configure(AUDIO_DECODER_CONFIG);
}

function primeAudioOutputForActivation() {
  const AudioContextConstructor = window.AudioContext || window.webkitAudioContext;
  if (typeof AudioContextConstructor !== 'function') return;
  if (!audioContext || audioContext.state === 'closed') {
    try {
      audioContext = new AudioContextConstructor({ latencyHint: 'interactive', sampleRate: 48000 });
    } catch (_) {
      return;
    }
  }
  // This function is deliberately synchronous and is called before the first
  // await in a click/keyboard connect handler, preserving WebKit user activation.
  void audioContext.resume().catch(() => {});
}

async function initializeAudioPipeline(session) {
  if (audioDecoder) {
    try { audioDecoder.close(); } catch (_) {}
    audioDecoder = null;
  }
  if (audioWorkletNode) {
    try { audioWorkletNode.disconnect(); } catch (_) {}
    audioWorkletNode = null;
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
  if (!audioContext) {
    return { supported: false, error: 'Web Audio output is unavailable' };
  }
  try {
    if (audioContext.sampleRate !== 48000) {
      throw new Error(`audio output opened at ${audioContext.sampleRate} Hz instead of 48000 Hz`);
    }
    await audioContext.audioWorklet.addModule(new URL('./audio-worklet.js', import.meta.url));
    audioWorkletNode = new AudioWorkletNode(audioContext, 'sigil-audio-processor', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [2],
    });
    session.workletNode = audioWorkletNode;
    audioWorkletNode.port.onmessage = (event) => {
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
        audioBufferedFrames = Number.isSafeInteger(event.data.bufferedFrames)
          ? event.data.bufferedFrames : audioBufferedFrames;
        audioUnderflows = Number.isSafeInteger(event.data.underflows)
          ? event.data.underflows : audioUnderflows;
        audioUnderflowDurationMicros = Number.isFinite(event.data.underflowDurationMicros)
          && event.data.underflowDurationMicros >= 0
          ? event.data.underflowDurationMicros : audioUnderflowDurationMicros;
        audioSilentDurationMicros = Number.isFinite(event.data.silentDurationMicros)
          && event.data.silentDurationMicros >= 0
          ? event.data.silentDurationMicros : audioSilentDurationMicros;
        if (audioAvailable && !audioMuted) {
          setAudioState(event.data.started ? 'playing' : 'priming', 'bounded Opus playback');
        } else {
          updateAudioUI();
        }
      } else if (event.data?.type === 'error') {
        disableAudioForSession(`AudioWorklet failed: ${event.data.error}`, session);
      }
    };
    audioWorkletNode.port.onmessageerror = () => {
      disableAudioForSession('AudioWorklet returned an unreadable message', session);
    };
    audioWorkletNode.onprocessorerror = () => {
      disableAudioForSession('AudioWorklet processor stopped unexpectedly', session);
    };
    audioWorkletNode.connect(audioContext.destination);
    audioWorkletNode.port.postMessage({ type: 'mute', muted: audioMuted });
    configureAudioDecoder(session);
    await audioContext.resume();
    setAudioState(
      audioContext.state === 'running' ? 'negotiating' : 'blocked',
      audioContext.state === 'running'
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
  resetAvSyncTelemetry(session);
  activeAudioSession = null;
  activeAudioChannel = null;
  audioAvailable = false;
  if (session) {
    session.expectedGeneration = null;
    session.messageTracker.clear();
    session.workletNode = null;
  }
  if (audioDecoder) {
    try { audioDecoder.close(); } catch (_) {}
    audioDecoder = null;
  }
  if (audioWorkletNode) {
    try { audioWorkletNode.port.postMessage({ type: 'clear' }); } catch (_) {}
    try { audioWorkletNode.disconnect(); } catch (_) {}
    audioWorkletNode = null;
  }
  const context = audioContext;
  audioContext = null;
  if (context && context.state !== 'closed') {
    try { await context.close(); } catch (_) {}
  }
  audioBufferedFrames = 0;
  if (resetStatus) setAudioState('unavailable', 'not connected');
}

async function toggleAudioMute() {
  if (!audioAvailable) return;
  audioMuted = !audioMuted;
  try {
    if (!audioMuted && audioContext?.state !== 'running') await audioContext?.resume();
    audioWorkletNode?.port.postMessage({ type: 'mute', muted: audioMuted });
    if (!audioMuted && audioContext?.state !== 'running') {
      setAudioState('blocked', 'WebKit suspended audio output; click the audio button to retry');
    } else {
      updateAudioUI();
    }
  } catch (error) {
    setAudioState('error', `Audio output activation failed: ${error}`);
  }
}

async function connectHost() {
  if (connecting || connected) return;
  connecting = true;
  let rustConnectionCommitted = false;
  let pointerSession = null;
  let frameSession = null;
  try {
    const pin = document.getElementById('pin-input').value.trim();
    if (!developmentConnectionMode && !pin) {
      setStatus('err', 'enter pin');
      return;
    }
    primeAudioOutputForActivation();
    setStatus('pending', 'connecting...');
    // Reset before opening the channel so early frames cannot race a
    // post-connect reset and disappear from the live diagnostics.
    if (activeFrameSession) activeFrameSession.closing = true;
    activeFrameSession = null;
    activeFrameGeneration = null;
    resetStreamTelemetry();
    pointerSurfaceDimensions = null;
    remotePointerPosition = null;
    remotePointerVisible = false;
    const Channel = window.__TAURI__.core.Channel;
    if (typeof Channel !== 'function') throw new Error('Tauri binary channels are unavailable');
    resetAudioTelemetry();
    const audioSession = newAudioSession();
    frameSession = newFrameSession();
    pointerSession = newPointerSession();
    activeAudioSession = audioSession;
    activeFrameSession = frameSession;
    activePointerSession = pointerSession;
    const audioSupport = await initializeAudioPipeline(audioSession);
    if (!audioSupport.supported) setAudioState('unavailable', audioSupport.error);
    activeFrameChannel = new Channel((message) => handleBinaryFrameMessage(message, frameSession));
    activeAudioChannel = new Channel((message) => handleBinaryAudioMessage(message, audioSession));
    activePointerChannel = new Channel(
      (message) => handlePointerPositionFeedback(message, pointerSession),
    );
    const result = await invoke('iroh_client_connect', {
      pin,
      frameChannel: activeFrameChannel,
      audioChannel: activeAudioChannel,
      pointerChannel: activePointerChannel,
      audioSupported: audioSupport.supported,
    });
    rustConnectionCommitted = committedRustConnection(result);
    if (result.connected
      && (!Number.isSafeInteger(result.media_generation) || result.media_generation <= 0)) {
      throw new Error('host returned an invalid media generation');
    }
    if (result.connected
      && result.audio_available === true
      && (!Number.isSafeInteger(result.audio_generation) || result.audio_generation <= 0)) {
      throw new Error('host returned an invalid audio generation');
    }
    if (pointerSession.failed) {
      throw new Error(pointerSession.failureDetail || 'host returned invalid pointer-position feedback');
    }
    if (frameSession.failed) {
      throw new Error(frameSession.failureDetail || 'host returned invalid frame data');
    }
    const connectedPointerSurfaceDimensions = result.connected
      ? validatePointerSurfaceDimensions(result.pointer_surface_dimensions)
      : null;
    const pointerFeedbackAvailable = result.connected
      && result.pointer_position_feedback_available === true;
    if (pointerFeedbackAvailable && connectedPointerSurfaceDimensions === null) {
      throw new Error('host offered pointer feedback without a native pointer surface');
    }
    const initialPointerFeedback = pointerFeedbackAvailable && pointerSession.received
      ? validatePointerPositionFeedback(pointerSession.latest, connectedPointerSurfaceDimensions)
      : null;
    if (result.connected) {
      activateConnectedFrameSession(frameSession, result.media_generation);
      connected = true;
      disconnecting = false;
      inputCapabilities = {
        relativePointer: result.relative_pointer_available === true,
        pointerPositionFeedback: pointerFeedbackAvailable,
        absolutePointer: result.absolute_pointer_available === true,
        keyboard: result.keyboard_available === true,
        text: result.text_available === true,
        gamepad: result.gamepad_available === true,
        control: result.control_available === true,
      };
      streamTransportMode = [
        'independent-v2',
        'reliable-v1',
        'reliable-v0',
      ].includes(result.media_transport)
        ? result.media_transport
        : 'unknown';
      pointerSurfaceDimensions = connectedPointerSurfaceDimensions;
      if (!inputCapabilities.pointerPositionFeedback) {
        remotePointerPosition = null;
        remotePointerVisible = false;
      } else if (initialPointerFeedback !== null) {
        remotePointerPosition = initialPointerFeedback.position;
        remotePointerVisible = initialPointerFeedback.pointer_visible;
      }
      // Treat inconsistent metadata as view-only. Rust reports every
      // fields explicitly; this keeps the webview fail-closed as well.
      inputCapabilities.control = inputCapabilities.control
        && (inputCapabilities.relativePointer
          || inputCapabilities.absolutePointer
          || inputCapabilities.keyboard
          || inputCapabilities.text
          || inputCapabilities.gamepad);
      controlMode = false;
      clearPendingInput();
      const connectionStatus = [
        'connected',
        inputCapabilities.control ? null : 'view only',
        result.development_mode ? 'dev direct-node' : null,
      ].filter(Boolean).join(' · ');
      setStatus('ok', connectionStatus);
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
      audioAvailable = result.audio_available === true;
      if (audioAvailable && audioSession.failed) {
        disableAudioForSession(audioSession.failureDetail || 'Audio output failed', audioSession);
      } else if (audioAvailable) {
        setAudioState(
          audioContext?.state === 'running' ? 'priming' : 'blocked',
          audioContext?.state === 'running'
            ? 'Waiting for bounded Opus prebuffer'
            : 'WebKit suspended audio output; activate the audio button to retry',
        );
      } else {
        await teardownAudioPipeline(false);
        setAudioState('unavailable', result.audio_error || audioSupport.error || 'host audio unavailable');
      }
      waitingForDecoderKeyframe = true;
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
      rustConnectionCommitted = false;
    } else {
      frameSession.closing = true;
      activeFrameSession = null;
      activeFrameGeneration = null;
      activeFrameChannel = null;
      activePointerChannel = null;
      activePointerSession = null;
      await teardownAudioPipeline();
      setStatus('err', 'failed');
    }
  } catch (e) {
    if (frameSession) frameSession.closing = true;
    if (activeFrameSession === frameSession) activeFrameSession = null;
    activeFrameGeneration = null;
    if (rustConnectionCommitted) {
      // Rust commits its active-connection guard before returning. A client-side
      // validation failure must close that exact committed attempt directly;
      // calling disconnect() here would race its UI teardown recursively.
      if (pointerSession) pointerSession.closing = true;
      try {
        await disconnectRejectedRustConnection(invoke, rustConnectionCommitted);
      } catch (disconnectError) {
        console.error('failed to close rejected connection:', disconnectError);
      }
    }
    activeFrameChannel = null;
    activePointerChannel = null;
    activePointerSession = null;
    await teardownAudioPipeline();
    console.error('connection failed:', e);
    setStatus('err', 'error');
  } finally {
    connecting = false;
  }
}

async function disconnect() {
  if (disconnecting) return;
  disconnecting = true;
  if (activeFrameSession) activeFrameSession.closing = true;
  activeFrameSession = null;
  activeFrameGeneration = null;
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
  try {
    await invoke('iroh_client_disconnect');
  } catch (e) { console.error(e); }
  activeFrameChannel = null;
  activePointerChannel = null;
  activePointerSession = null;
  await teardownAudioPipeline();
  teardownDecoderPipeline();
  connected = false;
  controlMode = false;
  inputCapabilities = {
    relativePointer: false,
    pointerPositionFeedback: false,
    absolutePointer: false,
    keyboard: false,
    text: false,
    gamepad: false,
    control: false,
  };
  pointerSurfaceDimensions = null;
  fittedStreamDimensions = null;
  lastObservedWindowSize = null;
  if (streamWindowResizeTimer !== null) clearTimeout(streamWindowResizeTimer);
  streamWindowResizeTimer = null;
  remotePointerPosition = null;
  remotePointerVisible = false;
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
  if (!developmentConnectionMode) {
    document.getElementById('pin-label').style.display = '';
    document.getElementById('pin-input').style.display = '';
    document.getElementById('controller-pin-main').style.display = '';
  }
  document.getElementById('intro-status').textContent = '';
  disconnecting = false;
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
let videoDecoder = null;
let decoderConfigured = false;

(async function detectWebCodecs() {
  await invoke('set_webcodecs_available', { available: hasWebCodecs });
})();

// ─── Frame decoding ───────────────────────────────────────────────────────────
let activeCodec = 'h264';
let droppedFrames = 0;
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
let receivedFrames = 0;
let decoderInputFrames = 0;
let decoderOutputFrames = 0;
let presentedFrames = 0;
let presentationDroppedFrames = 0;
let lastTransportFps = 0;
let lastFrontendSendFps = 0;
const frontendDeliveryRate = new RollingRateWindow();
const decoderInputRate = new RollingRateWindow();
const decoderOutputRate = new RollingRateWindow();
const presentationRate = new RollingRateWindow();
const frontendDeliveryCadence = new BoundedCadenceWindow();
const decoderInputCadence = new BoundedCadenceWindow();
const decoderOutputCadence = new BoundedCadenceWindow();
const presentationCadence = new BoundedCadenceWindow();
const decodeLatency = new BoundedLatencyWindow();
const clientPresentationLatency = new BoundedLatencyWindow();
const drawLatency = new BoundedLatencyWindow();
const avSkew = new BoundedValueWindow();
const decodeTimings = new Map();
const MAX_DECODE_TIMINGS = 8;
let streamPathMode = 'unknown';
let streamTransportMode = 'unknown';
let streamRttMs = null;
const MAX_DECODE_QUEUE_SIZE = 2;

function requestDecoderKeyframe(reason) {
  if (!Number.isSafeInteger(activeFrameGeneration) || activeFrameGeneration < 1) return;
  void invoke('iroh_client_request_keyframe', {
    generation: activeFrameGeneration,
    reason,
  }).catch((error) => {
    console.warn(`keyframe request failed (${reason}):`, error);
  });
}

const decoderRecovery = new DecoderRecoveryState({
  onKeyframeRequest: requestDecoderKeyframe,
});

function enterDecoderRecovery(reason, { restart = false } = {}) {
  const result = restart ? decoderRecovery.restart(reason) : decoderRecovery.enter(reason);
  // Closing WebCodecs and clearing the latest-frame presenter is part of the
  // recovery boundary. Do it immediately rather than allowing already queued
  // frames from a poisoned GOP to continue reaching the canvas.
  teardownDecoderPipeline();
  return result;
}

function finishVideoChunkEnqueue(keyframe, succeeded) {
  if (succeeded) {
    if (keyframe) decoderRecovery.confirmKeyframeEnqueued(true);
    return;
  }
  // A synchronous decode rejection is a decoder-error episode if playback
  // was previously healthy. If recovery is already active, enter() coalesces
  // with its existing request and leaves the keyframe gate closed.
  enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
}

function resetAvSyncTelemetry(session = activeAudioSession, { recoverRing = false } = {}) {
  avSkew.reset();
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
    || !audioAvailable
    || audioContext?.state !== 'running'
    || typeof audioContext.getOutputTimestamp !== 'function'
  ) return;

  let outputTimestamp;
  try {
    outputTimestamp = audioContext.getOutputTimestamp();
  } catch (_) {
    return;
  }
  const audioPtsMicros = projectAudioMediaPtsMicros(
    session.audioTimeline,
    outputTimestamp,
    presentationTimeMs,
  );
  const skewMs = audioMinusVideoSkewMs(audioPtsMicros, videoPtsMicros);
  if (skewMs !== null) avSkew.record(skewMs, presentationTimeMs);
}

const canvas = document.getElementById('frame-canvas');
const remotePointer = document.getElementById('remote-pointer');
const ctx = canvas.getContext('2d');
let lastVideoDrawCompletedAtMs = null;
const framePresenter = new LatestFramePresenter({
  requestFrame: (callback) => requestAnimationFrame(callback),
  cancelFrame: (handle) => cancelAnimationFrame(handle),
  setTimer: (callback, delayMs) => setTimeout(callback, delayMs),
  cancelTimer: (handle) => clearTimeout(handle),
  now: () => performance.now(),
  fallbackDelayMs: 25,
  draw: (frame) => {
    const startedAt = performance.now();
    ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
    const completedAt = performance.now();
    drawLatency.record(completedAt - startedAt, completedAt);
    lastVideoDrawCompletedAtMs = completedAt;
  },
  onPresent: (metadata, animationFrameTime) => {
    const now = lastVideoDrawCompletedAtMs ?? animationFrameTime;
    lastVideoDrawCompletedAtMs = null;
    presentedFrames++;
    presentationRate.record(now);
    presentationCadence.record(now);
    if (metadata && Number.isFinite(metadata.receivedAt)) {
      clientPresentationLatency.record(now - metadata.receivedAt, now);
    }
    if (metadata) sampleAvSkew(metadata.mediaPtsMicros, now);
  },
  onDrop: () => { presentationDroppedFrames++; },
});

function teardownDecoderPipeline() {
  resetAvSyncTelemetry();
  framePresenter.clear();
  decodeTimings.clear();
  if (videoDecoder) {
    try { videoDecoder.close(); } catch (_) {}
  }
  videoDecoder = null;
  decoderConfigured = false;
}

function resetStreamTelemetry() {
  teardownDecoderPipeline();
  decoderRecovery.reset();
  droppedFrames = 0;
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
  receivedFrames = 0;
  decoderInputFrames = 0;
  decoderOutputFrames = 0;
  presentedFrames = 0;
  presentationDroppedFrames = 0;
  lastTransportFps = 0;
  lastFrontendSendFps = 0;
  frontendDeliveryRate.reset();
  decoderInputRate.reset();
  decoderOutputRate.reset();
  presentationRate.reset();
  frontendDeliveryCadence.reset();
  decoderInputCadence.reset();
  decoderOutputCadence.reset();
  presentationCadence.reset();
  decodeLatency.reset();
  clientPresentationLatency.reset();
  drawLatency.reset();
  streamPathMode = 'unknown';
  streamTransportMode = 'unknown';
  streamRttMs = null;
}

function enqueueVideoChunk(chunk, receivedAt, codecLabel, mediaPtsMicros = null) {
  if (!videoDecoder) return false;
  const enqueuedAt = performance.now();
  while (decodeTimings.size >= MAX_DECODE_TIMINGS) {
    decodeTimings.delete(decodeTimings.keys().next().value);
  }
  decodeTimings.set(chunk.timestamp, { receivedAt, enqueuedAt, mediaPtsMicros });
  try {
    videoDecoder.decode(chunk);
    decoderInputFrames++;
    decoderInputRate.record(enqueuedAt);
    decoderInputCadence.record(enqueuedAt);
    return true;
  } catch (error) {
    decodeTimings.delete(chunk.timestamp);
    droppedFrames++;
    console.warn(`${codecLabel} decode failed:`, error);
    return false;
  }
}

function initWebCodecsDecoder(width, height, desc, codecStr) {
  teardownDecoderPipeline();
  console.log('WebCodecs configure:', codecStr, 'w:', width, 'h:', height, 'desc:', desc.byteLength, 'bytes');
  videoDecoder = new VideoDecoder({
    output: (frame) => {
      const now = performance.now();
      decoderOutputFrames++;
      decoderOutputRate.record(now);
      decoderOutputCadence.record(now);
      const timing = decodeTimings.get(frame.timestamp) ?? null;
      decodeTimings.delete(frame.timestamp);
      if (timing) decodeLatency.record(now - timing.enqueuedAt, now);
      framePresenter.enqueue(frame, timing);
    },
    error: (e) => {
      console.error('VideoDecoder error:', e);
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
    }
  });
  try {
    videoDecoder.configure(buildVideoDecoderConfig({
      codec: codecStr,
      width,
      height,
      description: desc,
    }));
    decoderConfigured = true;
    return true;
  } catch (error) {
    console.warn('WebCodecs configure failed:', error);
    enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
    return false;
  }
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
  const frontendDeliveryFps = frontendDeliveryRate.rate(now);
  const decodeFps = decoderInputRate.rate(now);
  const decoderOutputFps = decoderOutputRate.rate(now);
  const presentFps = presentationRate.rate(now);
  const decodePercentiles = decodeLatency.summary(now);
  const presentPercentiles = clientPresentationLatency.summary(now);
  const drawPercentiles = drawLatency.summary(now);
  const deliveryCadence = frontendDeliveryCadence.summary(now);
  const decoderInputIntervals = decoderInputCadence.summary(now);
  const decoderOutputIntervals = decoderOutputCadence.summary(now);
  const presentationIntervals = presentationCadence.summary(now);
  const avSkewPercentiles = avSkew.summary(now);
  document.getElementById('stream-received').textContent = receivedFrames;
  document.getElementById('stream-decoder-input').textContent = decoderInputFrames;
  document.getElementById('stream-decoder-output').textContent = decoderOutputFrames;
  document.getElementById('stream-presented').textContent = presentedFrames;
  document.getElementById('stream-decode-queue').textContent = videoDecoder?.decodeQueueSize ?? 0;
  document.getElementById('stream-present-queue').textContent = framePresenter.depth;
  const discardTelemetry = formatVideoDiscardTelemetry({
    transportDroppedFrames,
    frontendDroppedFrames,
    decoderDroppedFrames: droppedFrames,
    presenterOverwrittenFrames: presentationDroppedFrames,
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
  document.getElementById('stream-frontend-fps').textContent = frontendDeliveryFps.toFixed(1);
  document.getElementById('stream-decode-fps').textContent = decodeFps.toFixed(1);
  document.getElementById('stream-decoder-output-fps').textContent = decoderOutputFps.toFixed(1);
  document.getElementById('stream-present-fps').textContent = presentFps.toFixed(1);
  document.getElementById('stream-av-skew').textContent = avSkewPercentiles.count
    ? `${formatSignedMilliseconds(avSkewPercentiles.p50)} / ${formatSignedMilliseconds(avSkewPercentiles.p95)} / ${avSkewPercentiles.maxAbsolute.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-decode-latency').textContent = decodePercentiles.count
    ? `${decodePercentiles.p50.toFixed(1)} / ${decodePercentiles.p95.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-present-latency').textContent = presentPercentiles.count
    ? `${presentPercentiles.p50.toFixed(1)} / ${presentPercentiles.p95.toFixed(1)} ms`
    : '—';
  document.getElementById('stream-draw-latency').textContent = drawPercentiles.count
    ? `${drawPercentiles.p50.toFixed(2)} / ${drawPercentiles.p95.toFixed(2)} / ${drawPercentiles.p99.toFixed(2)} / ${drawPercentiles.max.toFixed(2)} ms`
    : '—';
  document.getElementById('stream-delivery-cadence').textContent = formatCadence(deliveryCadence);
  document.getElementById('stream-decoder-input-cadence').textContent = formatCadence(decoderInputIntervals);
  document.getElementById('stream-decoder-output-cadence').textContent = formatCadence(decoderOutputIntervals);
  document.getElementById('stream-present-cadence').textContent = formatCadence(presentationIntervals);
  document.getElementById('stream-codec').textContent = activeCodec;
  document.getElementById('stream-transport').textContent = streamTransportMode;
  document.getElementById('stream-path').textContent = streamPathMode;
  document.getElementById('stream-rtt').textContent = Number.isFinite(streamRttMs)
    ? `${streamRttMs.toFixed(1)} ms`
    : '— ms';
}

setInterval(() => {
  if (connected) updateStreamStats();
}, 250);

function clientChromeHeight() {
  const topbar = document.querySelector('.topbar');
  const bottombar = document.querySelector('.bottombar');
  return Math.max(0, (topbar?.offsetHeight ?? 0) + (bottombar?.offsetHeight ?? 0));
}

function sizeCanvasToIncomingStream() {
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
  if (!connected || frameWidth < 1 || frameHeight < 1) return false;
  const dimensions = `${frameWidth}x${frameHeight}`;
  if (fittedStreamDimensions === dimensions) return false;
  const geometry = fitInitialStreamWindow({
    frameWidth,
    frameHeight,
    chromeHeight: clientChromeHeight(),
    availableWidth: window.screen.availWidth,
    availableHeight: window.screen.availHeight,
  });
  const applied = await applyClientWindowGeometry(geometry, true);
  if (applied) fittedStreamDimensions = dimensions;
  return applied;
}

function scheduleStreamWindowAspectCorrection() {
  const observed = { width: window.innerWidth, height: window.innerHeight };
  const previous = lastObservedWindowSize ?? observed;
  lastObservedWindowSize = observed;
  if (streamWindowResizeTimer !== null) clearTimeout(streamWindowResizeTimer);
  streamWindowResizeTimer = setTimeout(() => {
    streamWindowResizeTimer = null;
    if (!connected || frameWidth < 1 || frameHeight < 1) return;
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

function processFramePayload(payload, binaryData = null) {
  const receivedAt = performance.now();
  frontendDeliveryRate.record(receivedAt);
  frontendDeliveryCadence.record(receivedAt);
  const { width, height, data, codec, keyframe, pts_micros: ptsMicros, discontinuity } = payload;
  if (discontinuity) resetAvSyncTelemetry();
  const dimensionsChanged = frameWidth !== width || frameHeight !== height;
  frameWidth = width;
  frameHeight = height;
  if (canvas.width !== width) canvas.width = width;
  if (canvas.height !== height) canvas.height = height;
  if (dimensionsChanged) sizeCanvasToIncomingStream();
  if (dimensionsChanged && connected) void fitWindowToIncomingStream();

  if (codec && codec !== activeCodec) {
    activeCodec = codec;
    decoderConfigured = false;
    enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
    console.log('codec changed:', activeCodec);
  }

  receivedFrames++;

  if (hasWebCodecs) {
    // Binary channels provide an exact view over the IPC ArrayBuffer. The
    // base64 branch only supports an older in-process sender during migration.
    const raw = binaryData ?? Uint8Array.from(atob(data), c => c.charCodeAt(0));
    const decoderBacklogged = videoDecoder?.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE;
    if (discontinuity) {
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DISCONTINUITY);
    } else if (decoderBacklogged) {
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.FRONTEND_BACKPRESSURE);
    }
    if (decoderRecovery.shouldDropFrame({ keyframe })) {
      if (!decoderRecovery.requestIssued) {
        enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
      }
      droppedFrames++;
      return;
    }
    const mediaPtsMicros = exactMediaTimestampMicros(ptsMicros);
    const chunkTimestamp = mediaPtsMicros !== null
      ? mediaPtsMicros
      : performance.now() * 1000;

    if (activeCodec === 'av1') {
      // ── AV1 path ──
      const obus = parseAv1Obus(raw);
      if (keyframe) {
        const seqHeader = obus.find(o => o.type === 12);
        if (seqHeader && !decoderConfigured) {
          initWebCodecsDecoder(width, height, buildAv1Description(seqHeader.data), av1CodecStr(seqHeader.data));
        }
      }
      if (!decoderConfigured || !videoDecoder) { droppedFrames++; return; }
      const frameObus = obus.filter(o => o.type !== 2 && o.type !== 12);
      if (frameObus.length > 0) {
        let totalLen = 0;
        for (const o of frameObus) totalLen += o.data.length;
        const chunkData = new Uint8Array(totalLen);
        let off = 0;
        for (const o of frameObus) { chunkData.set(o.data, off); off += o.data.length; }
        const chunk = new EncodedVideoChunk({
          type: keyframe ? 'key' : 'delta',
          timestamp: chunkTimestamp,
          data: chunkData,
        });
        const enqueued = enqueueVideoChunk(chunk, receivedAt, 'av1', mediaPtsMicros);
        finishVideoChunkEnqueue(keyframe, enqueued);
      }

    } else if (activeCodec === 'h265') {
      // ── H.265 path ──
      const nals = parseAnnexBNals(raw);
      if (keyframe) {
        let vps = null, sps = null, pps = null;
        for (const nal of nals) {
          const t = hevcNalType(nal);
          if (t === 32) vps = nal;
          else if (t === 33) sps = nal;
          else if (t === 34) pps = nal;
        }
        if (vps && sps && pps && !decoderConfigured) {
          initWebCodecsDecoder(width, height, buildHvcDescription(vps, sps, pps), hevcCodecStr(sps));
        }
      }
      if (!decoderConfigured || !videoDecoder) { droppedFrames++; return; }
      const sliceNals = nals.filter(nal => {
        const t = hevcNalType(nal);
        return t !== 32 && t !== 33 && t !== 34 && t !== 35;
      });
      if (sliceNals.length > 0) {
        const chunk = new EncodedVideoChunk({
          type: keyframe ? 'key' : 'delta',
          timestamp: chunkTimestamp,
          data: nalsToLengthPrefixed(sliceNals),
        });
        const enqueued = enqueueVideoChunk(chunk, receivedAt, 'hevc', mediaPtsMicros);
        finishVideoChunkEnqueue(keyframe, enqueued);
      }

    } else {
      // ── H.264 path (default) ──
      const nals = parseAnnexBNals(raw);
      if (keyframe) {
        let sps = null, pps = null;
        for (const nal of nals) {
          const t = h264NalType(nal);
          if (t === 7) sps = nal;
          else if (t === 8) pps = nal;
        }
        if (sps && pps && !decoderConfigured) {
          initWebCodecsDecoder(width, height, buildAvcDescription(sps, pps), avcCodecStr(sps));
        }
      }
      if (!decoderConfigured || !videoDecoder) { droppedFrames++; return; }
      const sliceNals = nals.filter(nal => {
        const t = h264NalType(nal);
        return t !== 7 && t !== 8 && t !== 9;
      });
      if (sliceNals.length > 0) {
        const chunk = new EncodedVideoChunk({
          type: keyframe ? 'key' : 'delta',
          timestamp: chunkTimestamp,
          data: nalsToLengthPrefixed(sliceNals),
        });
        const enqueued = enqueueVideoChunk(chunk, receivedAt, 'h264', mediaPtsMicros);
        finishVideoChunkEnqueue(keyframe, enqueued);
      }
    }
  } else {
    // JPEG fallback (openh264 decode → JPEG encode in Rust)
    const bytes = Uint8Array.from(atob(data), c => c.charCodeAt(0));
    const blob = new Blob([bytes], { type: 'image/jpeg' });
    const url = URL.createObjectURL(blob);
    const img = new Image();
    img.onload = () => {
      const drawStartedAt = performance.now();
      ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
      URL.revokeObjectURL(url);
      const now = performance.now();
      presentedFrames++;
      presentationRate.record(now);
      presentationCadence.record(now);
      drawLatency.record(now - drawStartedAt, now);
      clientPresentationLatency.record(now - receivedAt, now);
    };
    img.src = url;
  }
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
      processFramePayload(payload);
    }
  }
}

function failFrameSession(session, error) {
  if (!isActiveFrameSession(session, activeFrameSession)) return;
  session.failed = true;
  session.failureDetail = `Frame delivery failed: ${error}`;
  console.error(session.failureDetail);
  if (connected) void disconnect();
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
    processFramePayload({
      width: frame.width,
      height: frame.height,
      data: null,
      codec: frame.codec,
      keyframe: frame.keyframe,
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
  resetAvSyncTelemetry(session);
  session.failed = true;
  session.failureDetail = detail;
  audioAvailable = false;
  if (audioDecoder) {
    try { audioDecoder.close(); } catch (_) {}
    audioDecoder = null;
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
    audioPacketsReceived++;
    if (!audioDecoder || audioDecoder.state === 'closed') {
      audioDecoderDropped++;
      updateAudioUI();
      return;
    }
    const queuedAudioPackets = audioDecoder.decodeQueueSize;
    if (packet.discontinuity || queuedAudioPackets >= MAX_AUDIO_DECODE_QUEUE_SIZE) {
      resetAvSyncTelemetry(session, { recoverRing: true });
      audioDecoderDropped += queuedAudioPackets;
      configureAudioDecoder(session);
    }
    audioDecoder.decode(new EncodedAudioChunk({
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
    if (delivery.accepted) processFramePayload(event.payload);
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
  streamPathMode = typeof event.payload.path_mode === 'string'
    ? event.payload.path_mode
    : streamPathMode;
  streamRttMs = Number.isFinite(event.payload.path_rtt_ms)
    ? event.payload.path_rtt_ms
    : streamRttMs;
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
  if (disconnecting || !session || event.payload?.available !== false) return;
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
  if (!isCurrentAudioGeneration(
    event.payload?.generation,
    activeAudioSession?.expectedGeneration,
  )) return;
  audioTransportDropped = Number.isSafeInteger(event.payload?.sequence_dropped_total)
    ? event.payload.sequence_dropped_total : audioTransportDropped;
  audioFrontendDropped = Number.isSafeInteger(event.payload?.frontend_dropped_total)
    ? event.payload.frontend_dropped_total : audioFrontendDropped;
  updateAudioUI();
});

// ─── Input ────────────────────────────────────────────────────────────────────
async function toggleControl() {
  if (!connected || !inputCapabilities.control || controlTransitionInProgress) return;
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
    if (!acquired || disconnecting || !connected || !inputCapabilities.control) {
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
  if (!connected || !inputCapabilities.gamepad) return false;
  return sendInput({ t: 'gp', state: neutralGamepadInputState() });
}

function describeInputCapabilities() {
  const accepted = [];
  if (inputCapabilities.relativePointer) accepted.push('relative pointer');
  else if (inputCapabilities.absolutePointer) accepted.push('pointer');
  if (inputCapabilities.keyboard) accepted.push('keyboard');
  if (inputCapabilities.text) accepted.push('text');
  if (inputCapabilities.gamepad) accepted.push('gamepad');
  return accepted.length > 0 ? accepted.join(' + ') : 'view only';
}

function pointerInputAvailable() {
  return inputCapabilities.relativePointer || inputCapabilities.absolutePointer;
}

async function requestRelativePointerLock() {
  if (!inputCapabilities.relativePointer) return true;
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
  return resolvePointerSurfaceSize(
    pointerSurfaceDimensions,
    frameWidth,
    frameHeight,
    inputCapabilities.relativePointer,
  );
}

function renderRemotePointer() {
  const surface = currentPointerSurfaceSize();
  const visible = controlMode
    && inputCapabilities.relativePointer
    && inputCapabilities.pointerPositionFeedback
    && remotePointerPosition !== null
    && remotePointerVisible
    && surface !== null;
  remotePointer.classList.toggle('visible', visible);
  if (!visible) return;
  const canvasRect = canvas.getBoundingClientRect();
  const mainRect = canvas.parentElement.getBoundingClientRect();
  remotePointer.style.left = `${canvasRect.left - mainRect.left
    + (remotePointerPosition.x / surface.width) * canvasRect.width}px`;
  remotePointer.style.top = `${canvasRect.top - mainRect.top
    + (remotePointerPosition.y / surface.height) * canvasRect.height}px`;
}

function releasePointerLock() {
  const releaseGeneration = controlTransitionGeneration;
  void releaseNativeCursorGrab(releaseGeneration);
  if (document.pointerLockElement !== canvas || typeof document.exitPointerLock !== 'function') return;
  try { document.exitPointerLock(); } catch (_) {}
}

function updateControlUI() {
  const el = document.getElementById('control-toggle');
  const available = connected && inputCapabilities.control;
  if (!available) controlMode = false;
  el.textContent = available
    ? `${controlMode ? 'controlling' : 'take control'} · ${describeInputCapabilities()}${controlMode && inputCapabilities.relativePointer ? ' · Ctrl+Alt+Esc to exit' : controlMode && inputCapabilities.gamepad ? ' · hold Back+Start to exit' : ''}`
    : 'view only · input unavailable';
  el.classList.toggle('active', controlMode);
  el.classList.toggle('disabled', !available);
  el.setAttribute('aria-disabled', available ? 'false' : 'true');
  document.body.classList.toggle(
    'native-pointer-control',
    controlMode && inputCapabilities.relativePointer,
  );
  canvas.classList.toggle(
    'relative-control',
    controlMode && inputCapabilities.relativePointer,
  );
  renderRemotePointer();
  const streamControl = document.getElementById('stream-control');
  if (streamControl) {
    streamControl.textContent = available ? describeInputCapabilities() : 'view only · unavailable';
  }
}

function scaleCoords(clientX, clientY) {
  const rect = canvas.getBoundingClientRect();
  const scaleX = frameWidth / rect.width;
  const scaleY = frameHeight / rect.height;
  return {
    x: Math.round((clientX - rect.left) * scaleX),
    y: Math.round((clientY - rect.top) * scaleY),
  };
}

function mapKey(e) {
  const key = e.key;
  const map = {
    'ArrowUp': 'Up', 'ArrowDown': 'Down', 'ArrowLeft': 'Left', 'ArrowRight': 'Right',
    ' ': 'Space', 'Delete': 'Delete', 'Backspace': 'Backspace',
    'Enter': 'Enter', 'Tab': 'Tab', 'Escape': 'Escape',
    'Shift': 'Shift', 'Control': 'Control', 'Alt': 'Alt', 'Meta': 'Meta',
    'Home': 'Home', 'End': 'End', 'PageUp': 'PageUp', 'PageDown': 'PageDown',
  };
  const asciiPrintable = key.length === 1
    && key.codePointAt(0) >= 0x20
    && key.codePointAt(0) <= 0x7e;
  return map[key] || (asciiPrintable ? key : null);
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
  if (!inputCapabilities.control) return false;
  if (event.t === 'mm') return inputCapabilities.absolutePointer;
  if (event.t === 'mr') return inputCapabilities.relativePointer;
  if (event.t === 'mp') return inputCapabilities.relativePointer;
  if (['mc', 'md', 'mu', 'ms'].includes(event.t)) return pointerInputAvailable();
  if (['kd', 'ku', 'kt'].includes(event.t)) return inputCapabilities.keyboard;
  if (event.t === 'tx') return inputCapabilities.text;
  if (event.t === 'gp') return inputCapabilities.gamepad;
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
        if (connected) void disconnect();
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
          if (connected) void disconnect();
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
  if (inputCapabilities.relativePointer) {
    const movement = scaleRelativePointerDelta(
      e.movementX,
      e.movementY,
    );
    sendInput({ t: 'mr', ...movement });
    return;
  }
}, { capture: true });

canvas.addEventListener('mousemove', (e) => {
  if (!controlMode || !inputCapabilities.absolutePointer || inputCapabilities.relativePointer) return;
  const { x, y } = scaleCoords(e.clientX, e.clientY);
  sendInput({ t: 'mm', x, y });
});

function handleMouseDown(e) {
  if (!controlMode || !pointerInputAvailable()) return;
  if (inputCapabilities.relativePointer) e.stopPropagation();
  else if (e.target !== canvas) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  e.preventDefault();
  if (inputCapabilities.relativePointer && document.pointerLockElement !== canvas) {
    void requestBrowserPointerLock().catch((error) => {
      console.warn('browser pointer lock retry failed:', error);
    });
  }
  if (!heldInputs.trackMouseButton(btn)) return;
  if (!sendInput({ t: 'md', b: btn })) heldInputs.takeMouseButtonRelease(btn);
}

window.addEventListener('mousedown', handleMouseDown, { capture: true });

function releaseMouseButton(e) {
  if (!connected || !pointerInputAvailable()) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  const release = heldInputs.takeMouseButtonRelease(btn);
  if (!release) return;
  e.preventDefault();
  sendInput(release, { release: true });
}

window.addEventListener('mouseup', (e) => {
  if (controlMode && inputCapabilities.relativePointer) e.stopPropagation();
  releaseMouseButton(e);
}, { capture: true });

window.addEventListener('contextmenu', (e) => {
  if (!controlMode || !pointerInputAvailable()) return;
  if (!inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
}, { capture: true });

window.addEventListener('wheel', (e) => {
  if (!controlMode || !pointerInputAvailable()) return;
  if (!inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
  if (inputCapabilities.relativePointer) e.stopPropagation();
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
  if (printableText && inputCapabilities.text) {
    e.preventDefault();
    sendInput({ t: 'tx', s: e.key });
    return;
  }
  if (!inputCapabilities.keyboard) return;
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
  if (developmentConnectionMode) return;
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
  if (!connected || !controlMode || !inputCapabilities.gamepad) return;
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

  const remoteRoute = connected
    && controlMode
    && inputCapabilities.gamepad
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
  if (!controlMode || !inputCapabilities.relativePointer) return;
  // WebKit rebuilds its native cursor rectangles after the window-level focus
  // event. Reassert from the webview on the next frame so the final rectangle
  // is the transparent one used during relative control.
  requestAnimationFrame(() => {
    if (!controlMode || !inputCapabilities.relativePointer) return;
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
  if (!developmentConnectionMode) scanFido();
  requestAnimationFrame(pollControllers);
}

initialize();
