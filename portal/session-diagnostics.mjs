const MODES = new Set(['legacy_exclusive_v1', 'moq_multi_viewer_v2']);
const HANDLE_PATTERN = /^[A-Za-z0-9-]{1,32}$/;

function unsigned(value, label, { positive = false } = {}) {
  if (!Number.isSafeInteger(value) || value < (positive ? 1 : 0)) {
    throw new TypeError(`${label} must be an exact ${positive ? 'positive ' : ''}unsigned integer`);
  }
  return value;
}

function optionalUnsigned(value, label, options) {
  return value === null ? null : unsigned(value, label, options);
}

export function normalizeSessionDiagnostics(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('session diagnostics must be an object');
  }
  if (!MODES.has(value.mode)) throw new TypeError('session diagnostics mode is unsupported');
  const staleSnapshotTotal = unsigned(value.stale_snapshot_total, 'stale_snapshot_total');
  const invalidSnapshotTotal = unsigned(value.invalid_snapshot_total, 'invalid_snapshot_total');

  if (value.mode === 'legacy_exclusive_v1') {
    for (const field of [
      'local_handle', 'focus_generation', 'roster_revision', 'roster_age_ms',
      'subscription_expires_at_unix', 'subscription_seconds_remaining',
    ]) {
      if (value[field] !== null) throw new TypeError(`legacy session diagnostics require null ${field}`);
    }
    return {
      mode: value.mode,
      label: 'legacy exclusive v1',
      localHandle: null,
      focusGeneration: null,
      rosterRevision: null,
      rosterAgeMs: null,
      subscriptionExpiresAtUnix: null,
      subscriptionSecondsRemaining: null,
      staleSnapshotTotal,
      invalidSnapshotTotal,
    };
  }

  if (typeof value.local_handle !== 'string' || !HANDLE_PATTERN.test(value.local_handle)) {
    throw new TypeError('local_handle must be a bounded opaque viewer handle');
  }
  const rosterRevision = unsigned(value.roster_revision, 'roster_revision', { positive: true });
  const rosterAgeMs = unsigned(value.roster_age_ms, 'roster_age_ms');
  const subscriptionExpiresAtUnix = unsigned(
    value.subscription_expires_at_unix,
    'subscription_expires_at_unix',
    { positive: true },
  );
  const subscriptionSecondsRemaining = unsigned(
    value.subscription_seconds_remaining,
    'subscription_seconds_remaining',
  );
  return {
    mode: value.mode,
    label: 'native MoQ multi-viewer v2',
    localHandle: value.local_handle,
    focusGeneration: optionalUnsigned(value.focus_generation, 'focus_generation', { positive: true }),
    rosterRevision,
    rosterAgeMs,
    subscriptionExpiresAtUnix,
    subscriptionSecondsRemaining,
    staleSnapshotTotal,
    invalidSnapshotTotal,
  };
}

export function sessionDiagnosticsPresentation(session) {
  if (session === null) return 'unavailable';
  if (session.mode === 'legacy_exclusive_v1') {
    return `${session.label} · implicit focus · no roster · stale/invalid ${session.staleSnapshotTotal}/${session.invalidSnapshotTotal}`;
  }
  const focus = session.focusGeneration === null ? 'spectator' : `focus generation ${session.focusGeneration}`;
  return `${session.label} · ${session.localHandle} · ${focus} · roster r${session.rosterRevision} age ${session.rosterAgeMs} ms · subscription ${session.subscriptionSecondsRemaining} s remaining · stale/invalid ${session.staleSnapshotTotal}/${session.invalidSnapshotTotal}`;
}
