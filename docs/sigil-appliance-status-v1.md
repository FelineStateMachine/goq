# Sigil appliance status v1

`sigil appliance status` is the local, read-only contract for appliance
management surfaces such as the planned Decky Loader plugin. Decky remains an
unprivileged controller over the existing `sigil-host.service`; it does not
launch Sigil as a child, host streaming itself, or add a network listener.

```bash
sigil appliance status \
  --config ~/.config/sigil-spark/host.toml \
  --json
```

The command validates the owner-only configuration and identity, inspects the
authorization document without creating files or taking its writer lock, and
combines that durable state with Sigil's bounded runtime heartbeat. Output is a
single JSON document with `schema_version: 1`.

The public document includes only:

- Sigil version and overall `ready`, `active`, `degraded`, or `unavailable`
  state.
- Redacted host and enrolled-Portal fingerprints.
- Canonically ordered enrollment grants, epoch, and enrollment time.
- Runtime freshness, daemon state, uptime, session state, and a closed set of
  error codes.

It never includes complete endpoint IDs, invitation material, replay-ledger
entries, session nonces, addresses, PipeWire object names, filesystem paths, or
free-form error text.

## Runtime authority

While serving, Sigil holds an exclusive nonblocking lock at
`<state_path>/daemon-v1.lock`. A second daemon using the same state directory
fails before capture, input, or network initialization. The zero-length lock
file remains in place; the open file descriptor is the lifetime guard. When an
XDG runtime directory is available, Sigil also holds
`$XDG_RUNTIME_DIR/sigil-spark/daemon-global-v1.lock`. That per-user lock enforces
the product's one-daemon boundary even if two commands reference different
state directories. Configured production service mode requires a valid private
`XDG_RUNTIME_DIR` and fails before capture or networking if it is unavailable.
Only the explicit direct test-pattern proof may fall back to its per-state lock
without publishing a heartbeat.

Sigil atomically publishes
`$XDG_RUNTIME_DIR/sigil-spark/daemon-status-v1.json` at most once per two-second
heartbeat, plus explicit lifecycle transitions. Keeping the heartbeat in the
per-user runtime directory prevents high-frequency process state from entering
durable backups or surviving a reboot. The status file and its child directory
must be regular, owner-only objects. Reads and writes are bounded, refuse
symlinks, and use no-follow opens. Heartbeat I/O runs outside Tokio's async
worker threads.

The status command treats a missing runtime document as `offline`. A heartbeat
older than ten seconds is `stale`; stale data never claims a live session or
live uptime. Clean shutdown writes `stopping`, shuts down the Iroh router, and
then writes `stopped`. A crash leaves a bounded document that naturally becomes
stale, while the runtime directory itself is discarded at reboot. Every
runtime document is bound to the current host identity before its state is
combined with durable enrollment data.

The Decky backend will merge this document with `systemctl --user` unit state.
Service control, transactional configuration, enrollment reset, and identity
factory reset are separate follow-up contracts; they are intentionally not
part of status v1.
