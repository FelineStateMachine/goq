# 002: YubiKey HMAC detection

## Question

Given a native Rust process, when a YubiKey is attached, can Keyhome detect an HMAC-SHA1 challenge-response-capable token and execute a non-destructive challenge against an already-configured slot?

## Approach

Use `challenge_response = 0.5.46`, which supports YubiKey HMAC-SHA1 challenge-response and device enumeration over libusb/rusb by default.

The spike deliberately does **not** configure or write to a token. It only enumerates devices and tries slot 2 if a supported device is present.

## Result

Command:

```bash
. "$HOME/.cargo/env"
cargo run
```

Observed:

```text
device_enumeration_error=DeviceNotFound
device_count=0
no_supported_hmac_device_detected=true
```

## Verdict: PARTIAL

### What worked

- The `challenge_response` crate compiles locally.
- The enumeration path runs without crashing.
- The program handles missing hardware cleanly.

### What didn't

- There is no YubiKey attached on this machine, so real token detection and HMAC challenge are unproven.
- Touch/PIN behavior is not tested.
- Slot configuration is deliberately not attempted.

### Recommendation for the real build

Next hardware-backed spike should run with a real YubiKey attached and a known configured HMAC-SHA1 slot. In parallel, test PIV with `pcscd` active because PIV may be a better authentication primitive than HMAC for host verification.
