# Keyhome Setup

## Prerequisites (Ubuntu/Debian)

```bash
# Tauri v2 system dependencies
sudo apt install -y libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev

# FIDO2 + screen capture dependencies
sudo apt install -y libudev-dev libusb-1.0-0-dev pkg-config

# xcap wayland support (even on X11, xcap pulls these in)
sudo apt install -y libwayland-dev libpipewire-0.3-dev libgbm-dev

# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Clone
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

## Usage

### As Host (Server)
1. Click "Start host" — the app creates an Iroh endpoint and waits for connections
2. Copy the address JSON (click "Copy address")
3. Share the address with the client machine

### As Client
1. Paste the host's address JSON into the "Connect to Host" input
2. Click "Connect" — the app connects over Iroh and streams screen frames
3. The remote screen appears in the viewer canvas

## Architecture

- **Transport**: Iroh (peer-to-peer, relay-assisted)
- **Screen capture**: xcap (X11/PipeWire)
- **FIDO2**: ctap-hid-fido2 (CTAP 2.0/2.1 over HID)
- **Frame encoding**: JPEG (MVP — upgrade to H.264 later)
- **Protocol**: BiStream with frame headers `[width:u32][height:u32][jpeg_len:u32][jpeg_data]`

## Spikes (evidence)

| Spike | Status | What it proves |
|-------|--------|----------------|
| 001-iroh-native-ping | ✅ PASS | Iroh endpoints + ALPN routing work in native Rust |
| 002-yubikey-hmac | ⚠️ PARTIAL | challenge_response crate works but only for YubiKey (not Titan) |
| 003-fido2-hid | ✅ PASS | ctap-hid-fido2 communicates with Google Titan v2 |
| 004-derive-iroh-identity | ✅ PASS | Titan hmac-secret → Iroh SecretKey → working endpoint (6.5ms RTT) |
| 005-frame-stream | ✅ PASS | Screen capture → JPEG → Iroh stream → client receives frames |
