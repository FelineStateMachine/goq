import { fitStreamSurface } from './window-geometry.mjs';

export function createWindowRuntime({
  getFormat,
  getSurfaceBounds,
  setSurfaceSize,
} = {}) {
  const requiredCallbacks = {
    getFormat,
    getSurfaceBounds,
    setSurfaceSize,
  };
  for (const [name, callback] of Object.entries(requiredCallbacks)) {
    if (typeof callback !== 'function') throw new TypeError(`${name} must be a function`);
  }

  function sizeSurfaceToIncomingStream() {
    const { width: frameWidth, height: frameHeight } = getFormat();
    if (frameWidth < 1 || frameHeight < 1) return false;
    const bounds = getSurfaceBounds();
    if (!bounds || bounds.width <= 0 || bounds.height <= 0) return false;
    const surface = fitStreamSurface({
      frameWidth,
      frameHeight,
      availableWidth: bounds.width,
      availableHeight: bounds.height,
    });
    setSurfaceSize(surface);
    return true;
  }

  return Object.freeze({ sizeSurfaceToIncomingStream });
}
