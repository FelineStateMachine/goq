# Keyhome

Keyhome is a native remote-control client concept: open the app, plug in a YubiKey, tap or enter PIN when required, and connect to the paired home machine over an end-to-end encrypted Iroh tunnel.

The point of this repo is to test viability before building a polished product. The MVP should prove four things:

1. A Tauri Rust backend can reliably detect and challenge a YubiKey across Linux/macOS/Windows.
2. The YubiKey can carry or derive the identity material needed to authenticate the user and find the remote machine.
3. A native Iroh node can dial the host agent and establish an authenticated encrypted session.
4. Remote desktop control can be streamed with acceptable latency after hardware-backed auth.

## Architecture hypothesis

```text
+------------------ Keyhome Tauri Client ------------------+
| UI webview                                                |
| - connection state                                        |
| - video/input surface                                     |
| - setup and diagnostics                                   |
|                                                          |
| Tauri IPC                                                 |
|                                                          |
| Rust backend                                              |
| - YubiKey detection/challenge/PIN flow                    |
| - token-resident peer/address config                      |
| - native Iroh endpoint                                    |
| - remote-control protocol client                          |
+----------------------------+-----------------------------+
                             |
                             | E2EE Iroh protocol
                             v
+----------------------- Host Agent ------------------------+
| - paired node identity                                    |
| - screen capture / encode                                 |
| - keyboard + pointer injection                            |
| - user consent / local safety controls                    |
+-----------------------------------------------------------+
```

## Planning artifacts

- `openspec/project.md` — project conventions and scope boundaries.
- `openspec/specs/yubikey-identity/spec.md` — hardware identity and token data requirements.
- `openspec/specs/iroh-session/spec.md` — peer discovery, authenticated dialing, and encrypted channel requirements.
- `openspec/specs/remote-control/spec.md` — desktop streaming and input behavior requirements.
- `openspec/changes/validate-tauri-yubikey-iroh-mvp/` — first MVP viability-testing plan.

## Non-goals for the first MVP

- General-purpose account systems.
- Browser-only implementation.
- Cloud relay ownership beyond what Iroh itself requires.
- Polished installer/update channels.
- Production-grade cross-platform remote-control permissions UX.

## Initial technology candidates

- App shell: Tauri 2.
- Backend: Rust.
- Network: Iroh native Rust stack.
- Hardware auth: YubiKey challenge-response/PIV/OpenPGP/CTAP investigation; exact mode is part of the viability work.
- Host agent: Rust service/binary, initially local or same-LAN before hardening NAT traversal and unattended access.
