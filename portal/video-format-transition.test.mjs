import assert from 'node:assert/strict';
import test from 'node:test';

import { VideoFormatTransitionGuard } from './video-format-transition.mjs';

const frame = (overrides = {}) => ({
  sequence: 1,
  codec: 'h264',
  width: 1280,
  height: 800,
  keyframe: true,
  codecConfig: true,
  discontinuity: false,
  ...overrides,
});

test('format transitions require configured keyframes and advance epochs monotonically', () => {
  const guard = new VideoFormatTransitionGuard();
  assert.equal(guard.plan(frame({ keyframe: false, codecConfig: false })).action, 'recover');

  const initial = guard.plan(frame());
  assert.equal(initial.reconfigure, true);
  assert.equal(initial.epoch, 1);
  guard.commit(initial);

  const ordinary = guard.plan(frame({ sequence: 2, keyframe: false, codecConfig: false }));
  assert.equal(ordinary.reconfigure, false);
  assert.equal(ordinary.epoch, 1);
  guard.commit(ordinary);

  const resized = guard.plan(frame({ sequence: 3, width: 640, height: 400 }));
  assert.equal(resized.reconfigure, true);
  assert.equal(resized.epoch, 2);
  guard.commit(resized);
  assert.deepEqual(guard.format, { codec: 'h264', width: 640, height: 400 });
});

test('stale outputs and unconfigured format changes cannot commit', () => {
  const guard = new VideoFormatTransitionGuard();
  guard.commit(guard.plan(frame()));
  assert.equal(guard.plan(frame({ sequence: 1 })).action, 'drop-stale');
  assert.equal(guard.plan(frame({ sequence: 2, width: 640, height: 400,
    keyframe: false, codecConfig: false })).action, 'recover');

  const first = guard.plan(frame({ sequence: 2, keyframe: false, codecConfig: false }));
  const competing = guard.plan(frame({ sequence: 3, keyframe: false, codecConfig: false }));
  guard.commit(first);
  assert.throws(() => guard.commit(competing), /stale video format transition/);
});

test('a discontinuity starts a new configured epoch even at the same dimensions', () => {
  const guard = new VideoFormatTransitionGuard();
  guard.commit(guard.plan(frame()));
  const boundary = guard.plan(frame({ sequence: 2, discontinuity: true }));
  assert.equal(boundary.reconfigure, true);
  assert.equal(boundary.epoch, 2);
});
