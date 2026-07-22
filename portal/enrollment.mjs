const NODE_ID_PATTERN = /^[0-9a-f]{64}$/;
const GRANTS = Object.freeze(['view', 'pointer_keyboard', 'gamepad']);

export function shortPeerFingerprint(nodeId) {
  if (typeof nodeId !== 'string' || !NODE_ID_PATTERN.test(nodeId)) {
    throw new TypeError('node ID must be 64 lowercase hexadecimal characters');
  }
  return `${nodeId.slice(0, 8)}…${nodeId.slice(-8)}`;
}

export function normalizeInvitationSummary(value, nowUnix = Math.floor(Date.now() / 1000)) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('invitation summary must be an object');
  }
  const allowed = new Set(['host_node_id', 'peer_node_id', 'expires_at_unix', 'grants']);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new TypeError(`unknown invitation summary field: ${key}`);
  }
  if (!NODE_ID_PATTERN.test(value.host_node_id || '')) throw new TypeError('invalid host node ID');
  if (!NODE_ID_PATTERN.test(value.peer_node_id || '')) throw new TypeError('invalid peer node ID');
  if (!Number.isSafeInteger(value.expires_at_unix) || value.expires_at_unix <= nowUnix) {
    throw new TypeError('invitation is expired or has an invalid expiry');
  }
  if (!Array.isArray(value.grants) || value.grants.length === 0 || value.grants.length > GRANTS.length) {
    throw new TypeError('invitation grants must be a non-empty bounded list');
  }
  const grants = [];
  for (const grant of value.grants) {
    if (!GRANTS.includes(grant) || grants.includes(grant)) {
      throw new TypeError('invitation contains an unknown or duplicate grant');
    }
    grants.push(grant);
  }
  return Object.freeze({
    hostNodeId: value.host_node_id,
    peerNodeId: value.peer_node_id,
    hostFingerprint: shortPeerFingerprint(value.host_node_id),
    peerFingerprint: shortPeerFingerprint(value.peer_node_id),
    expiresAtUnix: value.expires_at_unix,
    grants: Object.freeze(grants),
  });
}

export function grantLabel(grant) {
  switch (grant) {
    case 'view': return 'view stream';
    case 'pointer_keyboard': return 'keyboard + mouse';
    case 'gamepad': return 'gamepad';
    default: throw new TypeError(`unknown invitation grant: ${grant}`);
  }
}

export function formatInvitationExpiry(expiresAtUnix, nowUnix = Math.floor(Date.now() / 1000)) {
  if (!Number.isSafeInteger(expiresAtUnix) || !Number.isSafeInteger(nowUnix)) {
    throw new TypeError('expiry values must be integer Unix seconds');
  }
  const remaining = expiresAtUnix - nowUnix;
  if (remaining <= 0) return 'expired';
  if (remaining < 120) return 'expires in under 2 minutes';
  const minutes = Math.ceil(remaining / 60);
  return `expires in ${minutes} minutes`;
}
