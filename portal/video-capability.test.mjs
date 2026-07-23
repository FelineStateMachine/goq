import assert from 'node:assert/strict';
import test from 'node:test';

import {
  detectAndPublishVideoDeliveryMode,
  probeH264WebCodecsSupport,
} from './video-capability.mjs';

test('requires VideoDecoder and its asynchronous support probe', async () => {
  assert.equal(await probeH264WebCodecsSupport({ VideoDecoder: undefined }), false);
  assert.equal(await probeH264WebCodecsSupport({ VideoDecoder: class {} }), false);
});

test('reports only explicit support for Sigil current H.264 format', async () => {
  let probedConfig = null;
  class SupportedDecoder {
    static async isConfigSupported(config) {
      probedConfig = config;
      return { supported: true, config };
    }
  }

  assert.equal(await probeH264WebCodecsSupport({ VideoDecoder: SupportedDecoder }), true);
  assert.deepEqual(probedConfig, {
    codec: 'avc1.64001f',
    optimizeForLatency: true,
  });

  class UnsupportedDecoder {
    static async isConfigSupported(config) {
      return { supported: false, config };
    }
  }
  assert.equal(await probeH264WebCodecsSupport({ VideoDecoder: UnsupportedDecoder }), false);
});

test('fails closed when the webview rejects its H.264 support probe', async () => {
  const warnings = [];
  class RejectingDecoder {
    static async isConfigSupported() {
      throw new Error('codec support unavailable');
    }
  }

  assert.equal(await probeH264WebCodecsSupport({
    VideoDecoder: RejectingDecoder,
    logger: { warn: (...args) => warnings.push(args) },
  }), false);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0][0], /capability probe failed/);
});

test('awaits the probe before publishing the exact raw delivery mode', async () => {
  const calls = [];
  let finishProbe;
  class DelayedDecoder {
    static isConfigSupported() {
      calls.push('probe');
      return new Promise((resolve) => { finishProbe = resolve; });
    }
  }
  const pending = detectAndPublishVideoDeliveryMode({
    VideoDecoder: DelayedDecoder,
    invokeCommand: async (...args) => { calls.push(args); },
  });

  await Promise.resolve();
  assert.deepEqual(calls, ['probe']);
  finishProbe({ supported: true });
  assert.equal(await pending, true);
  assert.deepEqual(calls, [
    'probe',
    ['set_webcodecs_available', { available: true }],
  ]);
});

test('keeps frontend on JPEG when capability publication fails', async () => {
  const errors = [];
  class SupportedDecoder {
    static async isConfigSupported() { return { supported: true }; }
  }

  assert.equal(await detectAndPublishVideoDeliveryMode({
    VideoDecoder: SupportedDecoder,
    invokeCommand: async () => { throw new Error('native command failed'); },
    logger: { warn() {}, error: (...args) => errors.push(args) },
  }), false);
  assert.equal(errors.length, 1);
  assert.match(errors[0][0], /using JPEG fallback/);
});

test('rejects a missing native publication boundary', async () => {
  await assert.rejects(detectAndPublishVideoDeliveryMode(), TypeError);
});
