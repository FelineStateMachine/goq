# Change: Validate Tauri + YubiKey + Iroh MVP

## Summary

Create the first evidence-producing MVP path for Keyhome: a native Tauri app authenticates with a YubiKey, obtains or unlocks pairing material, starts a native Iroh endpoint, dials a host agent, and proves a minimal remote-control loop.

## Motivation

The architecture is plausible but has four hard risks that must be tested before product build-out:

1. Native YubiKey APIs differ by mode, OS, permissions, and crate maturity.
2. Storing or deriving Iroh identity/addressing material from a YubiKey may require a different token mode than the original idea assumes.
3. Iroh's native Rust stack is likely suitable, but the protocol binding and identity story need a concrete spike.
4. Remote desktop capture/input injection is OS-permission-heavy and may dominate MVP complexity.

## Scope

This change plans the MVP viability work. It does not require polished UI or production security hardening.

In scope:

- Tauri app scaffold with Rust commands for auth/connect diagnostics.
- YubiKey mode comparison spike: HMAC-SHA1 challenge-response, PIV, and/or CTAP2 where practical.
- Pairing-data design for host node identity and client authentication material.
- Native Iroh client and host-agent connectivity proof.
- Minimal remote-control proof: frame stream plus pointer/keyboard event path, even if initially local-only.
- Evidence log documenting supported platforms, crate choices, blockers, and latency observations.

Out of scope:

- SaaS account model.
- Production pairing ceremony UX.
- Installer signing, auto-update, and full permission onboarding.
- Multi-host management beyond one paired host.

## Dependencies

- Baseline specs: `yubikey-identity`, `iroh-session`, `remote-control`.
- Hardware: at least one YubiKey with the modes being tested.
- Host OS permissions for capture/input injection.

## Success criteria

The MVP is viable if it can demonstrate, on at least one developer machine pair or loopback setup:

1. YubiKey-gated auth succeeds through the Tauri backend.
2. Pairing material is read, derived, or unlocked only after the verification step.
3. Iroh client and host establish an authenticated protocol channel.
4. The client renders host frames and sends input events over that channel.
5. The repo contains exact commands and evidence for what worked and what failed.
