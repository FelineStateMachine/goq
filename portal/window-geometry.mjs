const MAX_SURFACE_DIMENSION = 8192;

function positiveFinite(value, name) {
  if (!Number.isFinite(value) || value <= 0) throw new RangeError(`${name} must be positive`);
  return value;
}

function roundedSurface(width, height) {
  return Object.freeze({
    width: Math.round(Math.min(MAX_SURFACE_DIMENSION, width)),
    height: Math.round(Math.min(MAX_SURFACE_DIMENSION, height)),
  });
}

export function fitStreamSurface({ frameWidth, frameHeight, availableWidth, availableHeight }) {
  const width = positiveFinite(frameWidth, 'frame width');
  const height = positiveFinite(frameHeight, 'frame height');
  const widthLimit = positiveFinite(availableWidth, 'available surface width');
  const heightLimit = positiveFinite(availableHeight, 'available surface height');
  const fittedWidth = Math.min(widthLimit, heightLimit * (width / height));
  return roundedSurface(fittedWidth, fittedWidth / (width / height));
}
