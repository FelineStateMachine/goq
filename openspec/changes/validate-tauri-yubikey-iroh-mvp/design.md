# Design: Viability-testing MVP

## Architecture decision

Use Tauri as a thin UI shell and put all risky integrations in Rust. The UI should call explicit commands such as `detect_yubikey`, `authenticate_token`, `load_pairing`, `connect_host`, `start_remote_control`, and `disconnect`.

## YubiKey investigation plan

Test modes in this order unless early evidence changes the ranking:

1. HMAC-SHA1 challenge-response: attractive for deterministic secret derivation, but may have tooling and setup constraints.
2. PIV: strong private-key boundary and mature tooling, but less direct for storing arbitrary peer data.
3. CTAP2/FIDO2 extensions: useful if native support provides better cross-platform behavior than browser WebAuthn PRF, but API maturity must be verified.

The MVP should not assume the final token mode until the spike proves:

- user-presence/PIN behavior,
- crate availability and maintenance,
- OS permission requirements,
- ability to store, read, derive, or unlock pairing material,
- failure behavior when the token is absent or wrong.

## Pairing material model

Candidate model:

- Remote host identity: stored as a small token-readable blob or token-unlocked local encrypted blob.
- Client authentication identity: derived from challenge-response or backed by a token private key.
- Local cache: allowed only for non-secret metadata unless a spec change explicitly permits encrypted caching.

If YubiKey storage proves too constrained, the fallback is a local encrypted pairing file whose decrypting key is token-derived.

## Iroh protocol model

Start with one custom ALPN/protocol for diagnostics, then split streams as needed:

- auth/handshake,
- control events,
- frame transport,
- diagnostics/logs.

The host must verify that the connecting client presents the expected token-bound identity, not merely that it knows a node ID.

## Remote-control spike model

Do not start with perfect remote desktop. Start with the smallest loop that proves the integration boundary:

1. Host sends synthetic frames or a captured low-resolution screen frame stream.
2. Client renders frames in the Tauri webview.
3. Client sends pointer/keyboard events.
4. Host logs or applies input events depending on OS permissions.

Then replace synthetic/capture/log paths with real capture and injection one OS at a time.

## Evidence artifacts

Create `docs/evidence/` entries during implementation. Each entry should include commands run, platform, crate versions, result, and exact blockers.
