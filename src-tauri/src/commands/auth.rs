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

// These values are enrollment compatibility identifiers, not display
// branding. Changing any of them in place would derive a different Portal
// identity or create a second resident credential for existing users. See
// docs/compatibility-identifiers.md before introducing a versioned successor.
const FIDO_RP_ID: &str = "sigil";
const FIDO_HMAC_SALT_MESSAGE: &str = "sigil-iroh-identity-v1";
const FIDO_RESIDENT_USER_ID: &[u8] = b"sigil-user";
const FIDO_RESIDENT_USER_NAME: &str = "sigil";
const FIDO_RESIDENT_USER_DISPLAY_NAME: &str = "Sigil";
const PORTAL_CLIENT_IDENTITY_DOMAIN: &[u8] = b"goq/portal-client-identity/v1\0";

// ─── FIDO2 HMAC-Secret Derivation ────────────────────────────────────────────

fn fido_hmac_salt() -> [u8; 32] {
    Sha256::digest(FIDO_HMAC_SALT_MESSAGE.as_bytes()).into()
}

pub fn derive_secret_from_key(pin: &str) -> anyhow::Result<[u8; 32]> {
    let cfg = Cfg::init();
    let device = FidoKeyHidFactory::create(&cfg)
        .context("Security key not found. Make sure it is plugged in.")?;

    let salt = fido_hmac_salt();

    // Try resident key first
    let challenge = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(FIDO_RP_ID, &challenge)
        .pin(pin)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    if let Ok(assertions) = device.get_assertion_with_args(&get_args) {
        return extract_hmac_secret(&assertions);
    }

    // No resident key — create one
    let user_entity = PublicKeyCredentialUserEntity::new(
        Some(FIDO_RESIDENT_USER_ID),
        Some(FIDO_RESIDENT_USER_NAME),
        Some(FIDO_RESIDENT_USER_DISPLAY_NAME),
    );

    let challenge = verifier::create_challenge();
    let make_args = MakeCredentialArgsBuilder::new(FIDO_RP_ID, &challenge)
        .pin(pin)
        .user_entity(&user_entity)
        .resident_key()
        .extensions(&[Mext::HmacSecret(Some(true))])
        .build();

    let attestation = device
        .make_credential_with_args(&make_args)
        .context("make_credential failed")?;

    let verify_result = verifier::verify_attestation(FIDO_RP_ID, &challenge, &attestation);
    if !verify_result.is_success {
        anyhow::bail!("Attestation verification failed");
    }
    let credential_id = verify_result.credential_id;

    let challenge2 = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(FIDO_RP_ID, &challenge2)
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
    for ext in &assertions[0].extensions {
        if let Gext::HmacSecret(Some(output)) = ext {
            let mut secret = [0u8; 32];
            secret.copy_from_slice(&output[..]);
            return Ok(secret);
        }
    }
    anyhow::bail!("No hmac-secret in assertion response")
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
    fn portal_identity_compatibility_identifiers_are_golden() {
        assert_eq!(FIDO_RP_ID, "sigil");
        assert_eq!(FIDO_HMAC_SALT_MESSAGE, "sigil-iroh-identity-v1");
        assert_eq!(FIDO_RESIDENT_USER_ID, b"sigil-user");
        assert_eq!(FIDO_RESIDENT_USER_NAME, "sigil");
        assert_eq!(FIDO_RESIDENT_USER_DISPLAY_NAME, "Sigil");
        assert_eq!(
            fido_hmac_salt(),
            [
                0x14, 0x7e, 0xc5, 0x1f, 0xc9, 0x5a, 0x4a, 0xdd, 0x3b, 0x73, 0xf9, 0xf3, 0x66, 0xb1,
                0x57, 0xfb, 0x5d, 0xec, 0x70, 0x6a, 0x12, 0x17, 0x31, 0x51, 0x06, 0x27, 0x28, 0xdb,
                0xad, 0x15, 0x84, 0xaa,
            ]
        );
        assert_eq!(
            PORTAL_CLIENT_IDENTITY_DOMAIN,
            b"goq/portal-client-identity/v1\0"
        );

        let root = [0x2a; 32];
        let first = portal_client_secret_from_hmac(root);
        let second = portal_client_secret_from_hmac(root);
        assert_eq!(first.to_bytes(), second.to_bytes());
        assert_eq!(
            first.to_bytes(),
            [
                0x78, 0xcc, 0xce, 0x6e, 0x04, 0x07, 0x0f, 0xff, 0x0a, 0xc5, 0xf7, 0x4c, 0xde, 0x6d,
                0x94, 0x8e, 0xa4, 0x80, 0x42, 0xa6, 0x6f, 0x45, 0xa8, 0xc5, 0x8a, 0x96, 0xeb, 0x45,
                0x9c, 0xa1, 0x0d, 0xd9,
            ]
        );
        assert_eq!(
            first.public().to_string(),
            "0383aa3774fe624d7a3bc9189c64770e077c43db7315dde1df19085538adc136"
        );
        assert_ne!(first.to_bytes(), root);
        assert_ne!(
            first.to_bytes(),
            portal_client_secret_from_hmac([0x2b; 32]).to_bytes()
        );
    }
}
