import assert from 'node:assert/strict';
import test from 'node:test';

import { buildVideoDecoderConfig } from './video-decoder-config.mjs';

test('requests the standardized WebCodecs low-latency decoder mode', () => {
  const description = new Uint8Array([1, 100, 0, 31]);
  const config = buildVideoDecoderConfig({
    codec: 'avc1.64001f',
    width: 1280,
    height: 800,
    description,
  });

  assert.deepEqual(config, {
    codec: 'avc1.64001f',
    codedWidth: 1280,
    codedHeight: 800,
    description,
    optimizeForLatency: true,
  });
  assert.equal(config.description, description);
  assert.equal(Object.hasOwn(config, 'optimizeForRealtimeUse'), false);
});

test('decoder config rejects invalid codecs, dimensions, and descriptions', () => {
  const valid = {
    codec: 'avc1.64001f',
    width: 1280,
    height: 800,
    description: new Uint8Array([1]),
  };

  assert.throws(() => buildVideoDecoderConfig({ ...valid, codec: '  ' }), TypeError);
  assert.throws(() => buildVideoDecoderConfig({ ...valid, width: 0 }), RangeError);
  assert.throws(() => buildVideoDecoderConfig({ ...valid, width: 8192, height: 8192 }), RangeError);
  assert.throws(() => buildVideoDecoderConfig({ ...valid, description: [1] }), TypeError);
});
