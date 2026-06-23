use anyhow::Result;
use challenge_response::{
    ChallengeResponse,
    config::{Config, Mode, Slot},
};

fn main() -> Result<()> {
    let mut client = ChallengeResponse::new()?;
    let devices = match client.find_all_devices() {
        Ok(devices) => devices,
        Err(err) => {
            println!("device_enumeration_error={err:?}");
            println!("device_count=0");
            println!("no_supported_hmac_device_detected=true");
            return Ok(());
        }
    };
    println!("device_count={}", devices.len());
    for (idx, device) in devices.iter().enumerate() {
        println!(
            "device[{idx}] name={:?} vendor_id=0x{:04x} product_id=0x{:04x} serial={:?} bus={} address={}",
            device.name,
            device.vendor_id,
            device.product_id,
            device.serial,
            device.bus_id,
            device.address_id
        );
    }

    if let Some(device) = devices.into_iter().next() {
        // Non-destructive read path. This assumes a YubiKey already has HMAC-SHA1 challenge-response
        // configured in slot 2. It does NOT write configuration to the token.
        let config = Config::new_from(device)
            .set_variable_size(true)
            .set_mode(Mode::Sha1)
            .set_slot(Slot::Slot2);
        let challenge = b"keyhome viability challenge v1";
        match client.challenge_response_hmac(challenge, config) {
            Ok(hmac) => println!("hmac_sha1_slot2={}", hex::encode(hmac.as_ref())),
            Err(err) => println!("hmac_sha1_slot2_error={err:?}"),
        }
    } else {
        println!("no_supported_hmac_device_detected=true");
    }

    Ok(())
}
