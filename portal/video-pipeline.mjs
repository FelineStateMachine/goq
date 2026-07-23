import {
  parseAnnexBNals,
  nalsToLengthPrefixed,
  buildAvcDescription,
  avcCodecStr,
  h264NalType,
} from './codecs.js';
import { exactMediaTimestampMicros } from './av-sync.mjs';
import {
  BoundedCadenceWindow,
  BoundedLatencyWindow,
  BoundedValueWindow,
  LatestFramePresenter,
  RollingRateWindow,
} from './stream-metrics.mjs';
import { VideoFormatTransitionGuard } from './video-format-transition.mjs';
import { buildVideoDecoderConfig } from './video-decoder-config.mjs';
import {
  DECODER_RECOVERY_REASONS,
  DecoderRecoveryState,
} from './decoder-recovery.mjs';

export const MAX_DECODE_QUEUE_SIZE = 2;
const MAX_DECODE_TIMINGS = 8;
const PRESENTER_QUEUE_CAPACITY = 2;

export function createVideoPipelineSession({
  canvas,
  context,
  requestKeyframe = () => {},
  resetAudioSync = () => {},
  sampleAudioSkew = () => null,
  onFormatChanged = () => {},
  now = () => performance.now(),
  createDecoder = (callbacks) => new VideoDecoder(callbacks),
  createEncodedChunk = (init) => new EncodedVideoChunk(init),
  requestFrame = (callback) => requestAnimationFrame(callback),
  cancelFrame = (handle) => cancelAnimationFrame(handle),
  setTimer = (callback, delayMs) => setTimeout(callback, delayMs),
  cancelTimer = (handle) => clearTimeout(handle),
}) {
  let videoDecoder = null;
  let decoderConfigured = false;
  let activeCodec = 'h264';
  let frameWidth = 0;
  let frameHeight = 0;
  let activeVideoFormatEpoch = 0;
  let droppedFrames = 0;
  let receivedFrames = 0;
  let decoderInputFrames = 0;
  let decoderOutputFrames = 0;
  let presentedFrames = 0;
  let presentationDroppedFrames = 0;
  let lastVideoDrawCompletedAtMs = null;

  const frontendDeliveryRate = new RollingRateWindow();
  const decoderInputRate = new RollingRateWindow();
  const decoderOutputRate = new RollingRateWindow();
  const presentationRate = new RollingRateWindow();
  const frontendDeliveryCadence = new BoundedCadenceWindow();
  const decoderInputCadence = new BoundedCadenceWindow();
  const decoderOutputCadence = new BoundedCadenceWindow();
  const presentationCadence = new BoundedCadenceWindow();
  const decodeLatency = new BoundedLatencyWindow();
  const clientPresentationLatency = new BoundedLatencyWindow();
  const drawLatency = new BoundedLatencyWindow();
  const avSkew = new BoundedValueWindow();
  const decodeTimings = new Map();
  const decoderRecovery = new DecoderRecoveryState({
    onKeyframeRequest: requestKeyframe,
  });
  const videoFormatTransition = new VideoFormatTransitionGuard();

  const framePresenter = new LatestFramePresenter({
    requestFrame,
    cancelFrame,
    setTimer,
    cancelTimer,
    now,
    fallbackDelayMs: 25,
    draw: (frame) => {
      const startedAt = now();
      context.drawImage(frame, 0, 0, canvas.width, canvas.height);
      const completedAt = now();
      drawLatency.record(completedAt - startedAt, completedAt);
      lastVideoDrawCompletedAtMs = completedAt;
    },
    onPresent: (metadata, animationFrameTime) => {
      const presentedAt = lastVideoDrawCompletedAtMs ?? animationFrameTime;
      lastVideoDrawCompletedAtMs = null;
      presentedFrames++;
      presentationRate.record(presentedAt);
      presentationCadence.record(presentedAt);
      if (metadata && Number.isFinite(metadata.receivedAt)) {
        clientPresentationLatency.record(presentedAt - metadata.receivedAt, presentedAt);
      }
      if (metadata) {
        const skewMs = sampleAudioSkew(metadata.mediaPtsMicros, presentedAt);
        if (skewMs !== null && skewMs !== undefined) avSkew.record(skewMs, presentedAt);
      }
    },
    onDrop: () => { presentationDroppedFrames++; },
  });

  function resetAvSync(...args) {
    avSkew.reset();
    resetAudioSync(...args);
  }

  function teardown() {
    resetAvSync();
    framePresenter.clear();
    decodeTimings.clear();
    if (videoDecoder) {
      try { videoDecoder.close(); } catch (_) {}
    }
    videoDecoder = null;
    decoderConfigured = false;
  }

  function enterDecoderRecovery(reason, { restart = false, requestKeyframe: shouldRequest = true } = {}) {
    const result = restart
      ? decoderRecovery.restart(reason)
      : decoderRecovery.enter(reason, { requestKeyframe: shouldRequest });
    // Closing WebCodecs and clearing the latest-frame presenter is part of the
    // recovery boundary. Do it immediately rather than allowing already queued
    // frames from a poisoned GOP to continue reaching the canvas.
    teardown();
    return result;
  }

  function finishVideoChunkEnqueue(keyframe, succeeded) {
    if (succeeded) {
      if (keyframe) decoderRecovery.confirmKeyframeEnqueued(true);
      return;
    }
    // A synchronous decode rejection is a decoder-error episode if playback
    // was previously healthy. If recovery is already active, enter() coalesces
    // with its existing request and leaves the keyframe gate closed.
    enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
  }

  function reset() {
    teardown();
    decoderRecovery.reset();
    videoFormatTransition.reset();
    activeVideoFormatEpoch = 0;
    droppedFrames = 0;
    receivedFrames = 0;
    decoderInputFrames = 0;
    decoderOutputFrames = 0;
    presentedFrames = 0;
    presentationDroppedFrames = 0;
    frontendDeliveryRate.reset();
    decoderInputRate.reset();
    decoderOutputRate.reset();
    presentationRate.reset();
    frontendDeliveryCadence.reset();
    decoderInputCadence.reset();
    decoderOutputCadence.reset();
    presentationCadence.reset();
    decodeLatency.reset();
    clientPresentationLatency.reset();
    drawLatency.reset();
  }

  function enqueueVideoChunk(chunk, receivedAt, codecLabel, mediaPtsMicros = null) {
    if (!videoDecoder) return false;
    const enqueuedAt = now();
    while (decodeTimings.size >= MAX_DECODE_TIMINGS) {
      decodeTimings.delete(decodeTimings.keys().next().value);
    }
    decodeTimings.set(chunk.timestamp, { receivedAt, enqueuedAt, mediaPtsMicros });
    try {
      videoDecoder.decode(chunk);
      decoderInputFrames++;
      decoderInputRate.record(enqueuedAt);
      decoderInputCadence.record(enqueuedAt);
      return true;
    } catch (error) {
      decodeTimings.delete(chunk.timestamp);
      droppedFrames++;
      console.warn(`${codecLabel} decode failed:`, error);
      return false;
    }
  }

  function initWebCodecsDecoder(width, height, desc, codecStr) {
    teardown();
    console.log('WebCodecs configure:', codecStr, 'w:', width, 'h:', height, 'desc:', desc.byteLength, 'bytes');
    const decoder = createDecoder({
      output: (frame) => {
        if (videoDecoder !== decoder) {
          frame.close();
          return;
        }
        const outputAt = now();
        decoderOutputFrames++;
        decoderOutputRate.record(outputAt);
        decoderOutputCadence.record(outputAt);
        const timing = decodeTimings.get(frame.timestamp) ?? null;
        decodeTimings.delete(frame.timestamp);
        if (timing) decodeLatency.record(outputAt - timing.enqueuedAt, outputAt);
        framePresenter.enqueue(frame, timing);
      },
      error: (error) => {
        if (videoDecoder !== decoder) return;
        console.error('VideoDecoder error:', error);
        enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
      },
    });
    videoDecoder = decoder;
    try {
      decoder.configure(buildVideoDecoderConfig({
        codec: codecStr,
        width,
        height,
        description: desc,
      }));
      decoderConfigured = true;
      return true;
    } catch (error) {
      console.warn('WebCodecs configure failed:', error);
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_ERROR);
      return false;
    }
  }

  function commitVideoFrame(plan, keyframe, enqueued) {
    if (!enqueued) {
      finishVideoChunkEnqueue(keyframe, false);
      return false;
    }
    let committed;
    try {
      committed = videoFormatTransition.commit(plan);
    } catch (error) {
      console.warn('discarding stale video format transaction:', error);
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
      return false;
    }
    if (plan.reconfigure) {
      activeCodec = committed.format.codec;
      frameWidth = committed.format.width;
      frameHeight = committed.format.height;
      activeVideoFormatEpoch = committed.epoch;
      if (canvas.width !== frameWidth) canvas.width = frameWidth;
      if (canvas.height !== frameHeight) canvas.height = frameHeight;
      onFormatChanged({
        codec: activeCodec,
        width: frameWidth,
        height: frameHeight,
        epoch: activeVideoFormatEpoch,
      });
    }
    finishVideoChunkEnqueue(keyframe, true);
    return true;
  }

  function processFramePayload(payload, binaryData) {
    const receivedAt = now();
    frontendDeliveryRate.record(receivedAt);
    frontendDeliveryCadence.record(receivedAt);
    const {
      width,
      height,
      codec = 'h264',
      keyframe,
      codecConfig = keyframe,
      sequence = null,
      pts_micros: ptsMicros,
      discontinuity = false,
    } = payload;
    if (discontinuity) {
      resetAvSync();
    }
    receivedFrames++;

    // Binary channels provide an exact view over the IPC ArrayBuffer.
    const raw = binaryData;
    const decoderBacklogged = videoDecoder?.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE;
    if (decoderBacklogged) {
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.FRONTEND_BACKPRESSURE);
    }
    const decoderBoundary = discontinuity || !decoderConfigured;
    let formatPlan;
    try {
      formatPlan = videoFormatTransition.plan({
        sequence,
        codec,
        width,
        height,
        keyframe,
        codecConfig,
        discontinuity: decoderBoundary,
      });
    } catch (error) {
      console.warn('invalid video format transition:', error);
      droppedFrames++;
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
      return;
    }
    if (formatPlan.action === 'drop-stale') {
      droppedFrames++;
      return;
    }
    if (formatPlan.action === 'recover') {
      droppedFrames++;
      enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
      return;
    }
    if (formatPlan.reconfigure) {
      // A delivered discontinuity keyframe is the recovery boundary we were
      // waiting for, not evidence that another keyframe is missing.
      if (discontinuity) {
        enterDecoderRecovery(DECODER_RECOVERY_REASONS.DISCONTINUITY, {
          requestKeyframe: !keyframe,
        });
      } else {
        enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET, {
          requestKeyframe: false,
        });
      }
    }
    if (decoderRecovery.shouldDropFrame({ keyframe })) {
      if (!decoderRecovery.requestIssued) {
        enterDecoderRecovery(DECODER_RECOVERY_REASONS.DECODER_RESET);
      }
      droppedFrames++;
      return;
    }
    const mediaPtsMicros = exactMediaTimestampMicros(ptsMicros);
    const chunkTimestamp = mediaPtsMicros !== null
      ? mediaPtsMicros
      : now() * 1000;

    const nals = parseAnnexBNals(raw);
    if (keyframe) {
      let sps = null, pps = null;
      for (const nal of nals) {
        const type = h264NalType(nal);
        if (type === 7) sps = nal;
        else if (type === 8) pps = nal;
      }
      if (sps && pps && formatPlan.reconfigure) {
        initWebCodecsDecoder(width, height, buildAvcDescription(sps, pps), avcCodecStr(sps));
      }
    }
    if (!decoderConfigured || !videoDecoder) { droppedFrames++; return; }
    const sliceNals = nals.filter(nal => {
      const type = h264NalType(nal);
      return type !== 7 && type !== 8 && type !== 9;
    });
    if (sliceNals.length > 0) {
      const chunk = createEncodedChunk({
        type: keyframe ? 'key' : 'delta',
        timestamp: chunkTimestamp,
        data: nalsToLengthPrefixed(sliceNals),
      });
      const enqueued = enqueueVideoChunk(chunk, receivedAt, 'h264', mediaPtsMicros);
      commitVideoFrame(formatPlan, keyframe, enqueued);
    }
  }

  function snapshot(at = now()) {
    return {
      activeCodec,
      frameWidth,
      frameHeight,
      activeVideoFormatEpoch,
      receivedFrames,
      decoderInputFrames,
      decoderOutputFrames,
      presentedFrames,
      droppedFrames,
      presentationDroppedFrames,
      frontendDeliveryFps: frontendDeliveryRate.rate(at),
      decodeFps: decoderInputRate.rate(at),
      decoderOutputFps: decoderOutputRate.rate(at),
      presentFps: presentationRate.rate(at),
      decodePercentiles: decodeLatency.summary(at),
      presentPercentiles: clientPresentationLatency.summary(at),
      drawPercentiles: drawLatency.summary(at),
      deliveryCadence: frontendDeliveryCadence.summary(at),
      decoderInputIntervals: decoderInputCadence.summary(at),
      decoderOutputIntervals: decoderOutputCadence.summary(at),
      presentationIntervals: presentationCadence.summary(at),
      avSkewPercentiles: avSkew.summary(at),
      decoderQueueDepth: videoDecoder?.decodeQueueSize ?? 0,
      decoderQueueCapacity: MAX_DECODE_QUEUE_SIZE,
      presenterQueueDepth: framePresenter.depth,
      presenterQueueCapacity: PRESENTER_QUEUE_CAPACITY,
      recovering: decoderRecovery.recovering,
    };
  }

  return Object.freeze({
    processFramePayload,
    resetAvSync,
    reset,
    teardown,
    snapshot,
    get format() {
      return {
        codec: activeCodec,
        width: frameWidth,
        height: frameHeight,
        epoch: activeVideoFormatEpoch,
      };
    },
  });
}
