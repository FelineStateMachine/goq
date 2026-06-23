# Evidence: YubiKey HMAC detection spike

## Spike

`spikes/002-yubikey-hmac-detection`

## Question

Can a native Rust process compile and run a non-destructive YubiKey HMAC-SHA1 detection/challenge path?

## Command

```bash
cd /home/tank/repos/keyhome/spikes/002-yubikey-hmac-detection
. "$HOME/.cargo/env"
cargo run
```

## Observed output

```text
device_enumeration_error=DeviceNotFound
device_count=0
no_supported_hmac_device_detected=true
```

## Result

**PARTIAL.**

What worked:

- `challenge_response = 0.5.46` compiled successfully on local Rust 1.96.0.
- The code exercised the library's enumeration path without writing to a token.
- Absence of a configured/attached supported token is reported cleanly.

What is not proven yet:

- Actual YubiKey detection with hardware attached.
- Touch/PIN behavior.
- HMAC output from an already-configured slot.
- Token slot configuration flow.

## Local blocker

No YubiKey is attached to this machine. `lsusb` showed no Yubico vendor device. `pcscd` is inactive, which will matter for PIV tests.
