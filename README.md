# Sigil Spark

Sigil Spark is a native Tauri client and bare-metal Linux host for one-to-one,
low-latency Steam streaming over iroh.

The project starts from Sigil's working remote-desktop implementation. Native
iroh connectivity, hardware video encoding, WebCodecs decoding, FIDO-derived
identity, and remote input are already present. The next phase replaces the
desktop-oriented hot path with a headless Gamescope pipeline.

## Target architecture

```text
Bare-metal Linux host
  Gamescope headless
    -> PipeWire video/audio
    -> hardware H.264 + Opus
    -> iroh/MoQ

Tauri client
  native iroh endpoint
    -> binary Tauri channel
    -> WebCodecs video
    -> audio output
  controller/keyboard/mouse
    -> iroh input protocol
    -> host uinput devices
```

The host is intended to run without a physical display or desktop environment.
Gamescope supplies the private graphical session and virtual display seen by
Steam and games.

## Current migration status

- A shared, bounded and versioned protocol crate owns handshakes, H.264 media,
  input messages, limits, and ALPNs.
- The Linux host is a pure Rust binary with no Tauri or webview dependency.
- The Bazzite path captures the exact Gamescope PipeWire node and uses AMD
  GstVA H.264 at the fixed 1280×800/60 first target.
- The client delivers encoded frames to WebCodecs through a raw Tauri binary
  channel. The handoff is capped at four frames, the decode queue and
  presentation queue at two, and a bounded watchdog recovers a suspended
  webview animation-frame callback without presenting a frame twice. Its
  transport/frontend/decoder drop counters are reported separately.
- Media and input use separate Iroh connections; one active client is enforced.
- Linux `uinput` supports bounded relative-mouse and keyboard injection plus a
  separate Xbox-style virtual gamepad, with strict device
  ownership/mode/ACL preflight and neutralization when a session ends.
- The installed Tauri application is client-only: it contains no host daemon,
  host registration, capture, encoder, or desktop input-injection path.
- FIDO2 `hmac-secret` identity derivation remains in the normal client flow.
  Debug builds have an explicit, visibly labeled direct-node bypass for test
  hosts; release builds reject it.
- Controller-first client navigation includes a D-pad PIN pad, negotiated
  latest-state gamepad routing, and a one-second Back+Start escape chord.
- The Bazzite host has an allowlisted deterministic runtime package with
  checksum-bound release IDs, serialized install/upgrade, tamper-checked
  rollback, and package-owned user assets that follow the active release.

The software JPEG client decode fallback and legacy wire compatibility remain.
Media still uses reliable QUIC rather than drop-aware Iroh/MoQ. A dedicated
persistent PipeWire sink, bounded Opus datagrams, WebCodecs decode, and
AudioWorklet playback are implemented; longer-run audio/video synchronization
measurement remains. Client authorization still needs
short-lived capability tickets; the debug direct-node bypass is routing, not
authentication.

## Immediate milestones

1. **Done:** split the bare-metal host into a pure Rust binary; keep Tauri client-only.
2. **Hardware-proven:** capture the Gamescope PipeWire node without an XDG portal or physical display; cold-boot service proof remains.
3. **Hardware-proven:** encode H.264 at 1280x800/60 with bounded buffers and no B-frames.
4. **Done:** replace base64 WebCodecs delivery with a bounded binary Tauri channel.
5. Replace FIFO frame delivery with drop-aware iroh/MoQ media delivery.
6. **Host hardware-proven; physical client controller pending:** the virtual
   Xbox-style controller negotiated over Iroh and produced the expected
   button, stick, trigger, D-pad, and neutral-release events on the Bazzite
   host. Client controller navigation and mapping are covered by focused tests;
   the remaining integration gate is forwarding a physical controller attached
   to the client. Keyboard injection is hardware-proven, and the conventional
   relative mouse replaces Gamescope-incompatible absolute motion.
7. **Live-proven:** bounded PipeWire audio capture, Opus delivery, and client
   playback. Add a persistent headless sink and quantify A/V synchronization.
8. Replace or supplement FIDO pairing with short-lived capability tickets.

## Development

Requirements:

- Rust 1.91 or newer (the repository pins Rust 1.95)
- Tauri v2 system dependencies
- A FIDO2 key with `hmac-secret` support for the normal identity flow

Provision a dedicated AMD host with the
[fresh Bazzite host runbook](docs/fresh-bazzite-host.md). The runbook also
defines the temporary `slate` stand-in used for protocol and daemon extraction.
Run `scripts/bazzite-inventory.sh` on a candidate host for a read-only report;
add `--smoke` to exercise a bounded 1280×800/60 VA-API encode. On the first SSH
login after a physically headless cold boot, use `--cold-boot` for a strict,
read-only connector, session, service, PipeWire, and boot-order gate.

Run the complete local demo gate before transferring a snapshot:

```bash
./scripts/verify-demo-build.sh
```

After transferring the two prebuilt Linux binaries to a Bazzite host, stage an
exact hash-pinned release without starting it:

```bash
scripts/stage-bazzite-release.sh \
  --release-id <commit-or-source-snapshot-sha256> \
  --host-binary <absolute-path> --host-sha256 <sha256> \
  --probe-binary <absolute-path> --probe-sha256 <sha256>
```

The stager atomically updates the user-owned `current` symlink only after both
binaries, their dynamic libraries, and bounded startup commands validate. Host
identity, hardware-specific configuration, and service activation remain
separate gates in the Bazzite runbook.

Create the Bazzite runtime package from a committed source revision. Product
mode exports clean `HEAD` and builds the host and probe itself with locked
`cargo-zigbuild` in an isolated target directory; it never accepts externally
supplied binaries. Product packages require a clean worktree and detached
Minisign signature; the explicit development flags are only for temporary
testing:

```bash
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host.tar.gz \
  --minisign-key /absolute/path/to/release.key

# Temporary development package only:
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host-dev.tar.gz \
  --allow-dirty --allow-unsigned

# Temporary externally built binaries require both development flags:
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host-prebuilt-dev.tar.gz \
  --allow-dirty --allow-unsigned \
  --host-binary /absolute/path/to/sigil-host \
  --probe-binary /absolute/path/to/sigil-probe
```

The package contains only the two Linux binaries, installer/rollback tools,
service/audio/udev assets, license, checksums, and build provenance. It cannot
contain the source tree, `.env` files, identities, host configuration, or test
evidence. Its release ID is the SHA-256 of the complete installed-file checksum
manifest. Identical inputs produce a byte-identical archive. The manifest marks
product binaries as built from clean `HEAD`; caller-supplied development
binaries are explicitly marked as having unverified provenance.

Verify the detached signature against the separately trusted public key before
extracting, then run `payload/stage-this-release.sh` as the gaming user. Install
and upgrade never restart PipeWire or start/enable the service. Roll back an
installed upgrade with `sigil-spark-host-rollback`; add `--restart` only when a
service interruption is intended.

The macOS Tauri build currently produces an arm64 DMG for development. A public
client release additionally requires Developer ID signing with hardened
runtime, notarization, stapling, and strict Gatekeeper verification; ad-hoc
development signatures are not a distributable package. With Apple credentials
configured as described by the official
[Tauri macOS signing guide](https://v2.tauri.app/distribute/sign/macos/), run:

```bash
scripts/package-macos-client.sh --output-dir /absolute/release/directory
```

Run the current client/host application:

```bash
source ~/.cargo/env
cargo tauri dev
```

On Linux with NVIDIA:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo tauri dev
```

## License

MIT
