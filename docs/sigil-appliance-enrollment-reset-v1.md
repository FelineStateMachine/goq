# Sigil appliance enrollment reset v1

`sigil appliance enrollment-reset` is the local mutation used by appliance
management surfaces to revoke the one enrolled Portal without rotating the
Sigil host identity:

```bash
sigil appliance enrollment-reset \
  --config "$HOME/.config/sigil-spark/host.toml" \
  --expected-host-fingerprint '12345678…90abcdef' \
  --json
```

The caller must first stop `sigil-host.service` and wait for it to become
inactive. The command acquires both daemon-lifetime locks without waiting and
fails before changing authorization state if a daemon still owns either lock.
It never stops or starts systemd itself. This keeps service policy in the Decky
backend and prevents a reset from leaving a live remote session attached.

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

Every successful invocation advances the authorization epoch, even when no
Portal is enrolled, because an unredeemed invitation may still exist outside
the host. The host identity stays byte-for-byte unchanged. Nonexpired replay
records remain intact, and all invitations from earlier epochs become stale.
A retry after a lost success response may advance the epoch again; that is a
safe additional invalidation.

The legacy `sigil enrollment revoke` spelling is a strict alias. It also
requires `--expected-host-fingerprint` and `--json`, so there is no
confirmation-free or live-daemon bypass.
