const H264_PROBE_CONFIG = Object.freeze({
  // Sigil's proven encoder path currently emits High Profile, Level 3.1 AVC.
  // Deliberately omit coded dimensions: this tests the emitted codec contract
  // without pinning Portal's capability decision to one host resolution.
  codec: 'avc1.64001f',
  optimizeForLatency: true,
});

/**
 * Report WebCodecs only when this webview can decode Sigil's current H.264
 * format. API presence alone is insufficient on codec-stripped webviews.
 */
export async function probeH264WebCodecsSupport({
  VideoDecoder = globalThis.VideoDecoder,
  logger = console,
} = {}) {
  if (typeof VideoDecoder !== 'function'
    || typeof VideoDecoder.isConfigSupported !== 'function') return false;

  try {
    const support = await VideoDecoder.isConfigSupported({ ...H264_PROBE_CONFIG });
    return support?.supported === true;
  } catch (error) {
    logger.warn('WebCodecs H.264 capability probe failed:', error);
    return false;
  }
}

/** Publish one delivery mode to both halves of Portal before a connection. */
export async function detectAndPublishVideoDeliveryMode({
  invokeCommand,
  VideoDecoder = globalThis.VideoDecoder,
  logger = console,
} = {}) {
  if (typeof invokeCommand !== 'function') {
    throw new TypeError('invokeCommand must be a function');
  }

  const available = await probeH264WebCodecsSupport({ VideoDecoder, logger });
  try {
    const effectiveAvailable = await invokeCommand('set_webcodecs_available', { available });
    if (typeof effectiveAvailable !== 'boolean') {
      throw new TypeError('native video delivery publication returned an invalid mode');
    }
    if (available && !effectiveAvailable) {
      logger.warn('development JPEG compatibility mode forced; WebCodecs probe passed but is not selected');
    }
    return effectiveAvailable;
  } catch (error) {
    logger.error('could not publish WebCodecs capability; using JPEG fallback:', error);
    return false;
  }
}
