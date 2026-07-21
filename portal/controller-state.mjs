export const STANDARD_BUTTON_COUNT = 17;
export const STANDARD_AXIS_COUNT = 4;
export const STICK_NAV_THRESHOLD = 0.55;
export const NAV_REPEAT_DELAY_MS = 360;
export const NAV_REPEAT_INTERVAL_MS = 110;

const BUTTON_A = 0;
const BUTTON_B = 1;
const BUTTON_DPAD_UP = 12;
const BUTTON_DPAD_DOWN = 13;
const BUTTON_DPAD_LEFT = 14;
const BUTTON_DPAD_RIGHT = 15;

const PROTOCOL_BUTTONS = Object.freeze({
  a: 0,
  b: 1,
  x: 2,
  y: 3,
  left_shoulder: 4,
  right_shoulder: 5,
  back: 8,
  start: 9,
  left_stick: 10,
  right_stick: 11,
  dpad_up: 12,
  dpad_down: 13,
  dpad_left: 14,
  dpad_right: 15,
  guide: 16,
});

function finiteUnit(value) {
  if (!Number.isFinite(value)) return 0;
  return Math.max(-1, Math.min(1, value));
}

function buttonValue(button) {
  if (button == null) return 0;
  if (typeof button === 'number') return Math.max(0, Math.min(1, button));
  const value = Math.max(0, Math.min(1, Number(button.value) || 0));
  // Preserve analog trigger travel. `pressed` is only a fallback for digital
  // implementations that omit the expected value=1 sample.
  return button.pressed === true && value === 0 ? 1 : value;
}

export function disconnectedControllerState(sequence = 0) {
  return Object.freeze({
    connected: false,
    index: null,
    id: '',
    mapping: '',
    sequence,
    timestamp: 0,
    axes: Object.freeze([0, 0, 0, 0]),
    buttons: Object.freeze(Array(STANDARD_BUTTON_COUNT).fill(0)),
  });
}

/**
 * Convert a browser Gamepad into one fixed-size, immutable state. Truncating to
 * the standard mapping prevents unusual devices from growing transport state.
 */
export function normalizeGamepad(gamepad, sequence = 0) {
  if (!gamepad || gamepad.connected === false) return disconnectedControllerState(sequence);
  const axes = Array.from(
    { length: STANDARD_AXIS_COUNT },
    (_, index) => finiteUnit(Number(gamepad.axes?.[index]) || 0),
  );
  const buttons = Array.from(
    { length: STANDARD_BUTTON_COUNT },
    (_, index) => buttonValue(gamepad.buttons?.[index]),
  );
  return Object.freeze({
    connected: true,
    index: Number.isInteger(gamepad.index) ? gamepad.index : 0,
    id: String(gamepad.id || '').slice(0, 256),
    mapping: String(gamepad.mapping || '').slice(0, 32),
    sequence,
    timestamp: Number.isFinite(gamepad.timestamp) ? gamepad.timestamp : 0,
    axes: Object.freeze(axes),
    buttons: Object.freeze(buttons),
  });
}

export function controllerStateSignature(state) {
  return [
    state.connected ? 1 : 0,
    state.index ?? -1,
    state.id,
    state.mapping,
    ...state.axes,
    ...state.buttons,
  ].join('|');
}

export function neutralGamepadInputState() {
  return {
    a: false,
    b: false,
    x: false,
    y: false,
    left_shoulder: false,
    right_shoulder: false,
    back: false,
    start: false,
    guide: false,
    left_stick: false,
    right_stick: false,
    dpad_up: false,
    dpad_down: false,
    dpad_left: false,
    dpad_right: false,
    left_x: 0,
    left_y: 0,
    right_x: 0,
    right_y: 0,
    left_trigger: 0,
    right_trigger: 0,
  };
}

/**
 * Prevent the A press used to cross the local control boundary from also
 * activating the remote game. Only A release opens the gate; harmless analog
 * stick or trigger drift must not keep controller forwarding disabled.
 */
export class ControllerActivationGate {
  constructor() {
    this.waitingForARelease = false;
  }

  arm() {
    this.waitingForARelease = true;
  }

  reset() {
    this.waitingForARelease = false;
  }

  accepts(inputState) {
    if (!this.waitingForARelease) return true;
    if (inputState?.a === true) return false;
    this.waitingForARelease = false;
    return true;
  }

  get active() {
    return this.waitingForARelease;
  }
}

function signedAxis(value) {
  return Math.round(finiteUnit(value) * 32767);
}

function unsignedTrigger(value) {
  return Math.round(Math.max(0, Math.min(1, Number(value) || 0)) * 32767);
}

/** Map the browser standard layout to the bounded Sigil input protocol. */
export function toGamepadInputState(state) {
  const output = neutralGamepadInputState();
  if (!state?.connected || state.mapping !== 'standard') return output;
  const pressed = (name) => (state.buttons[PROTOCOL_BUTTONS[name]] || 0) >= 0.5;
  for (const name of Object.keys(PROTOCOL_BUTTONS)) output[name] = pressed(name);
  // Browsers expose LT/RT as standard buttons 6/7, with value in [0, 1].
  output.left_trigger = unsignedTrigger(state.buttons[6]);
  output.right_trigger = unsignedTrigger(state.buttons[7]);
  output.left_x = signedAxis(state.axes[0]);
  output.left_y = signedAxis(state.axes[1]);
  output.right_x = signedAxis(state.axes[2]);
  output.right_y = signedAxis(state.axes[3]);
  // A malformed or unusual browser sample cannot assert opposites together.
  if (output.dpad_up && output.dpad_down) {
    output.dpad_up = false;
    output.dpad_down = false;
  }
  if (output.dpad_left && output.dpad_right) {
    output.dpad_left = false;
    output.dpad_right = false;
  }
  return output;
}

/** Reserve the local Back+Start escape chord without consuming either button alone. */
export function maskGamepadEscapeChord(inputState) {
  if (inputState?.back !== true || inputState?.start !== true) return inputState;
  return {
    ...inputState,
    back: false,
    start: false,
  };
}

/**
 * Keep an already selected standard controller, otherwise prefer the first
 * connected standard mapping. With no standard mapping, retain the selected
 * device when possible and finally fall back to stable browser array order.
 */
export function selectPreferredController(gamepads, selectedIndex = null) {
  if (!Array.isArray(gamepads)) return null;
  const connected = gamepads.filter((gamepad) => gamepad && gamepad.connected !== false);
  const selected = connected.find((gamepad) => gamepad.index === selectedIndex) ?? null;
  if (selected?.mapping === 'standard') return selected;
  return connected.find((gamepad) => gamepad.mapping === 'standard') ?? selected ?? connected[0] ?? null;
}

export class GamepadEscapeHold {
  constructor(holdMs = 1000) {
    this.holdMs = holdMs;
    this.startedAt = null;
    this.triggered = false;
  }

  update(state, nowMs) {
    const held = state?.connected
      && (state.buttons[PROTOCOL_BUTTONS.back] || 0) >= 0.5
      && (state.buttons[PROTOCOL_BUTTONS.start] || 0) >= 0.5;
    if (!held) {
      this.startedAt = null;
      this.triggered = false;
      return false;
    }
    if (this.startedAt === null) this.startedAt = nowMs;
    if (!this.triggered && nowMs - this.startedAt >= this.holdMs) {
      this.triggered = true;
      return true;
    }
    return false;
  }

  reset() {
    this.startedAt = null;
    this.triggered = false;
  }
}

export function navigationDirection(state, threshold = STICK_NAV_THRESHOLD) {
  if (!state?.connected) return null;
  const pressed = (index) => (state.buttons[index] || 0) >= 0.5;
  if (pressed(BUTTON_DPAD_UP)) return 'up';
  if (pressed(BUTTON_DPAD_DOWN)) return 'down';
  if (pressed(BUTTON_DPAD_LEFT)) return 'left';
  if (pressed(BUTTON_DPAD_RIGHT)) return 'right';

  const x = state.axes[0] || 0;
  const y = state.axes[1] || 0;
  if (Math.abs(x) < threshold && Math.abs(y) < threshold) return null;
  if (Math.abs(x) > Math.abs(y)) return x < 0 ? 'left' : 'right';
  return y < 0 ? 'up' : 'down';
}

/** Turns sampled controller state into edge-triggered actions plus bounded nav repeat. */
export class ControllerActionRepeater {
  constructor({
    repeatDelayMs = NAV_REPEAT_DELAY_MS,
    repeatIntervalMs = NAV_REPEAT_INTERVAL_MS,
  } = {}) {
    this.repeatDelayMs = repeatDelayMs;
    this.repeatIntervalMs = repeatIntervalMs;
    this.reset();
  }

  reset() {
    this.direction = null;
    this.nextRepeatAt = 0;
    this.aPressed = false;
    this.bPressed = false;
  }

  update(state, nowMs) {
    if (!state?.connected) {
      this.reset();
      return [];
    }
    const actions = [];
    const direction = navigationDirection(state);
    if (direction !== this.direction) {
      this.direction = direction;
      this.nextRepeatAt = direction === null ? 0 : nowMs + this.repeatDelayMs;
      if (direction !== null) actions.push({ type: 'navigate', direction });
    } else if (direction !== null && nowMs >= this.nextRepeatAt) {
      actions.push({ type: 'navigate', direction });
      this.nextRepeatAt = nowMs + this.repeatIntervalMs;
    }

    const aPressed = (state.buttons[BUTTON_A] || 0) >= 0.5;
    const bPressed = (state.buttons[BUTTON_B] || 0) >= 0.5;
    if (aPressed && !this.aPressed) actions.push({ type: 'activate' });
    if (bPressed && !this.bPressed) actions.push({ type: 'back' });
    this.aPressed = aPressed;
    this.bPressed = bPressed;
    return actions;
  }
}

function center(rect) {
  return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
}

/**
 * Pick the nearest element in a visual direction. The large cross-axis penalty
 * makes rows and columns feel stable while still allowing irregular layouts.
 */
export function chooseDirectionalIndex(rects, currentIndex, direction) {
  if (!Array.isArray(rects) || rects.length === 0) return -1;
  if (!Number.isInteger(currentIndex) || currentIndex < 0 || currentIndex >= rects.length) return 0;
  const origin = center(rects[currentIndex]);
  let bestIndex = currentIndex;
  let bestScore = Infinity;
  for (let index = 0; index < rects.length; index++) {
    if (index === currentIndex) continue;
    const candidate = center(rects[index]);
    const dx = candidate.x - origin.x;
    const dy = candidate.y - origin.y;
    let primary;
    let cross;
    if (direction === 'left') { primary = -dx; cross = Math.abs(dy); }
    else if (direction === 'right') { primary = dx; cross = Math.abs(dy); }
    else if (direction === 'up') { primary = -dy; cross = Math.abs(dx); }
    else if (direction === 'down') { primary = dy; cross = Math.abs(dx); }
    else return currentIndex;
    if (primary <= 1) continue;
    const score = primary + cross * 2.5;
    if (score < bestScore) {
      bestScore = score;
      bestIndex = index;
    }
  }
  return bestIndex;
}

/**
 * A single-slot async publisher. If transport is slow, intermediate samples are
 * replaced and only the latest controller state is delivered after it catches up.
 */
export class LatestControllerStatePublisher {
  constructor() {
    this.latest = disconnectedControllerState();
    this.handler = null;
    this.revision = 0;
    this.deliveredRevision = 0;
    this.inFlight = false;
  }

  setHandler(handler) {
    if (handler !== null && typeof handler !== 'function') {
      throw new TypeError('controller state handler must be a function or null');
    }
    this.handler = handler;
    if (handler) this.#pump();
  }

  publish(state) {
    this.latest = state;
    this.revision += 1;
    this.#pump();
  }

  #pump() {
    if (this.inFlight || !this.handler || this.deliveredRevision === this.revision) return;
    const revision = this.revision;
    const state = this.latest;
    const handler = this.handler;
    this.inFlight = true;
    Promise.resolve()
      .then(() => handler(state))
      .catch((error) => console.warn('controller state handler failed:', error))
      .finally(() => {
        this.inFlight = false;
        this.deliveredRevision = revision;
        this.#pump();
      });
  }
}
