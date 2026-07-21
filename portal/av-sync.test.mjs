import assert from 'node:assert/strict';
import test from 'node:test';

import {
  audioMinusVideoSkewMs,
  audioOutputTimelineFromStats,
  exactMediaTimestampMicros,
  formatSignedMilliseconds,
  isCurrentAvSyncEpoch,
  nextAvSyncEpoch,
  projectAudioMediaPtsMicros,
} from './av-sync.mjs';

test('media timestamps stay exact and reject fallback clock values', () => {
  assert.equal(exactMediaTimestampMicros(123_456), 123_456);
  assert.equal(exactMediaTimestampMicros(12.5), null);
  assert.equal(exactMediaTimestampMicros(-1), null);
  assert.equal(exactMediaTimestampMicros(Number.MAX_SAFE_INTEGER + 1), null);
});

test('A/V epochs reject pre-reset deliveries and wrap safely', () => {
  assert.equal(nextAvSyncEpoch(0), 1);
  assert.equal(nextAvSyncEpoch(Number.MAX_SAFE_INTEGER), 0);
  assert.equal(isCurrentAvSyncEpoch(1, 1), true);
  assert.equal(isCurrentAvSyncEpoch(0, 1), false);
  assert.equal(isCurrentAvSyncEpoch(1.5, 1), false);
  assert.throws(() => nextAvSyncEpoch(-1), /invalid A\/V sync epoch/);
});

test('audio output projection aligns worklet and performance clock snapshots', () => {
  const timeline = audioOutputTimelineFromStats({
    renderedMediaEndPtsMicros: 2_000_000,
    outputContextTimeEndSeconds: 4,
  });
  assert.deepEqual(timeline, {
    mediaEndPtsMicros: 2_000_000,
    outputContextTimeEndSeconds: 4,
  });
  const projected = projectAudioMediaPtsMicros(
    timeline,
    { contextTime: 3.9, performanceTime: 1000 },
    1025,
  );
  assert.ok(Math.abs(projected - 1_925_000) < 0.001);
});

test('projection declines to guess without an authoritative output timestamp', () => {
  const timeline = {
    mediaEndPtsMicros: 2_000_000,
    outputContextTimeEndSeconds: 4,
  };
  assert.equal(projectAudioMediaPtsMicros(timeline, null, 1000), null);
  assert.equal(
    projectAudioMediaPtsMicros(timeline, { contextTime: 0, performanceTime: 0 }, 1000),
    null,
  );
  assert.equal(
    projectAudioMediaPtsMicros(null, { contextTime: 4, performanceTime: 1000 }, 1000),
    null,
  );
  assert.equal(audioOutputTimelineFromStats({ renderedMediaEndPtsMicros: null }), null);
});

test('signed skew explicitly reports audio ahead and audio behind', () => {
  assert.equal(audioMinusVideoSkewMs(1_925_000, 1_900_000), 25);
  assert.equal(audioMinusVideoSkewMs(1_875_000, 1_900_000), -25);
  assert.equal(formatSignedMilliseconds(25), '+25.0');
  assert.equal(formatSignedMilliseconds(-25), '−25.0');
  assert.equal(formatSignedMilliseconds(Number.NaN), null);
});
