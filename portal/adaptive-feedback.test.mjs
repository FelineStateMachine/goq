import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ADAPTIVE_COUNTER_DELTA_MAX,
  AdaptiveFeedbackPublisher,
  formatAdaptiveDecision,
  normalizeAdaptiveDecisionEnvelope,
  normalizeAdaptiveFeedbackSnapshot,
} from './adaptive-feedback.mjs';

function snapshot(overrides = {}) {
  return {
    frontendQueueDepth: 1,
    frontendQueueCapacity: 4,
    decoderQueueDepth: 1,
    decoderQueueCapacity: 2,
    presenterQueueDepth: 1,
    presenterQueueCapacity: 2,
    transportDroppedTotal: 3,
    frontendDroppedTotal: 4,
    decoderDroppedTotal: 5,
    presenterDroppedTotal: 6,
    transportDeliveryP95Ms: 18.5,
    decodeLatencyP95Ms: 4.5,
    presentationLatencyP95Ms: 24.5,
    resyncActive: false,
    lastSequence: 99,
    ...overrides,
  };
}

function flushPromises() {
  return new Promise((resolve) => setImmediate(resolve));
}

test('feedback snapshots reject unsafe values and clamp wire bounds', () => {
  assert.throws(() => normalizeAdaptiveFeedbackSnapshot(snapshot({ decoderQueueDepth: NaN })), /safe integer/);
  assert.throws(() => normalizeAdaptiveFeedbackSnapshot(snapshot({ resyncActive: 1 })), /boolean/);
  const value = normalizeAdaptiveFeedbackSnapshot(snapshot({
    decoderQueueDepth: 99,
    transportDeliveryP95Ms: 1e9,
  }));
  assert.equal(value.decode_queue_depth, 2);
  assert.equal(value.transport_delivery_p95_ms, 60_000);
});

test('publisher is generation-scoped, one hertz, single-flight, and reports deltas', async () => {
  let now = 0;
  const calls = [];
  const completions = [];
  const publisher = new AdaptiveFeedbackPublisher({
    now: () => now,
    invokeCommand: (command, args) => {
      calls.push({ command, args });
      return new Promise((resolve) => completions.push(resolve));
    },
  });
  publisher.start(7, true);
  assert.equal(publisher.publish(snapshot()), false);
  now = 1000;
  assert.equal(publisher.publish(snapshot()), true);
  assert.equal(publisher.publish(snapshot({ transportDroppedTotal: 10 })), false);
  now = 2000;
  assert.equal(publisher.publish(snapshot({ transportDroppedTotal: 11 })), false);
  assert.equal(calls.length, 1);
  completions.shift()(true);
  await flushPromises();
  assert.equal(publisher.publish(snapshot({ transportDroppedTotal: 12 })), true);
  assert.equal(calls[1].args.generation, 7);
  assert.equal(calls[1].args.report.transport_dropped_delta, 9);
  assert.equal(calls[1].args.report.frontend_dropped_delta, 0);
});

test('counter deltas saturate and counter resets do not underflow', async () => {
  let now = 0;
  const calls = [];
  const publisher = new AdaptiveFeedbackPublisher({
    now: () => now,
    invokeCommand: async (_command, args) => {
      calls.push(args.report);
      return true;
    },
  });
  publisher.start(1, true);
  publisher.publish(snapshot({
    transportDroppedTotal: 0,
    frontendDroppedTotal: 0,
    decoderDroppedTotal: 0,
    presenterDroppedTotal: 0,
  }));
  now = 1000;
  publisher.publish(snapshot({ transportDroppedTotal: ADAPTIVE_COUNTER_DELTA_MAX + 10 }));
  await flushPromises();
  now = 2000;
  publisher.publish(snapshot({ transportDroppedTotal: 1 }));
  assert.equal(calls[0].transport_dropped_delta, ADAPTIVE_COUNTER_DELTA_MAX);
  assert.equal(calls[1].transport_dropped_delta, 0);
});

test('stale completion cannot unlock or alter a newer generation', async () => {
  let resolveOld;
  let now = 0;
  const calls = [];
  const publisher = new AdaptiveFeedbackPublisher({
    now: () => now,
    invokeCommand: (_command, args) => {
      calls.push(args.generation);
      if (args.generation === 1) return new Promise((resolve) => { resolveOld = resolve; });
      return Promise.resolve(true);
    },
  });
  publisher.start(1, true);
  publisher.publish(snapshot());
  now = 1000;
  publisher.publish(snapshot());
  publisher.start(2, true);
  publisher.publish(snapshot());
  now = 2000;
  publisher.publish(snapshot());
  resolveOld();
  await flushPromises();
  assert.deepEqual(calls, [1, 2]);
});

test('false and rejected sends retain cumulative loss until a report is accepted', async () => {
  let now = 0;
  const reports = [];
  const outcomes = [
    () => false,
    () => Promise.reject(new Error('closed')),
    () => true,
    () => true,
  ];
  const originalWarn = console.warn;
  console.warn = () => {};
  try {
    const publisher = new AdaptiveFeedbackPublisher({
      now: () => now,
      invokeCommand: (_command, args) => {
        reports.push(args.report);
        return outcomes.shift()();
      },
    });
    publisher.start(1, true);
    publisher.publish(snapshot({
      transportDroppedTotal: 0,
      frontendDroppedTotal: 0,
      decoderDroppedTotal: 0,
      presenterDroppedTotal: 0,
    }));
    now = 1000;
    publisher.publish(snapshot({ transportDroppedTotal: 3 }));
    await flushPromises();
    now = 2000;
    publisher.publish(snapshot({ transportDroppedTotal: 7 }));
    await flushPromises();
    now = 3000;
    publisher.publish(snapshot({ transportDroppedTotal: 9 }));
    await flushPromises();
    now = 4000;
    publisher.publish(snapshot({ transportDroppedTotal: 10 }));
    assert.deepEqual(
      reports.map((report) => report.transport_dropped_delta),
      [3, 7, 9, 1],
    );
    assert.deepEqual(
      reports.map((report) => report.interval_ms),
      [1000, 2000, 3000, 1000],
    );
  } finally {
    console.warn = originalWarn;
  }
});

test('a synchronous invoke throw clears single-flight state and permits retry', async () => {
  let attempts = 0;
  let now = 0;
  const originalWarn = console.warn;
  console.warn = () => {};
  try {
    const publisher = new AdaptiveFeedbackPublisher({
      now: () => now,
      invokeCommand: () => {
        attempts++;
        if (attempts === 1) throw new Error('synchronous bridge failure');
        return Promise.resolve(true);
      },
    });
    publisher.start(1, true);
    publisher.publish(snapshot());
    now = 1000;
    assert.equal(publisher.publish(snapshot()), false);
    assert.equal(publisher.inFlight, false);
    assert.equal(publisher.publish(snapshot()), true);
    await flushPromises();
    assert.equal(publisher.inFlight, false);
    assert.equal(attempts, 2);
  } finally {
    console.warn = originalWarn;
  }
});

test('publisher measures and bounds the actual baseline interval', async () => {
  let now = 0;
  const reports = [];
  const publisher = new AdaptiveFeedbackPublisher({
    now: () => now,
    invokeCommand: async (_command, args) => {
      reports.push(args.report);
      return true;
    },
  });
  publisher.start(1, true);
  publisher.publish(snapshot());
  now = 249;
  assert.equal(publisher.publish(snapshot()), false);
  now = 1_275;
  assert.equal(publisher.publish(snapshot()), true);
  await flushPromises();
  now = 8_000;
  assert.equal(publisher.publish(snapshot({ transportDroppedTotal: 7 })), true);
  assert.deepEqual(reports.map((report) => report.interval_ms), [1_275, 5_000]);
});

test('decision diagnostics reject stale generations and say advisory not applied', () => {
  assert.equal(normalizeAdaptiveDecisionEnvelope({ generation: 8, decision: {} }, 7), null);
  const decision = normalizeAdaptiveDecisionEnvelope({
    generation: 7,
    decision: {
      decision_id: 4,
      report_id: 3,
      target_kbps: 8000,
      floor_kbps: 4000,
      ceiling_kbps: 20000,
      state: 'hold',
      reasons: ['clean-recovery'],
      applied: false,
    },
  }, 7);
  assert.match(formatAdaptiveDecision(decision, true), /advisory only \(not applied\)/);
  assert.equal(formatAdaptiveDecision(null, false), 'unavailable');
});
