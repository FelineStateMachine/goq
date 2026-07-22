import { browserPointerLockLossRequiresControlExit } from './input-state.mjs';
import { ControllerActivationGate, neutralGamepadInputState } from './controller-state.mjs';

export const BROWSER_POINTER_LOCK_TIMEOUT_MS = 500;
export const NATIVE_CURSOR_RELEASE_RETRY_DELAYS_MS = Object.freeze([0, 16, 50]);

export function createControlRuntime({
  getConnection,
  inputRuntime,
  invokeCursorGrab,
  pointerLock,
  publishController,
  resetControllerEscape,
  onChange,
  onReleaseFailure,
  pointerLockTimeoutMs = BROWSER_POINTER_LOCK_TIMEOUT_MS,
  scheduleTimeout = globalThis.setTimeout,
  cancelTimeout = globalThis.clearTimeout,
  logger = console,
} = {}) {
  if (typeof getConnection !== 'function') throw new TypeError('getConnection must be a function');
  if (!inputRuntime || typeof inputRuntime.send !== 'function') {
    throw new TypeError('inputRuntime must provide input queue operations');
  }
  if (
    typeof inputRuntime.clear !== 'function'
    || typeof inputRuntime.releaseHeld !== 'function'
    || typeof inputRuntime.drain !== 'function'
  ) {
    throw new TypeError('inputRuntime must provide clear, release, and drain operations');
  }
  if (typeof invokeCursorGrab !== 'function') throw new TypeError('invokeCursorGrab must be a function');
  if (!pointerLock || typeof pointerLock.getOwner !== 'function') {
    throw new TypeError('pointerLock must provide an ownership surface');
  }
  if (
    typeof pointerLock.eventTarget?.addEventListener !== 'function'
    || typeof pointerLock.eventTarget?.removeEventListener !== 'function'
  ) {
    throw new TypeError('pointerLock must provide an event target');
  }
  if (typeof publishController !== 'function') throw new TypeError('publishController must be a function');
  if (typeof resetControllerEscape !== 'function') {
    throw new TypeError('resetControllerEscape must be a function');
  }
  if (typeof onChange !== 'function') throw new TypeError('onChange must be a function');
  if (typeof onReleaseFailure !== 'function') {
    throw new TypeError('onReleaseFailure must be a function');
  }

  const controllerActivationGate = new ControllerActivationGate();
  let controlMode = false;
  let controlTransitionInProgress = false;
  let controlTransitionGeneration = 0;
  let nativeCursorCommand = Promise.resolve();
  let browserPointerLockRequired = true;

  function connection() {
    return getConnection();
  }

  function queueNeutralControllerState() {
    const state = connection();
    if (!state.connected || !state.capabilities.gamepad) return false;
    return inputRuntime.send({ t: 'gp', state: neutralGamepadInputState() });
  }

  function setNativeCursorGrab(grab) {
    const command = nativeCursorCommand.then(() => invokeCursorGrab(grab));
    nativeCursorCommand = command.catch(() => {});
    return command;
  }

  function waitForBrowserPointerLockOwnership(timeoutMs = pointerLockTimeoutMs) {
    if (pointerLock.getOwner() === pointerLock.target) return Promise.resolve();
    return new Promise((resolve, reject) => {
      let timeoutId;
      const cleanup = () => {
        cancelTimeout(timeoutId);
        pointerLock.eventTarget.removeEventListener('pointerlockchange', handleChange);
        pointerLock.eventTarget.removeEventListener('pointerlockerror', handleError);
      };
      const settle = (callback, value) => {
        cleanup();
        callback(value);
      };
      const handleChange = () => {
        if (pointerLock.getOwner() === pointerLock.target) {
          settle(resolve);
        } else {
          settle(reject, new Error('browser Pointer Lock ownership was lost'));
        }
      };
      const handleError = () => {
        settle(reject, new Error('browser Pointer Lock request was rejected'));
      };
      pointerLock.eventTarget.addEventListener('pointerlockchange', handleChange);
      pointerLock.eventTarget.addEventListener('pointerlockerror', handleError);
      timeoutId = scheduleTimeout(() => {
        settle(reject, new Error('browser Pointer Lock ownership timed out'));
      }, timeoutMs);
    });
  }

  async function releaseNativeCursorGrab(releaseGeneration) {
    let lastError = null;
    for (const delayMs of NATIVE_CURSOR_RELEASE_RETRY_DELAYS_MS) {
      if (releaseGeneration !== controlTransitionGeneration || controlMode) return false;
      if (delayMs > 0) {
        await new Promise((resolve) => scheduleTimeout(resolve, delayMs));
        if (releaseGeneration !== controlTransitionGeneration || controlMode) return false;
      }
      try {
        await setNativeCursorGrab(false);
        return true;
      } catch (error) {
        lastError = error;
      }
    }
    logger.error('native cursor release failed after bounded retries:', lastError);
    if (!controlMode && releaseGeneration === controlTransitionGeneration) {
      onReleaseFailure(lastError);
    }
    return false;
  }

  function releasePointerLock() {
    const releaseGeneration = controlTransitionGeneration;
    void releaseNativeCursorGrab(releaseGeneration);
    if (
      pointerLock.getOwner() !== pointerLock.target
      || typeof pointerLock.exit !== 'function'
    ) return;
    try { pointerLock.exit(); } catch (_) {}
  }

  function exit({ resetEscape = false } = {}) {
    controlTransitionGeneration += 1;
    controlTransitionInProgress = false;
    controllerActivationGate.reset();
    if (resetEscape) resetControllerEscape();
    queueNeutralControllerState();
    inputRuntime.releaseHeld();
    controlMode = false;
    releasePointerLock();
    onChange();
  }

  function exitAfterBrowserPointerLockFailure() {
    if (!browserPointerLockRequired || (!controlMode && !controlTransitionInProgress)) {
      return false;
    }
    exit({ resetEscape: true });
    return true;
  }

  async function requestBrowserPointerLock() {
    if (!browserPointerLockRequired) return;
    try {
      if (typeof pointerLock.request !== 'function') {
        throw new Error('browser Pointer Lock is unavailable');
      }
      const request = pointerLock.request();
      if (request && typeof request.then === 'function') await request;
      await waitForBrowserPointerLockOwnership();
      // Pointer Lock and the native window grab are separate on Linux. Reassert
      // the native state only after the browser proves ownership.
      await setNativeCursorGrab(true);
    } catch (error) {
      exitAfterBrowserPointerLockFailure();
      throw error;
    }
  }

  async function requestRelativePointerLock() {
    if (!connection().capabilities.relativePointer) return true;
    try {
      const nativeResult = await setNativeCursorGrab(true);
      // Older commands returned no value, so only an explicit false disables
      // the browser fallback.
      browserPointerLockRequired = nativeResult !== false;
      await requestBrowserPointerLock();
      return true;
    } catch (error) {
      logger.warn('relative pointer lock unavailable:', error);
      return false;
    }
  }

  async function toggle({ controllerInitiated = false } = {}) {
    const initial = connection();
    if (!initial.connected || !initial.capabilities.control || controlTransitionInProgress) return;
    if (controlMode) {
      exit();
      return;
    }

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
      if (
        !controlMode
        && !controlTransitionInProgress
        && pointerLock.getOwner() === pointerLock.target
      ) {
        releasePointerLock();
      }
      return;
    }
    const current = connection();
    if (!acquired
      || current.disconnecting
      || !current.connected
      || !current.capabilities.control) {
      exitAfterBrowserPointerLockFailure();
      return;
    }
    controlTransitionInProgress = false;
    controlMode = true;
    // Preserve the distinct controller branch: it adds a neutral state before
    // republishing the current controller snapshot through the activation gate.
    if (controllerInitiated) {
      queueNeutralControllerState();
      publishController();
    } else {
      publishController();
    }
    onChange();
  }

  function handleBrowserPointerLockChange() {
    if (!browserPointerLockLossRequiresControlExit({
      browserPointerLockRequired,
      pointerLockElement: pointerLock.getOwner(),
      expectedElement: pointerLock.target,
      controlMode,
      controlTransitionInProgress,
    })) return false;
    return exitAfterBrowserPointerLockFailure();
  }

  async function prepareDisconnect(timeoutMs = 250) {
    exit();
    await inputRuntime.drain(timeoutMs);
  }

  function resetAcceptedConnection() {
    controlMode = false;
    inputRuntime.clear();
  }

  function resetDisconnected() {
    controlMode = false;
    inputRuntime.clear();
    onChange();
  }

  function setInactiveIfUnavailable(available) {
    if (!available) controlMode = false;
    return controlMode;
  }

  return Object.freeze({
    acceptsControllerInput: (state) => controllerActivationGate.accepts(state),
    exit,
    handleBrowserPointerLockChange,
    prepareDisconnect,
    queueNeutralControllerState,
    reassertNativeGrab: () => setNativeCursorGrab(true),
    releasePointerLock,
    requestBrowserPointerLock,
    resetAcceptedConnection,
    resetControllerActivation: () => controllerActivationGate.reset(),
    resetDisconnected,
    setInactiveIfUnavailable,
    toggle,
    get active() { return controlMode; },
    get activationGateActive() { return controllerActivationGate.active; },
    get browserPointerLockRequired() { return browserPointerLockRequired; },
    get generation() { return controlTransitionGeneration; },
    get transitioning() { return controlTransitionInProgress; },
  });
}
