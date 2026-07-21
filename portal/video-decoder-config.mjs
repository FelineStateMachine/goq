import {
  MAX_FRAME_DIMENSION,
  MAX_FRAME_PIXELS,
} from './frame-envelope.mjs';

/** Build the exact low-latency WebCodecs configuration used by every codec. */
export function buildVideoDecoderConfig({ codec, width, height, description }) {
  if (typeof codec !== 'string' || codec.trim().length === 0) {
    throw new TypeError('video decoder codec must be a non-empty string');
  }
  if (
    !Number.isSafeInteger(width) ||
    !Number.isSafeInteger(height) ||
    width <= 0 ||
    height <= 0 ||
    width > MAX_FRAME_DIMENSION ||
    height > MAX_FRAME_DIMENSION ||
    width * height > MAX_FRAME_PIXELS
  ) {
    throw new RangeError(`invalid video decoder dimensions: ${width}x${height}`);
  }
  if (!(description instanceof ArrayBuffer) && !ArrayBuffer.isView(description)) {
    throw new TypeError('video decoder description must be a BufferSource');
  }

  return {
    codec,
    codedWidth: width,
    codedHeight: height,
    description,
    optimizeForLatency: true,
  };
}
