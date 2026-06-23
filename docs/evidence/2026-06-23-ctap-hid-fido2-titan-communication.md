# Evidence: FIDO2 CTAP via ctap-hid-fido2 — Titan Key Communication

## Spike

`spikes/003-fido2-hid-enumeration`

## Date

2026-06-23

## Question

Can a native Rust process communicate with a Google Titan Security Key v2 for FIDO2 CTAP operations (device info, PIN status, credential management)?

## Approach

Used the [`ctap-hid-fido2`](https://github.com/gebogebogebo/ctap-hid-fido2) crate (v3.5.11) — a full CTAP 2.0/2.1 client library. This crate uses `hidapi` (v2.6.6) under the hood, which links against `libusb-1.0` and `libudev`.

### Prerequisites installed

```bash
sudo apt install -y libusb-1.0-0-dev libudev-dev pkg-config
```

### Cargo.toml

```toml
[dependencies]
ctap-hid-fido2 = "3.5"
```

### Spike code

`spikes/003-fido2-hid-enumeration/src/main.rs` — enumerates FIDO HID devices, creates a `FidoKeyHid`, calls `get_info()` and `get_pin_retries()`.

## Observed output

```
=== Spike 003: FIDO2 CTAP via ctap-hid-fido2 ===

Scanning for FIDO HID devices (usage page 0xf1d0)...
Found 1 FIDO device(s):

  vid=0x18d1 pid=0x9470 info="product=Titan Security Key v2 usage_page=61904 usage=1 serial_number=2 path="/dev/hidraw3""

Creating FidoKeyHid via factory...
FidoKeyHid created OK.

Calling get_info()...
  ✓ get_info() succeeded!
  Info { versions: ["FIDO_2_0", "U2F_V2"], extensions: ["credProtect", "hmac-secret"], aaguid: [66, 180, 251, 74, 40, 102, 67, 178, 155, 247, 108, 102, 105, 194, 229, 211], options: [("rk", true), ("clientPin", true)], max_msg_size: 2200, pin_uv_auth_protocols: [1], max_credential_count_in_list: 0, max_credential_id_length: 0, transports: [], algorithms: [], max_serialized_large_blob_array: 0, force_pin_change: false, min_pin_length: 0, firmware_version: 0, max_cred_blob_length: 0, max_rpids_for_set_min_pin_length: 0, preferred_platform_uv_attempts: 0, uv_modality: 0, remaining_discoverable_credentials: 0, attestation_formats: [] }

Calling get_pin_retries()...
  ✓ PIN retries: 8

Done.
```

## Result: PASS — full CTAP2 communication established

### Key findings

| Property | Value |
|---|---|
| Device | Google Titan Security Key v2 |
| VID:PID | 18d1:9470 |
| HID path | /dev/hidraw3 |
| CTAP versions | FIDO_2_0, U2F_V2 |
| Extensions | **credProtect**, **hmac-secret** |
| Options | resident keys (rk=true), clientPin=true |
| PIN retries | 8 |
| Max message size | 2200 bytes |
| PIN UV auth protocols | [1] (protocol 1 = hmac-secret) |

### Significance for keyhome

1. **`hmac-secret` extension available** — this is the mechanism for deriving stable keying material from the security key. A challenge is sent to the key, and it returns an HMAC-SHA-256 of the challenge using a secret stored on the key. This can be used to derive Iroh node identities or pairing material.

2. **`clientPin` enabled** — PIN-protected operations work. This means we can require user verification (PIN + touch) before deriving keying material, satisfying the "something you know + something you have" security model.

3. **`credProtect` extension** — credentials can be protected with PIN policy (credProtect level 2 = userVerification required).

4. **No `FIDO_2_1`** — the Titan reports FIDO_2_0 only. Some CTAP 2.1 features (credential management, bio enrollment) won't be available. For keyhome's use case (hmac-secret + PIN), FIDO_2_0 is sufficient.

### Crate assessment

`ctap-hid-fido2` v3.5.11 is a viable dependency for keyhome:
- Clean API: `FidoKeyHidFactory::create()`, `get_info()`, `make_credential()`, `get_assertion()`, `get_hmac_secret()`
- Handles CTAPHID framing, channel allocation, and response parsing
- Uses `hidapi` which works over `/dev/hidraw` on Linux (no kernel driver detachment needed)
- Active maintenance, supports multiple FIDO2 key brands

### What was needed

- `libudev-dev` and `libusb-1.0-0-dev` system packages (build-time)
- `plugdev` group membership for `/dev/hidraw` access (runtime)
- No root/sudo needed for device communication — hidapi works over the existing hidraw path

## Next steps

1. **Task 2.2**: Implement a PIN-protected `get_hmac_secret` call — send a challenge, require PIN, receive HMAC output. This proves the "verification path requiring touch or PIN" requirement.
2. **Task 2.3**: Use the HMAC output to derive Iroh pairing material — prove that the key's output can seed an Iroh identity or shared secret.
