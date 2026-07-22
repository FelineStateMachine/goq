import assert from 'node:assert/strict';
import test from 'node:test';

import {
  MAX_DECODE_QUEUE_SIZE,
  createVideoPipelineSession,
} from './video-pipeline.mjs';

const h264Keyframe = () => new Uint8Array([
  0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f,
  0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80,
  0, 0, 0, 1, 0x09, 0xf0,
  0, 0, 0, 1, 0x65, 0x88, 0x84,
]);

const h264Delta = () => new Uint8Array([
  0, 0, 0, 1, 0x09, 0xf0,
  0, 0, 0, 1, 0x41, 0x9a,
]);

const h265Keyframe = () => new Uint8Array([
  0, 0, 0, 1, 0x40, 0x01, 0x0c,
  0, 0, 0, 1, 0x42, 0x01, 0x01, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x5d, 0x78,
  0, 0, 0, 1, 0x44, 0x01, 0xc0,
  0, 0, 0, 1, 0x46, 0x01,
  0, 0, 0, 1, 0x26, 0x01, 0xaa,
]);

const av1Keyframe = () => new Uint8Array([
  0x62, 0x02, 0x20, 0x00,
  0x12, 0x00,
  0x32, 0x02, 0xaa, 0xbb,
]);

function harness({ hasWebCodecs = true } = {}) {
  let clock = 10;
  const decoders = [];
  const chunks = [];
  const keyframeRequests = [];
  const audioResets = [];
  const formats = [];
  const draws = [];
  const revokedUrls = [];
  const images = [];
  const canvas = { width: 0, height: 0 };
  const context = {
    drawImage: (...args) => draws.push(args),
  };

  class FakeDecoder {
    constructor(callbacks) {
      this.callbacks = callbacks;
      this.decodeQueueSize = 0;
      this.decoded = [];
      this.configureCalls = [];
      this.closeCalls = 0;
      decoders.push(this);
    }

    configure(config) { this.configureCalls.push(config); }
    decode(chunk) { this.decoded.push(chunk); }
    close() { this.closeCalls++; }
  }

  const pipeline = createVideoPipelineSession({
    hasWebCodecs,
    canvas,
    context,
    requestKeyframe: (reason) => keyframeRequests.push(reason),
    resetAudioSync: (...args) => audioResets.push(args),
    sampleAudioSkew: () => null,
    onFormatChanged: (format) => formats.push(format),
    now: () => clock,
    createDecoder: (callbacks) => new FakeDecoder(callbacks),
    createEncodedChunk: (init) => {
      const chunk = { ...init };
      chunks.push(chunk);
      return chunk;
    },
    decodeBase64: () => new Uint8Array([1, 2, 3]),
    createBlob: (parts, options) => ({ parts, options }),
    createObjectUrl: () => 'blob:test-frame',
    revokeObjectUrl: (url) => revokedUrls.push(url),
    createImage: () => {
      const image = { onload: null, src: null };
      images.push(image);
      return image;
    },
    requestFrame: () => 1,
    cancelFrame: () => {},
    setTimer: () => 2,
    cancelTimer: () => {},
  });

  return {
    pipeline,
    canvas,
    decoders,
    chunks,
    keyframeRequests,
    audioResets,
    formats,
    draws,
    revokedUrls,
    images,
    advance(milliseconds = 1) { clock += milliseconds; },
  };
}

function processH264Keyframe(subject, overrides = {}) {
  subject.pipeline.processFramePayload({
    width: 1280,
    height: 800,
    codec: 'h264',
    keyframe: true,
    codecConfig: true,
    sequence: 1,
    pts_micros: 1_000,
    discontinuity: false,
    data: null,
    ...overrides,
  }, h264Keyframe());
}

test('configured H.264 keyframes commit format only after decode enqueue', () => {
  const subject = harness();
  processH264Keyframe(subject);

  assert.equal(subject.decoders.length, 1);
  assert.equal(subject.decoders[0].configureCalls.length, 1);
  assert.deepEqual(subject.decoders[0].configureCalls[0], {
    codec: 'avc1.64001f',
    codedWidth: 1280,
    codedHeight: 800,
    description: subject.decoders[0].configureCalls[0].description,
    optimizeForLatency: true,
  });
  assert.deepEqual(subject.pipeline.format, {
    codec: 'h264', width: 1280, height: 800, epoch: 1,
  });
  assert.deepEqual(subject.formats, [subject.pipeline.format]);
  assert.equal(subject.chunks.length, 1);
  assert.equal(subject.chunks[0].type, 'key');
  assert.deepEqual([...subject.chunks[0].data], [0, 0, 0, 3, 0x65, 0x88, 0x84]);
  assert.deepEqual(subject.keyframeRequests, []);
  const stats = subject.pipeline.snapshot();
  assert.equal(stats.receivedFrames, 1);
  assert.equal(stats.decoderInputFrames, 1);
  assert.equal(stats.droppedFrames, 0);
  assert.equal(stats.recovering, false);
  assert.equal(stats.decoderQueueCapacity, MAX_DECODE_QUEUE_SIZE);
});

test('configured H.265 keyframes keep parameter sets out of the decoded chunk', () => {
  const subject = harness();
  subject.pipeline.processFramePayload({
    width: 1920,
    height: 1080,
    codec: 'h265',
    keyframe: true,
    codecConfig: true,
    sequence: 1,
    pts_micros: 2_000,
    discontinuity: false,
    data: null,
  }, h265Keyframe());

  assert.equal(subject.decoders.length, 1);
  assert.deepEqual(subject.decoders[0].configureCalls[0], {
    codec: 'hvc1.01.4.L120.B0',
    codedWidth: 1920,
    codedHeight: 1080,
    description: subject.decoders[0].configureCalls[0].description,
    optimizeForLatency: true,
  });
  assert.equal(subject.decoders[0].configureCalls[0].description.byteLength, 58);
  assert.deepEqual(subject.pipeline.format, {
    codec: 'h265', width: 1920, height: 1080, epoch: 1,
  });
  assert.deepEqual(subject.formats, [subject.pipeline.format]);
  assert.equal(subject.chunks.length, 1);
  assert.equal(subject.chunks[0].type, 'key');
  assert.equal(subject.chunks[0].timestamp, 2_000);
  assert.deepEqual([...subject.chunks[0].data], [0, 0, 0, 3, 0x26, 0x01, 0xaa]);
});

test('configured AV1 keyframes keep sequence and temporal headers out of the decoded chunk', () => {
  const subject = harness();
  subject.pipeline.processFramePayload({
    width: 1280,
    height: 720,
    codec: 'av1',
    keyframe: true,
    codecConfig: true,
    sequence: 1,
    pts_micros: 3_000,
    discontinuity: false,
    data: null,
  }, av1Keyframe());

  assert.equal(subject.decoders.length, 1);
  assert.deepEqual(subject.decoders[0].configureCalls[0], {
    codec: 'av01.1.00M.08',
    codedWidth: 1280,
    codedHeight: 720,
    description: subject.decoders[0].configureCalls[0].description,
    optimizeForLatency: true,
  });
  assert.deepEqual(
    [...new Uint8Array(subject.decoders[0].configureCalls[0].description)],
    [0x62, 0x02, 0x20, 0x00],
  );
  assert.deepEqual(subject.pipeline.format, {
    codec: 'av1', width: 1280, height: 720, epoch: 1,
  });
  assert.deepEqual(subject.formats, [subject.pipeline.format]);
  assert.equal(subject.chunks.length, 1);
  assert.equal(subject.chunks[0].type, 'key');
  assert.equal(subject.chunks[0].timestamp, 3_000);
  assert.deepEqual([...subject.chunks[0].data], [0x32, 0x02, 0xaa, 0xbb]);
});

test('stale decoder callbacks cannot mutate a replacement decoder session', () => {
  const subject = harness();
  processH264Keyframe(subject);
  const stale = subject.decoders[0];

  processH264Keyframe(subject, { sequence: 2, discontinuity: true, pts_micros: 2_000 });
  assert.equal(subject.decoders.length, 2);
  const staleFrame = { timestamp: 1_000, closeCalls: 0, close() { this.closeCalls++; } };
  stale.callbacks.output(staleFrame);
  stale.callbacks.error(new Error('stale decoder error'));

  assert.equal(staleFrame.closeCalls, 1);
  assert.equal(subject.pipeline.snapshot().decoderOutputFrames, 0);
  assert.deepEqual(subject.keyframeRequests, []);
  assert.equal(subject.pipeline.format.epoch, 2);
});

test('decode queue overload enters one bounded recovery episode and drops deltas', () => {
  const subject = harness();
  processH264Keyframe(subject);
  subject.decoders[0].decodeQueueSize = MAX_DECODE_QUEUE_SIZE;
  subject.pipeline.processFramePayload({
    width: 1280,
    height: 800,
    codec: 'h264',
    keyframe: false,
    codecConfig: false,
    sequence: 2,
    pts_micros: 2_000,
    data: null,
  }, h264Delta());

  assert.deepEqual(subject.keyframeRequests, ['frontend-backpressure']);
  assert.equal(subject.decoders[0].closeCalls, 1);
  const stats = subject.pipeline.snapshot();
  assert.equal(stats.receivedFrames, 2);
  assert.equal(stats.decoderInputFrames, 1);
  assert.equal(stats.droppedFrames, 1);
  assert.equal(stats.recovering, true);
  assert.equal(stats.decoderQueueDepth, 0);
});

test('JPEG fallback keeps its asynchronous base64 draw behavior', () => {
  const subject = harness({ hasWebCodecs: false });
  subject.pipeline.processFramePayload({
    width: 640,
    height: 400,
    data: 'AQID',
    keyframe: true,
  }, new Uint8Array([9, 9, 9]));

  assert.deepEqual(subject.pipeline.format, {
    codec: 'h264', width: 640, height: 400, epoch: 0,
  });
  assert.deepEqual(subject.formats, [subject.pipeline.format]);
  assert.equal(subject.canvas.width, 640);
  assert.equal(subject.canvas.height, 400);
  assert.equal(subject.images[0].src, 'blob:test-frame');
  assert.deepEqual(subject.draws, []);
  assert.deepEqual(subject.revokedUrls, []);

  subject.advance();
  subject.images[0].onload();
  assert.equal(subject.draws.length, 1);
  assert.deepEqual(subject.revokedUrls, ['blob:test-frame']);
  assert.equal(subject.pipeline.snapshot().presentedFrames, 1);
});

test('reset closes decoding state but preserves the last dimensions and codec quirk', () => {
  const subject = harness();
  processH264Keyframe(subject);
  subject.pipeline.reset();

  assert.equal(subject.decoders[0].closeCalls, 1);
  assert.deepEqual(subject.pipeline.format, {
    codec: 'h264', width: 1280, height: 800, epoch: 0,
  });
  const stats = subject.pipeline.snapshot();
  assert.equal(stats.receivedFrames, 0);
  assert.equal(stats.decoderInputFrames, 0);
  assert.equal(stats.recovering, true);
});
