import {
  HeldInputState,
  MAX_HELD_KEYS,
  MAX_HELD_MOUSE_BUTTONS,
  PointerMotionBuffer,
  restoreRejectedPointerMotion,
} from './input-state.mjs';

export const RELIABLE_INPUT_QUEUE_LIMIT = 128;
export const IN_FLIGHT_INPUT_RESTORE_RESERVE = 1;
export const RELIABLE_INPUT_ENQUEUE_LIMIT = RELIABLE_INPUT_QUEUE_LIMIT
  - IN_FLIGHT_INPUT_RESTORE_RESERVE;
export const POINTER_MOTION_BARRIER_RESERVE = 2;
export const RELEASE_INPUT_RESERVE = MAX_HELD_KEYS
  + MAX_HELD_MOUSE_BUTTONS
  + POINTER_MOTION_BARRIER_RESERVE;
export const REGULAR_RELIABLE_INPUT_LIMIT = RELIABLE_INPUT_ENQUEUE_LIMIT
  - RELEASE_INPUT_RESERVE;
export const INPUT_RETRY_MS = 8;

export function createInputRuntime({
  invokeCommand,
  getCapabilities,
  isConnected,
  onFatal,
  resetControllerActivation,
  scheduleTimeout = globalThis.setTimeout,
  scheduleMicrotask = globalThis.queueMicrotask,
  now = () => performance.now(),
  logger = console,
} = {}) {
  if (typeof invokeCommand !== 'function') throw new TypeError('invokeCommand must be a function');
  if (typeof getCapabilities !== 'function') throw new TypeError('getCapabilities must be a function');
  if (typeof isConnected !== 'function') throw new TypeError('isConnected must be a function');
  if (typeof onFatal !== 'function') throw new TypeError('onFatal must be a function');
  if (typeof resetControllerActivation !== 'function') {
    throw new TypeError('resetControllerActivation must be a function');
  }

  const reliableInputQueue = [];
  const heldInputs = new HeldInputState();
  const pendingPointerMotion = new PointerMotionBuffer();
  let pendingGamepadInput = null;
  let inputPumpRunning = false;
  let inputPumpScheduled = false;

  function clear() {
    reliableInputQueue.length = 0;
    heldInputs.clear();
    resetControllerActivation();
    pendingPointerMotion.clear();
    pendingGamepadInput = null;
  }

  function pointerAvailable() {
    const capabilities = getCapabilities();
    return capabilities.relativePointer || capabilities.absolutePointer;
  }

  function inputEventAvailable(event) {
    const capabilities = getCapabilities();
    if (!capabilities.control) return false;
    if (event.t === 'mm') return capabilities.absolutePointer;
    if (event.t === 'mr') return capabilities.relativePointer;
    if (event.t === 'mp') return capabilities.relativePointer;
    if (['mc', 'md', 'mu', 'ms'].includes(event.t)) return pointerAvailable();
    if (['kd', 'ku', 'kt'].includes(event.t)) return capabilities.keyboard;
    if (event.t === 'tx') return capabilities.text;
    if (event.t === 'gp') return capabilities.gamepad;
    return false;
  }

  function hasPending() {
    return reliableInputQueue.length > 0
      || pendingPointerMotion.pending
      || pendingGamepadInput !== null;
  }

  function schedulePump() {
    if (inputPumpRunning || inputPumpScheduled) return;
    inputPumpScheduled = true;
    scheduleTimeout(() => {
      inputPumpScheduled = false;
      void pump();
    }, 0);
  }

  function signalFatal(reason, error = null) {
    if (isConnected()) void onFatal({ reason, error });
  }

  function send(event, { release = false } = {}) {
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
        logger.error(release
          ? 'input release reserve exhausted; closing control session'
          : 'reliable input queue full; closing control session');
        // Defer teardown until the caller has removed any transition it could
        // not enqueue from held-state tracking.
        scheduleMicrotask(() => signalFatal(
          release ? 'input release reserve exhausted' : 'reliable input queue full',
        ));
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
    schedulePump();
    return true;
  }

  function releaseHeld() {
    for (const release of heldInputs.releaseEvents()) {
      // Ordinary reliable events cannot consume this reserved capacity.
      send(release, { release: true });
    }
    heldInputs.clear();
  }

  async function drain(timeoutMs) {
    const deadline = now() + timeoutMs;
    while ((hasPending() || inputPumpRunning) && now() < deadline) {
      await new Promise((resolve) => scheduleTimeout(resolve, INPUT_RETRY_MS));
    }
  }

  function takePending() {
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

  function restorePending(item) {
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

  async function pump() {
    if (inputPumpRunning) return;
    inputPumpRunning = true;
    try {
      while (hasPending()) {
        const item = takePending();
        if (!item) break;
        let accepted;
        try {
          accepted = await invokeCommand('iroh_client_send_input', { event: item.event });
        } catch (error) {
          logger.warn('input send failed:', error);
          clear();
          return;
        }
        if (!accepted) {
          try {
            restorePending(item);
          } catch (error) {
            logger.error('input restore failed:', error);
            clear();
            signalFatal('input restore failed', error);
            return;
          }
          await new Promise((resolve) => scheduleTimeout(resolve, INPUT_RETRY_MS));
        }
      }
    } finally {
      inputPumpRunning = false;
      if (hasPending()) schedulePump();
    }
  }

  return Object.freeze({
    clear,
    drain,
    hasPending,
    inputEventAvailable,
    pointerAvailable,
    releaseHeld,
    send,
    takeKeyRelease: (id) => heldInputs.takeKeyRelease(id),
    takeMouseButtonRelease: (button) => heldInputs.takeMouseButtonRelease(button),
    trackKey: (id, value) => heldInputs.trackKey(id, value),
    trackMouseButton: (button) => heldInputs.trackMouseButton(button),
  });
}
