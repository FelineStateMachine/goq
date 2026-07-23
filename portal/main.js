import {
  mapCanvasPointToSurface,
  resolvePointerSurfaceSize,
  scaleRelativePointerDelta,
  validatePointerSurfaceDimensions,
  validatePointerPositionFeedback,
  browserMouseButtonCode,
} from './input-state.mjs';
import { createInputRuntime } from './input-runtime.mjs';
import { createControlRuntime } from './control-runtime.mjs';
import { formatVideoDiscardTelemetry } from './frame-stats.mjs';
import { networkDiagnosticsPresentation } from './network-diagnostics.mjs';
import {
  createPointerFeedbackRuntime,
  newPointerSession,
} from './pointer-feedback.mjs';
import { newConnectionState } from './connection-state.mjs';
import { formatSignedMilliseconds } from './av-sync.mjs';
import { audioButtonPresentation } from './audio-ui.mjs';
import {
  AUDIO_SAMPLE_RATE,
  createAudioPipelineSession,
} from './audio-pipeline.mjs';
import {
  chooseDirectionalIndex,
} from './controller-state.mjs';
import { createControllerRuntime } from './controller-runtime.mjs';
import { createWindowRuntime } from './window-runtime.mjs';
import {
  createVideoPipelineSession,
} from './video-pipeline.mjs';
import { detectAndPublishVideoDeliveryMode } from './video-capability.mjs';
import {
  formatAdaptiveDecision,
} from './adaptive-feedback.mjs';
import { createStreamRuntime } from './stream-runtime.mjs';
import {
  formatInvitationExpiry,
  grantLabel,
  normalizeInvitationSummary,
  shortPeerFingerprint,
} from './enrollment.mjs';
import { mapKey } from './keyboard-map.mjs';

let enrollmentReady = false;
let pendingInvitationSummary = null;
let currentEnrollmentStatus = null;
let activePointerSession = null;
let controllerActivationInProgress = false;
let controlRuntime = null;
let controllerRuntime = null;

const invoke = (...args) => window.__TAURI__.core.invoke(...args);
const listen = (...args) => window.__TAURI__.event.listen(...args);
const pointerFeedbackRuntime = createPointerFeedbackRuntime({
  getActiveSession: () => activePointerSession,
  getCapabilities: () => ({
    connected: connectionState.connected,
    pointerPositionFeedback: connectionState.inputCapabilities.pointerPositionFeedback,
  }),
  getSurface: currentPointerSurfaceSize,
  render: renderRemotePointer,
  disconnect,
  logger: console,
});
const inputRuntime = createInputRuntime({
  invokeCommand: invoke,
  getCapabilities: () => connectionState.inputCapabilities,
  isConnected: () => connectionState.connected,
  onFatal: () => { void disconnect(); },
  resetControllerActivation: () => controlRuntime?.resetControllerActivation(),
});
let videoPipeline = null;
let streamRuntime = null;
const audioPipeline = createAudioPipelineSession({
  invokeCommand: invoke,
  onUpdate: updateAudioUI,
  resetAvSync: (...args) => videoPipeline?.resetAvSync(...args),
  isDisconnecting: () => connectionState.disconnecting,
});

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
  const session = audioPipeline.session;
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
    ringOverflow.textContent = `${(session.workletDroppedFrames * 1000 / AUDIO_SAMPLE_RATE).toFixed(1)} ms`;
  }
  if (ringRecoveryDiscarded) {
    ringRecoveryDiscarded.textContent = `${(session.workletRecoveryDiscardedFrames * 1000 / AUDIO_SAMPLE_RATE).toFixed(1)} ms`;
  }
  if (ringBuffered) {
    ringBuffered.textContent = `${(session.bufferedFrames * 1000 / AUDIO_SAMPLE_RATE).toFixed(1)} ms`;
  }
  if (underruns) {
    underruns.textContent = `${session.underflows} events / ${(session.underflowDurationMicros / 1000).toFixed(1)} ms`;
  }
  if (silence) silence.textContent = `${(session.silentDurationMicros / 1000).toFixed(1)} ms`;
}

async function toggleAudioMute() {
  await audioPipeline.toggleMute();
}

async function createConnectionAttempt({ createChannel: Channel }) {
  // Reset before opening the channel so early frames cannot race a
  // post-connect reset and disappear from the live diagnostics.
  streamRuntime.prepareConnection();
  if (typeof Channel !== 'function') throw new Error('Tauri binary channels are unavailable');
  const audioSession = audioPipeline.beginAttempt();
  const frameSession = streamRuntime.openFrameSession();
  const pointerSession = newPointerSession();
  activePointerSession = pointerSession;
  const audioAttempt = await audioPipeline.createAttemptChannel(Channel, audioSession);
  const frameChannel = new Channel(
    (message) => streamRuntime.handleBinaryFrame(message, frameSession),
  );
  pointerSession.channel = new Channel(
    (message) => pointerFeedbackRuntime.handleMessage(message, pointerSession),
  );
  return {
    audioSession: audioAttempt.session,
    frameSession,
    pointerSession,
    audioSupport: audioAttempt.support,
    connectionArgs: {
      frameChannel,
      audioChannel: audioAttempt.channel,
      pointerChannel: pointerSession.channel,
      audioSupported: audioAttempt.support.supported,
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
  streamRuntime.activateFrameSession(attempt.frameSession, {
    generation: result.media_generation,
    adaptiveFeedbackAvailable: result.adaptive_feedback_available === true,
    adaptiveFeedbackError: result.adaptive_feedback_error,
  });
}

function closeConnectionAttempt(attempt, { committed = false } = {}) {
  streamRuntime.closeFrameSession(attempt?.frameSession);
  if (committed && attempt?.pointerSession) attempt.pointerSession.closing = true;
}

async function teardownConnectionAttempt(attempt) {
  const pointerSession = attempt?.pointerSession ?? activePointerSession;
  if (activePointerSession === pointerSession) activePointerSession = null;
  if (pointerSession) pointerSession.channel = null;
  audioPipeline.releaseAttempt(attempt?.audioSession);
  await audioPipeline.teardown();
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
  controlRuntime.resetAcceptedConnection();
}

async function showConnectedAttempt({ result, attempt }) {
  await audioPipeline.acceptConnectedResult(result, {
    session: attempt.audioSession,
    support: attempt.audioSupport,
  });
  updatePanelVisibility();
  document.getElementById('node-id-text').textContent = result.host_node_id.substring(0, 16) + '...';
  document.getElementById('frame-canvas').style.display = 'block';
  document.getElementById('placeholder').classList.add('hidden');
  document.getElementById('control-bar').classList.add('visible');
  void windowRuntime.fitWindowToIncomingStream();
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
  streamRuntime.prepareDisconnect();
  // Give release commands a short, finite opportunity to cross the bounded
  // Tauri queue before closing it. A broken stream cannot stall disconnect.
  await controlRuntime.prepareDisconnect(250);
}

async function teardownConnectedResources() {
  const pointerSession = activePointerSession;
  activePointerSession = null;
  if (pointerSession) pointerSession.channel = null;
  await audioPipeline.teardown();
  streamRuntime.teardown();
}

async function showDisconnectedState() {
  windowRuntime.reset();
  controlRuntime.resetDisconnected();
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
  beforeConnect: audioPipeline.prepareForConnection,
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
// Finish the codec probe and publish the same delivery mode to Rust before
// exposing any connect handlers. This keeps raw WebCodecs frames and the Rust
// JPEG compatibility path from disagreeing during startup.
const hasWebCodecs = await detectAndPublishVideoDeliveryMode({ invokeCommand: invoke });

// ─── Frame decoding ───────────────────────────────────────────────────────────
const canvas = document.getElementById('frame-canvas');
const remotePointer = document.getElementById('remote-pointer');
const ctx = canvas.getContext('2d');
const windowRuntime = createWindowRuntime({
  isConnected: () => connectionState.connected,
  getFormat: () => videoPipeline.format,
  getGeneration: () => streamRuntime.generation,
  getChromeHeight: () => {
    const topbar = document.querySelector('.topbar');
    const bottombar = document.querySelector('.bottombar');
    return Math.max(0, (topbar?.offsetHeight ?? 0) + (bottombar?.offsetHeight ?? 0));
  },
  getScreenBounds: () => ({
    width: window.screen.availWidth,
    height: window.screen.availHeight,
  }),
  getWindowSize: () => ({ width: window.innerWidth, height: window.innerHeight }),
  getSurfaceBounds: () => {
    const main = canvas.parentElement;
    if (!main) return null;
    const bounds = main.getBoundingClientRect();
    return { width: bounds.width, height: bounds.height };
  },
  setSurfaceSize: ({ width, height }) => {
    canvas.style.width = `${width}px`;
    canvas.style.height = `${height}px`;
  },
  applyNativeGeometry: (geometry, unmaximize) => invoke('set_client_window_size', {
    logicalWidth: geometry.width,
    logicalHeight: geometry.height,
    unmaximize,
  }),
});
controlRuntime = createControlRuntime({
  getConnection: () => ({
    connected: connectionState.connected,
    disconnecting: connectionState.disconnecting,
    capabilities: connectionState.inputCapabilities,
  }),
  inputRuntime,
  invokeCursorGrab: (grab) => invoke('set_client_cursor_grab', { grab }),
  pointerLock: {
    target: canvas,
    getOwner: () => document.pointerLockElement,
    request: () => {
      if (typeof canvas.requestPointerLock !== 'function') {
        throw new Error('browser Pointer Lock is unavailable');
      }
      return canvas.requestPointerLock();
    },
    exit: () => {
      if (typeof document.exitPointerLock === 'function') document.exitPointerLock();
    },
    eventTarget: document,
  },
  publishController: () => controllerRuntime?.publishCurrentState(),
  resetControllerEscape: () => controllerRuntime?.resetEscape(),
  onChange: () => updateControlUI(),
  onReleaseFailure: () => setStatus('err', 'cursor release failed · quit app'),
});
videoPipeline = createVideoPipelineSession({
  hasWebCodecs,
  canvas,
  context: ctx,
  requestKeyframe: (reason) => streamRuntime?.requestKeyframe(reason),
  resetAudioSync: audioPipeline.resetSyncTelemetry,
  sampleAudioSkew: audioPipeline.sampleSkew,
  onFormatChanged: () => {
    windowRuntime.sizeSurfaceToIncomingStream();
    if (connectionState.connected) void windowRuntime.fitWindowToIncomingStream();
  },
});
streamRuntime = createStreamRuntime({
  invokeCommand: invoke,
  videoPipeline,
  isConnected: () => connectionState.connected,
  disconnect,
});

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
  const {
    video,
    transportDroppedFrames,
    transportObjectDroppedFrames,
    transportLateObjectDroppedFrames,
    frontendDroppedFrames,
    frontendQueueDroppedFrames,
    frontendResyncDroppedFrames,
    frontendQueueStats,
    frontendResyncStats,
    transportIntervalStats,
    frontendIpcSendDurationStats,
    rustTimingWindow,
    networkDiagnostics,
    lastTransportFps,
    lastFrontendSendFps,
    streamPathMode,
    streamRttMs,
    adaptiveFeedbackAvailable,
    adaptiveFeedbackError,
    adaptiveDecision,
  } = streamRuntime.snapshot(now);
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

  streamRuntime.publishAdaptiveFeedback(video);
}

setInterval(() => {
  if (connectionState.connected) updateStreamStats();
}, 250);

// The software decoder/JPEG compatibility path intentionally remains an event:
// it is only selected when WebCodecs is unavailable and is not latency-critical.
listen('frame', (event) => {
  streamRuntime.handleLegacyFrame(event.payload);
});

listen('frame-stats', (event) => {
  if (streamRuntime.handleFrameStats(event.payload)) updateStreamStats();
});

listen('adaptive-bitrate-decision', (event) => {
  if (streamRuntime.handleAdaptiveDecision(event.payload)) updateStreamStats();
});

listen('adaptive-feedback-state', (event) => {
  if (streamRuntime.handleAdaptiveFeedbackState(event.payload)) updateStreamStats();
});

listen('frame-error', (event) => {
  streamRuntime.handleFrameError(event.payload);
});

listen('audio-state', (event) => {
  audioPipeline.handleNativeState(event.payload);
});

listen('audio-stats', (event) => {
  audioPipeline.handleNativeStats(event.payload);
});

// ─── Input ────────────────────────────────────────────────────────────────────
async function toggleControl() {
  // Capture synchronous controller provenance before cursor acquisition yields.
  // The DOM click dispatcher does not await this async handler.
  await controlRuntime.toggle({ controllerInitiated: controllerActivationInProgress });
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

function handleBrowserPointerLockChange() {
  controlRuntime.handleBrowserPointerLockChange();
}

document.addEventListener('pointerlockchange', handleBrowserPointerLockChange);

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
  const visible = controlRuntime.active
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

function updateControlUI() {
  const el = document.getElementById('control-toggle');
  const capabilities = connectionState.inputCapabilities;
  const available = connectionState.connected && capabilities.control;
  const controlling = controlRuntime.setInactiveIfUnavailable(available);
  el.textContent = available
    ? `${controlling ? 'controlling' : 'take control'} · ${describeInputCapabilities()}${controlling && capabilities.relativePointer ? ' · Ctrl+Alt+Esc to exit' : controlling && capabilities.gamepad ? ' · hold Back+Start to exit' : ''}`
    : 'view only · input unavailable';
  el.classList.toggle('active', controlling);
  el.classList.toggle('disabled', !available);
  el.setAttribute('aria-disabled', available ? 'false' : 'true');
  document.body.classList.toggle(
    'native-pointer-control',
    controlling && capabilities.relativePointer,
  );
  canvas.classList.toggle(
    'relative-control',
    controlling && capabilities.relativePointer,
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

window.addEventListener('mousemove', (e) => {
  if (!controlRuntime.active || !pointerInputAvailable()) return;
  if (connectionState.inputCapabilities.relativePointer) {
    const movement = scaleRelativePointerDelta(
      e.movementX,
      e.movementY,
    );
    inputRuntime.send({ t: 'mr', ...movement });
    return;
  }
}, { capture: true });

canvas.addEventListener('mousemove', (e) => {
  if (!controlRuntime.active
    || !connectionState.inputCapabilities.absolutePointer
    || connectionState.inputCapabilities.relativePointer) return;
  const { x, y } = scaleCoords(e.clientX, e.clientY);
  inputRuntime.send({ t: 'mm', x, y });
});

function handleMouseDown(e) {
  if (!controlRuntime.active || !pointerInputAvailable()) return;
  if (connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  else if (e.target !== canvas) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  e.preventDefault();
  if (connectionState.inputCapabilities.relativePointer && document.pointerLockElement !== canvas) {
    void controlRuntime.requestBrowserPointerLock().catch((error) => {
      console.warn('browser pointer lock retry failed:', error);
    });
  }
  if (!inputRuntime.trackMouseButton(btn)) return;
  if (!inputRuntime.send({ t: 'md', b: btn })) inputRuntime.takeMouseButtonRelease(btn);
}

window.addEventListener('mousedown', handleMouseDown, { capture: true });

function releaseMouseButton(e) {
  if (!connectionState.connected || !pointerInputAvailable()) return;
  const btn = browserMouseButtonCode(e.button);
  if (btn === null) return;
  const release = inputRuntime.takeMouseButtonRelease(btn);
  if (!release) return;
  e.preventDefault();
  inputRuntime.send(release, { release: true });
}

window.addEventListener('mouseup', (e) => {
  if (controlRuntime.active && connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  releaseMouseButton(e);
}, { capture: true });

window.addEventListener('contextmenu', (e) => {
  if (!controlRuntime.active || !pointerInputAvailable()) return;
  if (!connectionState.inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
}, { capture: true });

window.addEventListener('wheel', (e) => {
  if (!controlRuntime.active || !pointerInputAvailable()) return;
  if (!connectionState.inputCapabilities.relativePointer && e.target !== canvas) return;
  e.preventDefault();
  if (connectionState.inputCapabilities.relativePointer) e.stopPropagation();
  inputRuntime.send({ t: 'ms', dx: Math.round(e.deltaX), dy: Math.round(e.deltaY) });
}, { passive: false, capture: true });

window.addEventListener('keydown', (e) => {
  if (!controlRuntime.active) return;
  if (e.key === 'Escape' && e.ctrlKey && e.altKey) {
    e.preventDefault();
    controlRuntime.exit();
    return;
  }
  const printableText = e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey;
  if (printableText && connectionState.inputCapabilities.text) {
    e.preventDefault();
    inputRuntime.send({ t: 'tx', s: e.key });
    return;
  }
  if (!connectionState.inputCapabilities.keyboard) return;
  const k = mapKey(e);
  if (k) {
    e.preventDefault();
    const keyId = e.code || e.key;
    const tracking = inputRuntime.trackKey(keyId, k);
    if (tracking === 'repeat') return;
    if (tracking === 'full') {
      console.error('held key limit reached; refusing additional key transition');
      return;
    }
    if (!inputRuntime.send({ t: 'kd', k })) inputRuntime.takeKeyRelease(keyId);
  }
});

window.addEventListener('keyup', (e) => {
  const keyId = e.code || e.key;
  const release = inputRuntime.takeKeyRelease(keyId);
  if (!release) return;
  e.preventDefault();
  inputRuntime.send(release, { release: true });
});

window.addEventListener('blur', () => {
  if (!controlRuntime.active && !controlRuntime.transitioning) return;
  controlRuntime.exit();
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

controllerRuntime = createControllerRuntime({
  schedulePoll: (callback) => requestAnimationFrame(callback),
  isRemoteRoute: () => connectionState.connected
    && controlRuntime.active
    && connectionState.inputCapabilities.gamepad,
  sendRemoteState: (state) => {
    if (!controlRuntime.acceptsControllerInput(state)) return;
    inputRuntime.send({ t: 'gp', state });
  },
  onNavigate: navigateControllerFocus,
  onActivate: activateControllerFocus,
  onBack: controllerBack,
  onStatus: updateControllerStatus,
  onExit: () => {
    controlRuntime.exit();
    setControllerFocus(document.getElementById('control-toggle'));
  },
});

document.addEventListener('pointerdown', () => {
  document.querySelectorAll('.controller-focus').forEach((item) => item.classList.remove('controller-focus'));
}, { capture: true });

window.addEventListener('resize', () => {
  windowRuntime.sizeSurfaceToIncomingStream();
  renderRemotePointer();
  windowRuntime.scheduleAspectCorrection();
});

window.addEventListener('focus', () => {
  if (!controlRuntime.active || !connectionState.inputCapabilities.relativePointer) return;
  // WebKit rebuilds its native cursor rectangles after the window-level focus
  // event. Reassert from the webview on the next frame so the final rectangle
  // is the transparent one used during relative control.
  requestAnimationFrame(() => {
    if (!controlRuntime.active || !connectionState.inputCapabilities.relativePointer) return;
    void controlRuntime.reassertNativeGrab().catch((error) => {
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
    getLatestState: () => controllerRuntime.latest,
    setObserver: controllerRuntime.setObserver,
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
  requestAnimationFrame(controllerRuntime.poll);
}

initialize();
