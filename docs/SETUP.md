# Keyhome Setup

## Prerequisites

### Rust (required on all platforms)

Keyhome uses Rust edition 2024 and requires **Rust 1.85 or later**.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

> Always run `source ~/.cargo/env` before any `cargo` command in a new shell.

### Linux (Ubuntu/Debian)

```bash
# Tauri v2 system dependencies
sudo apt install -y libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev

# FIDO2 + screen capture dependencies
sudo apt install -y libudev-dev libusb-1.0-0-dev pkg-config

# xcap Wayland support (needed even on X11)
sudo apt install -y libwayland-dev libpipewire-0.3-dev libgbm-dev

# ffmpeg — screen capture + hardware encoding (NVENC/VAAPI)
sudo apt install -y ffmpeg
```

If you are on Linux with an NVIDIA GPU, set this environment variable before running or building:

```bash
export WEBKIT_DISABLE_DMABUF_RENDERER=1
```

### macOS

Install the Tauri CLI (v2):

```bash
cargo install tauri-cli --version "^2"

# ffmpeg — screen capture + hardware encoding (VideoToolbox)
brew install ffmpeg
```

Grant the following permissions in **System Settings > Privacy & Security**:

- **Accessibility** — required for input injection (mouse/keyboard control)
- **Screen Recording** — required for screen capture

### Clone

```bash
git clone https://github.com/FelineStateMachine/keyhome.git
cd keyhome
```

## Run

```bash
# Development mode (opens the Tauri window)
cargo tauri dev

# Or build a debug binary
cargo tauri build --debug
```

On Linux with NVIDIA, prefix with the env var:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo tauri dev
```

## Usage

Keyhome uses a FIDO2 security key (Google Titan) to derive Iroh peer identities. There is no address copy/paste — the key handles identity for you.

### 1. Register (one-time per machine)

Click **Register** and tap your Titan key. This creates a resident (discoverable) credential stored on the key itself. You only do this once per machine.

### 2. Host

Click **Start host**, then either:

- **Tap your Titan key** — the app derives the Iroh identity from the key and starts listening, or
- **Use saved keyring** — if you have previously hosted, the app can reuse the stored identity from the system keyring without requiring a tap.

The host is now reachable over Iroh at an endpoint ID derived from the Titan key.

### 3. Connect

Click **Connect** and tap your Titan key. The app derives the same host endpoint ID from the key and dials it over Iroh. The remote screen appears in the viewer canvas. Input (mouse and keyboard) is forwarded to the host.

> Both machines must use the same Titan key (or a key that produces the same hmac-secret output). The key derivation replaces manual address sharing entirely.

## Architecture

- **Transport**: Iroh 1.0 (peer-to-peer, relay-assisted)
- **Screen capture + encoding**: ffmpeg subprocess (x11grab + NVENC/VAAPI on Linux, avfoundation + VideoToolbox on macOS)
- **Fallback**: xcap capture + openh264 software encoding (if ffmpeg not installed)
- **FIDO2**: ctap-hid-fido2 (CTAP 2.0/2.1 over HID)
- **Frame encoding**: H.264 (ffmpeg/NVENC primary, openh264 fallback for client-side decode)
- **Input injection**: enigo (cross-platform mouse/keyboard)
- **Protocol**: BiStream with frame header `[width:u32][height:u32][h264_len:u32][keyframe:u8][h264_data]`
- **Identity**: Iroh SecretKey derived from Titan hmac-secret extension; no address copy/paste

## Spikes (evidence)

| Spike | Status | What it proves |
|-------|--------|----------------|
| 001-iroh-native-ping | ✅ PASS | Iroh endpoints + ALPN routing work in native Rust |
| 002-yubikey-hmac-detection | ⚠️ PARTIAL | challenge_response crate works but only for YubiKey (not Titan) |
| 003-fido2-hid-enumeration | ✅ PASS | ctap-hid-fido2 communicates with Google Titan v2 |
| 004-hmac-iroh-derivation | ✅ PASS | Titan hmac-secret → Iroh SecretKey → working endpoint (6.5ms RTT) |
| 005-frame-stream | ✅ PASS | Screen capture → JPEG → Iroh stream → client receives frames |
| 006-input-injection | ✅ PASS | enigo input injection works over Iroh bidirectional stream |
| 007-titan-no-copy | ✅ PASS | Both sides derive the same Iroh identity from Titan — no address sharing needed |
