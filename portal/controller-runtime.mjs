import {
  ControllerActionRepeater,
  GamepadEscapeHold,
  LatestControllerStatePublisher,
  controllerStateSignature,
  disconnectedControllerState,
  maskGamepadEscapeChord,
  normalizeGamepad,
  selectPreferredController,
  toGamepadInputState,
} from './controller-state.mjs';

export function createControllerRuntime({
  getGamepads = () => globalThis.navigator?.getGamepads?.() ?? [],
  schedulePoll = (callback) => requestAnimationFrame(callback),
  isRemoteRoute = () => false,
  sendRemoteState = () => {},
  onNavigate = () => {},
  onActivate = () => {},
  onBack = () => {},
  onStatus = () => {},
  onExit = () => {},
  warn = (...args) => console.warn(...args),
  publisher = new LatestControllerStatePublisher(),
  actions = new ControllerActionRepeater(),
  escape = new GamepadEscapeHold(1000),
} = {}) {
  let sequence = 0;
  let selectedIndex = null;
  let currentState = disconnectedControllerState();
  let lastSignature = controllerStateSignature(currentState);
  let observer = null;

  publisher.setHandler((state) => {
    if (observer) {
      try {
        observer(state);
      } catch (error) {
        warn('controller observer failed:', error);
      }
    }
    // Route state whenever the remote control path is active. Disconnected and
    // non-standard samples intentionally map to neutral so controller loss
    // cannot leave held input on the host.
    if (!isRemoteRoute()) return;
    sendRemoteState(maskGamepadEscapeChord(toGamepadInputState(state)));
  });

  function publishCurrentState() {
    publisher.publish(currentState);
  }

  function poll(nowMs) {
    let gamepads = [];
    try {
      gamepads = Array.from(getGamepads() ?? []).filter(Boolean);
    } catch (error) {
      warn('gamepad poll failed:', error);
    }
    const gamepad = selectPreferredController(gamepads, selectedIndex);
    const nextIndex = gamepad?.index ?? null;
    if (nextIndex !== selectedIndex) {
      selectedIndex = nextIndex;
      actions.reset();
      escape.reset();
      lastSignature = '';
    }

    currentState = normalizeGamepad(gamepad, sequence + 1);
    const signature = controllerStateSignature(currentState);
    if (signature !== lastSignature) {
      sequence += 1;
      currentState = normalizeGamepad(gamepad, sequence);
      lastSignature = signature;
      publisher.publish(currentState);
      onStatus(currentState);
    }

    const remoteRoute = isRemoteRoute()
      && currentState.connected
      && currentState.mapping === 'standard';
    if (remoteRoute) {
      if (escape.update(currentState, nowMs)) {
        onExit();
        actions.reset();
      }
    } else {
      escape.reset();
      for (const action of actions.update(currentState, nowMs)) {
        if (action.type === 'navigate') onNavigate(action.direction);
        else if (action.type === 'activate') onActivate();
        else if (action.type === 'back') onBack();
      }
    }
    schedulePoll(poll);
  }

  function start() {
    schedulePoll(poll);
  }

  function setObserver(nextObserver) {
    if (nextObserver !== null && typeof nextObserver !== 'function') {
      throw new TypeError('observer must be a function or null');
    }
    observer = nextObserver;
  }

  return Object.freeze({
    poll,
    publishCurrentState,
    resetEscape: () => escape.reset(),
    setObserver,
    start,
    get latest() {
      return publisher.latest;
    },
    get current() {
      return currentState;
    },
  });
}
