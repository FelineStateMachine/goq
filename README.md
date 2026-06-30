# sigil

Your key is the address. Plug in, tap, connect.

## What is it?

Sigil is a Tauri v2 desktop app (Rust backend, vanilla JS frontend) that lets you remote into your machine using a FIDO2 security key as the sole authentication factor. The key produces a `hmac-secret` that becomes the 32-byte seed for an Iroh P2P endpoint. Both host and client derive the same identity from the same key. The key handles addressing.

Screen capture and encoding run through an ffmpeg subprocess with hardware acceleration (NVENC, VAAPI, QSV, AMF, VideoToolbox) up to 60fps. The client decodes via WebCodecs with hardware acceleration for H.264, H.265, and AV1. A software fallback (xcap + openh264) is available if ffmpeg isn't installed.

## Why use it?

- **Shared identity.** The FIDO2 key derives the Iroh peer identity. Both sides get the same node ID from the same key.
- **End-to-end encrypted.** Iroh handles the transport with built-in E2EE. The key never leaves the hardware token.
- **Hardware-accelerated video.** ffmpeg handles capture and encode with your GPU. WebCodecs handles decode on the client. Up to 60fps.
- **Cross-platform.** Linux, macOS, Windows. Auto-detects the best encoder for your hardware.
- **Peer-to-peer.** Via Iroh relay. No third-party services or accounts.
- **Single binary.** Tauri bundles everything except ffmpeg (system dependency).

## Install

### Prerequisites

- **Rust 1.85+** ([install](https://rustup.rs))
- **ffmpeg** for screen capture and hardware encoding
- **FIDO2 security key** with hmac-secret support (YubiKey, Google Titan, etc.)

### Linux (Ubuntu/Debian) — system deps

```bash
sudo apt install -y ffmpeg libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libudev-dev libusb-1.0-0-dev pkg-config libwayland-dev libpipewire-0.3-dev libgbm-dev
```

On NVIDIA, set `WEBKIT_DISABLE_DMABUF_RENDERER=1` before running.

### macOS

```bash
brew install ffmpeg
```

Grant **Accessibility** and **Screen Recording** permissions in System Settings > Privacy & Security.

### Windows

```powershell
choco install ffmpeg
```

### Install

```bash
cargo install --git https://args.io/cat/sigil sigil
```

### Usage

1. **Register** (one-time per machine): click register, tap your key. Creates a resident credential.
2. **Host**: click host. The app starts listening on an Iroh endpoint derived from your key.
3. **Connect**: on another machine, click connect, tap the same key. The screen appears, input is forwarded.

Keyboard shortcuts: type your PIN, then press `c` (connect), `r` (register), or `h` (host).

Full setup details: [docs/SETUP.md](docs/SETUP.md)

## Docs

| Document | Description |
|---|---|
| [Setup Guide](docs/SETUP.md) | Platform-specific install, dependencies, usage, architecture, protocol |
| [Architecture](docs/SETUP.md#architecture) | Transport, encoder backends, wire protocol, connection tracking |
| [Spikes](docs/SETUP.md#spikes-evidence) | Seven viability spikes with pass/fail evidence |
| [Agent Guide](AGENTS.md) | Development context for AI agents working on the codebase |
| [FIDO2 HID Evidence](docs/evidence/2026-06-23-ctap-hid-fido2-titan-communication.md) | ctap-hid-fido2 communication with FIDO2 keys |
| [HMAC → Iroh Derivation](docs/evidence/2026-06-23-hmac-iroh-derivation.md) | hmac-secret → Iroh SecretKey proof |
| [Iroh Native Ping](docs/evidence/2026-06-23-iroh-native-ping.md) | Iroh connectivity between native endpoints |
| [YubiKey HMAC Detection](docs/evidence/2026-06-23-yubikey-hmac-detection.md) | YubiKey HMAC-secret detection |
| [FIDO2 HID vs hidraw](docs/evidence/2026-06-23-fido2-hid-raw-hidraw.md) | HID vs hidraw approach comparison |

## Tech stack

| Layer | Technology |
|---|---|
| App shell | Tauri v2 |
| Backend | Rust (edition 2024) |
| P2P transport | Iroh 1.0 (relay-assisted, E2EE) |
| Hardware auth | FIDO2 CTAP via `ctap-hid-fido2` |
| Capture + encode | ffmpeg subprocess (NVENC/VAAPI/QSV/AMF/VideoToolbox) |
| Fallback capture | `xcap` + `openh264` (software) |
| Codecs | H.264, H.265, AV1 (configurable) |
| Client decode | WebCodecs (hardware-accelerated, all three codecs) |
| Input injection | `enigo` |
| Identity storage | OS keyring |
| Frontend | Vanilla JS (no npm, no build step) |

## License

MIT
