export const MAX_HELD_KEYS = 32;
export const MAX_HELD_MOUSE_BUTTONS = 3;
export const MAX_RELATIVE_POINTER_DELTA = 32767;
export const MIN_POINTER_SURFACE_WIDTH = 64;
export const MAX_POINTER_SURFACE_WIDTH = 7680;
export const MIN_POINTER_SURFACE_HEIGHT = 64;
export const MAX_POINTER_SURFACE_HEIGHT = 4320;

export function inputCapabilityLabel(capabilities = {}) {
  const accepted = [];
  const pointer = capabilities.relativePointer === true
    || capabilities.absolutePointer === true;
  if (pointer && capabilities.keyboard === true) accepted.push('kbm');
  else {
    if (pointer) accepted.push('mouse');
    if (capabilities.keyboard === true) accepted.push('keyboard');
  }
  if (capabilities.text === true) accepted.push('text');
  if (capabilities.gamepad === true) accepted.push('controller');
  return accepted.length > 0 ? accepted.join(' + ') : 'view only';
}

function roundedRelativeAxis(value) {
  if (!Number.isFinite(value)) return 0;
  const rounded = Math.round(value);
  return Number.isSafeInteger(rounded) ? rounded : 0;
}

function boundedRelativeAxis(value) {
  const rounded = roundedRelativeAxis(value);
  return Math.max(
    -MAX_RELATIVE_POINTER_DELTA,
    Math.min(MAX_RELATIVE_POINTER_DELTA, rounded),
  );
}

export function scaleRelativePointerDelta(dx, dy) {
  // Relative motion is device displacement, not a position in either the
  // streamed surface or the local canvas. Rescaling it by those dimensions
  // changes sensitivity whenever the window or stream resolution changes.
  return {
    dx: roundedRelativeAxis(dx),
    dy: roundedRelativeAxis(dy),
  };
}

export function validatePointerSurfaceDimensions(value) {
  if (value === null || value === undefined) return null;
  const width = value?.width;
  const height = value?.height;
  if (
    !Number.isSafeInteger(width)
    || width < MIN_POINTER_SURFACE_WIDTH
    || width > MAX_POINTER_SURFACE_WIDTH
    || !Number.isSafeInteger(height)
    || height < MIN_POINTER_SURFACE_HEIGHT
    || height > MAX_POINTER_SURFACE_HEIGHT
  ) {
    throw new TypeError('invalid pointer surface dimensions');
  }
  return { width, height };
}

export function resolvePointerSurfaceSize(
  pointerSurfaceDimensions,
  frameWidth,
  frameHeight,
  _relativePointer,
) {
  if (pointerSurfaceDimensions !== null) {
    return pointerSurfaceDimensions;
  }
  if (
    !Number.isFinite(frameWidth)
    || frameWidth <= 0
    || !Number.isFinite(frameHeight)
    || frameHeight <= 0
  ) return null;
  return { width: frameWidth, height: frameHeight };
}

export function mapCanvasPointToSurface({ clientX, clientY, rect, surface }) {
  if (
    !Number.isFinite(clientX)
    || !Number.isFinite(clientY)
    || !Number.isFinite(rect?.left)
    || !Number.isFinite(rect?.top)
    || !Number.isFinite(rect?.width)
    || rect.width <= 0
    || !Number.isFinite(rect?.height)
    || rect.height <= 0
    || !Number.isSafeInteger(surface?.width)
    || surface.width <= 0
    || !Number.isSafeInteger(surface?.height)
    || surface.height <= 0
  ) return null;
  return {
    x: Math.max(0, Math.min(surface.width - 1,
      Math.floor((clientX - rect.left) * surface.width / rect.width))),
    y: Math.max(0, Math.min(surface.height - 1,
      Math.floor((clientY - rect.top) * surface.height / rect.height))),
  };
}

export function validatePointerPositionFeedback(value, surface = null) {
  if (!Number.isSafeInteger(value?.sequence) || value.sequence < 0) {
    throw new TypeError('invalid pointer feedback sequence');
  }
  if (value.position === null) {
    const pointerVisible = value.pointer_visible;
    if (pointerVisible !== undefined && typeof pointerVisible !== 'boolean') {
      throw new TypeError('invalid pointer feedback visibility');
    }
    if (pointerVisible === true) {
      throw new TypeError('visible pointer feedback requires a position');
    }
    return {
      sequence: value.sequence,
      position: null,
      pointer_visible: pointerVisible ?? false,
    };
  }
  const x = value.position?.x;
  const y = value.position?.y;
  if (
    !Number.isSafeInteger(x)
    || x < 0
    || x > MAX_RELATIVE_POINTER_DELTA
    || !Number.isSafeInteger(y)
    || y < 0
    || y > MAX_RELATIVE_POINTER_DELTA
  ) {
    throw new TypeError('invalid pointer feedback position');
  }
  if (surface !== null && (x >= surface.width || y >= surface.height)) {
    throw new TypeError('pointer feedback is outside the negotiated surface');
  }
  const pointerVisible = value.pointer_visible;
  if (pointerVisible !== undefined && typeof pointerVisible !== 'boolean') {
    throw new TypeError('invalid pointer feedback visibility');
  }
  return {
    sequence: value.sequence,
    position: { x, y },
    // Legacy hosts omitted the visibility field and only sent a position while
    // Gamescope's cursor was visible.
    pointer_visible: pointerVisible ?? true,
  };
}

export function advanceRemotePointerPosition(position, movement, width, height) {
  const dimensions = [width, height];
  if (
    !position
    || dimensions.some((value) => !Number.isFinite(value) || value <= 0)
  ) return null;
  const x = Number.isFinite(position.x) ? position.x : 0;
  const y = Number.isFinite(position.y) ? position.y : 0;
  const dx = Number.isFinite(movement?.dx) ? movement.dx : 0;
  const dy = Number.isFinite(movement?.dy) ? movement.dy : 0;
  return {
    x: Math.max(0, Math.min(Math.floor(width) - 1, Math.round(x + dx))),
    y: Math.max(0, Math.min(Math.floor(height) - 1, Math.round(y + dy))),
  };
}

export function browserPointerLockLossRequiresControlExit({
  browserPointerLockRequired,
  pointerLockElement,
  expectedElement,
  controlMode,
  controlTransitionInProgress,
}) {
  return browserPointerLockRequired === true
    && pointerLockElement !== expectedElement
    && (controlMode === true || controlTransitionInProgress === true);
}

// Relative motion is displacement, not latest-value state. Preserve every
// browser sample in one constant-size accumulator. Each take emits at most one
// protocol-sized chunk and subtracts it, so large totals are never clamped or
// reordered around a following pointer transition.
export class RelativePointerAccumulator {
  constructor() {
    this.dx = 0;
    this.dy = 0;
  }

  add(dx, dy) {
    const nextDx = roundedRelativeAxis(dx);
    const nextDy = roundedRelativeAxis(dy);
    if (nextDx === 0 && nextDy === 0) return false;
    const totalDx = this.dx + nextDx;
    const totalDy = this.dy + nextDy;
    if (!Number.isSafeInteger(totalDx) || !Number.isSafeInteger(totalDy)) {
      throw new RangeError('relative pointer displacement overflow');
    }
    this.dx = totalDx;
    this.dy = totalDy;
    return true;
  }

  take() {
    if (!this.pending) return null;
    const dx = boundedRelativeAxis(this.dx);
    const dy = boundedRelativeAxis(this.dy);
    const event = { t: 'mr', dx, dy };
    this.dx -= dx;
    this.dy -= dy;
    return event;
  }

  restore(event) {
    if (event?.t !== 'mr') return false;
    return this.add(event.dx, event.dy);
  }

  clear() {
    this.dx = 0;
    this.dy = 0;
  }

  get pending() {
    return this.dx !== 0 || this.dy !== 0;
  }

  get chunkCount() {
    return Math.max(
      Math.ceil(Math.abs(this.dx) / MAX_RELATIVE_POINTER_DELTA),
      Math.ceil(Math.abs(this.dy) / MAX_RELATIVE_POINTER_DELTA),
    );
  }
}

export class PointerMotionBuffer {
  constructor() {
    this.absolute = null;
    this.relative = new RelativePointerAccumulator();
  }

  setAbsolute(event) {
    if (event?.t !== 'mm') return false;
    this.absolute = event;
    return true;
  }

  addRelative(dx, dy) {
    return this.relative.add(dx, dy);
  }

  take() {
    if (this.absolute !== null) {
      const event = this.absolute;
      this.absolute = null;
      return event;
    }
    return this.relative.take();
  }

  takeBarrierBefore(event) {
    const ordered = [];
    let motion;
    while ((motion = this.take()) !== null) ordered.push(motion);
    ordered.push(event);
    return ordered;
  }

  restore(event) {
    if (event?.t === 'mm') {
      // Absolute motion is latest-value state. A rejected older invocation
      // must not replace a newer coordinate sampled while it was in flight.
      if (this.absolute !== null) return false;
      return this.setAbsolute(event);
    }
    if (event?.t === 'mr') return this.relative.restore(event);
    return false;
  }

  clear() {
    this.absolute = null;
    this.relative.clear();
  }

  get pending() {
    return this.absolute !== null || this.relative.pending;
  }

  get barrierLength() {
    return Number(this.absolute !== null) + this.relative.chunkCount;
  }
}

export function restoreRejectedPointerMotion(
  reliableQueue,
  motionBuffer,
  event,
  queueCapacity,
) {
  if (!Array.isArray(reliableQueue) || !(motionBuffer instanceof PointerMotionBuffer)) {
    throw new TypeError('invalid pointer restore state');
  }
  if (!Number.isSafeInteger(queueCapacity) || queueCapacity <= 0) {
    throw new TypeError('invalid reliable queue capacity');
  }
  if (event?.t !== 'mm' && event?.t !== 'mr') {
    throw new TypeError('rejected pointer motion is invalid');
  }
  if (reliableQueue.length > 0) {
    if (reliableQueue.length >= queueCapacity) {
      throw new Error('reliable input restore reserve exhausted');
    }
    reliableQueue.unshift(event);
    return 'queued';
  }
  return motionBuffer.restore(event) ? 'motion' : 'superseded';
}

export function browserMouseButtonCode(button) {
  if (button === 0) return 1;
  if (button === 2) return 2;
  if (button === 1) return 3;
  return null;
}

// Retains values only so matching releases can be emitted. Callers must never
// include key values in diagnostics.
export class HeldInputState {
  constructor(maxHeldKeys = MAX_HELD_KEYS) {
    this.maxHeldKeys = maxHeldKeys;
    this.keys = new Map();
    this.mouseButtons = new Set();
  }

  trackKey(id, value) {
    if (this.keys.has(id)) return 'repeat';
    if (this.keys.size >= this.maxHeldKeys) return 'full';
    this.keys.set(id, value);
    return 'tracked';
  }

  takeKeyRelease(id) {
    if (!this.keys.has(id)) return null;
    const value = this.keys.get(id);
    this.keys.delete(id);
    return { t: 'ku', k: value };
  }

  trackMouseButton(button) {
    if (!Number.isInteger(button) || button < 1 || button > MAX_HELD_MOUSE_BUTTONS) return false;
    if (this.mouseButtons.has(button)) return false;
    if (this.mouseButtons.size >= MAX_HELD_MOUSE_BUTTONS) return false;
    this.mouseButtons.add(button);
    return true;
  }

  takeMouseButtonRelease(button) {
    if (!this.mouseButtons.delete(button)) return null;
    return { t: 'mu', b: button };
  }

  releaseEvents() {
    const releases = [];
    for (const value of this.keys.values()) releases.push({ t: 'ku', k: value });
    for (const button of this.mouseButtons.values()) releases.push({ t: 'mu', b: button });
    return releases;
  }

  clear() {
    this.keys.clear();
    this.mouseButtons.clear();
  }

  get size() {
    return this.keys.size + this.mouseButtons.size;
  }
}
