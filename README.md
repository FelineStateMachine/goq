# goq.sh

**Your key is the address. Plug in, tap, play.**

goq turns one Linux machine into a personal Steam streaming appliance. You
dedicate that machine to your games, plug in nothing but power and network,
and play from another computer with a controller. The goal is the latency of
sitting at the machine, not the latency of a screen-sharing tool.

goq is not a multi-tenant streaming service and not a desktop-sharing tool.
It streams a private [Gamescope](https://github.com/ValveSoftware/gamescope)
session that exists only for your games: the host needs no monitor, no
desktop environment, and no interactive login. Exactly one client is admitted
to exactly one session at a time, authenticated by a hardware security key.

## The two programs

- **Sigil** is the host: a pure Rust daemon for a bare-metal, physically
  headless Linux machine. Gamescope owns the private game display, PipeWire
  provides capture, and hardware H.264 plus Opus leave the machine over
  [iroh](https://github.com/n0-computer/iroh), so the client reaches the host
  by its cryptographic identity rather than an IP address.
- **Portal** is the client: an installed Tauri desktop application with a
  native iroh endpoint, WebCodecs video decode, AudioWorklet playback, and
  controller-first navigation.

```text
Sigil · bare-metal Linux host          Portal · installed desktop client
  Gamescope headless session             native iroh endpoint
    -> PipeWire video/audio                -> binary Tauri channels
    -> hardware H.264 + Opus               -> WebCodecs video + audio out
    -> iroh / MoQ media objects          controller, keyboard, mouse
  uinput virtual devices        <----      -> iroh input protocol
```

## Design principles

- **Latency over history.** Every capture, encode, transport, decode, and
  input queue is bounded. Stale video is dropped or cancelled rather than
  buffered, so a slow receiver never accumulates a playable backlog. Media
  travels as bounded MoQ groups: one GOP is one group, and a newer
  independently decodable group cancels its predecessor.
- **Input is independent.** Controller, keyboard, and relative mouse travel
  on their own iroh connection, unaffected by media backpressure. Session
  teardown always releases held keys and emits a neutral gamepad state, even
  after an error or disconnect.
- **Fail closed.** Portal derives its stable iroh identity from a FIDO2 key
  with `hmac-secret`. A signed, short-lived, peer-bound invitation from Sigil
  enrolls that one client once, with independently granted view,
  pointer/keyboard, and gamepad permissions and durable replay protection.
  Every ordinary launch after that is **PIN -> tap -> play**.
- **Strict product boundary.** Sigil never depends on Tauri or a webview.
  Portal never bundles capture, encoding, a daemon, or host input injection.

## Status

goq is pre-release. The reference host is an AMD machine running
[Bazzite](https://bazzite.gg); H.264 at 1280x800/60 is the known-safe
measured benchmark, while production capture accepts any even native mode
within the protocol's bounded pixel limits.

Working today:

- The pure Rust Sigil daemon: Gamescope PipeWire capture at the display's
  native mode, AMD GstVA H.264 encoding, and bounded PipeWire/Opus audio.
- Authenticated session admission over a control connection, then a
  session-scoped upstream `iroh-moq` broadcast with a strictly validated
  catalog, with earlier custom media protocols kept as explicit compatibility
  fallbacks.
- Adaptive bitrate and motion-sensitive resolution on the opt-in in-process
  encoder backend: session-authenticated receiver feedback, hysteresis, and
  changes that commit only on exact encoder readback or a target-size IDR.
- Portal playback through bounded binary Tauri channels into WebCodecs and an
  AudioWorklet, with per-stage queue, drop, and latency diagnostics.
- Linux `uinput` keyboard, relative mouse, and an Xbox-style virtual gamepad,
  with strict device preflight and end-of-session neutralization.
- One-time FIDO-bound enrollment, replay protection, and controller-usable
  onboarding in Portal.
- A deterministic, checksum-bound Bazzite runtime package with serialized
  install/upgrade and tamper-checked rollback, plus the foundation of a
  controller-first Decky Loader management plugin over a redacted local
  appliance-status contract.

The streaming-hardening roadmap, where the remaining work is hardware
acceptance on the reference host, is tracked as
[MoQ recovery hardening](https://github.com/FelineStateMachine/goq/issues/7),
[adaptive bitrate](https://github.com/FelineStateMachine/goq/issues/8),
[motion-sensitive resolution](https://github.com/FelineStateMachine/goq/issues/9),
and [automatic codec selection](https://github.com/FelineStateMachine/goq/issues/10).
The remaining public-alpha acceptance gates (headless cold boot, physical
controller gameplay, sustained A/V measurement, and the signed public release
path) are tracked in issues
[#4](https://github.com/FelineStateMachine/goq/issues/4) to
[#6](https://github.com/FelineStateMachine/goq/issues/6).

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/sigil-protocol` | Bounded, versioned handshakes, media/audio headers, input messages, limits, ALPNs |
| `crates/sigil-host` | The `sigil` daemon and the `sigil-probe` diagnostic |
| `src-tauri/` | Portal's native side: iroh transport, FIDO2 identity, binary channels |
| `portal/` | Portal frontend: WebCodecs decode, audio playback, controller navigation |
| `decky/` | Decky Loader management plugin for the Sigil appliance |
| `website/` | The static goq.sh site, published via a Cloudflare Worker |
| `scripts/` | Build, packaging, verification, and host provisioning tooling |
| `docs/` | Runbooks and contracts for provisioning, releases, and hardware acceptance |

## Getting started

You need:

- Rust 1.91 or newer (the repository pins Rust 1.95)
- Tauri v2 system dependencies
- A FIDO2 security key with `hmac-secret` support
- For a host: a dedicated AMD Linux machine (Bazzite is the packaged target)

Building Sigil with the optional Linux `in-process-gstreamer` feature
additionally needs the GStreamer core, app, and video development libraries;
the matching runtime libraries and plugins must be installed on the host.

Run Portal against a Sigil host during development:

```bash
source ~/.cargo/env
cargo tauri dev
```

On Linux with NVIDIA, prefix with `WEBKIT_DISABLE_DMABUF_RENDERER=1`.

### Provision a host

Activate an installed dedicated AMD host with the portable
[Sigil host activation guide](docs/sigil-host-activation.md). To evaluate a
candidate machine first, run `scripts/bazzite-inventory.sh` for a read-only
hardware report; add `--smoke` to exercise a bounded 1280x800/60 VA-API
encode, or `--cold-boot` on the first login after a physically headless boot
for a strict readiness gate.

### Enroll one Portal

On Portal's first launch, enter the security-key PIN and choose **show portal
id**. On the Sigil host, create an invitation for that exact peer:

```bash
sigil invitation create \
  --config ~/.config/sigil-spark/host.toml \
  --peer PORTAL_PEER_ID \
  --pointer-keyboard \
  --gamepad \
  --output ~/portal.goq-invite
```

Move the owner-only invitation file to the client and open it with Portal.
Sigil consumes it once; every later launch is **PIN -> tap -> play**. To move
Portal to a different Sigil host, revoke the enrollment on the old host and
use **client -> reset enrollment**. A new invitation never silently replaces
a working enrollment.

## Verification

Run the complete repository gate before sharing work:

```bash
./scripts/verify-demo-build.sh
```

It covers Rust format, tests, and clippy, the Linux cross-build when
available, frontend syntax and tests, ShellCheck, package tests, and loopback
transport proof. Website and installer changes go through
`./scripts/verify-website.sh`. Hardware acceptance evidence lives under
[`docs/hardware-uat/`](docs/hardware-uat/README.md), separate from local
tests.

## Releases

Sigil is machine setup, not a desktop download: the public bootstrap is
`https://goq.sh/install-sigil`, and it intentionally fails closed until the
Minisign publisher trust root and the first signed release exist. Its release
assets carry a single frozen build-target suffix, `linux-glibc2.17-x86_64`,
pinned by `release/sigil-target-contract.txt`; that name describes the binary
ABI (glibc 2.17, x86-64), not a distribution, so one build runs across the
supported AMD hosts. Portal is a compiled, signed desktop application, never a
shell install; the first public target is macOS arm64. The packaging, signing,
and publication ceremonies are documented in
[public release delivery](docs/public-release-delivery.md) and the
[Portal release runbook](docs/portal-release.md).

## License

[MIT](LICENSE)
