# Evidence: native Iroh ping spike

## Spike

`spikes/001-iroh-native-ping`

## Question

Can a native Rust process create Iroh endpoints, produce peer dial material, and establish an encrypted diagnostic protocol session?

## Command

```bash
cd /home/tank/repos/keyhome/spikes/001-iroh-native-ping
. "$HOME/.cargo/env"
cargo run -- loopback
```

## Observed output

```text
endpoint_online=true
ticket=endpoint...
endpoint_online=true
accepted connection from d3042a2cc056a6cef5f6b43ae1e5d8ae71687749dd1f8589f260fed307f7e9fc
loopback_ping_rtt_reported_ms=4.140
loopback_wall_time_ms=10.161
```

## Result

**VALIDATED for local native Iroh diagnostic connectivity.**

The spike used current crates:

- `iroh = 1.0.0`
- `iroh-ping = 1.0.0`
- `iroh-tickets = 1.0.0`

It proved endpoint creation, endpoint ticket generation, protocol routing through ALPN, authenticated encrypted connection setup by Iroh endpoint identity, and diagnostic RTT measurement in a same-machine loopback.

## Caveats

- Current Iroh requires Rust 1.91+. The system Rust was 1.75.0, so local rustup stable 1.96.0 was installed and used.
- Cross-machine/NAT traversal is not proven yet.
- Running `receiver` as a long-lived background Hermes process did not surface stdout before termination, so separate-process sender/receiver needs a different harness or foreground terminal split.
