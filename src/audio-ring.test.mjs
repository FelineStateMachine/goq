import assert from 'node:assert/strict';
import test from 'node:test';

import {
  AUDIO_FRAME_DURATION_MICROS,
  BoundedAudioMessageTracker,
  BoundedAudioRing,
} from './audio-ring.mjs';

function stereo(values) {
  return [Float32Array.from(values), Float32Array.from(values, (value) => -value)];
}

test('prebuffers, renders in order, and re-prebuffers after underflow', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 4 });
  const output = stereo([0, 0, 0, 0]);
  ring.write(stereo([1, 2]));
  assert.equal(ring.read(output), 0);
  ring.write(stereo([3, 4]));
  assert.equal(ring.read(output), 4);
  assert.deepEqual([...output[0]], [1, 2, 3, 4]);
  assert.deepEqual([...output[1]], [-1, -2, -3, -4]);

  ring.write(stereo([5, 6, 7, 8]));
  const longOutput = stereo([0, 0, 0, 0, 0, 0]);
  assert.equal(ring.read(longOutput), 4);
  assert.equal(ring.underflows, 1);
  assert.equal(ring.started, false);
});

test('drops oldest frames on overflow and remains strictly bounded', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2, 3]));
  assert.equal(ring.write(stereo([4, 5, 6])), 2);
  assert.equal(ring.length, 4);
  const output = stereo([0, 0, 0, 0]);
  ring.read(output);
  assert.deepEqual([...output[0]], [3, 4, 5, 6]);
  assert.equal(ring.droppedFrames, 2);
});

test('mute consumes current audio and clear removes all session state', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 2 });
  ring.write(stereo([1, 2, 3, 4]));
  const output = stereo([9, 9]);
  assert.equal(ring.read(output, true), 2);
  assert.deepEqual([...output[0]], [0, 0]);
  assert.equal(ring.length, 2);
  ring.clear();
  assert.equal(ring.length, 0);
  assert.equal(ring.started, false);
  assert.equal(ring.read(output), 0);
});

test('rejects malformed channel data and invalid bounds', () => {
  assert.throws(() => new BoundedAudioRing({ capacityFrames: 2, startFrames: 3 }));
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  assert.throws(() => ring.write([Float32Array.of(1)]));
  assert.throws(() => ring.write([Float32Array.of(1), Float32Array.of(1, 2)]));
  assert.throws(() => ring.read([Float32Array.of(0)]));
});

test('bounds decoded PCM messages until the worklet accepts their exact IDs', () => {
  const tracker = new BoundedAudioMessageTracker(3);
  const first = tracker.reserve();
  const second = tracker.reserve();
  const third = tracker.reserve();
  assert.deepEqual([first, second, third], [1, 2, 3]);
  assert.equal(tracker.size, 3);
  assert.equal(tracker.reserve(), null);
  assert.equal(tracker.droppedMessages, 1);
  assert.equal(tracker.accept(99), false);
  assert.equal(tracker.accept(second), true);
  assert.equal(tracker.accept(second), false);
  assert.equal(tracker.reserve(), 4);
  assert.equal(tracker.size, 3);
});

test('recovery preserves transferred PCM ownership until exact worklet acknowledgments', () => {
  const tracker = new BoundedAudioMessageTracker(2);
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 2 });
  const first = tracker.reserve();
  const second = tracker.reserve();
  ring.write(stereo([1, 2, 3, 4]));

  // The recovery clear is FIFO behind both transferred sample messages. It may
  // discard their ring frames, but cannot release main-thread ownership early.
  ring.clear({ recovery: true });
  assert.equal(tracker.size, 2);
  assert.equal(tracker.reserve(), null);
  assert.equal(ring.snapshot().recoveryDiscardedFrames, 4);

  assert.equal(tracker.accept(first), true);
  assert.equal(tracker.reserve(), 3);
  assert.equal(tracker.size, 2);
  assert.equal(tracker.accept(second), true);
});

test('clears pending worklet ownership and resets tracker telemetry', () => {
  assert.throws(() => new BoundedAudioMessageTracker(0));
  const tracker = new BoundedAudioMessageTracker(1);
  tracker.reserve();
  tracker.reserve();
  tracker.clear();
  assert.equal(tracker.size, 0);
  assert.equal(tracker.droppedMessages, 1);
  tracker.reset();
  assert.equal(tracker.droppedMessages, 0);
  assert.equal(tracker.reserve(), 1);
});

test('ring telemetry exposes worklet overflow drops', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2, 3]));
  ring.write(stereo([4, 5, 6]));
  assert.equal(ring.snapshot().droppedFrames, 2);
  assert.equal(ring.snapshot().recoveryDiscardedFrames, 0);
});

test('counts only explicitly marked recovery clears in exact PCM frames', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 2 });
  ring.write(stereo([1, 2, 3]));
  ring.clear();
  assert.equal(ring.snapshot().recoveryDiscardedFrames, 0);

  ring.write(stereo([4, 5, 6, 7]));
  ring.clear({ recovery: true });
  assert.equal(ring.snapshot().recoveryDiscardedFrames, 4);

  ring.write(stereo([8, 9]));
  ring.clear({ recovery: true });
  assert.equal(ring.snapshot().recoveryDiscardedFrames, 6);
  assert.equal(ring.snapshot().droppedFrames, 0);
});

test('tracks media PTS exactly across partial reads', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 2 });
  ring.write(stereo([1, 2, 3, 4, 5, 6]), 1_000_000);

  assert.equal(ring.read(stereo([0, 0])), 2);
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    1_000_000 + 2 * AUDIO_FRAME_DURATION_MICROS,
  );
  assert.equal(ring.read(stereo([0, 0, 0])), 3);
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    1_000_000 + 5 * AUDIO_FRAME_DURATION_MICROS,
  );
});

test('keeps timestamp metadata aligned while dropping oldest overflow frames', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2, 3]), 0);
  ring.write(stereo([4, 5, 6]), 100_000);

  const output = stereo([0, 0, 0, 0]);
  assert.equal(ring.read(output), 4);
  assert.deepEqual([...output[0]], [3, 4, 5, 6]);
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    100_000 + 3 * AUDIO_FRAME_DURATION_MICROS,
  );
});

test('offsets PTS when an oversized packet drops its own oldest frames', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  assert.equal(ring.write(stereo([1, 2, 3, 4, 5, 6]), 500_000), 2);

  const output = stereo([0, 0, 0, 0]);
  assert.equal(ring.read(output), 4);
  assert.deepEqual([...output[0]], [3, 4, 5, 6]);
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    500_000 + 6 * AUDIO_FRAME_DURATION_MICROS,
  );
});

test('clears the rendered PTS anchor and accepts a discontinuous new timeline', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2]), 10_000);
  ring.read(stereo([0, 0]));
  assert.notEqual(ring.snapshot().renderedMediaEndPtsMicros, null);

  ring.clear();
  assert.equal(ring.snapshot().renderedMediaEndPtsMicros, null);
  ring.write(stereo([3, 4]), 9_000_000);
  ring.read(stereo([0, 0]));
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    9_000_000 + 2 * AUDIO_FRAME_DURATION_MICROS,
  );
});

test('counts every silent and underflow frame without rounding duration', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 8, startFrames: 2 });
  const quantum = stereo([0, 0, 0, 0]);

  ring.write(stereo([1]), 0);
  assert.equal(ring.read(quantum), 0);
  assert.deepEqual(
    {
      silentFrames: ring.snapshot().silentFrames,
      underflowFrames: ring.snapshot().underflowFrames,
    },
    { silentFrames: 4, underflowFrames: 0 },
  );

  ring.write(stereo([2]), 20_000);
  assert.equal(ring.read(quantum), 2);
  assert.equal(ring.underflows, 1);
  assert.equal(ring.snapshot().silentFrames, 6);
  assert.equal(ring.snapshot().underflowFrames, 2);
  assert.equal(ring.snapshot().renderedMediaEndPtsMicros, null);

  assert.equal(ring.read(quantum), 0);
  const snapshot = ring.snapshot();
  assert.equal(snapshot.silentFrames, 10);
  assert.equal(snapshot.silentDurationMicros, 10 * AUDIO_FRAME_DURATION_MICROS);
  assert.equal(snapshot.underflowFrames, 6);
  assert.equal(snapshot.underflowDurationMicros, 6 * AUDIO_FRAME_DURATION_MICROS);
});

test('untimestamped and silent quanta expose a nullable media PTS', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2]));
  ring.read(stereo([0, 0]));
  assert.equal(ring.snapshot().renderedMediaEndPtsMicros, null);

  ring.clear();
  ring.read(stereo([0, 0]));
  assert.equal(ring.snapshot().renderedMediaEndPtsMicros, null);
  assert.throws(() => ring.write(stereo([1, 2]), Number.NaN));
  assert.throws(() => ring.write(stereo([1, 2]), 1.5));
  assert.throws(() => ring.write(stereo([1, 2]), Number.MAX_SAFE_INTEGER + 1));
});

test('muting consumes media timestamps while preserving intentional silence semantics', () => {
  const ring = new BoundedAudioRing({ capacityFrames: 4, startFrames: 2 });
  ring.write(stereo([1, 2]), 1_000);
  const output = stereo([9, 9]);
  assert.equal(ring.read(output, true), 2);
  assert.deepEqual([...output[0]], [0, 0]);
  assert.equal(ring.snapshot().silentFrames, 0);
  assert.equal(
    ring.snapshot().renderedMediaEndPtsMicros,
    1_000 + 2 * AUDIO_FRAME_DURATION_MICROS,
  );
});
