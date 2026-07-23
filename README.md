# goq.sh

goq.sh is one-to-one, low-latency Steam streaming over iroh: Portal is the
installed Tauri client, and Sigil is the pure Rust bare-metal Linux host.

The project starts from an inherited working remote-desktop implementation. Native
iroh connectivity, hardware video encoding, WebCodecs decoding, FIDO-derived
identity, and remote input are already present. The next phase replaces the
desktop-oriented hot path with a headless Gamescope pipeline.

## Target architecture

```text
Sigil · bare-metal Linux host
  Gamescope headless
    -> PipeWire video/audio
    -> hardware H.264 + Opus
    -> iroh/MoQ

Portal · Tauri client
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
- The Linux host is the pure Rust `sigil` binary with no Tauri or webview
  dependency.
- The core host captures the standard upstream Gamescope PipeWire video source,
  derives the encoded size from its bounded native caps by default, and uses
  AMD GstVA H.264. An explicit width/height pair remains available for
  downscale proofs; 1280×800/60 is the first measured target, not a product
  constraint. Bazzite remains the only packaged appliance, and the GPD Pocket
  4 run is reference-host evidence rather than a completed hardware matrix.
  SteamOS packaging, cold boot, encoder availability, and cross-host hardware
  UAT remain unproven.
- Gamescope video keeps the proven external `gst-launch` pipeline as its
  configuration default. Linux builds made explicitly with
  `in-process-gstreamer` can opt into an in-process pipeline for bounded
  bitrate, force-keyframe, and two-tier resolution control. Published Sigil
  packages include that backend while keeping the external pipeline as the
  runtime default.
- Portal reports bounded receiver queue, drop, latency, and recovery telemetry
  over a session-authenticated feedback stream. The host combines it with
  trusted path and scheduler pressure, applies hysteresis, and commits an
  in-process CBR bitrate decision only after exact encoder readback. External
  encoder sessions remain explicitly advisory-only.
- On the opt-in in-process backend, fresh damage-driven frame progress and
  authenticated receiver pressure select either the resolved native mode or
  an exact same-aspect three-quarter motion tier when one exists. A switch
  commits only when GStreamer emits a target-size IDR with SPS/PPS; Portal then
  starts a new decoder epoch while pointer mapping remains tied independently
  to the native Gamescope surface. Modes without an exact even reduced tier
  continue at native resolution instead of rejecting the stream.
- Portal delivers encoded frames to WebCodecs through a raw Tauri binary
  channel. The handoff is capped at four frames, the decode queue and
  presentation queue at two, and a bounded watchdog recovers a suspended
  webview animation-frame callback without presenting a frame twice. Its
  transport/frontend/decoder drop counters are reported separately.
- Media and input use separate Iroh connections; one active client is enforced.
- Linux `uinput` supports bounded relative-mouse and keyboard injection plus a
  separate Xbox-style virtual gamepad, with strict device
  ownership/mode/ACL preflight and neutralization when a session ends.
- The installed Portal application is client-only: it contains no host daemon,
  host registration, capture, encoder, or desktop input-injection path.
- FIDO2 `hmac-secret` derives Portal's stable Iroh peer identity. A signed,
  short-lived Sigil invitation enrolls that peer once with exact view,
  pointer/keyboard, and gamepad grants; ordinary reconnects remain
  **PIN -> tap -> play**. Debug builds retain a visibly labeled routing bypass,
  while release builds reject it.
- Controller-first client navigation includes a D-pad PIN pad, negotiated
  latest-state gamepad routing, and a one-second Back+Start escape chord.
- The Bazzite host has an allowlisted deterministic runtime package with
  checksum-bound release IDs, serialized install/upgrade, tamper-checked
  rollback, and package-owned user assets that follow the active release.
- Sigil exposes a versioned, redacted local appliance-status document and
  daemon-lifetime lock as the foundation for a controller-first Decky Loader
  management plugin. The plugin remains a UI over the user service, not a
  second streaming host.

The software JPEG client decode fallback and legacy wire compatibility remain.
The preferred video path authenticates the client over a dedicated control
connection, then admits that exact peer to a session-scoped upstream
`iroh-moq` broadcast. Sigil publishes an immutable `catalog.json` snapshot;
Portal discovers and strictly validates the versioned Goq extension before it
subscribes to the H.264 track. The extension identifies Goq's `SGV1` media
envelope and GOP layout, so it does not falsely advertise the track as a
standard Hang rendition. Older hosts without a catalog retain an explicit
static-track compatibility path. Configured GOPs become bounded native MoQ
groups, and a new independently decodable group cancels the stale predecessor.
The custom media v3, v2, and v1 transports remain explicit compatibility modes.
A dedicated persistent PipeWire sink, bounded Opus datagrams, WebCodecs decode,
and AudioWorklet playback are implemented; longer-run audio/video
synchronization measurement remains. The debug direct-node bypass is routing,
not authentication.

## Immediate milestones

1. **Done:** split Sigil into a pure Rust host binary; keep Portal client-only.
2. **GPD Pocket 4/Bazzite reference-host evidence:** capture the exact Gamescope
   PipeWire node without an XDG portal. Physically headless cold boot and
   service startup remain public-alpha acceptance gates.
3. **GPD Pocket 4/Bazzite reference-host evidence:** encode H.264 at
   1280x800/60 with bounded buffers and no B-frames.
4. **Done:** replace base64 WebCodecs delivery with a bounded binary Tauri channel.
5. **Upstream Iroh/MoQ transport implemented; hardware proof pending:** an
   authenticated control lease gates a session-scoped upstream MoQ broadcast;
   bounded native groups cancel stale GOPs and recover through explicit
   keyframe requests. The opt-in in-process encoder turns recovery requests
   into coalesced forced IDRs without blocking media publication; the external
   encoder responds at its next natural configured IDR. Bazzite/package
   acceptance and induced-loss relay proof remain in issue #7.
6. **GPD Pocket 4/Bazzite reference-host evidence; physical client controller
   pending:** the virtual Xbox-style controller negotiated over Iroh and
   produced the expected button, stick, trigger, D-pad, and neutral-release
   events on that host. Client controller navigation and mapping are covered
   by focused tests; the remaining integration gate is forwarding a physical
   controller attached to the client. Keyboard injection was exercised on the
   same reference host, and the conventional relative mouse replaces
   Gamescope-incompatible absolute motion.
7. **GPD Pocket 4/Bazzite reference-host evidence:** bounded PipeWire audio
   capture, a persistent headless sink, Opus delivery, and client playback.
   Quantify longer-run A/V synchronization.
8. **Done:** add signed, peer-bound, one-time capability enrollment with
   durable replay protection and controller-usable Portal onboarding.
9. **Adaptive control implemented; hardware proof pending:** feedback remains
   constant-size and generation-scoped, coalescing retains cumulative pressure,
   and `applied=true` requires exact in-process encoder readback. Native Ubuntu
   x264 control is CI-proven; Bazzite GstVA loss/relay acceptance and product
   packaging remain in issue #8.
10. **Motion-sensitive resolution implemented; hardware proof pending:** the
    in-process backend lowers resolution during sustained damage-driven motion
    or receiver pressure and restores native detail only after stillness,
    clean feedback, and cooldown. Every transition is an exact configured-IDR
    discontinuity with truthful per-frame dimensions. Portal reconfigures
    WebCodecs atomically and preserves native pointer mapping. Ubuntu x264 CI
    covers downscale and restore; repeated GstVA/Portal acceptance on Bazzite
    remains in issue #9.

## Development

Requirements:

- Rust 1.91 or newer (the repository pins Rust 1.95)
- Tauri v2 system dependencies
- A FIDO2 key with `hmac-secret` support for the normal identity flow

The Linux `in-process-gstreamer` host feature additionally needs the GStreamer
core, app, and video development libraries at build time. Its matching runtime
libraries and plugins, including `appsink`, must be installed on the host.
Published Sigil packages include this feature while retaining the external
pipeline as the runtime default until a host opts into the in-process backend.

### Enroll one Portal

On first launch, enter the security-key PIN and choose **show portal id**. On
the Sigil host, create an invitation for that exact peer:

```bash
sigil invitation create \
  --config ~/.config/sigil-spark/host.toml \
  --peer PORTAL_PEER_ID \
  --pointer-keyboard \
  --gamepad \
  --output ~/portal.goq-invite
```

Move the owner-only invitation file to the client, open it with Portal, and
confirm the displayed host, peer, expiry, and grants. Sigil consumes it once;
future launches are simply **PIN -> tap -> play**. `--print-deep-link` is
available for an explicit `goq://invite/...` handoff, but prints the short-lived
credential to the terminal and should not be used in recorded shells.

To move Portal to another Sigil, revoke the host enrollment first, disconnect
Portal, then use **client -> reset enrollment** and confirm the displayed host.
The reset is intentionally explicit and controller reachable; importing a new
invitation never silently replaces a working enrollment.

Activate an installed dedicated AMD host with the portable, package-owned
[Sigil host activation guide](docs/sigil-host-activation.md). It uses the
current gaming user, dynamically discovered hardware, and installed package
assets without assuming a maintainer hostname or checkout path. The
[fresh Bazzite host runbook](docs/fresh-bazzite-host.md) remains the engineering
and hardware-acceptance log, including the temporary `slate` stand-in.
The backward-compatible [appliance status v1 contract](docs/sigil-appliance-status-v1.md)
and explicit [v2 contract](docs/sigil-appliance-status-v2.md) define the local,
redacted interface intended for the Decky management surface.
The [transactional configuration v1 contract](docs/sigil-appliance-config-v1.md)
defines its bounded, crash-recoverable settings workflow.
The [enrollment reset v1 contract](docs/sigil-appliance-enrollment-reset-v1.md)
defines the offline, identity-preserving reset used by that surface.
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
separate gates in the package-owned activation guide.

The thin binary stager is only for unmanaged development layouts. It refuses
activation when package-managed service, audio, rollback, or udev links follow
`current`; use the complete runtime package installer for those hosts so the
active release always includes matching assets and rollback metadata.

Create the Bazzite runtime package from a committed source revision. Product
mode exports clean tagged `HEAD` and builds the host and probe itself with
locked `cargo-zigbuild` in an isolated target directory; it never accepts
externally supplied binaries. The builder emits an unsigned candidate for the
separate offline signing ceremony; the explicit development flags are only for
temporary testing:

```bash
scripts/package-bazzite-release.sh \
  --release-tag v0.1.0 \
  --output /tmp/sigil-v0.1.0-linux-glibc2.17-x86_64.tar.gz

# Temporary development package only:
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host-dev.tar.gz \
  --allow-dirty --allow-unsigned

# Temporary externally built binaries require both development flags:
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host-prebuilt-dev.tar.gz \
  --allow-dirty --allow-unsigned \
  --host-binary /absolute/path/to/sigil \
  --probe-binary /absolute/path/to/sigil-probe
```

Verify the unsigned candidate, then follow the offline signing and protected
draft-promotion ceremony in [public release delivery](docs/public-release-delivery.md).
The GitHub workflow and package builder never receive the Minisign secret.

The package contains the primary `sigil` host executable, its byte-identical
`sigil-host` compatibility copy, the `sigil-probe` diagnostic, installer and
rollback tools, service/audio/udev assets, the portable activation guide,
license, checksums, and build provenance. It cannot contain the source tree,
`.env` files, identities, host configuration, or test evidence. Its release ID
is the SHA-256 of the complete installed-file checksum manifest. Identical
inputs produce a byte-identical archive. The manifest marks product binaries
as built from clean `HEAD`; caller-supplied development binaries are explicitly
marked as having unverified provenance.

During the legacy rollback window, `sigil-host.service` deliberately starts
the byte-identical `sigil-host` compatibility filename. Interactive and
documented host commands use `sigil`; retaining the service filename allows an
older installed rollback helper to validate and reactivate a newer release.

Verify the detached signature against the separately trusted public key before
extracting, then run `payload/stage-this-release.sh` as the gaming user. Install
and upgrade never restart PipeWire or start/enable the service. Roll back an
installed upgrade with `sigil-spark-host-rollback`; add `--restart` only when a
service interruption is intended.

The macOS Portal build currently produces an arm64 DMG for development. A public
Portal release additionally requires Developer ID signing with hardened
runtime, notarization, stapling, and strict Gatekeeper verification; ad-hoc
development signatures are not a distributable package. Before tagging, the
public TeamIdentifier in `release/portal-apple-team-id.txt` must match the
protected certificate, and the tag-ref workflow attests every published Portal
asset. With Apple credentials configured as described by the official
[Tauri macOS signing guide](https://v2.tauri.app/distribute/sign/macos/), run:

```bash
scripts/package-macos-client.sh \
  --release-tag v0.1.0 \
  --output-dir /absolute/release/directory
```

The repository publication and website-manifest procedure is documented in the
[Portal release runbook](docs/portal-release.md).

Run Portal against Sigil during development:

```bash
source ~/.cargo/env
cargo tauri dev
```

On Linux with NVIDIA:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo tauri dev
```

The static `website/` directory is published as Cloudflare Worker static assets
after a merge to `main`. See the [Cloudflare Worker release setup](docs/cloudflare-worker.md)
for the one-time service and GitHub environment configuration.

Portal is distributed as a compiled, signed desktop application; it is never
installed through a shell pipe. Sigil uses a separate Bazzite machine bootstrap
at `https://goq.sh/install-sigil`. That bootstrap intentionally fails closed
until the Minisign publisher trust root and first signed release exist. The
[public release delivery contract](docs/public-release-delivery.md) defines the
remaining signing and asset gates.

The promoted streaming-hardening roadmap is tracked as bounded
[Iroh/MoQ media objects](https://github.com/FelineStateMachine/goq/issues/7),
[adaptive bitrate](https://github.com/FelineStateMachine/goq/issues/8),
[motion-sensitive resolution](https://github.com/FelineStateMachine/goq/issues/9),
and [Auto Codec](https://github.com/FelineStateMachine/goq/issues/10). H.264 at
1280×800/60 remains a known-safe benchmark while production capture accepts
any even native mode within the protocol's bounded pixel limits.

## License

MIT
