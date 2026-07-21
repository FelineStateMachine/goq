import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { AUDIO_FRAME_DURATION_MICROS } from './audio-ring.mjs';

test('worklet reports its timeline and applies each recovery epoch plus clear atomically', async () => {
  const posted = [];
  let Processor = null;

  class TestAudioWorkletProcessor {
    constructor() {
      this.port = {
        onmessage: null,
        postMessage(message) {
          posted.push(message);
        },
      };
    }
  }

  globalThis.AudioWorkletProcessor = TestAudioWorkletProcessor;
  globalThis.registerProcessor = (name, processor) => {
    assert.equal(name, 'sigil-audio-processor');
    Processor = processor;
  };
  globalThis.sampleRate = 48_000;
  globalThis.currentFrame = 0;

  const workletUrl = new URL('./audio-worklet.js', import.meta.url);
  const ringUrl = new URL('./audio-ring.mjs', import.meta.url).href;
  const source = (await readFile(workletUrl, 'utf8'))
    .replace("'./audio-ring.mjs'", JSON.stringify(ringUrl));
  await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
  assert.equal(typeof Processor, 'function');

  const processor = new Processor();
  const frames = 2_048;
  processor.port.onmessage({
    data: {
      type: 'samples',
      id: 7,
      timestampMicros: 1_000_000,
      channels: [new Float32Array(frames), new Float32Array(frames)],
    },
  });
  assert.deepEqual(posted.shift(), { type: 'accepted', id: 7 });

  for (let quantum = 0; quantum < 16; quantum++) {
    globalThis.currentFrame = quantum * 128;
    assert.equal(
      processor.process([], [[new Float32Array(128), new Float32Array(128)]]),
      true,
    );
  }

  assert.equal(posted.length, 1);
  const stats = posted.shift();
  assert.equal(stats.type, 'stats');
  assert.equal(stats.avSyncEpoch, 0);
  assert.equal(stats.renderedMediaEndPtsMicros, 1_000_000 + frames * AUDIO_FRAME_DURATION_MICROS);
  assert.equal(stats.outputQuantumFrames, 128);
  assert.equal(stats.outputContextFrameEnd, 2_048);
  assert.equal(stats.outputContextTimeEndSeconds, 2_048 / 48_000);
  assert.equal(stats.outputContextSampleRate, 48_000);
  assert.equal(stats.silentFrames, 0);
  assert.equal(stats.underflowFrames, 0);

  processor.port.onmessage({ data: { type: 'av-sync-epoch', avSyncEpoch: 1 } });
  globalThis.currentFrame = frames;
  processor.process([], [[new Float32Array(128), new Float32Array(128)]]);
  assert.equal(posted.length, 1, 'underrun must invalidate the timeline immediately');
  const underrunStats = posted.shift();
  assert.equal(underrunStats.avSyncEpoch, 1);
  assert.equal(underrunStats.renderedMediaEndPtsMicros, null);
  assert.equal(underrunStats.underflows, 1);

  for (let quantum = 1; quantum < 16; quantum++) {
    globalThis.currentFrame = frames + quantum * 128;
    processor.process([], [[new Float32Array(128), new Float32Array(128)]]);
  }
  const postResetStats = posted.shift();
  assert.equal(postResetStats.avSyncEpoch, 1);
  assert.equal(postResetStats.renderedMediaEndPtsMicros, null);

  processor.port.onmessage({
    data: {
      type: 'samples',
      id: 8,
      timestampMicros: 2_000_000,
      channels: [new Float32Array(256), new Float32Array(256)],
    },
  });
  assert.deepEqual(posted.shift(), { type: 'accepted', id: 8 });
  processor.port.onmessage({ data: { type: 'clear' } });
  for (let quantum = 0; quantum < 16; quantum++) {
    processor.process([], [[new Float32Array(128), new Float32Array(128)]]);
  }
  assert.equal(posted.shift().recoveryDiscardedFrames, 0);

  processor.port.onmessage({
    data: {
      type: 'samples',
      id: 9,
      timestampMicros: 3_000_000,
      channels: [new Float32Array(320), new Float32Array(320)],
    },
  });
  assert.deepEqual(posted.shift(), { type: 'accepted', id: 9 });
  processor.port.onmessage({ data: { type: 'recover', avSyncEpoch: 2 } });
  assert.equal(processor.avSyncEpoch, 2);
  assert.equal(processor.ring.length, 0);
  assert.equal(processor.ring.recoveryDiscardedFrames, 320);
  for (let quantum = 0; quantum < 16; quantum++) {
    processor.process([], [[new Float32Array(128), new Float32Array(128)]]);
  }
  const firstRecoveryStats = posted.shift();
  assert.equal(firstRecoveryStats.avSyncEpoch, 2);
  assert.equal(firstRecoveryStats.recoveryDiscardedFrames, 320);

  processor.port.onmessage({
    data: {
      type: 'samples',
      id: 10,
      timestampMicros: 4_000_000,
      channels: [new Float32Array(64), new Float32Array(64)],
    },
  });
  assert.deepEqual(posted.shift(), { type: 'accepted', id: 10 });
  processor.port.onmessage({ data: { type: 'recover', avSyncEpoch: 3 } });
  assert.equal(processor.avSyncEpoch, 3);
  assert.equal(processor.ring.length, 0);
  assert.equal(processor.ring.recoveryDiscardedFrames, 384);
  for (let quantum = 0; quantum < 16; quantum++) {
    processor.process([], [[new Float32Array(128), new Float32Array(128)]]);
  }
  const secondRecoveryStats = posted.shift();
  assert.equal(secondRecoveryStats.avSyncEpoch, 3);
  assert.equal(secondRecoveryStats.recoveryDiscardedFrames, 384);
});
