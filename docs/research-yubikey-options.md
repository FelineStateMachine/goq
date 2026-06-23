# YubiKey technology options

Research date: 2026-06-23.

## Short verdict

Use two tracks for viability:

1. **HMAC-SHA1 challenge-response** via `challenge_response` for deterministic token-derived secrets. This is the best fit for deriving/unlocking Keyhome client identity, but requires the YubiKey OTP slot to be preconfigured and is less like a modern passkey UX.
2. **PIV** via `yubikey` for hardware-backed signatures/private keys. This is the best fit for explicit client authentication to the host, but remote peer metadata probably needs local encrypted storage or a token data-object design.

Treat CTAP2/WebAuthn as a later option, not the first Keyhome path. Native CTAP crates are available, but the current maintained `webauthn-authenticator-rs` line is browser/WebAuthn-shaped and pulls in server challenge/assertion semantics. It may still be useful for passkey-style UX, but it is not the shortest path to deterministic Iroh identity material.

## Candidate crates

| Area | Crate | Current evidence | Fit for Keyhome |
| --- | --- | --- | --- |
| HMAC-SHA1 challenge-response | `challenge_response = 0.5.46` | Crates.io/docs: supports YubiKey 2.2+, HMAC-SHA1 and OTP challenge-response, config APIs, default `rusb`, experimental `nusb`. Local spike compiled and ran. | Strong candidate for deriving/unlocking local secrets after token presence/touch, assuming slot setup is acceptable. |
| PIV smartcard | `yubikey = 0.8.0` stable / `0.9.0-pre.0` latest | Crates.io/docs: pure Rust cross-platform host-side driver over PC/SC for PIV keys, YubiKey 4/5 support, PIN/access policies. Latest pre-release MSRV 1.85; stable 0.8.0 MSRV 1.65. | Strong candidate for signing/authenticating sessions; weaker for arbitrary metadata storage unless PIV data objects are enough. |
| CTAP2/WebAuthn client | `webauthn-authenticator-rs = 0.5.5` | Crates.io/docs: supports CTAP2 over USB/NFC/BLE depending features, but MSRV 1.88+ and WebAuthn semantics. | Possible future passkey-like path; not first choice for deterministic Iroh seed/addressing. |
| Low-level FIDO HID | `fido-hid-rs` | Used by `webauthn-authenticator-rs`; latest MSRV 1.88. | Useful only if we need lower-level CTAP transport control. |

## Local environment observations

- Installed local rustup stable because system Cargo/Rust was too old for current Iroh (`rustc 1.75.0` vs Iroh 1.0.0 MSRV 1.91).
- Active local Rust after sourcing `$HOME/.cargo/env`: `rustc 1.96.0`, `cargo 1.96.0`.
- No YubiKey is attached to this machine right now. `lsusb` showed no Yubico vendor device.
- `pcscd` is inactive, which matters for PIV/PCSC tests.
- `/dev/hidraw*` devices exist but are root-owned; FIDO/HID tests may need udev rules or permissions.

## Security design implication

Do not store the Iroh private seed as plain token-readable metadata. Safer MVP shape:

- token challenge/PIV unlocks or signs,
- local encrypted pairing file stores host metadata and client identity material where needed,
- host verifies a token-bound client identity during protocol handshake,
- YubiKey absence blocks connection.

If we can fit host node ID into token-readable metadata safely, that is acceptable for non-secret address material. The secret should stay token-bound or encrypted under token-derived material.
