use std::fmt;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(any(target_os = "linux", test))]
use std::sync::Mutex;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, bail};
use sigil_protocol::{Capability, InputEvent};

use crate::config::{HostConfig, InputMode};

mod acl;
mod keymap;
#[cfg(target_os = "linux")]
mod linux;

const ACK_ONLY_CAPABILITIES: &[Capability] = &[Capability::InputAck];
const UINPUT_CAPABILITIES: &[Capability] = &[
    Capability::RelativePointer,
    Capability::Keyboard,
    Capability::Gamepad,
    Capability::InputAck,
];

#[cfg(any(target_os = "linux", test))]
// This Linux input identity tuple is an external ABI consumed by Gamescope,
// udev/libinput rules, and hardware evidence. Preserve it exactly; see
// docs/compatibility-identifiers.md.
const UINPUT_BUS_TYPE: u16 = 0x06; // Linux BUS_VIRTUAL
#[cfg(any(target_os = "linux", test))]
const UINPUT_VENDOR_ID: u16 = 0x5347;
#[cfg(any(target_os = "linux", test))]
const UINPUT_DEVICE_VERSION: u16 = 1;
#[cfg(any(target_os = "linux", test))]
const POINTER_PRODUCT_ID: u16 = 1;
#[cfg(any(target_os = "linux", test))]
const GAMEPAD_PRODUCT_ID: u16 = 2;
#[cfg(any(target_os = "linux", test))]
const KEYBOARD_PRODUCT_ID: u16 = 3;
#[cfg(any(target_os = "linux", test))]
const POINTER_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Pointer";
#[cfg(any(target_os = "linux", test))]
const KEYBOARD_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Keyboard";
#[cfg(any(target_os = "linux", test))]
const GAMEPAD_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Gamepad";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputDisposition {
    Probed,
    Disabled,
    Observed,
    #[cfg(target_os = "linux")]
    Injected,
    TextIgnored,
}

#[derive(Clone)]
pub struct InputBackend {
    mode: InputMode,
    #[cfg(target_os = "linux")]
    device: Option<Arc<std::sync::Mutex<linux::UinputDevice>>>,
}

impl fmt::Debug for InputBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputBackend")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl InputBackend {
    /// Construct the backend before the network endpoint is exposed. For
    /// uinput this opens, validates, and registers the virtual device.
    pub fn initialize(config: &HostConfig) -> Result<Self> {
        match config.input_mode {
            InputMode::Disabled | InputMode::Log => Ok(Self {
                mode: config.input_mode.clone(),
                #[cfg(target_os = "linux")]
                device: None,
            }),
            InputMode::Uinput => {
                #[cfg(target_os = "linux")]
                {
                    let settings = config
                        .uinput
                        .as_ref()
                        .context("missing validated uinput configuration")?;
                    let device = linux::UinputDevice::open(settings)?;
                    Ok(Self {
                        mode: InputMode::Uinput,
                        device: Some(Arc::new(std::sync::Mutex::new(device))),
                    })
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = config;
                    bail!("input_mode uinput is supported only on Linux")
                }
            }
        }
    }

    pub fn capabilities(&self) -> &'static [Capability] {
        capabilities_for_mode(&self.mode)
    }

    pub fn apply(&self, event: &InputEvent, negotiated: &[Capability]) -> Result<InputDisposition> {
        ensure_event_was_negotiated(&self.mode, event, negotiated)?;
        if matches!(event, InputEvent::Probe) {
            return Ok(InputDisposition::Probed);
        }
        match self.mode {
            InputMode::Disabled => Ok(InputDisposition::Disabled),
            InputMode::Log => Ok(InputDisposition::Observed),
            InputMode::Uinput => {
                if matches!(event, InputEvent::Text { .. }) {
                    return Ok(InputDisposition::TextIgnored);
                }
                #[cfg(target_os = "linux")]
                {
                    let device = self
                        .device
                        .as_ref()
                        .context("uinput backend was not initialized")?;
                    device
                        .lock()
                        .map_err(|_| anyhow::anyhow!("uinput backend lock poisoned"))?
                        .apply(event)?;
                    Ok(InputDisposition::Injected)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = event;
                    bail!("uinput backend is unavailable on this platform")
                }
            }
        }
    }

    /// Release any transitions held by the ending input session so a dropped
    /// connection cannot leave a key or mouse button pressed.
    pub fn reset_session(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(device) = &self.device {
            reset_through_poisoned_lock(device, linux::UinputDevice::release_all)?;
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
fn reset_through_poisoned_lock<T>(
    device: &Mutex<T>,
    reset: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    let mut device = match device.lock() {
        Ok(device) => device,
        Err(poisoned) => {
            // A panic while applying input poisons this mutex. Teardown must
            // still recover the device solely to release held transitions;
            // ordinary input continues to reject the poisoned lock.
            tracing::warn!("recovering poisoned uinput lock for session reset");
            poisoned.into_inner()
        }
    };
    reset(&mut device)
}

fn ensure_event_was_negotiated(
    mode: &InputMode,
    event: &InputEvent,
    negotiated: &[Capability],
) -> Result<()> {
    if !matches!(mode, InputMode::Uinput) {
        return Ok(());
    }
    let required = match event {
        InputEvent::Probe => Some(Capability::InputAck),
        InputEvent::MouseMove { .. } => Some(Capability::AbsolutePointer),
        InputEvent::MouseMoveRelative { .. }
        | InputEvent::MousePositionSync { .. }
        | InputEvent::MouseClick { .. }
        | InputEvent::MouseDown { .. }
        | InputEvent::MouseUp { .. }
        | InputEvent::MouseScroll { .. } => Some(Capability::RelativePointer),
        InputEvent::KeyDown { .. } | InputEvent::KeyUp { .. } | InputEvent::KeyClick { .. } => {
            Some(Capability::Keyboard)
        }
        InputEvent::Gamepad { .. } => Some(Capability::Gamepad),
        // Text remains an explicit content-free no-op for compatibility with
        // clients that emit it alongside negotiated physical key transitions.
        InputEvent::Text { .. } => None,
    };
    if required.is_some_and(|capability| !negotiated.contains(&capability)) {
        bail!("input event class was not negotiated for this session");
    }
    Ok(())
}

fn capabilities_for_mode(mode: &InputMode) -> &'static [Capability] {
    match mode {
        InputMode::Disabled | InputMode::Log => ACK_ONLY_CAPABILITIES,
        InputMode::Uinput => UINPUT_CAPABILITIES,
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use sigil_protocol::GamepadState;

    use super::*;

    #[test]
    fn virtual_input_topology_has_stable_distinct_identities() {
        assert_eq!(UINPUT_BUS_TYPE, 0x06);
        #[cfg(target_os = "linux")]
        assert_eq!(UINPUT_BUS_TYPE, input_linux::sys::BUS_VIRTUAL);
        assert_eq!(UINPUT_VENDOR_ID, 0x5347);
        assert_eq!(UINPUT_DEVICE_VERSION, 1);
        assert_eq!(POINTER_DEVICE_NAME, b"Sigil Spark Virtual Pointer");
        assert_eq!(KEYBOARD_DEVICE_NAME, b"Sigil Spark Virtual Keyboard");
        assert_eq!(GAMEPAD_DEVICE_NAME, b"Sigil Spark Virtual Gamepad");
        assert_eq!(POINTER_PRODUCT_ID, 1);
        assert_eq!(GAMEPAD_PRODUCT_ID, 2);
        assert_eq!(KEYBOARD_PRODUCT_ID, 3);
        assert_ne!(POINTER_PRODUCT_ID, KEYBOARD_PRODUCT_ID);
        assert_ne!(POINTER_PRODUCT_ID, GAMEPAD_PRODUCT_ID);
        assert_ne!(KEYBOARD_PRODUCT_ID, GAMEPAD_PRODUCT_ID);
    }

    #[test]
    fn session_reset_recovers_a_lock_poisoned_while_applying_input() {
        #[derive(Debug, Default)]
        struct DeviceState {
            released: bool,
        }

        let device = Arc::new(Mutex::new(DeviceState::default()));
        let panic_device = Arc::clone(&device);
        let panic_result = catch_unwind(AssertUnwindSafe(move || {
            let _device = panic_device.lock().unwrap();
            panic!("simulate panic while applying uinput");
        }));
        assert!(panic_result.is_err());
        assert!(device.lock().is_err());

        reset_through_poisoned_lock(device.as_ref(), |device| {
            device.released = true;
            Ok(())
        })
        .unwrap();

        let device = device.lock().unwrap_err().into_inner();
        assert!(device.released);
    }

    #[test]
    fn content_free_probe_requires_ack_and_never_reaches_uinput() {
        assert!(ensure_event_was_negotiated(&InputMode::Uinput, &InputEvent::Probe, &[]).is_err());
        assert!(
            ensure_event_was_negotiated(
                &InputMode::Uinput,
                &InputEvent::Probe,
                &[Capability::InputAck]
            )
            .is_ok()
        );
    }

    #[test]
    fn relative_pointer_events_require_relative_pointer_negotiation() {
        let movement = InputEvent::MouseMoveRelative { dx: 4, dy: -3 };
        let synchronization = InputEvent::MousePositionSync { x: 123, y: 456 };
        let click = InputEvent::MouseClick { b: 1 };
        let legacy = InputEvent::MouseMove { x: 10, y: 20 };
        for event in [&movement, &synchronization, &click] {
            assert!(ensure_event_was_negotiated(&InputMode::Uinput, event, &[]).is_err());
            assert!(
                ensure_event_was_negotiated(
                    &InputMode::Uinput,
                    event,
                    &[Capability::AbsolutePointer]
                )
                .is_err()
            );
            assert!(
                ensure_event_was_negotiated(
                    &InputMode::Uinput,
                    event,
                    &[Capability::RelativePointer]
                )
                .is_ok()
            );
        }
        assert!(
            ensure_event_was_negotiated(
                &InputMode::Uinput,
                &legacy,
                &[Capability::RelativePointer]
            )
            .is_err()
        );
    }

    #[test]
    fn uinput_rejects_unnegotiated_gamepad_snapshots() {
        let event = InputEvent::Gamepad {
            state: GamepadState::default(),
        };
        assert!(ensure_event_was_negotiated(&InputMode::Uinput, &event, &[]).is_err());
        assert!(
            ensure_event_was_negotiated(&InputMode::Uinput, &event, &[Capability::Gamepad]).is_ok()
        );
        assert!(ensure_event_was_negotiated(&InputMode::Log, &event, &[]).is_ok());
    }

    #[test]
    fn advertised_capabilities_follow_operational_mode() {
        let disabled = InputBackend {
            mode: InputMode::Disabled,
            #[cfg(target_os = "linux")]
            device: None,
        };
        assert_eq!(disabled.capabilities(), &[Capability::InputAck]);

        let log = InputBackend {
            mode: InputMode::Log,
            #[cfg(target_os = "linux")]
            device: None,
        };
        assert_eq!(log.capabilities(), &[Capability::InputAck]);
        assert!(!log.capabilities().contains(&Capability::Keyboard));
        assert_eq!(
            capabilities_for_mode(&InputMode::Uinput),
            &[
                Capability::RelativePointer,
                Capability::Keyboard,
                Capability::Gamepad,
                Capability::InputAck,
            ]
        );
        assert!(!capabilities_for_mode(&InputMode::Uinput).contains(&Capability::AbsolutePointer));
    }
}
