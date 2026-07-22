# Sigil appliance status v1

`sigil appliance status` is the backward-compatible, local read-only appliance
status contract:

```bash
sigil appliance status \
  --config ~/.config/sigil-spark/host.toml \
  --json
```

The single JSON document has `schema_version: 1`. It contains Sigil version and
overall state; redacted host and enrolled-Portal fingerprints; ordered grants,
epoch, and enrollment time; and bounded runtime freshness, daemon state,
uptime, session state, and closed error codes. It never contains complete
endpoint IDs, tickets, nonces, addresses, device selectors, filesystem paths,
or free-form errors.

Version 1 intentionally omits configuration revisions, pending transactions,
daemon instance IDs, and sticky ready evidence. Controllers that manage
configuration must explicitly request the
[status v2 contract](sigil-appliance-status-v2.md).

Runtime authority, lifecycle locking, freshness, owner-only storage, and clean
shutdown behavior are otherwise identical to status v2.
