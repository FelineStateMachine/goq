# Evidence: FIDO2 HID via raw hidraw

## Spike

`spikes/003-fido2-hid-enumeration`

## Question

Can a native Rust process communicate with a Google Titan Security Key v2 (18d1:9470) via raw hidraw or rusb for FIDO2 CTAP protocol operations?

## Approach

Tried three approaches:

1. **Raw hidraw syscalls** — direct std::io read/write to `/dev/hidraw3` (confirmed Titan path)
2. **rusb (libusb)** — enumerate and claim USB interface directly
3. **hidapi/hidapi-sys** — libusb-based HID wrapper

## Observed output

### Raw hidraw (non-blocking test)

```
write returned 64
no data read within 2s
```

CTAPHID_INIT packet sent, no response received.

### rusb attempt

```
titan_vid=0x18d1 pid=0x9470
bus=5 addr=2
Error: Access
```

rusb can enumerate the Titan but `device.open()` + `claim_interface()` fails with "Access" — kernel has claimed the device as hidraw.

### hidapi / fido-hid-rs / authenticator

All require `libudev-dev` system package which is not installable in this environment (no sudo).

## Result: PARTIAL

### What works

- rusb successfully enumerates the Titan (18d1:9470) via libusb without libudev.
- Raw hidraw write() succeeds (64 bytes written to /dev/hidraw3).
- USB descriptors confirm: HID class, IN endpoint 0x81, OUT endpoint 0x02, 64-byte interrupt packets.

### What's blocked

- **rusb claim_interface()**: kernel has claimed the USB interface as hidraw; unbind requires root.
- **hidapi / fido-hid-rs / authenticator**: all require `libudev-dev` pkg-config package.
- **libnfc**: not installed.
- **Raw hidraw read**: no response to CTAPHID_INIT within 2s; device may need report ID framing or user presence tap.

### Core constraint

On this Linux environment, FIDO2 HID device access from userspace Rust requires one of:

1. Root permissions to unbind kernel driver or claim USB interface directly.
2. `libudev-dev` installed to use udev-based device enumeration/claiming via crates.
3. A FIDO2 capable device already unbound from kernel hidraw.
4. libnfc available for NFC-attached FIDO2 access.

None of these are currently satisfied.

## Verdict: PARTIAL — hardware/gated

### What this means for keyhome

- FIDO2 HID native Rust path is **viable in principle** but requires either proper libudev or root.
- The Titan key itself is confirmed as a FIDO2 HID device with standard endpoints.
- User presence (touch) may be required to activate the CTAP protocol — "tap the key" UX.
- The `challenge_response` crate is YubiKey-only (vendor 0x1050); Google Titan (vendor 0x18d1) is not supported by it.

### Next steps for hardware testing

1. Install libudev-dev (sudo apt install libudev-dev) and retest fido-hid-rs or authenticator.
2. Run with sudo to test rusb claim_interface path directly.
3. Try touching the Titan while running the hidraw write test — user presence may be the missing signal.
4. Consider whether the host-side keyhome agent could expose a local FIDO2 relay that the Tauri app talks to over localhost instead of directly claiming the USB device.