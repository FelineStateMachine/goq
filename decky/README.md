# Goq Sigil for Decky

Goq Sigil is a controller-first, local management surface for the Sigil host
daemon. It supervises the existing `sigil-host.service`; Decky never owns the
streaming process and reloading or uninstalling the plugin does not stop Sigil.

The initial compatibility target is Bazzite with a separately installed,
compatible Sigil release. SteamOS remains unproven until its hardware
acceptance milestone is complete.

## Supported management

- View bounded appliance, daemon, enrollment, and session status.
- View the single paired Portal and its grants.
- Restart the fixed per-user Sigil service.
- Validate and transactionally apply the allowlisted stream configuration.
- Roll back a pending configuration transaction.
- Revoke Portal enrollment without rotating the Sigil identity.

Live stream diagnostics and identity factory reset remain visibly unavailable
until Sigil exports dedicated bounded contracts. The plugin does not read
private identity, authorization, invitation, or raw TOML bytes.

## Development

The toolchain follows the official Decky plugin template and pins pnpm 9.

```sh
pnpm install --frozen-lockfile
pnpm test
pnpm typecheck
pnpm build
```

Backend tests use only the Python standard library and mocked Sigil/systemd
adapters. Hardware acceptance is still required for Steam focus navigation,
Decky reload, cold boot, daemon recovery, rollback, reset, and uninstall.
