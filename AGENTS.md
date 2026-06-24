# Sigil agent guide

Sigil is a Tauri v2 remote desktop app with FIDO2 key authentication over Iroh P2P. The app is built and functional — spikes are done, the core flow works end to end.

## Current focus

The project is past the spike/validation phase. ffmpeg subprocess is now the primary capture+encode path (xcap+openh264 is the fallback), achieving ~28 fps with NVENC (up from ~1–2 fps). The bottleneck has moved to network/relay throughput and client-side decode.

Active work areas:

- **Codec configurability** — H.264, H.265, and AV1 are all supported. Users can select codec + encoder backend (nvenc, vaapi, qsv, amf, videotoolbox, software) in the info panel.
- **Multi-codec WebCodecs decode** — frontend handles H.264 (AVCC), H.265 (hvcC), and AV1 (OBU) for hardware-accelerated decode.
- **Connection tracking** — fixed with `conn.closed()` future + `tokio::select!` for reliable disconnect detection; connection counter now properly decrements.
- **Wire protocol** — header expanded to 14 bytes: `width(4) + height(4) + size(4) + keyframe(1) + codec(1)`. Codec byte: 0=h264, 1=h265, 2=av1.
- **Client-side stats** — info panel shows codec received, dropped frames, total frames received when connected as client. Encoder config and host performance sections are hidden when connected.
- **Remaining performance work** — network/relay throughput, client-side decode efficiency.

## Key files

- `src-tauri/src/commands.rs` — all backend logic (FIDO2 key derivation, Iroh networking, screen capture, H.264 encoding, input injection)
- `src/ui/index.html` — the entire frontend (single-file UI)

## Tech notes

- **Rust edition 2024** — requires Rust 1.85+.
- **Iroh 1.0 API** — uses the latest Iroh endpoint, protocol, and ticket APIs. Older Iroh 0.x code will not compile.
- Always run `source ~/.cargo/env` before any `cargo` command.
- On Linux with NVIDIA, set `WEBKIT_DISABLE_DMABUF_RENDERER=1` before running or building.

## Working rules

- Use OpenSpec for capability planning before coding.
- Treat `openspec/` artifacts as the source of truth for requirements and scope.
- Do not build a browser-only version; the core architecture is native Tauri + Rust backend.
- Prefer small, verifiable Rust spikes over broad scaffolding when exploring new capabilities.

## Validated assumptions

The highest-risk assumptions have all been validated via spikes:

1. ~~Native YubiKey flows usable from a Tauri backend.~~ ✅ Validated (spikes 002, 003 — ctap-hid-fido2 communicates with Google Titan v2)
2. ~~Safe storage or derivation of Iroh identity/addressing material from a YubiKey.~~ ✅ Validated (spike 004 — Titan hmac-secret → Iroh SecretKey → working endpoint, 6.5ms RTT)
3. ~~Iroh session setup and authenticated protocol binding.~~ ✅ Validated (spikes 001, 007 — Iroh endpoints, ALPN routing, and key-derived identity all work)
4. ~~Remote desktop stream/input latency and OS permissions.~~ ✅ Validated (spikes 005, 006 — frame streaming and input injection over Iroh both functional)

## OpenSpec workflow

- Create/modify proposal, design, tasks, and delta specs under `openspec/changes/<change>/`.
- Validate with `openspec validate --all --strict` before reporting specs as done.
- Do not implement active changes until approved.
