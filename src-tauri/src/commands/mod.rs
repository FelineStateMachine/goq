#![allow(unused_imports)]

pub mod auth;
pub mod input;
pub mod network;
pub mod state;
pub mod streaming;

pub use auth::{
    fido_device_info, fido_pin_retries, host_registration_status, host_unregister,
    key_derive_identity, key_register_host, FidoDeviceInfo, KeyIdentity, PinRetries,
    RegistrationStatus,
};
pub use input::InputEvent;
pub use network::{
    iroh_client_connect, iroh_client_disconnect, iroh_client_send_input, iroh_host_start,
    iroh_host_stop, iroh_host_status, ConnectResult, FramePayload, HostStatus,
};
pub use state::{
    is_daemon_mode, is_webcodecs_available, set_webcodecs_available, AppState, EncoderConfig,
    HostState,
};
pub use streaming::{detect_available_encoders, get_encoder_config, set_encoder_config};
