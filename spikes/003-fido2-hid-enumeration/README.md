# 003: FIDO2 CTAP via ctap-hid-fido2

## Question

Can a native Rust process communicate with a Google Titan Security Key v2 for FIDO2 CTAP operations?

## Result: PASS

### Observed output

```
Found 1 FIDO device(s):
  vid=0x18d1 pid=0x9470 info="product=Titan Security Key v2 usage_page=61904 usage=1 serial_number=2 path="/dev/hidraw3""

get_info() succeeded:
  versions: ["FIDO_2_0", "U2F_V2"]
  extensions: ["credProtect", "hmac-secret"]
  options: [("rk", true), ("clientPin", true)]
  max_msg_size: 2200

get_pin_retries() succeeded: 8
```

## How

Used [`ctap-hid-fido2`](https://github.com/gebogebogebo/ctap-hid-fido2) v3.5.11 crate, which uses `hidapi` over `/dev/hidraw3`.

Prerequisites: `sudo apt install -y libusb-1.0-0-dev libudev-dev pkg-config`

### Key capabilities confirmed

- **hmac-secret** extension available — can derive stable keying material from the Titan
- **clientPin** enabled — PIN-protected operations work
- **credProtect** available — credentials can have PIN policy
- FIDO_2_0 only (no CTAP 2.1), but sufficient for keyhome's hmac-secret + PIN use case

## Command

```bash
. "$HOME/.cargo/env" && cargo run --quiet
```
