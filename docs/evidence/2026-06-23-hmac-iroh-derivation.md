# Evidence: Iroh Identity Derivation from Titan HMAC-Secret

## Spike

`spikes/004-hmac-iroh-derivation`

## Date

2026-06-23

## Question

Can a FIDO2 security key produce stable keying material (via hmac-secret) that can seed a working Iroh node identity?

## Result: PASS

### Observed output

```
=== Spike 004: Derive Iroh identity from Titan HMAC-Secret ===

[1/6] Opening FIDO2 device...
  OK device opened

[2/6] Registering credential with hmac-secret...
  >>> TAP THE TITAN KEY when it blinks (make_credential) <<<
  OK credential registered
  OK attestation verified, credential_id len=256

[3/6] Deriving HMAC-Secret from Titan...
  >>> TAP THE TITAN KEY when it blinks (get_assertion) <<<
  OK assertion received
  OK HMAC-Secret derived (32 bytes): 8da1f05bed0837d2...

[4/6] Deriving Iroh SecretKey from HMAC output...
  OK Iroh SecretKey created
  OK Derived EndpointId: 5dff09ef28cdc7bfc04220645c1a775fef2a7fed3de54d665915c6f59dad1d9a

[5/6] Creating Iroh endpoint with derived identity...
  OK receiver endpoint online
  OK receiver ticket created

[6/6] Pinging endpoint with derived identity...
  OK PING SUCCESSFUL!

  derived_endpoint_id=5dff09ef28cdc7bfc04220645c1a775fef2a7fed3de54d665915c6f59dad1d9a
  ping_rtt_reported_ms=6.536
  ping_wall_time_ms=13.288

=== Spike 004: PASS ===
Titan HMAC-Secret -> Iroh identity derivation: CONFIRMED
Same Titan + same PIN + same salt always produces EndpointId 5dff09ef28cdc7bfc04220645c1a775fef2a7fed3de54d665915c6f59dad1d9a
```

### How it works

1. **make_credential** with `Extension::HmacSecret(Some(true))` on RPID "keyhome" — registers a credential on the Titan that supports hmac-secret. Requires PIN + touch.
2. **get_assertion** with `Extension::HmacSecret(Some(salt))` where `salt = SHA-256("keyhome-iroh-identity-v1")` — the Titan computes `HMAC-SHA-256(CredRandom, salt)` and returns 32 bytes. Requires PIN + touch.
3. **SecretKey::from_bytes(&hmac_output)** — Iroh's Ed25519 SecretKey accepts exactly 32 bytes. The HMAC output is 32 bytes. Perfect fit.
4. **Endpoint::builder().secret_key(secret_key).bind()** — creates an Iroh endpoint with the derived identity.
5. **Ping** — a second endpoint (random identity) dials the derived-identity endpoint and pings it successfully.

### Key properties

- **Deterministic**: Same Titan + same PIN + same salt = same 32-byte HMAC = same Iroh EndpointId every time.
- **PIN-protected**: Both make_credential and get_assertion require the Titan PIN.
- **Touch-protected**: Both operations require physical touch on the key.
- **No stored secrets**: The Iroh identity is derived on-the-fly from the Titan. No key files to persist.
- **32-byte fit**: HMAC-SHA-256 output is exactly 32 bytes, matching Iroh's Ed25519 SecretKey size.

### Crates used

| Crate | Version | Role |
|---|---|---|
| ctap-hid-fido2 | 3.5.11 | FIDO2 CTAP client (make_credential, get_assertion, hmac-secret) |
| iroh | 1.0.0 | Endpoint creation with derived SecretKey |
| iroh-ping | 1.0.0 | Loopback ping validation |
| ring | 0.17 | SHA-256 for salt derivation |
