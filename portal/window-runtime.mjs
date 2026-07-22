import {
  constrainStreamWindowResize,
  fitInitialStreamWindow,
  fitStreamSurface,
  streamAspectKey,
} from './window-geometry.mjs';

export const STREAM_WINDOW_CORRECTION_DELAY_MS = 80;

export function createWindowRuntime({
  isConnected,
  getFormat,
  getGeneration,
  getChromeHeight,
  getScreenBounds,
  getWindowSize,
  getSurfaceBounds,
  setSurfaceSize,
  applyNativeGeometry,
  scheduleTimeout = globalThis.setTimeout,
  cancelTimeout = globalThis.clearTimeout,
  logger = console,
} = {}) {
  const requiredCallbacks = {
    isConnected,
    getFormat,
    getGeneration,
    getChromeHeight,
    getScreenBounds,
    getWindowSize,
    getSurfaceBounds,
    setSurfaceSize,
    applyNativeGeometry,
  };
  for (const [name, callback] of Object.entries(requiredCallbacks)) {
    if (typeof callback !== 'function') throw new TypeError(`${name} must be a function`);
  }

  let fittedStreamAspect = null;
  let pendingStreamFit = null;
  let lastObservedWindowSize = null;
  let streamWindowResizeTimer = null;

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

  async function applyWindowGeometry(geometry, unmaximize) {
    try {
      const applied = await applyNativeGeometry(geometry, unmaximize);
      if (applied) lastObservedWindowSize = { ...geometry };
      return applied;
    } catch (error) {
      logger.warn('could not apply stream window geometry:', error);
      return false;
    }
  }

  async function fitWindowToIncomingStream() {
    const {
      width: frameWidth,
      height: frameHeight,
      epoch: activeVideoFormatEpoch,
    } = getFormat();
    if (!isConnected() || frameWidth < 1 || frameHeight < 1) return false;
    const aspect = streamAspectKey(frameWidth, frameHeight);
    if (fittedStreamAspect === aspect || pendingStreamFit?.aspect === aspect) return false;
    const request = { aspect, generation: getGeneration(), epoch: activeVideoFormatEpoch };
    pendingStreamFit = request;
    const screen = getScreenBounds();
    const geometry = fitInitialStreamWindow({
      frameWidth,
      frameHeight,
      chromeHeight: getChromeHeight(),
      availableWidth: screen.width,
      availableHeight: screen.height,
    });
    const applied = await applyWindowGeometry(geometry, true);
    const currentFormat = getFormat();
    const currentAspect = currentFormat.width > 0 && currentFormat.height > 0
      ? streamAspectKey(currentFormat.width, currentFormat.height)
      : null;
    const current = pendingStreamFit === request
      && isConnected()
      && getGeneration() === request.generation
      && currentFormat.epoch === request.epoch
      && currentAspect === request.aspect;
    if (pendingStreamFit === request) pendingStreamFit = null;
    if (applied && current) fittedStreamAspect = request.aspect;
    return applied && current;
  }

  function scheduleAspectCorrection() {
    const observed = getWindowSize();
    const previous = lastObservedWindowSize ?? observed;
    lastObservedWindowSize = observed;
    if (streamWindowResizeTimer !== null) cancelTimeout(streamWindowResizeTimer);
    streamWindowResizeTimer = scheduleTimeout(() => {
      streamWindowResizeTimer = null;
      const { width: frameWidth, height: frameHeight } = getFormat();
      if (!isConnected() || frameWidth < 1 || frameHeight < 1) return;
      const current = getWindowSize();
      let geometry;
      try {
        geometry = constrainStreamWindowResize({
          frameWidth,
          frameHeight,
          chromeHeight: getChromeHeight(),
          width: current.width,
          height: current.height,
          previousWidth: previous.width,
          previousHeight: previous.height,
        });
      } catch (error) {
        logger.warn('could not constrain stream window geometry:', error);
        return;
      }
      if (geometry !== null) void applyWindowGeometry(geometry, false);
    }, STREAM_WINDOW_CORRECTION_DELAY_MS);
  }

  function reset() {
    fittedStreamAspect = null;
    pendingStreamFit = null;
    lastObservedWindowSize = null;
    if (streamWindowResizeTimer !== null) cancelTimeout(streamWindowResizeTimer);
    streamWindowResizeTimer = null;
  }

  function snapshot() {
    return Object.freeze({
      fittedStreamAspect,
      pendingStreamFit: pendingStreamFit === null ? null : Object.freeze({ ...pendingStreamFit }),
      lastObservedWindowSize: lastObservedWindowSize === null
        ? null : Object.freeze({ ...lastObservedWindowSize }),
      correctionScheduled: streamWindowResizeTimer !== null,
    });
  }

  return Object.freeze({
    applyWindowGeometry,
    fitWindowToIncomingStream,
    reset,
    scheduleAspectCorrection,
    sizeSurfaceToIncomingStream,
    snapshot,
  });
}
