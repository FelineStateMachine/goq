import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  DECODER_RECOVERY_REASONS,
  DecoderRecoveryState,
} from './decoder-recovery.mjs';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

test('connect wiring uses the recovery state instead of removed legacy state', () => {
  assert.doesNotMatch(main, /waitingForDecoderKeyframe/);
  assert.match(main, /decoderRecovery\.reset\(\);/);
});

test('decoder callbacks cannot mutate a replacement decoder session', () => {
  assert.match(
    main,
    /output: \(frame\) => \{\s*if \(videoDecoder !== decoder\) \{\s*frame\.close\(\);\s*return;/,
  );
  assert.match(main, /error: \(e\) => \{\s*if \(videoDecoder !== decoder\) return;/);
});

test('drops deltas until a recovery keyframe is successfully enqueued', () => {
  const recovery = new DecoderRecoveryState();

  assert.equal(recovery.recovering, true);
  assert.equal(recovery.shouldDropFrame({ keyframe: false }), true);
  assert.equal(recovery.shouldDropFrame({ keyframe: true }), false);
  assert.equal(recovery.confirmKeyframeEnqueued(false), false);
  assert.equal(recovery.recovering, true);
  assert.equal(recovery.shouldDropFrame({ keyframe: false }), true);
  assert.equal(recovery.confirmKeyframeEnqueued(true), true);
  assert.equal(recovery.recovering, false);
  assert.equal(recovery.shouldDropFrame({ keyframe: false }), false);
});

test('coalesces keyframe requests throughout one recovery episode', () => {
  const requests = [];
  const recovery = new DecoderRecoveryState({
    initiallyRecovering: false,
    onKeyframeRequest: (reason) => requests.push(reason),
  });

  assert.deepEqual(recovery.enter(DECODER_RECOVERY_REASONS.TRANSPORT_GAP), {
    entered: true,
    requested: true,
  });
  assert.deepEqual(recovery.enter(DECODER_RECOVERY_REASONS.DELIVERY_TIMEOUT), {
    entered: false,
    requested: false,
  });
  assert.deepEqual(recovery.enter(DECODER_RECOVERY_REASONS.FRONTEND_BACKPRESSURE), {
    entered: false,
    requested: false,
  });
  assert.deepEqual(requests, [DECODER_RECOVERY_REASONS.TRANSPORT_GAP]);

  recovery.confirmKeyframeEnqueued(true);
  recovery.enter(DECODER_RECOVERY_REASONS.FRONTEND_BACKPRESSURE);
  assert.deepEqual(requests, [
    DECODER_RECOVERY_REASONS.TRANSPORT_GAP,
    DECODER_RECOVERY_REASONS.FRONTEND_BACKPRESSURE,
  ]);
});

test('every supported failure reason notifies synchronously when entering recovery', () => {
  for (const reason of Object.values(DECODER_RECOVERY_REASONS)) {
    const requests = [];
    const recovery = new DecoderRecoveryState({
      initiallyRecovering: false,
      onKeyframeRequest: (requestedReason) => requests.push(requestedReason),
    });

    const result = recovery.enter(reason);
    assert.deepEqual(result, { entered: true, requested: true });
    assert.deepEqual(requests, [reason]);
    assert.equal(recovery.recovering, true);
  }
});

test('an initially waiting decoder requests immediately on its first loss signal', () => {
  const requests = [];
  const recovery = new DecoderRecoveryState({
    onKeyframeRequest: (reason) => requests.push(reason),
  });

  assert.deepEqual(recovery.enter(DECODER_RECOVERY_REASONS.DISCONTINUITY), {
    entered: false,
    requested: true,
  });
  assert.equal(recovery.reason, DECODER_RECOVERY_REASONS.DISCONTINUITY);
  assert.deepEqual(requests, [DECODER_RECOVERY_REASONS.DISCONTINUITY]);
});

test('synchronous keyframe failure stays coalesced until an explicit restart', () => {
  const requests = [];
  const recovery = new DecoderRecoveryState({
    initiallyRecovering: false,
    onKeyframeRequest: (reason) => requests.push(reason),
  });

  recovery.enter(DECODER_RECOVERY_REASONS.DECODER_ERROR);
  recovery.confirmKeyframeEnqueued(false);
  recovery.enter(DECODER_RECOVERY_REASONS.DECODER_ERROR);
  assert.deepEqual(requests, [DECODER_RECOVERY_REASONS.DECODER_ERROR]);

  assert.deepEqual(recovery.restart(DECODER_RECOVERY_REASONS.DECODER_RESET), {
    entered: true,
    requested: true,
  });
  assert.deepEqual(requests, [
    DECODER_RECOVERY_REASONS.DECODER_ERROR,
    DECODER_RECOVERY_REASONS.DECODER_RESET,
  ]);
});

test('session reset clears request coalescing without notifying an inactive host', () => {
  const requests = [];
  const recovery = new DecoderRecoveryState({
    initiallyRecovering: false,
    onKeyframeRequest: (reason) => requests.push(reason),
  });

  recovery.enter(DECODER_RECOVERY_REASONS.TRANSPORT_GAP);
  recovery.reset();
  assert.equal(recovery.recovering, true);
  assert.equal(recovery.reason, null);
  assert.equal(recovery.requestIssued, false);
  assert.deepEqual(requests, [DECODER_RECOVERY_REASONS.TRANSPORT_GAP]);
});

test('rejects malformed reasons and frame decisions', () => {
  const recovery = new DecoderRecoveryState();
  assert.throws(() => recovery.enter('loss'), /unsupported decoder recovery reason/);
  assert.throws(() => recovery.restart(null), /unsupported decoder recovery reason/);
  assert.throws(() => recovery.shouldDropFrame({ keyframe: 1 }), /keyframe must be a boolean/);
  assert.throws(() => recovery.confirmKeyframeEnqueued('yes'), /succeeded must be a boolean/);
});
