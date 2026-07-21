export const AUDIO_ENVELOPE_HEADER_LENGTH = 48;
export const AUDIO_CHANNEL_PREFIX_LENGTH = 24;
export const MAX_AUDIO_PAYLOAD_LENGTH = 512;
export const OPUS_SAMPLE_RATE = 48000;
export const OPUS_CHANNELS = 2;
export const OPUS_FRAME_SAMPLES = 960;
export const AUDIO_TERMINAL_STATE_CAPACITY = 4;

const CHANNEL_MAGIC = [0x53, 0x47, 0x41, 0x43]; // SGAC
const CHANNEL_VERSION = 1;
const MAGIC = [0x53, 0x47, 0x41, 0x31]; // SGA1
const VERSION = 1;
const OPUS_CODEC = 1;
const DISCONTINUITY_FLAG = 1 << 0;
const KNOWN_FLAGS = DISCONTINUITY_FLAG;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);

function exactUnsigned(value, field) {
  if (value > MAX_SAFE_BIGINT) throw new Error(`${field} exceeds JavaScript's exact integer range`);
  return Number(value);
}

function exactTimestamp(value) {
  if (value < 0n || value > MAX_SAFE_BIGINT) {
    throw new Error('audio PTS is outside the supported exact range');
  }
  return Number(value);
}

export function isCurrentAudioGeneration(generation, expectedGeneration) {
  return Number.isSafeInteger(generation)
    && generation >= 0
    && Number.isSafeInteger(expectedGeneration)
    && expectedGeneration >= 0
    && generation === expectedGeneration;
}

export function isCurrentAudioDelivery(packet, expectedGeneration) {
  return isCurrentAudioGeneration(packet?.generation, expectedGeneration);
}

export function stageAudioTerminalState(pendingStates, payload) {
  if (!Array.isArray(pendingStates)
    || !Number.isSafeInteger(payload?.generation)
    || payload.generation <= 0
    || payload.available !== false) {
    throw new TypeError('invalid audio terminal state');
  }
  const duplicate = pendingStates.findIndex(
    (state) => state.generation === payload.generation,
  );
  if (duplicate >= 0) pendingStates.splice(duplicate, 1);
  pendingStates.push(payload);
  pendingStates.sort((left, right) => right.generation - left.generation);
  if (pendingStates.length > AUDIO_TERMINAL_STATE_CAPACITY) {
    pendingStates.length = AUDIO_TERMINAL_STATE_CAPACITY;
  }
}

export function takeAudioTerminalState(pendingStates, generation) {
  if (!Array.isArray(pendingStates)) throw new TypeError('invalid audio terminal buffer');
  const terminal = pendingStates.find(
    (state) => isCurrentAudioGeneration(state.generation, generation),
  ) ?? null;
  pendingStates.length = 0;
  return terminal;
}

/** Parse one validated, delivery-token-prefixed Opus datagram from the Tauri channel. */
export function parseAudioEnvelope(message) {
  if (!(message instanceof ArrayBuffer)) {
    throw new TypeError('audio channel message must be an ArrayBuffer');
  }
  const minimumLength = AUDIO_CHANNEL_PREFIX_LENGTH + AUDIO_ENVELOPE_HEADER_LENGTH;
  if (message.byteLength < minimumLength) {
    throw new Error('audio channel message is shorter than its header');
  }
  const bytes = new Uint8Array(message);
  const view = new DataView(message);
  for (let index = 0; index < CHANNEL_MAGIC.length; index++) {
    if (bytes[index] !== CHANNEL_MAGIC[index]) throw new Error('invalid audio delivery magic');
  }
  if (view.getUint16(4, false) !== CHANNEL_VERSION) {
    throw new Error('unsupported audio delivery version');
  }
  if (view.getUint16(6, false) !== AUDIO_CHANNEL_PREFIX_LENGTH) {
    throw new Error('invalid audio delivery header length');
  }
  const generation = exactUnsigned(view.getBigUint64(8, false), 'audio generation');
  const deliveryId = exactUnsigned(view.getBigUint64(16, false), 'audio delivery ID');
  const base = AUDIO_CHANNEL_PREFIX_LENGTH;
  for (let index = 0; index < MAGIC.length; index++) {
    if (bytes[base + index] !== MAGIC[index]) throw new Error('invalid audio channel magic');
  }
  if (view.getUint16(base + 4, false) !== VERSION) {
    throw new Error('unsupported audio protocol version');
  }
  if (bytes[base + 6] !== AUDIO_ENVELOPE_HEADER_LENGTH) {
    throw new Error('invalid audio header length');
  }
  if (bytes[base + 7] !== OPUS_CODEC) {
    throw new Error(`unsupported audio codec: ${bytes[base + 7]}`);
  }
  const flags = bytes[base + 8];
  if ((flags & ~KNOWN_FLAGS) !== 0) throw new Error('audio channel has unknown flag bits');
  if (bytes[base + 9] !== OPUS_CHANNELS) {
    throw new Error(`unsupported audio channel count: ${bytes[base + 9]}`);
  }
  if (
    bytes[base + 10] !== 0
    || bytes[base + 11] !== 0
    || bytes[base + 18] !== 0
    || bytes[base + 19] !== 0
  ) {
    throw new Error('audio channel reserved bytes must be zero');
  }
  const sampleRate = view.getUint32(base + 12, false);
  const frameSamples = view.getUint16(base + 16, false);
  if (sampleRate !== OPUS_SAMPLE_RATE || frameSamples !== OPUS_FRAME_SAMPLES) {
    throw new Error(`unsupported audio format: ${sampleRate} Hz / ${frameSamples} samples`);
  }
  const payloadLength = view.getUint32(base + 20, false);
  if (payloadLength === 0 || payloadLength > MAX_AUDIO_PAYLOAD_LENGTH) {
    throw new Error(`invalid audio payload length: ${payloadLength}`);
  }
  const expectedLength = AUDIO_CHANNEL_PREFIX_LENGTH + AUDIO_ENVELOPE_HEADER_LENGTH + payloadLength;
  if (message.byteLength !== expectedLength) {
    throw new Error(`audio channel length mismatch: expected ${expectedLength}, got ${message.byteLength}`);
  }
  return {
    generation,
    deliveryId,
    codec: 'opus',
    discontinuity: (flags & DISCONTINUITY_FLAG) !== 0,
    channels: OPUS_CHANNELS,
    sampleRate,
    frameSamples,
    sequence: exactUnsigned(view.getBigUint64(base + 24, false), 'audio sequence'),
    captureTimestampMicros: exactUnsigned(
      view.getBigUint64(base + 32, false),
      'audio capture timestamp',
    ),
    ptsMicros: exactTimestamp(view.getBigInt64(base + 40, false)),
    data: new Uint8Array(message, base + AUDIO_ENVELOPE_HEADER_LENGTH, payloadLength),
  };
}
