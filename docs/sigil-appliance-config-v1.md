# Sigil appliance configuration v1

`sigil appliance config` is the local, machine-readable settings interface for
an unprivileged appliance controller such as the Decky plugin. It exposes a
small typed projection instead of accepting arbitrary TOML edits.

Every command requires the owner-only host configuration and explicit JSON
output:

```bash
sigil appliance config show --config ~/.config/sigil-spark/host.toml --json
sigil appliance config validate --config ~/.config/sigil-spark/host.toml --json <request.json
sigil appliance config set --config ~/.config/sigil-spark/host.toml --json <request.json
sigil appliance config commit --config ~/.config/sigil-spark/host.toml \
  --transaction <id> --expected-instance <id> --json
sigil appliance config rollback --config ~/.config/sigil-spark/host.toml \
  --transaction <id> --json
```

`show` returns the exact-byte SHA-256 revision, editable settings, and a pending
transaction summary. The editable v1 settings are:

- Native dimensions or an explicit even width and height.
- Frame rate.
- CBR bitrate or CQP quantizer for a configured Gamescope PipeWire source.

Source selection, codec, capture backend, executable and device paths,
PipeWire selectors, audio/input configuration, identity, and state paths are
not editable through this contract. Operator comments and all non-managed TOML
fields survive an update.

Requests are strict JSON and contain an optimistic-concurrency revision:

```json
{
  "schema_version": 1,
  "expected_revision": "sha256:<64 lowercase hexadecimal digits>",
  "settings": {
    "resolution": { "mode": "native" },
    "framerate": 60,
    "rate_control": { "mode": "cbr", "bitrate_kbps": 12000 }
  }
}
```

For a synthetic source, `rate_control` must be `null`. A fixed resolution uses
`{"mode":"fixed","width":1280,"height":800}`; CQP uses
`{"mode":"cqp","quantizer":24}`. `validate` performs no writes. A no-op
`set` returns `changed: false`, no transaction ID, and does not require a
restart.

## Controller sequence

Only a stopped Sigil can be changed. The controller performs:

1. Stop `sigil-host.service` and wait until it is inactive.
2. Call `set`. Sigil atomically installs the candidate and returns a transaction
   ID plus candidate revision.
3. Start the service and wait for appliance status to report a new instance in
   `ready` state with the candidate revision.
4. Stop the service cleanly and retain that exact instance ID.
5. Call `commit` with the transaction and instance IDs. If startup or shutdown
   fails, stop the service, call `rollback`, and restart the prior config.

Commit succeeds only when the bounded runtime document is fresh, belongs to the
current host, reports the exact candidate revision and a daemon instance newer
than the one present at `set`, reached `ready` at least once, stopped cleanly,
and contains no runtime error. A running daemon holds the same lifecycle locks,
so set, commit, rollback, enrollment reset, and service execution cannot race.
Configured service startup takes those locks before its definitive config reload
and transaction recovery. An interrupted prepared install is therefore
completed before capture or networking starts; startup can never launch bytes
that were read before a concurrent `set`.

## Durability and recovery

Config files and requests are bounded to 64 KiB and 16 KiB respectively.
Revisions hash the exact bytes read from the same opened file descriptor used
for parsing. Writes and recovery artifacts are owner-only, no-follow, atomic,
and directory-synced.

The journal records `prepared`, `pending_validation`, `committing`, or
`rolling_back`. Recovery deterministically finishes an interrupted candidate
install, commit cleanup, or byte-exact rollback. Live bytes that match neither
the journaled base nor candidate fail closed instead of being overwritten.
Only one transaction may be pending.

Management failures write one JSON object to stderr with `schema_version: 1`
and one closed error code: `unsupported_schema`, `invalid_request`,
`validation_failed`, `revision_conflict`, `lifecycle_busy`,
`transaction_busy`, `transaction_pending`, `transaction_not_found`,
`transaction_conflict`, `health_not_proven`, or `unsafe_storage`. They never
include paths, configuration contents, or free-form internal errors.
