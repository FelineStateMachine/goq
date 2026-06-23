//! Spike 003: FIDO2 CTAP via ctap-hid-fido2 crate
//!
//! Uses the ctap-hid-fido2 crate to enumerate and communicate with
//! any FIDO2-compliant HID security key (YubiKey, Titan, SoloKey, etc).
//!
//! This spike:
//! 1. Lists all HID devices matching FIDO usage page (0xf1d0)
//! 2. Opens the first FIDO device
//! 3. Calls get_info() to retrieve authenticator metadata
//! 4. Calls get_pin_retries() to check PIN state

use ctap_hid_fido2::{Cfg, FidoKeyHidFactory};

fn main() {
    println!("=== Spike 003: FIDO2 CTAP via ctap-hid-fido2 ===\n");

    // Step 1: List all FIDO HID devices
    println!("Scanning for FIDO HID devices (usage page 0xf1d0)...");
    let devices = ctap_hid_fido2::get_fidokey_devices();

    if devices.is_empty() {
        println!("No FIDO devices found.");
        return;
    }

    println!("Found {} FIDO device(s):\n", devices.len());
    for dev in &devices {
        println!(
            "  vid=0x{:04x} pid=0x{:04x} info={:?}",
            dev.vid, dev.pid, dev.info
        );
    }

    // Step 2: Create a FidoKeyHid and get device info
    println!("\nCreating FidoKeyHid via factory...");
    let cfg = Cfg::init();
    let fidokey = match FidoKeyHidFactory::create(&cfg) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("FidoKeyHidFactory::create() failed: {:?}", e);
            return;
        }
    };
    println!("FidoKeyHid created OK.");

    // Step 3: Get authenticator info
    println!("\nCalling get_info()...");
    match fidokey.get_info() {
        Ok(info) => {
            println!("  ✓ get_info() succeeded!");
            println!("  {:?}", info);
        }
        Err(e) => {
            eprintln!("  ✗ get_info() failed: {:?}", e);
        }
    }

    // Step 4: Check PIN retries
    println!("\nCalling get_pin_retries()...");
    match fidokey.get_pin_retries() {
        Ok(retries) => {
            println!("  ✓ PIN retries: {}", retries);
        }
        Err(e) => {
            eprintln!("  ✗ get_pin_retries() failed: {:?}", e);
        }
    }

    println!("\nDone.");
}