import assert from 'node:assert/strict';
import test from 'node:test';

import {
  committedRustConnection,
  disconnectRejectedRustConnection,
} from './connection-attempt.mjs';

test('only an explicitly connected result owns committed Rust state', () => {
  assert.equal(committedRustConnection({ connected: true }), true);
  assert.equal(committedRustConnection({ connected: false }), false);
  assert.equal(committedRustConnection({ connected: 1 }), false);
  assert.equal(committedRustConnection(null), false);
});

test('rejected committed connections invoke native disconnect exactly once', async () => {
  const commands = [];
  const invoke = async (command) => { commands.push(command); };
  assert.equal(await disconnectRejectedRustConnection(invoke, true), true);
  assert.deepEqual(commands, ['iroh_client_disconnect']);
  assert.equal(await disconnectRejectedRustConnection(invoke, false), false);
  assert.deepEqual(commands, ['iroh_client_disconnect']);
});
