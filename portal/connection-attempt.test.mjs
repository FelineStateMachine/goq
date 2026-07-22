import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  committedRustConnection,
  disconnectRejectedRustConnection,
} from './connection-attempt.mjs';

const main = await readFile(new URL('./main.js', import.meta.url), 'utf8');

test('connection diagnostics recognize grouped media v3', () => {
  assert.match(
    main,
    /streamTransportMode = \[\s*'grouped-v3',\s*'independent-v2',\s*'reliable-v1',\s*'reliable-v0',\s*\]/,
  );
});

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
