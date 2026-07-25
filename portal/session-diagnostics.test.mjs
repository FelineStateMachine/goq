import test from 'node:test';
import assert from 'node:assert/strict';
import {
  normalizeSessionDiagnostics,
  sessionDiagnosticsPresentation,
} from './session-diagnostics.mjs';

test('normalizes bounded v2 session ownership without peer identity material', () => {
  const session = normalizeSessionDiagnostics({
    mode: 'moq_multi_viewer_v2',
    local_handle: 'viewer-0123456789abcdef',
    focus_generation: 7,
    roster_revision: 12,
    roster_age_ms: 25,
    subscription_expires_at_unix: 2_000_000_000,
    subscription_seconds_remaining: 42,
    stale_snapshot_total: 2,
    invalid_snapshot_total: 1,
    peer_id: 'must-not-cross-the-allowlist',
  });
  assert.equal(session.focusGeneration, 7);
  assert.equal(session.rosterRevision, 12);
  assert.doesNotMatch(JSON.stringify(session), /peer_id|must-not-cross/);
  assert.match(sessionDiagnosticsPresentation(session), /roster r12 age 25 ms/);
});

test('labels legacy mode truthfully and forbids invented roster state', () => {
  const raw = {
    mode: 'legacy_exclusive_v1',
    local_handle: null,
    focus_generation: null,
    roster_revision: null,
    roster_age_ms: null,
    subscription_expires_at_unix: null,
    subscription_seconds_remaining: null,
    stale_snapshot_total: 0,
    invalid_snapshot_total: 0,
  };
  const legacy = normalizeSessionDiagnostics(raw);
  assert.match(sessionDiagnosticsPresentation(legacy), /legacy exclusive v1 · implicit focus/);
  assert.throws(() => normalizeSessionDiagnostics({
    ...raw,
    local_handle: 'invented-viewer',
  }), /require null local_handle/);
});

test('rejects invalid handles, zero generations, and inexact counters', () => {
  const valid = {
    mode: 'moq_multi_viewer_v2',
    local_handle: 'viewer-one',
    focus_generation: null,
    roster_revision: 1,
    roster_age_ms: 0,
    subscription_expires_at_unix: 2_000_000_000,
    subscription_seconds_remaining: 30,
    stale_snapshot_total: 0,
    invalid_snapshot_total: 0,
  };
  assert.throws(() => normalizeSessionDiagnostics({ ...valid, local_handle: 'raw/key' }), /opaque/);
  assert.throws(() => normalizeSessionDiagnostics({ ...valid, roster_revision: 0 }), /positive/);
  assert.throws(() => normalizeSessionDiagnostics({
    ...valid,
    stale_snapshot_total: Number.MAX_SAFE_INTEGER + 1,
  }), /exact/);
});
