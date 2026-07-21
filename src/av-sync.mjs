export function exactMediaTimestampMicros(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

export function nextAvSyncEpoch(current) {
  if (!Number.isSafeInteger(current) || current < 0) {
    throw new TypeError('invalid A/V sync epoch');
  }
  return current === Number.MAX_SAFE_INTEGER ? 0 : current + 1;
}

export function isCurrentAvSyncEpoch(value, current) {
  return Number.isSafeInteger(value) && value >= 0 && value === current;
}

export function audioOutputTimelineFromStats(value) {
  const mediaEndPtsMicros = value?.renderedMediaEndPtsMicros;
  const outputContextTimeEndSeconds = value?.outputContextTimeEndSeconds;
  if (
    !Number.isFinite(mediaEndPtsMicros)
    || mediaEndPtsMicros < 0
    || !Number.isFinite(outputContextTimeEndSeconds)
    || outputContextTimeEndSeconds < 0
  ) return null;
  return { mediaEndPtsMicros, outputContextTimeEndSeconds };
}

export function projectAudioMediaPtsMicros(
  timeline,
  outputTimestamp,
  presentationTimeMs,
) {
  if (
    !timeline
    || !Number.isFinite(timeline.mediaEndPtsMicros)
    || timeline.mediaEndPtsMicros < 0
    || !Number.isFinite(timeline.outputContextTimeEndSeconds)
    || timeline.outputContextTimeEndSeconds < 0
    || !Number.isFinite(outputTimestamp?.contextTime)
    || outputTimestamp.contextTime < 0
    || !Number.isFinite(outputTimestamp?.performanceTime)
    || outputTimestamp.performanceTime < 0
    || (outputTimestamp.contextTime === 0 && outputTimestamp.performanceTime === 0)
    || !Number.isFinite(presentationTimeMs)
  ) return null;

  const contextTimeAtPresentation = outputTimestamp.contextTime
    + ((presentationTimeMs - outputTimestamp.performanceTime) / 1000);
  const projected = timeline.mediaEndPtsMicros
    + ((contextTimeAtPresentation - timeline.outputContextTimeEndSeconds) * 1_000_000);
  return Number.isFinite(projected) && projected >= 0 ? projected : null;
}

// Positive means audio is ahead of video; negative means audio is behind.
export function audioMinusVideoSkewMs(audioPtsMicros, videoPtsMicros) {
  if (
    !Number.isFinite(audioPtsMicros)
    || audioPtsMicros < 0
    || !Number.isFinite(videoPtsMicros)
    || videoPtsMicros < 0
  ) return null;
  return (audioPtsMicros - videoPtsMicros) / 1000;
}

export function formatSignedMilliseconds(value) {
  if (!Number.isFinite(value)) return null;
  const sign = value >= 0 ? '+' : '−';
  return `${sign}${Math.abs(value).toFixed(1)}`;
}
