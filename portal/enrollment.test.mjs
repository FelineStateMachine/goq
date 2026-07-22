import assert from 'node:assert/strict';
import test from 'node:test';

import {
  formatInvitationExpiry,
  grantLabel,
  normalizeInvitationSummary,
  shortPeerFingerprint,
} from './enrollment.mjs';

const HOST = '12'.repeat(32);
const PEER = 'ab'.repeat(32);

test('normalizes only a bounded public invitation summary', () => {
  const summary = normalizeInvitationSummary({
    host_node_id: HOST,
    peer_node_id: PEER,
    expires_at_unix: 1_100,
    grants: ['view', 'gamepad'],
  }, 1_000);
  assert.equal(summary.hostFingerprint, '12121212…12121212');
  assert.equal(summary.peerFingerprint, 'abababab…abababab');
  assert.deepEqual(summary.grants, ['view', 'gamepad']);
  assert.equal('token' in summary, false);
});

test('rejects expired, malformed, duplicate, unknown, and raw-token-bearing summaries', () => {
  const base = { host_node_id: HOST, peer_node_id: PEER, expires_at_unix: 1_100, grants: ['view'] };
  assert.throws(() => normalizeInvitationSummary({ ...base, expires_at_unix: 1_000 }, 1_000));
  assert.throws(() => normalizeInvitationSummary({ ...base, host_node_id: 'nope' }, 1_000));
  assert.throws(() => normalizeInvitationSummary({ ...base, grants: ['view', 'view'] }, 1_000));
  assert.throws(() => normalizeInvitationSummary({ ...base, grants: ['admin'] }, 1_000));
  assert.throws(() => normalizeInvitationSummary({ ...base, token: 'secret' }, 1_000));
});

test('formats fingerprints, grants, and bounded expiry copy', () => {
  assert.equal(shortPeerFingerprint(PEER), 'abababab…abababab');
  assert.equal(grantLabel('pointer_keyboard'), 'keyboard + mouse');
  assert.equal(formatInvitationExpiry(1_030, 1_000), 'expires in under 2 minutes');
  assert.equal(formatInvitationExpiry(1_121, 1_000), 'expires in 3 minutes');
  assert.equal(formatInvitationExpiry(999, 1_000), 'expired');
});
