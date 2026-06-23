# 004: Derive Iroh identity from Titan HMAC-Secret

## Question

Can a FIDO2 security key produce stable keying material (via hmac-secret) that can seed a working Iroh node identity?

## Result: PASS

### Observed output

```
HMAC-Secret derived (32 bytes): 8da1f05bed0837d2...
Derived EndpointId: 5dff09ef28cdc7bfc04220645c1a775fef2a7fed3de54d665915c6f59dad1d9a
PING SUCCESSFUL: ping_rtt_reported_ms=6.536
```

## How

1. `make_credential` with `HmacSecret` extension on RPID "keyhome" (PIN + touch)
2. `get_assertion` with `HmacSecret(Some(salt))` where salt = SHA-256("keyhome-iroh-identity-v1") (PIN + touch)
3. Titan returns 32-byte HMAC-SHA-256(CredRandom, salt)
4. `SecretKey::from_bytes(&hmac_output)` → Iroh Ed25519 keypair (exact 32-byte fit)
5. `Endpoint::builder().secret_key(secret_key).bind()` → working Iroh endpoint
6. Loopback ping succeeds (6.5ms RTT)

## Key properties

- Deterministic: same Titan + same PIN + same salt = same EndpointId
- PIN-protected + touch-protected (two-factor derivation)
- No stored secrets — identity derived on-the-fly from the Titan

## Command

```bash
. "$HOME/.cargo/env" && cargo run --quiet -- <PIN>
```
