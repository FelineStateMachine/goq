import test from 'node:test';
import assert from 'node:assert/strict';
import {
  BoundedCadenceWindow,
  BoundedLatencyWindow,
  BoundedValueWindow,
  CADENCE_HITCH_33_333_MS,
  LatestFramePresenter,
  MAX_STREAM_RATE_SAMPLES,
  RollingRateWindow,
} from './stream-metrics.mjs';

test('cadence reports bounded interval percentiles and hitch counts', () => {
  const cadence = new BoundedCadenceWindow(5000, 8);
  let nowMs = 100;
  cadence.record(nowMs);
  for (const intervalMs of [16, 20, 26, 34, 50]) {
    nowMs += intervalMs;
    cadence.record(nowMs);
  }

  assert.deepEqual(cadence.summary(nowMs), {
    p50: 26,
    p95: 50,
    p99: 50,
    max: 50,
    count: 5,
    over25Ms: 3,
    over33Ms: 2,
  });
});

test('cadence uses strict hitch thresholds', () => {
  const cadence = new BoundedCadenceWindow();
  cadence.record(0);
  cadence.record(25);
  cadence.record(25 + CADENCE_HITCH_33_333_MS);
  cadence.record(25 + (2 * CADENCE_HITCH_33_333_MS) + 0.001);

  const summary = cadence.summary(25 + (2 * CADENCE_HITCH_33_333_MS) + 0.001);
  assert.equal(summary.over25Ms, 2);
  assert.equal(summary.over33Ms, 1);
});

test('cadence expires by time, caps interval count, and resets its anchor', () => {
  const cadence = new BoundedCadenceWindow(50, 3);
  for (const nowMs of [0, 10, 30, 60, 100]) cadence.record(nowMs);
  assert.equal(cadence.samples.length, 2);
  assert.deepEqual(cadence.samples.map((sample) => sample.value), [30, 40]);

  assert.deepEqual(cadence.summary(151), {
    p50: null,
    p95: null,
    p99: null,
    max: null,
    count: 0,
    over25Ms: 0,
    over33Ms: 0,
  });

  cadence.reset();
  cadence.record(5);
  assert.equal(cadence.samples.length, 0);
  cadence.record(15);
  assert.deepEqual(cadence.samples.map((sample) => sample.value), [10]);
});

test('cadence treats the first frame after damage-driven idle as a fresh anchor', () => {
  const cadence = new BoundedCadenceWindow(1000, 8);
  cadence.record(0);
  cadence.record(16);
  cadence.record(32);
  assert.equal(cadence.summary(32).count, 2);

  cadence.record(5032);
  assert.deepEqual(cadence.summary(5032), {
    p50: null,
    p95: null,
    p99: null,
    max: null,
    count: 0,
    over25Ms: 0,
    over33Ms: 0,
  });

  cadence.record(5048);
  assert.deepEqual(cadence.summary(5048), {
    p50: 16,
    p95: 16,
    p99: 16,
    max: 16,
    count: 1,
    over25Ms: 0,
    over33Ms: 0,
  });
});

test('cadence retains an interval exactly on the rolling-window boundary', () => {
  const cadence = new BoundedCadenceWindow(1000, 2);
  cadence.record(0);
  cadence.record(1000);
  assert.equal(cadence.summary(1000).count, 1);

  cadence.record(2000.001);
  assert.equal(cadence.summary(2000.001).count, 0);
});

test('cadence rejects invalid bounds and non-monotonic clocks', () => {
  assert.throws(() => new BoundedCadenceWindow(0), /positive/);
  assert.throws(() => new BoundedCadenceWindow(1000, 0), /positive/);
  assert.throws(() => new BoundedCadenceWindow(1000, 1.5), /positive/);

  const cadence = new BoundedCadenceWindow();
  assert.throws(() => cadence.record(Number.NaN), /finite/);
  cadence.record(10);
  assert.throws(() => cadence.record(9), /monotonic/);
  assert.throws(() => cadence.summary(9), /monotonic/);
  assert.throws(() => cadence.summary(Number.POSITIVE_INFINITY), /finite/);
});

test('reports a recent 60 fps cadence instead of a lifetime average', () => {
  const rate = new RollingRateWindow();
  for (let frame = 0; frame <= 60; frame++) rate.record((frame * 1000) / 60);
  assert.ok(Math.abs(rate.rate(1000) - 60) < 0.001);

  for (let frame = 1; frame <= 30; frame++) rate.record(1000 + (frame * 1000) / 30);
  assert.ok(Math.abs(rate.rate(2000) - 30) < 0.001);
});

test('signed value percentiles retain lead/lag direction and absolute worst case', () => {
  const skew = new BoundedValueWindow(2000, 4);
  for (const [index, value] of [-40, 10, -20, 30, 50].entries()) skew.record(value, index * 100);
  assert.equal(skew.samples.length, 4);
  assert.deepEqual(
    skew.summary(400),
    { p50: 10, p95: 50, maxAbsolute: 50, count: 4 },
  );
  assert.deepEqual(
    skew.summary(3000),
    { p50: null, p95: null, maxAbsolute: null, count: 0 },
  );
});

test('prunes idle samples and never grows beyond its fixed bound', () => {
  const rate = new RollingRateWindow(1000, MAX_STREAM_RATE_SAMPLES);
  for (let frame = 0; frame < 1000; frame++) rate.record(frame);
  assert.ok(rate.samples.length <= MAX_STREAM_RATE_SAMPLES);
  assert.equal(rate.rate(5000), 0);
  assert.equal(rate.samples.length, 0);
});

test('rejects invalid and non-monotonic clocks', () => {
  assert.throws(() => new RollingRateWindow(0), /positive/);
  assert.throws(() => new RollingRateWindow(1000, 1), /at least two/);
  const rate = new RollingRateWindow();
  rate.record(10);
  assert.throws(() => rate.record(9), /monotonic/);
  assert.throws(() => rate.rate(Number.NaN), /finite/);
});

test('latency percentiles are bounded, deterministic, and expire', () => {
  const latency = new BoundedLatencyWindow(2000, 4);
  for (const [index, value] of [40, 10, 30, 20, 50].entries()) latency.record(value, index * 100);
  assert.equal(latency.samples.length, 4);
  assert.deepEqual(latency.summary(400), {
    p50: 20,
    p95: 50,
    p99: 50,
    max: 50,
    count: 4,
  });
  assert.deepEqual(latency.summary(3000), {
    p50: null,
    p95: null,
    p99: null,
    max: null,
    count: 0,
  });
});

test('latest-frame presenter bounds jitter buffering at two and drops the oldest frame', () => {
  const callbacks = [];
  const cancelled = [];
  const drawn = [];
  const presented = [];
  const dropped = [];
  let nextHandle = 1;
  const presenter = new LatestFramePresenter({
    requestFrame: (callback) => {
      callbacks.push(callback);
      return nextHandle++;
    },
    cancelFrame: (handle) => cancelled.push(handle),
    draw: (frame) => drawn.push(frame.id),
    onPresent: (metadata) => presented.push(metadata),
    onDrop: (metadata) => dropped.push(metadata),
  });
  const frame = (id) => ({ id, closed: 0, close() { this.closed++; } });
  const first = frame(1);
  const second = frame(2);
  const third = frame(3);

  presenter.enqueue(first, 'first');
  presenter.enqueue(second, 'second');
  presenter.enqueue(third, 'third');
  assert.equal(callbacks.length, 1);
  assert.equal(presenter.depth, 2);
  assert.equal(first.closed, 1);
  assert.deepEqual(dropped, ['first']);

  callbacks.shift()(123);
  assert.deepEqual(drawn, [2]);
  assert.deepEqual(presented, ['second']);
  assert.equal(second.closed, 1);
  assert.equal(callbacks.length, 1);
  assert.equal(presenter.depth, 1);

  callbacks.shift()(140);
  assert.deepEqual(drawn, [2, 3]);
  assert.deepEqual(presented, ['second', 'third']);
  assert.equal(third.closed, 1);
  assert.equal(presenter.depth, 0);

  const fourth = frame(4);
  const fifth = frame(5);
  presenter.enqueue(fourth);
  presenter.enqueue(fifth);
  presenter.clear();
  assert.deepEqual(cancelled, [3]);
  assert.equal(fourth.closed, 1);
  assert.equal(fifth.closed, 1);
  assert.equal(presenter.depth, 0);
});

test('two-frame presenter smooths pair-bursty 60 fps input at display cadence', () => {
  let scheduledCallback = null;
  let nextHandle = 1;
  const presentedAt = [];
  const frames = [];
  let dropped = 0;
  const presenter = new LatestFramePresenter({
    requestFrame: (callback) => {
      assert.equal(scheduledCallback, null, 'only one animation frame may be scheduled');
      scheduledCallback = callback;
      return nextHandle++;
    },
    cancelFrame: () => { scheduledCallback = null; },
    draw: () => {},
    onPresent: (_metadata, nowMs) => presentedAt.push(nowMs),
    onDrop: () => dropped++,
  });
  const frame = (id) => {
    const value = { id, closed: 0, close() { this.closed++; } };
    frames.push(value);
    return value;
  };

  const displayIntervalMs = 1000 / 60;
  for (let displayTick = 0; displayTick < 60; displayTick++) {
    // Model decoder jitter as two frames delivered together every other
    // refresh. The average decoder-output rate is still exactly 60 fps.
    if (displayTick % 2 === 0) {
      presenter.enqueue(frame(displayTick), displayTick);
      presenter.enqueue(frame(displayTick + 1), displayTick + 1);
      assert.equal(presenter.depth, 2);
    }
    const callback = scheduledCallback;
    scheduledCallback = null;
    assert.ok(callback, `missing presentation callback at tick ${displayTick}`);
    callback(displayTick * displayIntervalMs);
    assert.ok(presenter.depth <= 2);
  }

  assert.equal(presentedAt.length, 60);
  assert.equal(dropped, 0);
  assert.equal(presenter.depth, 0);
  assert.equal(scheduledCallback, null);
  assert.ok(presentedAt.slice(1).every((nowMs, index) => (
    Math.abs((nowMs - presentedAt[index]) - displayIntervalMs) < 0.001
  )));
  assert.ok(frames.every((value) => value.closed === 1));
});
