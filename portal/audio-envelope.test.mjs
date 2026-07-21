import assert from 'node:assert/strict';
import test from 'node:test';

import {
  AUDIO_CHANNEL_PREFIX_LENGTH,
  AUDIO_ENVELOPE_HEADER_LENGTH,
  MAX_AUDIO_PAYLOAD_LENGTH,
  isCurrentAudioDelivery,
  isCurrentAudioGeneration,
  stageAudioTerminalState,
  takeAudioTerminalState,
  parseAudioEnvelope,
} from './audio-envelope.mjs';

const PACKET_BASE = AUDIO_CHANNEL_PREFIX_LENGTH;

function envelope({
  generation = 7n,
  deliveryId = 11n,
  flags = 1,
  channels = 2,
  sampleRate = 48000,
  frameSamples = 960,
  payload = Uint8Array.of(0xf8, 0xff, 0xfe),
  payloadLength = payload.length,
  sequence = 42n,
  captureTimestamp = 123456n,
  pts = 120000n,
} = {}) {
  const buffer = new ArrayBuffer(
    AUDIO_CHANNEL_PREFIX_LENGTH + AUDIO_ENVELOPE_HEADER_LENGTH + payload.length,
  );
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);
  bytes.set([0x53, 0x47, 0x41, 0x43]);
  view.setUint16(4, 1, false);
  view.setUint16(6, AUDIO_CHANNEL_PREFIX_LENGTH, false);
  view.setBigUint64(8, generation, false);
  view.setBigUint64(16, deliveryId, false);
  bytes.set([0x53, 0x47, 0x41, 0x31], PACKET_BASE);
  view.setUint16(PACKET_BASE + 4, 1, false);
  bytes[PACKET_BASE + 6] = AUDIO_ENVELOPE_HEADER_LENGTH;
  bytes[PACKET_BASE + 7] = 1;
  bytes[PACKET_BASE + 8] = flags;
  bytes[PACKET_BASE + 9] = channels;
  view.setUint32(PACKET_BASE + 12, sampleRate, false);
  view.setUint16(PACKET_BASE + 16, frameSamples, false);
  view.setUint32(PACKET_BASE + 20, payloadLength, false);
  view.setBigUint64(PACKET_BASE + 24, sequence, false);
  view.setBigUint64(PACKET_BASE + 32, captureTimestamp, false);
  view.setBigInt64(PACKET_BASE + 40, pts, false);
  bytes.set(payload, PACKET_BASE + AUDIO_ENVELOPE_HEADER_LENGTH);
  return buffer;
}

test('parses the delivery prefix and bounded Opus envelope without copying payload', () => {
  const buffer = envelope();
  const packet = parseAudioEnvelope(buffer);
  assert.deepEqual(
    {
      generation: packet.generation,
      deliveryId: packet.deliveryId,
      codec: packet.codec,
      discontinuity: packet.discontinuity,
      channels: packet.channels,
      sampleRate: packet.sampleRate,
      frameSamples: packet.frameSamples,
      sequence: packet.sequence,
      captureTimestampMicros: packet.captureTimestampMicros,
      ptsMicros: packet.ptsMicros,
    },
    {
      generation: 7,
      deliveryId: 11,
      codec: 'opus',
      discontinuity: true,
      channels: 2,
      sampleRate: 48000,
      frameSamples: 960,
      sequence: 42,
      captureTimestampMicros: 123456,
      ptsMicros: 120000,
    },
  );
  new Uint8Array(buffer)[PACKET_BASE + AUDIO_ENVELOPE_HEADER_LENGTH] = 9;
  assert.equal(packet.data[0], 9);
});

test('rejects malformed delivery and packet headers', () => {
  const mutations = [
    (bytes) => { bytes[0] = 0; },
    (bytes) => { bytes[5] = 2; },
    (bytes) => { bytes[7] = 23; },
    (bytes) => { bytes[PACKET_BASE] = 0; },
    (bytes) => { bytes[PACKET_BASE + 5] = 2; },
    (bytes) => { bytes[PACKET_BASE + 6] = 47; },
    (bytes) => { bytes[PACKET_BASE + 7] = 2; },
    (bytes) => { bytes[PACKET_BASE + 8] = 0x80; },
    (bytes) => { bytes[PACKET_BASE + 9] = 1; },
    (bytes) => { bytes[PACKET_BASE + 10] = 1; },
    (bytes) => { bytes[PACKET_BASE + 18] = 1; },
  ];
  for (const mutate of mutations) {
    const buffer = envelope();
    mutate(new Uint8Array(buffer));
    assert.throws(() => parseAudioEnvelope(buffer));
  }
  assert.throws(() => parseAudioEnvelope(envelope({ sampleRate: 44100 })));
  assert.throws(() => parseAudioEnvelope(envelope({ frameSamples: 480 })));
});

test('rejects length mismatches and integers unsafe to represent in JavaScript', () => {
  assert.throws(() => parseAudioEnvelope(envelope({ payloadLength: 0 })));
  assert.throws(() => parseAudioEnvelope(envelope({ payloadLength: 2 })));
  assert.throws(() => parseAudioEnvelope(envelope({ payloadLength: MAX_AUDIO_PAYLOAD_LENGTH + 1 })));
  assert.throws(() => parseAudioEnvelope(new ArrayBuffer(
    AUDIO_CHANNEL_PREFIX_LENGTH + AUDIO_ENVELOPE_HEADER_LENGTH - 1,
  )));
  assert.throws(() => parseAudioEnvelope(new Uint8Array(envelope())));
  assert.throws(() => parseAudioEnvelope(envelope({ generation: 9007199254740992n })));
  assert.throws(() => parseAudioEnvelope(envelope({ deliveryId: 9007199254740992n })));
  assert.throws(() => parseAudioEnvelope(envelope({ sequence: 9007199254740992n })));
  assert.throws(() => parseAudioEnvelope(envelope({ captureTimestamp: 9007199254740992n })));
  assert.throws(() => parseAudioEnvelope(envelope({ pts: -1n })));
});

test('matches deliveries only to the exact current safe generation', () => {
  const packet = parseAudioEnvelope(envelope({ generation: 91n }));
  assert.equal(isCurrentAudioDelivery(packet, 91), true);
  assert.equal(isCurrentAudioDelivery(packet, 90), false);
  assert.equal(isCurrentAudioDelivery(packet, null), false);
  assert.equal(isCurrentAudioDelivery(packet, Number.MAX_SAFE_INTEGER + 1), false);
  assert.equal(isCurrentAudioGeneration(91, 91), true);
  assert.equal(isCurrentAudioGeneration(undefined, undefined), false);
  assert.equal(isCurrentAudioGeneration(-1, -1), false);
});

test('pre-result audio terminal states are bounded and replay exact generation', () => {
  const pending = [];
  for (let generation = 1; generation <= 6; generation++) {
    stageAudioTerminalState(pending, {
      generation,
      available: false,
      error: `ended-${generation}`,
    });
  }
  assert.equal(pending.length, 4);
  assert.deepEqual(pending.map((state) => state.generation), [6, 5, 4, 3]);
  assert.deepEqual(takeAudioTerminalState(pending, 6), {
    generation: 6,
    available: false,
    error: 'ended-6',
  });
  assert.deepEqual(pending, []);
});
