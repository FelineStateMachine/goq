import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FRAME_ENVELOPE_HEADER_LENGTH,
  MAX_FRAME_PAYLOAD_LENGTH,
  parseFrameEnvelope,
} from './frame-envelope.mjs';

function frameEnvelope({
  codec = 1,
  flags = 7,
  width = 1280,
  height = 800,
  payload = Uint8Array.of(0, 0, 0, 1, 0x65),
  payloadLength = payload.length,
  sequence = 42n,
  captureTimestamp = 123456n,
  pts = 98765n,
} = {}) {
  const buffer = new ArrayBuffer(FRAME_ENVELOPE_HEADER_LENGTH + payload.length);
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);
  bytes.set([0x53, 0x47, 0x46, 0x52, 1, codec, flags, 0]);
  view.setUint16(8, width, false);
  view.setUint16(10, height, false);
  view.setUint32(12, payloadLength, false);
  view.setBigUint64(16, sequence, false);
  view.setBigUint64(24, captureTimestamp, false);
  view.setBigInt64(32, pts, false);
  bytes.set(payload, FRAME_ENVELOPE_HEADER_LENGTH);
  return buffer;
}

test('parses an exact v1 frame envelope without copying its payload', () => {
  const buffer = frameEnvelope();
  const frame = parseFrameEnvelope(buffer);
  assert.deepEqual(
    {
      width: frame.width,
      height: frame.height,
      codec: frame.codec,
      keyframe: frame.keyframe,
      discontinuity: frame.discontinuity,
      codecConfig: frame.codecConfig,
      sequence: frame.sequence,
      captureTimestampMicros: frame.captureTimestampMicros,
      ptsMicros: frame.ptsMicros,
    },
    {
      width: 1280,
      height: 800,
      codec: 'h264',
      keyframe: true,
      discontinuity: true,
      codecConfig: true,
      sequence: 42,
      captureTimestampMicros: 123456,
      ptsMicros: 98765,
    },
  );
  assert.deepEqual([...frame.data], [0, 0, 0, 1, 0x65]);
  new Uint8Array(buffer)[FRAME_ENVELOPE_HEADER_LENGTH] = 9;
  assert.equal(frame.data[0], 9);
});

test('flag bit positions match the sigil-protocol wire assignments', () => {
  const discontinuityOnly = parseFrameEnvelope(frameEnvelope({ flags: 0b100 }));
  assert.equal(discontinuityOnly.keyframe, false);
  assert.equal(discontinuityOnly.codecConfig, false);
  assert.equal(discontinuityOnly.discontinuity, true);

  const keyframeConfig = parseFrameEnvelope(frameEnvelope({ flags: 0b011 }));
  assert.equal(keyframeConfig.keyframe, true);
  assert.equal(keyframeConfig.codecConfig, true);
  assert.equal(keyframeConfig.discontinuity, false);
});

test('maps optional integer sentinels to null', () => {
  const frame = parseFrameEnvelope(frameEnvelope({
    sequence: 0xffffffffffffffffn,
    captureTimestamp: 0xffffffffffffffffn,
    pts: -0x8000000000000000n,
  }));
  assert.equal(frame.sequence, null);
  assert.equal(frame.captureTimestampMicros, null);
  assert.equal(frame.ptsMicros, null);
});

test('rejects malformed identity, version, codec, flags, and reserved fields', () => {
  const mutations = [
    (bytes) => { bytes[0] = 0; },
    (bytes) => { bytes[4] = 2; },
    (bytes) => { bytes[5] = 0; },
    (bytes) => { bytes[5] = 2; },
    (bytes) => { bytes[5] = 3; },
    (bytes) => { bytes[6] = 0x80; },
    (bytes) => { bytes[6] = 0b010; },
    (bytes) => { bytes[7] = 1; },
  ];
  for (const mutate of mutations) {
    const buffer = frameEnvelope();
    mutate(new Uint8Array(buffer));
    assert.throws(() => parseFrameEnvelope(buffer));
  }
});

test('rejects invalid dimensions and all length mismatches', () => {
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ width: 0 })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ width: 8192, height: 8192 })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ payloadLength: 0 })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ payloadLength: 4 })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ payloadLength: MAX_FRAME_PAYLOAD_LENGTH + 1 })));
  assert.throws(() => parseFrameEnvelope(new ArrayBuffer(FRAME_ENVELOPE_HEADER_LENGTH - 1)));
  assert.throws(() => parseFrameEnvelope(new Uint8Array(frameEnvelope())));
});

test('rejects timestamps that cannot be represented exactly', () => {
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ sequence: 9007199254740992n })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ captureTimestamp: 9007199254740992n })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ pts: -1n })));
  assert.throws(() => parseFrameEnvelope(frameEnvelope({ pts: 9007199254740992n })));
});
