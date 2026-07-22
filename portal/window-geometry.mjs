const MIN_WINDOW_WIDTH = 480;
const MIN_STREAM_HEIGHT = 270;
const MAX_WINDOW_DIMENSION = 8192;

function positiveFinite(value, name) {
  if (!Number.isFinite(value) || value <= 0) throw new RangeError(`${name} must be positive`);
  return value;
}

function geometryInputs({ frameWidth, frameHeight, chromeHeight }) {
  const width = positiveFinite(frameWidth, 'frame width');
  const height = positiveFinite(frameHeight, 'frame height');
  const chrome = Number(chromeHeight);
  if (!Number.isFinite(chrome) || chrome < 0 || chrome > 512) {
    throw new RangeError('window chrome height is invalid');
  }
  return { ratio: width / height, chrome };
}

function roundedGeometry(width, height) {
  return Object.freeze({
    width: Math.round(Math.min(MAX_WINDOW_DIMENSION, width)),
    height: Math.round(Math.min(MAX_WINDOW_DIMENSION, height)),
  });
}

export function streamAspectKey(frameWidth, frameHeight) {
  const width = positiveFinite(frameWidth, 'frame width');
  const height = positiveFinite(frameHeight, 'frame height');
  if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height)) {
    throw new RangeError('stream dimensions must be exact integers');
  }
  let a = width;
  let b = height;
  while (b !== 0) [a, b] = [b, a % b];
  return `${width / a}:${height / a}`;
}

export function fitInitialStreamWindow({
  frameWidth,
  frameHeight,
  chromeHeight,
  availableWidth,
  availableHeight,
  fill = 0.88,
}) {
  const { ratio, chrome } = geometryInputs({ frameWidth, frameHeight, chromeHeight });
  const screenWidth = positiveFinite(availableWidth, 'available width');
  const screenHeight = positiveFinite(availableHeight, 'available height');
  if (!Number.isFinite(fill) || fill < 0.5 || fill > 1) throw new RangeError('window fill is invalid');

  const widthLimit = screenWidth * fill;
  const heightLimit = screenHeight * fill;
  const streamHeightLimit = Math.max(1, heightLimit - chrome);
  let width = Math.min(widthLimit, streamHeightLimit * ratio);
  const minimumWidth = Math.min(MIN_WINDOW_WIDTH, widthLimit, streamHeightLimit * ratio);
  width = Math.max(minimumWidth, width);
  return roundedGeometry(width, chrome + width / ratio);
}

export function constrainStreamWindowResize({
  frameWidth,
  frameHeight,
  chromeHeight,
  width,
  height,
  previousWidth,
  previousHeight,
}) {
  const { ratio, chrome } = geometryInputs({ frameWidth, frameHeight, chromeHeight });
  const currentWidth = positiveFinite(width, 'window width');
  const currentHeight = positiveFinite(height, 'window height');
  const oldWidth = positiveFinite(previousWidth, 'previous window width');
  const oldHeight = positiveFinite(previousHeight, 'previous window height');
  const streamHeight = Math.max(1, currentHeight - chrome);
  const expectedHeight = chrome + currentWidth / ratio;
  if (Math.abs(expectedHeight - currentHeight) <= 1) return null;

  const widthDelta = Math.abs(currentWidth - oldWidth);
  const heightDelta = Math.abs(currentHeight - oldHeight);
  if (widthDelta >= heightDelta) {
    const adjustedWidth = Math.max(Math.min(MAX_WINDOW_DIMENSION, currentWidth), MIN_WINDOW_WIDTH);
    return roundedGeometry(adjustedWidth, chrome + adjustedWidth / ratio);
  }

  const adjustedStreamHeight = Math.max(
    Math.min(MAX_WINDOW_DIMENSION - chrome, streamHeight),
    MIN_STREAM_HEIGHT,
  );
  return roundedGeometry(adjustedStreamHeight * ratio, chrome + adjustedStreamHeight);
}

export function fitStreamSurface({ frameWidth, frameHeight, availableWidth, availableHeight }) {
  const { ratio } = geometryInputs({ frameWidth, frameHeight, chromeHeight: 0 });
  const widthLimit = positiveFinite(availableWidth, 'available surface width');
  const heightLimit = positiveFinite(availableHeight, 'available surface height');
  const width = Math.min(widthLimit, heightLimit * ratio);
  return roundedGeometry(width, width / ratio);
}
