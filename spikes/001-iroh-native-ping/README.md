# 001: Native Iroh ping

## Question

Given a native Rust process, when Keyhome creates Iroh endpoints and dials a peer by endpoint ticket, can it establish an encrypted Iroh protocol session and measure RTT?

## Approach

Use current `iroh = 1.0.0`, `iroh-ping = 1.0.0`, and `iroh-tickets = 1.0.0`. The spike supports:

- `cargo run -- loopback` — receiver and sender in one process.
- `cargo run -- receiver` — print a ticket and wait.
- `cargo run -- sender <ticket>` — dial another process.

## Result

Command:

```bash
. "$HOME/.cargo/env"
cargo run -- loopback
```

Observed:

```text
endpoint_online=true
ticket=endpoint...
endpoint_online=true
accepted connection from d3042a2cc056a6cef5f6b43ae1e5d8ae71687749dd1f8589f260fed307f7e9fc
loopback_ping_rtt_reported_ms=4.140
loopback_wall_time_ms=10.161
```

## Verdict: VALIDATED

### What worked

- Current native Iroh crates compile and run with Rust 1.96.0.
- Endpoint creation works.
- Endpoint tickets provide dial material.
- ALPN protocol routing works through `iroh-ping`.
- Same-machine loopback ping measured ~4 ms reported RTT / ~10 ms wall time.

### What didn't

- System Rust 1.75.0 was too old for current Iroh 1.0.0; current Iroh declares MSRV 1.91.
- Hermes background process logging did not expose long-lived receiver stdout before termination, so a better separate-process harness is needed for cross-process tests.

### Recommendation for the real build

Use current Iroh, require modern Rust via rustup/toolchain file, and build Keyhome's first host/client diagnostic around endpoint tickets + a custom ALPN before touching remote desktop streaming.
