# goq agent guide

goq is a one-host, one-user game-streaming appliance. The public project and
website are **goq.sh**. **Sigil** is the pure Rust Linux host command and
daemon. **Portal** is the installed Tauri client that opens a session to a
Sigil host.

## Product boundary

- Sigil runs on a dedicated, physically headless bare-metal Linux machine.
- Gamescope owns the private Steam/game display; capture must not require an
  interactive desktop or XDG ScreenCast portal.
- The host is a pure Rust daemon. It must never depend on Tauri, WebKit, or a
  host-facing UI.
- Portal is client-only. It must not bundle host capture, encoding, daemon,
  identity, configuration, or Linux input-injection assets.
- Iroh owns native connectivity, endpoint identity, encryption, direct-path
  discovery, and relay fallback.
- Exactly one media session and one matching client are active at a time.
- Controller is the primary interaction model. Keyboard and relative mouse are
  secondary inputs.

## Current architecture

- `crates/sigil-protocol` owns bounded, versioned handshakes, capabilities,
  media/audio headers, input messages, protocol limits, and ALPN constants.
- `crates/sigil-host` builds the `sigil` daemon and `sigil-probe` diagnostic.
  - `main.rs` owns the CLI, endpoint/router setup, and capture probes.
  - `server.rs` owns session admission and separate media, input, and audio
    protocols.
  - `source.rs` owns test-pattern and Gamescope PipeWire capture plus AMD GstVA
    H.264 encoding. External `gst-launch` remains the default; an explicitly
    feature-gated in-process backend owns bounded encoder control.
  - `input.rs` owns strict Linux `uinput` pointer, keyboard, and Xbox-style
    virtual gamepad devices.
  - `audio.rs` owns bounded PipeWire/Opus capture.
  - `identity.rs` and `config.rs` own fail-closed persistent host state.
- `src-tauri` is Portal's native boundary.
  - `commands/network.rs` owns Iroh connections, bounded media-object receive,
    binary Tauri channels, input transport, and diagnostics.
  - `commands/auth.rs` owns FIDO2 `hmac-secret` derivation.
  - `commands/state.rs` owns launch options and process-wide bounded state.
- `portal/main.js` and the focused `portal/*.mjs` modules own WebCodecs decode,
  AudioWorklet playback, controller-first navigation, input capture, A/V
  synchronization, window geometry, and client diagnostics.
- `website/` is the static goq.sh site. A merge to `main` publishes it through
  the `goq-sh` Cloudflare Worker after the static-site gate passes.

## Proven baseline

- Sigil is independent of Tauri/WebKit and passes the pure-host dependency
  gate.
- Gamescope's exact PipeWire node is resolved from strict configured
  properties. AMD GstVA H.264 sustains the fixed 1280x800/60 first target on
  the GPD Pocket 4 test host with bounded post-encode delivery.
- Existing host configurations default to the proven external `gst-launch`
  backend. `encoder_backend = "in-process-gstreamer"` fails closed unless the
  Linux binary was explicitly built with that feature and its runtime
  GStreamer elements are available. The opt-in backend currently requires CBR;
  CQP remains on the external compatibility backend.
- The preferred media path authenticates and claims the single client over
  `sigil/control/1`, then admits that exact peer to a session-scoped upstream
  `iroh-moq` broadcast and static H.264 track. One configured GOP maps to one
  bounded native MoQ group; a newer independently decodable group cancels its
  predecessor and provides the latest-frame barrier. The custom media v3, v2,
  and v1 protocols remain explicit compatibility fallbacks.
- Portal crosses Rust-to-webview video and audio through bounded binary Tauri
  channels. It reports transport, frontend, decoder, presentation, and audio
  queue/drop timing separately.
- Input uses an Iroh connection independent of media backpressure. The host
  advertises only operational capabilities and neutralizes every held pointer,
  keyboard, and gamepad state when a session ends.
- Relative pointer movement, scroll, keyboard, virtual gamepad reports, Opus
  audio, reconnects, and second-client rejection have focused and loopback
  coverage. Hardware acceptance that remains incomplete is listed below.
- Portal's window scales to the incoming stream while preserving aspect ratio;
  larger client screens do not stretch the host image.

## Security boundary

- Portal's FIDO `hmac-secret` derives its stable Iroh peer identity. Sigil signs
  a short-lived, host-bound invitation for that exact peer and atomically
  persists the enrolled grants before accepting the first media session.
- `--dev-connect` is a test-only routing bypass, never an authorization claim.
  Debug builds and the explicit `demo-direct-node` feature may accept it and
  must display the development warning. Ordinary release builds must reject it.
- Enrollment is one-time; ordinary startup remains **PIN -> tap -> play** and
  reconnects authenticate the enrolled Iroh peer without replaying a ticket.
- View, pointer/keyboard, and gamepad permissions must be independently
  grantable. Replay, expiry, peer mismatch, cross-host use, and capability
  escalation must fail closed.
- The authorization state and replay ledger are security-sensitive host state;
  preserve their owner-only permissions and atomic update rules.
- `docs/compatibility-identifiers.md` freezes Portal's FIDO/Iroh derivation,
  Sigil's local runtime child, and its Linux virtual-input identity tuple.
  Never rename one in place or add a silent alternate acceptance path.

## Release and installation boundary

- Sigil is machine setup, not a desktop download. The public bootstrap is
  `curl -fsSL https://goq.sh/install-sigil | bash`.
- `website/install-sigil` intentionally fails closed until a reviewed Minisign
  publisher key and immutable signed release tag are configured.
- Product Sigil packages must come from clean committed source, contain only
  the allowlisted runtime payload, carry complete checksums/provenance, and have
  a detached Minisign signature. Installation must preserve identity and host
  configuration and must not silently start, restart, or enable the service.
- Portal is a compiled desktop download, never a shell install. The first
  public target is macOS arm64 and requires Developer ID signing, hardened
  runtime, notarization, stapling, and strict Gatekeeper verification. Its
  TeamIdentifier must equal `release/portal-apple-team-id.txt`, and every
  Portal asset must carry protected exact-tag GitHub build provenance. Do not
  advertise an unavailable platform/architecture as a download.
- The Minisign secret, Apple certificate, notarization credentials, host
  identity, and FIDO-derived secrets must never enter the repository, release
  archive, logs, command line, or general website deployment environment.

## Latency and correctness invariants

- Bound every capture, encode, media, frontend, decode, audio, and input queue.
- Prefer dropping or cancelling stale video over increasing latency.
- Keep input transport independent from media and audio backpressure.
- Never send uncompressed video through Tauri IPC or Iroh.
- Never reintroduce base64 media events; use binary Tauri channels.
- A discontinuity must resume from a keyframe carrying codec configuration.
- A slow or resetting receiver must not accumulate a playable history.
- Keep H.264 at 1280x800/60 as the known-safe target until adaptive work is
  measured end to end. Do not select codecs by compression ratio alone.
- Preserve native pointer coordinates independently from encoded resolution or
  client window size.
- Session teardown must release all held input transitions and emit a neutral
  gamepad state even after an error or disconnect.

## Remaining public-alpha gates

- Issue #3's repository work is implemented: one-time capability enrollment,
  replay protection, grant intersection, and controller-usable Portal import.
  Its exact release candidates still participate in the issue #6 hardware UAT.
- Issue #4: configure the offline Minisign trust root, publish the signed Sigil
  asset set, and prove clean install plus upgrade/rollback from the public
  command.
- Issue #5: configure the committed Apple TeamIdentifier pin, then publish the
  signed/notarized/attested macOS arm64 Portal DMG, digest, and manifest.
- Issue #6: prove physically headless cold boot, physical client controller
  gameplay, mouse buttons consumed by Gamescope/an actual game, sustained A/V
  and resource percentiles without latency growth, difficult-NAT relay
  diagnostics, and reconnect/second-client rejection using the exact release
  candidates.
- Issues #7-#10 are streaming hardening: Iroh/MoQ recovery hardening, adaptive
  bitrate, motion-sensitive resolution, and automatic low-latency codec
  selection. The preferred path now uses pinned upstream `iroh-moq`; the
  external `gst-launch` encoder can still honor a keyframe request only at its
  next natural configured IDR. An opt-in in-process GStreamer control path now
  exists; hardware loss/relay proof remains before claiming issue #7 complete.
  The in-process encoder backend is not part of the product release feature set
  until its native Bazzite and cross-build/package dependency gates pass.

## Working rules

- Use Rust edition 2024. Sigil and Portal require Rust 1.91 or newer and the
  repository pins Rust 1.95 in `rust-toolchain.toml`; `sigil-protocol` retains a
  1.85 minimum. Source `~/.cargo/env` before invoking Cargo directly.
- Building Sigil with `in-process-gstreamer` requires the GStreamer core, app,
  and video development packages. Deployment requires their matching runtime
  libraries plus `appsink`, `pipewiresrc`, parser/conversion elements, and the
  selected GstVA encoder plugin; never treat build headers as runtime payload.
- Run `./scripts/verify-demo-build.sh` for the complete repository gate. It
  covers Rust format/tests/clippy, the Linux cross-build when available,
  frontend syntax/tests, ShellCheck, package tests, loopback transport, and
  release-profile containment of `--dev-connect`.
- Run `./scripts/verify-website.sh` for every website or public installer
  change. Exercise interactive website changes in a real browser.
- Treat `docs/fresh-bazzite-host.md`, `docs/public-release-delivery.md`, and
  GitHub issues #3-#10 as the current acceptance sources. Distinguish local
  tests from hardware proof and from still-pending operator acceptance.
- Treat `docs/hardware-uat/MATRIX.md` as the claim contract. Until both
  required rows pass the same exact candidate, name hardware results as
  reference-host evidence; do not use unqualified `hardware-proven` language.
  A development matrix pass is not release-matrix proof.
- Use the strict live capture gate on Bazzite when capture changes:

  ```bash
  sigil capture probe \
    --source gamescope-pipewire \
    --config ~/.config/sigil-spark/host.toml \
    --frames 300 \
    --expect-size 1280x800 \
    --minimum-fps 55
  ```

- Any change that can affect Sigil startup, capture, encoding, transport,
  decoding, A/V, input, or session behavior requires a real Portal -> UMPC
  Gamescope connection from the exact candidate commit. A successful config
  check, capture probe, headless probe, or loopback is supporting evidence, not
  a substitute for that end-to-end session. Fresh exact-commit builds are not
  expected to be enrolled: do not skip the connection because ordinary
  authentication blocks it. For development hardware testing, build Sigil with
  `demo-auth-bypass`, run it with
  `--dev-allow-unauthorized` and a bounded `--max-runtime-seconds`, and connect
  a debug Portal with `--dev-connect`. Verify the affected live diagnostics
  and interaction path, then stop the bypass process, undo temporary Gamescope
  controls, and restore and verify the ordinary authenticated
  `sigil-host.service` before calling the test complete.

- Do not report a runtime-affecting change as hardware-tested unless Portal
  actually established the UMPC session and exercised the affected live path.
  When authentication or identity is itself under test, the development bypass
  is not evidence for that behavior: complete the real LA_key PIN/tap
  enrollment, disconnect, and ticket-free reconnect with the exact candidate
  instead.

- On Linux with NVIDIA, set `WEBKIT_DISABLE_DMABUF_RENDERER=1` for Portal.
- Preserve `/Users/dami/Developer/sigil` untouched; it is the inherited source,
  not this working repository.
- Preserve unrelated worktree changes. Never sweep generated credentials,
  evidence, local Claude/Codex configuration, `.env` files, or test-host state
  into commits.
- Do not weaken a fail-closed production path to make a development proof pass.
  Keep explicit development flags visibly labeled and build-time contained.
