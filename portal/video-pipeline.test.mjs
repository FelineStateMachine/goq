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
  const imageStarts = [];
  const abortedImages = [];
  let nextObjectUrl = 0;
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
    createObjectUrl: () => `blob:test-frame-${++nextObjectUrl}`,
    revokeObjectUrl: (url) => revokedUrls.push(url),
    createImage: () => {
      let src = null;
      const image = {
        onload: null,
        onerror: null,
        get src() { return src; },
        set src(value) {
          src = value;
          imageStarts.push({ image: this, value });
        },
      };
      images.push(image);
      return image;
    },
    abortImage: (image) => {
      abortedImages.push(image);
      image.src = null;
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
    imageStarts,
    abortedImages,
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
  assert.equal(stats.presenterQueueDepth, 0);
  assert.equal(stats.presenterQueueCapacity, 2);
});

test('unsupported codecs are dropped before decoder construction', () => {
  for (const codec of ['h265', 'av1']) {
    const subject = harness();
    subject.pipeline.processFramePayload({
      width: 1280,
      height: 800,
      codec,
      keyframe: true,
      codecConfig: true,
      sequence: 1,
      pts_micros: 2_000,
      discontinuity: false,
      data: null,
    }, h264Keyframe());

    assert.equal(subject.decoders.length, 0);
    assert.equal(subject.chunks.length, 0);
    assert.deepEqual(subject.pipeline.format, {
      codec: 'h264', width: 0, height: 0, epoch: 0,
    });
    assert.equal(subject.pipeline.snapshot().droppedFrames, 1);
  }
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
  assert.equal(subject.images[0].src, 'blob:test-frame-1');
  assert.deepEqual(subject.draws, []);
  assert.deepEqual(subject.revokedUrls, []);
  assert.equal(subject.pipeline.snapshot().presenterQueueDepth, 1);
  assert.equal(subject.pipeline.snapshot().presenterQueueCapacity, 2);

  subject.advance();
  subject.images[0].onload();
  assert.equal(subject.draws.length, 1);
  assert.deepEqual(subject.revokedUrls, ['blob:test-frame-1']);
  assert.equal(subject.pipeline.snapshot().presentedFrames, 1);
  assert.equal(subject.pipeline.snapshot().presenterQueueDepth, 0);
  assert.equal(subject.pipeline.snapshot().presenterQueueCapacity, 2);
});

test('JPEG fallback suppresses an active stale decode before starting the newest frame', () => {
  const subject = harness({ hasWebCodecs: false });
  const payload = {
    width: 640,
    height: 400,
    data: 'AQID',
    keyframe: true,
  };
  subject.pipeline.processFramePayload(payload);
  const staleLoad = subject.images[0].onload;
  const staleError = subject.images[0].onerror;

  subject.advance(10);
  subject.pipeline.processFramePayload(payload);
  assert.equal(subject.images.length, 1);
  assert.deepEqual(subject.revokedUrls, []);
  const afterOverwrite = subject.pipeline.snapshot();
  assert.equal(afterOverwrite.presentationDroppedFrames, 1);
  assert.equal(afterOverwrite.presenterQueueDepth, 2);
  assert.equal(afterOverwrite.presenterQueueCapacity, 2);

  subject.advance(5);
  staleLoad();
  assert.deepEqual(subject.draws, []);
  assert.deepEqual(subject.revokedUrls, ['blob:test-frame-1']);
  assert.equal(subject.images.length, 2);
  assert.equal(subject.images[1].src, 'blob:test-frame-2');
  const newestLoad = subject.images[1].onload;
  assert.equal(subject.pipeline.snapshot().presenterQueueDepth, 1);

  subject.advance(5);
  newestLoad();
  staleError(new Error('stale JPEG failure'));
  const afterNewest = subject.pipeline.snapshot();
  assert.deepEqual(subject.draws.map(([image]) => image), [subject.images[1]]);
  assert.equal(afterNewest.presentedFrames, 1);
  assert.equal(afterNewest.presentPercentiles.count, 1);
  assert.equal(afterNewest.presentPercentiles.p50, 10);
  assert.equal(afterNewest.presentationDroppedFrames, 1);
  assert.equal(afterNewest.droppedFrames, 0);
  assert.equal(afterNewest.presenterQueueDepth, 0);
  assert.equal(afterNewest.presenterQueueCapacity, 2);
  assert.deepEqual(subject.revokedUrls, [
    'blob:test-frame-1',
    'blob:test-frame-2',
  ]);
});

test('JPEG fallback error callbacks affect only the current pending frame', () => {
  const subject = harness({ hasWebCodecs: false });
  const payload = {
    width: 640,
    height: 400,
    data: 'AQID',
    keyframe: true,
  };
  subject.pipeline.processFramePayload(payload);
  const staleError = subject.images[0].onerror;

  subject.pipeline.processFramePayload(payload);
  staleError(new Error('stale JPEG failure'));
  assert.equal(subject.pipeline.snapshot().droppedFrames, 0);
  assert.equal(subject.images.length, 2);
  const failedLoad = subject.images[1].onload;
  const currentError = subject.images[1].onerror;

  currentError(new Error('current JPEG failure'));
  failedLoad();
  const afterFailure = subject.pipeline.snapshot();
  assert.equal(afterFailure.droppedFrames, 1);
  assert.equal(afterFailure.presentedFrames, 0);
  assert.equal(afterFailure.presentationDroppedFrames, 1);
  assert.equal(afterFailure.presenterQueueDepth, 0);
  assert.equal(afterFailure.presenterQueueCapacity, 2);
  assert.deepEqual(subject.draws, []);
  assert.deepEqual(subject.revokedUrls, [
    'blob:test-frame-1',
    'blob:test-frame-2',
  ]);
});

test('JPEG fallback callbacks cannot survive a session reset', () => {
  const subject = harness({ hasWebCodecs: false });
  subject.pipeline.processFramePayload({
    width: 640,
    height: 400,
    data: 'AQID',
    keyframe: true,
  });
  const staleLoad = subject.images[0].onload;
  const staleError = subject.images[0].onerror;

  subject.pipeline.reset();
  subject.advance();
  staleLoad();
  staleError(new Error('reset JPEG failure'));

  const stats = subject.pipeline.snapshot();
  assert.equal(stats.receivedFrames, 0);
  assert.equal(stats.presentedFrames, 0);
  assert.equal(stats.droppedFrames, 0);
  assert.equal(stats.presentPercentiles.count, 0);
  assert.equal(stats.presenterQueueDepth, 0);
  assert.equal(stats.presenterQueueCapacity, 2);
  assert.deepEqual(subject.draws, []);
  assert.deepEqual(subject.revokedUrls, ['blob:test-frame-1']);
  assert.deepEqual(subject.abortedImages, [subject.images[0]]);
});

test('JPEG fallback bounds delayed decoding to one active and one coalesced latest payload', () => {
  const subject = harness({ hasWebCodecs: false });
  const payload = {
    width: 640,
    height: 400,
    data: 'AQID',
    keyframe: true,
  };
  subject.pipeline.processFramePayload(payload);
  const oldestLoad = subject.images[0].onload;

  for (let frame = 2; frame <= 10; frame++) {
    subject.advance();
    subject.pipeline.processFramePayload(payload);
  }

  assert.equal(subject.images.length, 1);
  assert.deepEqual(subject.imageStarts, [{
    image: subject.images[0],
    value: 'blob:test-frame-1',
  }]);
  assert.deepEqual(subject.revokedUrls, []);
  let stats = subject.pipeline.snapshot();
  assert.equal(stats.presentationDroppedFrames, 9);
  assert.equal(stats.presenterQueueDepth, 2);
  assert.equal(stats.presenterQueueCapacity, 2);

  subject.advance();
  oldestLoad();
  assert.equal(subject.images.length, 2);
  assert.deepEqual(subject.imageStarts, [
    { image: subject.images[0], value: 'blob:test-frame-1' },
    { image: subject.images[1], value: 'blob:test-frame-2' },
  ]);
  assert.deepEqual(subject.revokedUrls, ['blob:test-frame-1']);
  stats = subject.pipeline.snapshot();
  assert.equal(stats.presenterQueueDepth, 1);
  assert.equal(stats.presenterQueueCapacity, 2);

  subject.advance();
  subject.images[1].onload();
  stats = subject.pipeline.snapshot();
  assert.deepEqual(subject.draws.map(([image]) => image), [subject.images[1]]);
  assert.equal(stats.presentedFrames, 1);
  assert.equal(stats.presentPercentiles.p50, 2);
  assert.equal(stats.presentationDroppedFrames, 9);
  assert.equal(stats.presenterQueueDepth, 0);
  assert.equal(stats.presenterQueueCapacity, 2);
  assert.deepEqual(subject.revokedUrls, [
    'blob:test-frame-1',
    'blob:test-frame-2',
  ]);
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
