use super::state::{RPID, SALT_MESSAGE};
use anyhow::Context as _;
use ctap_hid_fido2::fidokey::{
    GetAssertionArgsBuilder, MakeCredentialArgsBuilder, get_assertion::Extension as Gext,
    get_assertion::get_assertion_params::Assertion, make_credential::Extension as Mext,
};
use ctap_hid_fido2::public_key_credential_user_entity::PublicKeyCredentialUserEntity;
use ctap_hid_fido2::{Cfg, FidoKeyHidFactory, verifier};
use iroh::SecretKey;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

const PORTAL_CLIENT_IDENTITY_DOMAIN: &[u8] = b"goq/portal-client-identity/v1\0";

// ─── FIDO2 HMAC-Secret Derivation ────────────────────────────────────────────

pub fn derive_secret_from_key(pin: &str) -> anyhow::Result<[u8; 32]> {
    let cfg = Cfg::init();
    let device = FidoKeyHidFactory::create(&cfg)
        .context("Security key not found. Make sure it is plugged in.")?;

    let salt: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(SALT_MESSAGE.as_bytes());
        let result = hasher.finalize();
        let mut s = [0u8; 32];
        s.copy_from_slice(&result);
        s
    };

    // Try resident key first
    let challenge = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(RPID, &challenge)
        .pin(pin)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    if let Ok(assertions) = device.get_assertion_with_args(&get_args) {
        return extract_hmac_secret(&assertions);
    }

    // No resident key — create one
    let user_entity =
        PublicKeyCredentialUserEntity::new(Some(b"sigil-user"), Some("sigil"), Some("Sigil"));

    let challenge = verifier::create_challenge();
    let make_args = MakeCredentialArgsBuilder::new(RPID, &challenge)
        .pin(pin)
        .user_entity(&user_entity)
        .resident_key()
        .extensions(&[Mext::HmacSecret(Some(true))])
        .build();

    let attestation = device
        .make_credential_with_args(&make_args)
        .context("make_credential failed")?;

    let verify_result = verifier::verify_attestation(RPID, &challenge, &attestation);
    if !verify_result.is_success {
        anyhow::bail!("Attestation verification failed");
    }
    let credential_id = verify_result.credential_id;

    let challenge2 = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(RPID, &challenge2)
        .pin(pin)
        .credential_id(&credential_id)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    let assertions = device
        .get_assertion_with_args(&get_args)
        .context("get_assertion failed")?;

    extract_hmac_secret(&assertions)
}

pub fn extract_hmac_secret(assertions: &[Assertion]) -> anyhow::Result<[u8; 32]> {
    let assertion = assertions.first().context(
        "Security key returned no assertions. Reconnect the key and try the PIN and tap again.",
    )?;

    for ext in &assertion.extensions {
        if let Gext::HmacSecret(Some(output)) = ext {
            return validate_hmac_secret_output(output);
        }
    }
    anyhow::bail!(
        "Security key assertion did not contain hmac-secret output. Use a compatible FIDO2 key and try again."
    )
}

fn validate_hmac_secret_output(output: &[u8]) -> anyhow::Result<[u8; 32]> {
    output.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Security key returned malformed hmac-secret output: expected 32 bytes, received {}. Reconnect the key or try a compatible FIDO2 key.",
            output.len()
        )
    })
}

pub fn derive_iroh_secret_from_key(pin: &str) -> anyhow::Result<SecretKey> {
    let hmac_secret = derive_secret_from_key(pin)?;
    Ok(portal_client_secret_from_hmac(hmac_secret))
}

/// Derive Portal's stable Iroh peer identity from the FIDO hmac-secret without
/// reusing the passkey output as another product identity directly. The same
/// tap still supplies both onboarding and ordinary connection attempts.
fn portal_client_secret_from_hmac(hmac_secret: [u8; 32]) -> SecretKey {
    let mut hasher = Sha256::new();
    hasher.update(PORTAL_CLIENT_IDENTITY_DOMAIN);
    hasher.update(hmac_secret);
    let digest: [u8; 32] = hasher.finalize().into();
    SecretKey::from_bytes(&digest)
}

// ─── FIDO2 Tauri Commands ─────────────────────────────────────────────────────

#[derive(Default, Serialize)]
pub struct FidoDeviceInfo {
    pub found: bool,
    pub vid: u16,
    pub pid: u16,
    pub product: String,
    pub versions: Vec<String>,
    pub extensions: Vec<String>,
    pub options: Vec<(String, bool)>,
    pub max_msg_size: u32,
    pub pin_retries: u32,
    pub error: Option<String>,
}

#[tauri::command]
pub fn fido_device_info() -> FidoDeviceInfo {
    let devices = ctap_hid_fido2::get_fidokey_devices();
    if devices.is_empty() {
        return FidoDeviceInfo {
            found: false,
            ..Default::default()
        };
    }

    let dev = &devices[0];
    let vid = dev.vid;
    let pid = dev.pid;
    // product_string is the human-readable name from the HID descriptor
    let product = if dev.product_string.is_empty() {
        dev.info.clone()
    } else {
        dev.product_string.clone()
    };

    // Try to open the device and query CTAP info; degrade gracefully if it fails
    // (device may be busy or need user presence for some operations)
    let cfg = Cfg::init();
    match FidoKeyHidFactory::create(&cfg) {
        Ok(device) => {
            let (versions, extensions, options, max_msg_size) = match device.get_info() {
                Ok(i) => (
                    i.versions.clone(),
                    i.extensions.clone(),
                    i.options.clone(),
                    i.max_msg_size as u32,
                ),
                Err(_) => (vec![], vec![], vec![], 0),
            };
            let pin_retries = device.get_pin_retries().unwrap_or(0);
            FidoDeviceInfo {
                found: true,
                vid,
                pid,
                product,
                versions,
                extensions,
                options,
                max_msg_size,
                pin_retries: pin_retries as u32,
                error: None,
            }
        }
        Err(e) => {
            // Device was enumerated but couldn't be opened — still report it as found
            FidoDeviceInfo {
                found: true,
                vid,
                pid,
                product,
                error: Some(format!("{:?}", e)),
                ..Default::default()
            }
        }
    }
}

#[derive(Serialize)]
pub struct PinRetries {
    pub retries: u32,
    pub error: Option<String>,
}

#[tauri::command]
pub fn fido_pin_retries() -> PinRetries {
    let cfg = Cfg::init();
    match FidoKeyHidFactory::create(&cfg) {
        Ok(device) => match device.get_pin_retries() {
            Ok(n) => PinRetries {
                retries: n as u32,
                error: None,
            },
            Err(e) => PinRetries {
                retries: 0,
                error: Some(format!("{:?}", e)),
            },
        },
        Err(e) => PinRetries {
            retries: 0,
            error: Some(format!("Device not found: {:?}", e)),
        },
    }
}

#[derive(Serialize)]
pub struct KeyIdentity {
    pub node_id: String,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn key_derive_identity(pin: String) -> KeyIdentity {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || derive_iroh_secret_from_key(&pin)),
    )
    .await;
    match result {
        Err(_) => KeyIdentity {
            node_id: String::new(),
            error: Some("Security key timed out (30s). Check that your key is connected.".into()),
        },
        Ok(Err(e)) => KeyIdentity {
            node_id: String::new(),
            error: Some(format!("Task error: {}", e)),
        },
        Ok(Ok(Err(e))) => KeyIdentity {
            node_id: String::new(),
            error: Some(format!("{:?}", e)),
        },
        Ok(Ok(Ok(secret))) => KeyIdentity {
            node_id: secret.public().to_string(),
            error: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_hmac_secret_rejects_empty_assertion_list() {
        let error = extract_hmac_secret(&[]).expect_err("empty assertions must fail closed");
        assert_eq!(
            error.to_string(),
            "Security key returned no assertions. Reconnect the key and try the PIN and tap again."
        );
    }

    #[test]
    fn validate_hmac_secret_output_rejects_non_32_byte_values() {
        for output in [&[0x2a; 31][..], &[0x2a; 33][..]] {
            let error =
                validate_hmac_secret_output(output).expect_err("invalid length must fail closed");
            assert!(
                error
                    .to_string()
                    .contains(&format!("expected 32 bytes, received {}", output.len()))
            );
        }
    }

    #[test]
    fn extract_hmac_secret_returns_exact_output() {
        let expected = [0x2a; 32];
        let assertion = Assertion {
            extensions: vec![Gext::HmacSecret(Some(expected))],
            ..Default::default()
        };

        assert_eq!(extract_hmac_secret(&[assertion]).unwrap(), expected);
    }

    #[test]
    fn portal_identity_derivation_is_stable_and_domain_separated() {
        let root = [0x2a; 32];
        let first = portal_client_secret_from_hmac(root);
        let second = portal_client_secret_from_hmac(root);
        assert_eq!(first.to_bytes(), second.to_bytes());
        assert_ne!(first.to_bytes(), root);
        assert_ne!(
            first.to_bytes(),
            portal_client_secret_from_hmac([0x2b; 32]).to_bytes()
        );
    }
}
