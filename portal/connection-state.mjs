import {
  committedRustConnection,
  disconnectRejectedRustConnection,
} from './connection-attempt.mjs';

// Mirrors MEDIA_TRANSPORT_NAMES in src-tauri/src/media/transport.rs.
export const MEDIA_TRANSPORT_ALLOWLIST = Object.freeze([
  'iroh-moq',
  'grouped-v3',
]);

export function normalizeMediaTransport(value) {
  return MEDIA_TRANSPORT_ALLOWLIST.includes(value) ? value : 'unknown';
}

function emptyInputCapabilities() {
  return {
    relativePointer: false,
    pointerPositionFeedback: false,
    absolutePointer: false,
    keyboard: false,
    text: false,
    gamepad: false,
    control: false,
  };
}

function normalizeInputCapabilities(result) {
  const capabilities = {
    relativePointer: result.relative_pointer_available === true,
    pointerPositionFeedback: result.pointer_position_feedback_available === true,
    absolutePointer: result.absolute_pointer_available === true,
    keyboard: result.keyboard_available === true,
    text: result.text_available === true,
    gamepad: result.gamepad_available === true,
    control: result.control_available === true,
  };
  capabilities.control = capabilities.control
    && (capabilities.relativePointer
      || capabilities.absolutePointer
      || capabilities.keyboard
      || capabilities.text
      || capabilities.gamepad);
  return capabilities;
}

function connectionStatus(result, capabilities) {
  return [
    'connected',
    capabilities.control ? null : 'view only',
    result.development_mode ? 'dev direct-node' : null,
  ].filter(Boolean).join(' · ');
}

function validateGeneration(result) {
  if (result.connected
    && (!Number.isSafeInteger(result.media_generation) || result.media_generation <= 0)) {
    throw new Error('host returned an invalid media generation');
  }
  if (result.connected
    && result.audio_available === true
    && (!Number.isSafeInteger(result.audio_generation) || result.audio_generation <= 0)) {
    throw new Error('host returned an invalid audio generation');
  }
}

const noop = () => {};
const asyncNoop = async () => {};

export function newConnectionState({
  invokeCommand,
  createChannel = null,
  beforeConnect = noop,
  createAttempt = asyncNoop,
  validateConnectedResult = noop,
  activateAttempt = noop,
  closeAttempt = noop,
  teardownAttempt = asyncNoop,
  beforeDisconnect = asyncNoop,
  teardownConnection = asyncNoop,
  onStatus = noop,
  onConnected = noop,
  onDisconnected = noop,
  onFailure = noop,
  onDisconnectError = noop,
  onNativeDisconnectError = noop,
} = {}) {
  if (typeof invokeCommand !== 'function') {
    throw new TypeError('invokeCommand must be a function');
  }

  const state = {
    connected: false,
    connecting: false,
    disconnecting: false,
    developmentMode: false,
    inputCapabilities: emptyInputCapabilities(),
    mediaTransport: 'unknown',
  };

  const snapshot = () => ({
    connected: state.connected,
    connecting: state.connecting,
    disconnecting: state.disconnecting,
    developmentMode: state.developmentMode,
    inputCapabilities: { ...state.inputCapabilities },
    mediaTransport: state.mediaTransport,
  });

  async function connect({ pin = '' } = {}) {
    if (state.connecting || state.connected) return false;
    state.connecting = true;
    let attempt = null;
    let rustConnectionCommitted = false;
    try {
      const normalizedPin = String(pin).trim();
      if (!state.developmentMode && !normalizedPin) {
        onStatus('err', 'enter pin');
        return false;
      }

      beforeConnect();
      onStatus('pending', 'connecting...');
      attempt = await createAttempt({ createChannel });
      const connectionArgs = attempt?.connectionArgs ?? {};
      const result = await invokeCommand('iroh_client_connect', {
        pin: normalizedPin,
        ...connectionArgs,
      });
      rustConnectionCommitted = committedRustConnection(result);
      validateGeneration(result);
      validateConnectedResult(result, attempt);

      if (!result.connected) {
        closeAttempt(attempt, { committed: false, rejected: true });
        await teardownAttempt(attempt);
        onStatus('err', 'failed');
        return false;
      }

      activateAttempt(result, attempt);
      state.connected = true;
      state.disconnecting = false;
      state.inputCapabilities = normalizeInputCapabilities(result);
      state.mediaTransport = normalizeMediaTransport(result.media_transport);
      onStatus('ok', connectionStatus(result, state.inputCapabilities), {
        result,
        attempt,
        state: snapshot(),
      });
      await onConnected({ result, attempt, state: snapshot() });
      rustConnectionCommitted = false;
      return true;
    } catch (error) {
      closeAttempt(attempt, { committed: rustConnectionCommitted, error });
      if (rustConnectionCommitted) {
        try {
          await disconnectRejectedRustConnection(invokeCommand, rustConnectionCommitted);
        } catch (disconnectError) {
          onNativeDisconnectError(disconnectError);
        }
      }
      await teardownAttempt(attempt);
      onFailure({ error, attempt, state: snapshot() });
      onStatus('err', 'error');
      return false;
    } finally {
      state.connecting = false;
    }
  }

  async function disconnect() {
    if (state.disconnecting) return false;
    state.disconnecting = true;
    await beforeDisconnect({ state: snapshot() });
    try {
      await invokeCommand('iroh_client_disconnect');
    } catch (error) {
      onDisconnectError(error);
    }
    await teardownConnection();
    state.connected = false;
    state.inputCapabilities = emptyInputCapabilities();
    state.mediaTransport = 'unknown';
    await onDisconnected({ state: snapshot() });
    state.disconnecting = false;
    return true;
  }

  return {
    get connected() { return state.connected; },
    get connecting() { return state.connecting; },
    get disconnecting() { return state.disconnecting; },
    get developmentMode() { return state.developmentMode; },
    get inputCapabilities() { return { ...state.inputCapabilities }; },
    get mediaTransport() { return state.mediaTransport; },
    snapshot,
    setDevelopmentMode(enabled) {
      state.developmentMode = enabled === true;
    },
    connect,
    disconnect,
  };
}
