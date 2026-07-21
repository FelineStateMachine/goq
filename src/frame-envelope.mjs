export const FRAME_ENVELOPE_HEADER_LENGTH = 40;
export const MAX_FRAME_PAYLOAD_LENGTH = 16 * 1024 * 1024;
export const MAX_FRAME_DIMENSION = 8192;
export const MAX_FRAME_PIXELS = 7680 * 4320;

const MAGIC = [0x53, 0x47, 0x46, 0x52]; // SGFR
const VERSION = 1;
const KNOWN_FLAGS = 0b00000011;
const KEYFRAME_FLAG = 1 << 0;
const DISCONTINUITY_FLAG = 1 << 1;
const OPTIONAL_U64_NONE = 0xffffffffffffffffn;
const OPTIONAL_I64_NONE = -0x8000000000000000n;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);
const CODECS = Object.freeze({ 1: 'h264', 2: 'h265', 3: 'av1' });

function exactOptionalUnsigned(value, field) {
  if (value === OPTIONAL_U64_NONE) return null;
  if (value > MAX_SAFE_BIGINT) {
    throw new Error(`${field} exceeds JavaScript's exact integer range`);
  }
  return Number(value);
}

function exactOptionalTimestamp(value) {
  if (value === OPTIONAL_I64_NONE) return null;
  if (value < 0n || value > MAX_SAFE_BIGINT) {
    throw new Error('frame PTS is outside the supported exact range');
  }
  return Number(value);
}

/**
 * Parse the fixed v1 Rust→webview frame envelope without allocating or copying
 * its encoded payload. Every byte in the header and the total message length is
 * validated before a payload view is returned.
 */
export function parseFrameEnvelope(message) {
  if (!(message instanceof ArrayBuffer)) {
    throw new TypeError('frame channel message must be an ArrayBuffer');
  }
  if (message.byteLength < FRAME_ENVELOPE_HEADER_LENGTH) {
    throw new Error('frame channel message is shorter than its header');
  }

  const bytes = new Uint8Array(message);
  const view = new DataView(message);
  for (let index = 0; index < MAGIC.length; index++) {
    if (bytes[index] !== MAGIC[index]) throw new Error('invalid frame channel magic');
  }
  if (bytes[4] !== VERSION) throw new Error(`unsupported frame channel version: ${bytes[4]}`);

  const codec = CODECS[bytes[5]];
  if (!codec) throw new Error(`unsupported frame channel codec: ${bytes[5]}`);
  const flags = bytes[6];
  if ((flags & ~KNOWN_FLAGS) !== 0) throw new Error('frame channel has unknown flag bits');
  if (bytes[7] !== 0) throw new Error('frame channel reserved byte must be zero');

  const width = view.getUint16(8, false);
  const height = view.getUint16(10, false);
  if (
    width === 0 ||
    height === 0 ||
    width > MAX_FRAME_DIMENSION ||
    height > MAX_FRAME_DIMENSION ||
    width * height > MAX_FRAME_PIXELS
  ) {
    throw new Error(`invalid frame channel dimensions: ${width}x${height}`);
  }

  const payloadLength = view.getUint32(12, false);
  if (payloadLength === 0 || payloadLength > MAX_FRAME_PAYLOAD_LENGTH) {
    throw new Error(`invalid frame channel payload length: ${payloadLength}`);
  }
  const expectedLength = FRAME_ENVELOPE_HEADER_LENGTH + payloadLength;
  if (message.byteLength !== expectedLength) {
    throw new Error(`frame channel length mismatch: expected ${expectedLength}, got ${message.byteLength}`);
  }

  const sequence = exactOptionalUnsigned(view.getBigUint64(16, false), 'frame sequence');
  const captureTimestampMicros = exactOptionalUnsigned(
    view.getBigUint64(24, false),
    'capture timestamp',
  );
  const ptsMicros = exactOptionalTimestamp(view.getBigInt64(32, false));

  return {
    width,
    height,
    codec,
    keyframe: (flags & KEYFRAME_FLAG) !== 0,
    discontinuity: (flags & DISCONTINUITY_FLAG) !== 0,
    sequence,
    captureTimestampMicros,
    ptsMicros,
    data: new Uint8Array(message, FRAME_ENVELOPE_HEADER_LENGTH, payloadLength),
  };
}
