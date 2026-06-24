use anyhow::Context as _;
use ctap_hid_fido2::fidokey::{
    GetAssertionArgsBuilder, MakeCredentialArgsBuilder,
    get_assertion::Extension as Gext,
    get_assertion::get_assertion_params::Assertion,
    make_credential::Extension as Mext,
};
use ctap_hid_fido2::public_key_credential_user_entity::PublicKeyCredentialUserEntity;
use ctap_hid_fido2::{verifier, Cfg, FidoKeyHidFactory};
use iroh::SecretKey;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use super::state::{KEYRING_ENTRY, KEYRING_SERVICE, RPID, SALT_MESSAGE};

// ─── Keyring Persistence ─────────────────────────────────────────────────────

pub fn store_identity_in_keyring(secret: &[u8; 32]) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    entry
        .set_secret(secret)
        .context("Failed to store identity in keyring")
}

pub fn load_identity_from_keyring() -> anyhow::Result<Option<[u8; 32]>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    match entry.get_secret() {
        Ok(bytes) => {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Ok(Some(arr))
            } else {
                anyhow::bail!("Keyring entry has wrong length: {}", bytes.len())
            }
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => anyhow::bail!("Failed to read keyring: {:?}", e),
    }
}

pub fn clear_identity_from_keyring() -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => anyhow::bail!("Failed to clear keyring: {:?}", e),
    }
}

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

    match device.get_assertion_with_args(&get_args) {
        Ok(assertions) => return extract_hmac_secret(&assertions),
        Err(_) => {}
    }

    // No resident key — create one
    let user_entity = PublicKeyCredentialUserEntity::new(
        Some(b"sigil-user"),
        Some("sigil"),
        Some("Sigil"),
    );

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
    let secret_bytes = derive_secret_from_key(pin)?;
    Ok(SecretKey::from_bytes(&secret_bytes))
}

// ─── FIDO2 Tauri Commands ─────────────────────────────────────────────────────

#[derive(Serialize)]
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

impl Default for FidoDeviceInfo {
    fn default() -> Self {
        Self {
            found: false,
            vid: 0,
            pid: 0,
            product: String::new(),
            versions: vec![],
            extensions: vec![],
            options: vec![],
            max_msg_size: 0,
            pin_retries: 0,
            error: None,
        }
    }
}

#[tauri::command]
pub fn fido_device_info() -> FidoDeviceInfo {
    let devices = ctap_hid_fido2::get_fidokey_devices();
    if devices.is_empty() {
        return FidoDeviceInfo { found: false, ..Default::default() };
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
                Ok(i) => (i.versions.clone(), i.extensions.clone(), i.options.clone(), i.max_msg_size as u32),
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
            Ok(n) => PinRetries { retries: n as u32, error: None },
            Err(e) => PinRetries { retries: 0, error: Some(format!("{:?}", e)) },
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
        Ok(Err(e)) => KeyIdentity { node_id: String::new(), error: Some(format!("Task error: {}", e)) },
        Ok(Ok(Err(e))) => KeyIdentity { node_id: String::new(), error: Some(format!("{:?}", e)) },
        Ok(Ok(Ok(secret))) => KeyIdentity { node_id: secret.public().to_string(), error: None },
    }
}

// ─── Registration Commands ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RegistrationStatus {
    pub registered: bool,
    pub node_id: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub fn host_registration_status() -> RegistrationStatus {
    match load_identity_from_keyring() {
        Ok(Some(bytes)) => {
            let secret = SecretKey::from_bytes(&bytes);
            RegistrationStatus {
                registered: true,
                node_id: Some(secret.public().to_string()),
                error: None,
            }
        }
        Ok(None) => RegistrationStatus { registered: false, node_id: None, error: None },
        Err(e) => RegistrationStatus {
            registered: false,
            node_id: None,
            error: Some(format!("{:?}", e)),
        },
    }
}

#[tauri::command]
pub async fn key_register_host(pin: String) -> Result<RegistrationStatus, String> {
    let secret = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || derive_secret_from_key(&pin)),
    )
    .await
    .map_err(|_| "Security key timed out (30s). Make sure your key is connected.".to_string())?
    .map_err(|e| format!("Task failed: {}", e))?
    .map_err(|e| format!("FIDO2 error: {:?}", e))?;

    let node_id = SecretKey::from_bytes(&secret).public().to_string();
    store_identity_in_keyring(&secret).map_err(|e| format!("Keyring store failed: {:?}", e))?;

    Ok(RegistrationStatus { registered: true, node_id: Some(node_id), error: None })
}

#[tauri::command]
pub fn host_unregister() -> Result<bool, String> {
    clear_identity_from_keyring().map_err(|e| format!("Keyring clear failed: {:?}", e))?;
    Ok(true)
}
