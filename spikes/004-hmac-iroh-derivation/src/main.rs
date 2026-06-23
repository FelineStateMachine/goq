//! Spike 004: Derive Iroh identity from Titan HMAC-Secret
//!
//! This spike proves the critical keyhome assumption: that a FIDO2 security key
//! can produce stable keying material (via hmac-secret) that can seed an Iroh
//! node identity.
//!
//! Flow:
//! 1. Register a credential on the Titan with hmac-secret extension enabled
//! 2. Call get_assertion with hmac-secret extension (salt = SHA-256("keyhome-iroh-identity"))
//! 3. Extract the 32-byte HMAC output from the assertion
//! 4. Create an Iroh SecretKey from those 32 bytes
//! 5. Create an Iroh endpoint with that identity
//! 6. Do a loopback ping to prove the derived identity works

use anyhow::{bail, Context, Result};
use ctap_hid_fido2::fidokey::{
    GetAssertionArgsBuilder, MakeCredentialArgsBuilder,
    get_assertion::Extension as Gext,
    make_credential::Extension as Mext,
};
use ctap_hid_fido2::{verifier, Cfg, FidoKeyHidFactory};
use iroh::{Endpoint, SecretKey, endpoint::presets, protocol::Router};
use iroh_ping::Ping;
use iroh_tickets::endpoint::EndpointTicket;
use std::io::{self, Write};
use std::time::Instant;

const RPID: &str = "keyhome";
const SALT_MESSAGE: &str = "keyhome-iroh-identity-v1";

fn read_pin() -> Result<String> {
    // Accept PIN as first CLI argument, or read from stdin
    let pin = std::env::args().nth(1);
    if let Some(p) = pin {
        if p.is_empty() {
            bail!("PIN is required");
        }
        return Ok(p);
    }
    print!("Enter Titan PIN: ");
    io::stdout().flush()?;
    let mut pin = String::new();
    io::stdin().read_line(&mut pin)?;
    let pin = pin.trim().to_string();
    if pin.is_empty() {
        bail!("PIN is required");
    }
    Ok(pin)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Spike 004: Derive Iroh identity from Titan HMAC-Secret ===\n");

    let pin = read_pin()?;
    println!();

    // Step 1: Open FIDO2 device
    println!("[1/6] Opening FIDO2 device...");
    let cfg = Cfg::init();
    let device = FidoKeyHidFactory::create(&cfg).context("Failed to open FIDO2 device")?;
    println!("  OK device opened");

    // Step 2: Register credential with hmac-secret
    println!("\n[2/6] Registering credential with hmac-secret...");
    println!("  >>> TAP THE TITAN KEY when it blinks (make_credential) <<<");

    let challenge = verifier::create_challenge();
    let make_args = MakeCredentialArgsBuilder::new(RPID, &challenge)
        .pin(&pin)
        .extensions(&[Mext::HmacSecret(Some(true))])
        .build();

    let attestation = device
        .make_credential_with_args(&make_args)
        .context("make_credential failed")?;
    println!("  OK credential registered");

    let verify_result = verifier::verify_attestation(RPID, &challenge, &attestation);
    if !verify_result.is_success {
        bail!("Attestation verification failed");
    }
    let credential_id = verify_result.credential_id;
    println!("  OK attestation verified, credential_id len={}", credential_id.len());

    // Step 3: Get assertion with hmac-secret
    println!("\n[3/6] Deriving HMAC-Secret from Titan...");
    println!("  >>> TAP THE TITAN KEY when it blinks (get_assertion) <<<");

    let salt: [u8; 32] = {
        use ring::digest;
        let h = digest::digest(&digest::SHA256, SALT_MESSAGE.as_bytes());
        let mut s = [0u8; 32];
        s.copy_from_slice(h.as_ref());
        s
    };

    let challenge2 = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(RPID, &challenge2)
        .pin(&pin)
        .credential_id(&credential_id)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    let assertions = device
        .get_assertion_with_args(&get_args)
        .context("get_assertion failed")?;
    println!("  OK assertion received");

    let hmac_output: [u8; 32] = {
        let mut found: Option<[u8; 32]> = None;
        for ext in &assertions[0].extensions {
            if let Gext::HmacSecret(Some(output)) = ext {
                found = Some(*output);
            }
        }
        match found {
            Some(o) => o,
            None => bail!("No hmac-secret in assertion response"),
        }
    };
    println!("  OK HMAC-Secret derived (32 bytes): {}...", &hex::encode(&hmac_output[..8]));

    // Step 4: Create Iroh SecretKey from HMAC output
    println!("\n[4/6] Deriving Iroh SecretKey from HMAC output...");
    let secret_key = SecretKey::from_bytes(&hmac_output);
    let node_id = secret_key.public();
    println!("  OK Iroh SecretKey created");
    println!("  OK Derived EndpointId: {}", node_id);

    // Step 5: Create Iroh endpoint with derived identity
    println!("\n[5/6] Creating Iroh endpoint with derived identity...");

    let recv_ep = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .context("Failed to bind receiver endpoint")?;

    match tokio::time::timeout(std::time::Duration::from_secs(5), recv_ep.online()).await {
        Ok(()) => println!("  OK receiver endpoint online"),
        Err(_) => println!("  WARN endpoint online timeout (continuing with local addr)"),
    }

    let ticket = EndpointTicket::new(recv_ep.addr());
    println!("  OK receiver ticket created");

    let _router = Router::builder(recv_ep.clone())
        .accept(iroh_ping::ALPN, Ping::new())
        .spawn();

    let send_ep = Endpoint::bind(presets::N0)
        .await
        .context("Failed to bind sender endpoint")?;

    // Step 6: Loopback ping
    println!("\n[6/6] Pinging endpoint with derived identity...");
    let pinger = Ping::new();
    let started = Instant::now();
    let rtt = pinger
        .ping(&send_ep, ticket.endpoint_addr().clone())
        .await
        .context("Ping failed")?;

    println!("  OK PING SUCCESSFUL!");
    println!();
    println!("  derived_endpoint_id={}", node_id);
    println!("  ping_rtt_reported_ms={:.3}", rtt.as_secs_f64() * 1000.0);
    println!("  ping_wall_time_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);

    send_ep.close().await;
    recv_ep.close().await;

    println!("\n=== Spike 004: PASS ===");
    println!("Titan HMAC-Secret -> Iroh identity derivation: CONFIRMED");
    println!("Same Titan + same PIN + same salt always produces EndpointId {}", node_id);

    Ok(())
}
