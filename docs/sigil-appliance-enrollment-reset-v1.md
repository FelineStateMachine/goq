# Sigil appliance enrollment reset v1

`sigil appliance enrollment-reset` is the stopped-service reset-all mutation
used by appliance management surfaces. It revokes every enrolled Portal
without rotating the Sigil host identity:

```bash
sigil appliance enrollment-reset \
  --config "$HOME/.config/sigil-spark/host.toml" \
  --expected-host-fingerprint '12345678…90abcdef' \
  --json
```

The caller must first stop `sigil-host.service` and wait for it to become
inactive. The command always acquires the durable
`<state_path>/daemon-v1.lock` without waiting. When a valid private
`XDG_RUNTIME_DIR` is available, it also acquires
`$XDG_RUNTIME_DIR/sigil-spark/daemon-global-v1.lock`; an unsafe configured
runtime directory fails closed. If the variable is unset, reset remains
available from SSH or a recovery shell using only the per-state lock. A live
daemon for that state still blocks the operation. The command never stops or
starts systemd itself. This keeps service policy in the Decky backend and
prevents a reset from leaving a live remote session attached.

Authorization writers have an explicit second level in that hierarchy:
`daemon-v1.lock` (and the global lifecycle lock when configured) is acquired
before `<state_path>/authorization-v1.lock`. The daemon holds the authorization
writer lease for its complete lifetime and serves handshakes from its validated
in-memory state. An offline reset temporarily becomes the sole writer only
after it owns the lifecycle scope. Writer-lease acquisition uses nonblocking
`flock` with a fixed 250 ms bound, so an abandoned or hung management process
cannot stall daemon startup or recovery indefinitely; terminating a crashed
owner releases the kernel lease and a retry loads the last complete atomic
state document. Live handshake contention fails closed immediately and may be
retried by Portal.

The writer now owns one strict, bounded `authorization-v2.json` document with a
global committed revision, up to 32 peer records, unique opaque viewer handles,
per-viewer grant revisions, the invitation replay ledger, and a bounded durable
security-mutation audit. On its first sole-writer open, Sigil converts a valid
development `authorization-v1.json` in memory, atomically installs v2, syncs the
directory, and removes v1. If v2 exists, v1 is never consulted. Unknown fields,
duplicate peers or handles, unsafe storage, oversized state, and ambiguous
versions fail closed.

The running daemon also binds
`<state_path>/authorization-admin-v1.sock`. Its parent and socket are owner-only,
the JSON request and response are bounded and strict, and no network listener
is added. `sigil invitation create`, `sigil enrollment list`, per-viewer
`sigil enrollment revoke --handle`, and `sigil enrollment set-capabilities`
use this path. A mutation is acknowledged only after the v2 document is
atomically durable, its committed revision is published, and the live registry
has applied the change. Input-grant reduction first invalidates focus and then
neutralizes all virtual input under the shared input-operation lock. Revoking a
viewer's view permission disconnects that v2 viewer. Opaque handles and coarse
grants are returned; raw peer keys and invitation bytes are not logged.

The expected fingerprint is the redacted value returned by
`sigil appliance status`. It is an accidental-target confirmation interlock,
not a new authentication factor. Existing same-user file ownership and mode
checks remain the authority boundary.

Success writes exactly one JSON object:

```json
{
  "schema_version": 1,
  "operation": "enrollment_reset",
  "host_fingerprint": "12345678…90abcdef",
  "had_enrollment": true,
  "previous_epoch": 4,
  "current_epoch": 5,
  "invitations_invalidated": true
}
```

Every successful reset-all invocation advances the authorization epoch, even
when no Portal is enrolled, because an unredeemed invitation may still exist outside
the host. The host identity stays byte-for-byte unchanged. Nonexpired replay
records remain intact, and all invitations from earlier epochs become stale.
A retry after a lost success response may advance the epoch again; that is a
safe additional invalidation.

The legacy `sigil enrollment revoke` spelling without `--handle` remains a
strict reset-all alias. It requires `--expected-host-fingerprint` and `--json`.
With `--handle`, it performs a live reset-one operation through the daemon's
owner-only socket and retains the same host-fingerprint interlock. These scopes
are explicit: reset-one does not advance the invitation epoch; reset-all does.
