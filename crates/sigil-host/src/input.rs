use std::fmt;
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, bail};
#[cfg(any(target_os = "linux", test))]
use sigil_protocol::GamepadState;
use sigil_protocol::{Capability, InputEvent};

use crate::config::{HostConfig, InputMode};

const ACK_ONLY_CAPABILITIES: &[Capability] = &[Capability::InputAck];
const UINPUT_CAPABILITIES: &[Capability] = &[
    Capability::RelativePointer,
    Capability::Keyboard,
    Capability::Gamepad,
    Capability::InputAck,
];

#[cfg(any(target_os = "linux", test))]
const UINPUT_VENDOR_ID: u16 = 0x5347;
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

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputDeviceClass {
    Pointer,
    Keyboard,
    Gamepad,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerReportEvent {
    RelativeX(i32),
    RelativeY(i32),
    Synchronize,
}

#[cfg(any(target_os = "linux", test))]
fn pointer_position_sync_report(x: i32, y: i32) -> [PointerReportEvent; 6] {
    [
        PointerReportEvent::RelativeX(sigil_protocol::RELATIVE_POINTER_DELTA_MIN),
        PointerReportEvent::RelativeY(sigil_protocol::RELATIVE_POINTER_DELTA_MIN),
        PointerReportEvent::Synchronize,
        PointerReportEvent::RelativeX(x),
        PointerReportEvent::RelativeY(y),
        PointerReportEvent::Synchronize,
    ]
}

#[cfg(any(target_os = "linux", test))]
fn input_device_class(event: &InputEvent) -> Option<InputDeviceClass> {
    match event {
        InputEvent::Probe => None,
        InputEvent::MouseMove { .. }
        | InputEvent::MouseMoveRelative { .. }
        | InputEvent::MousePositionSync { .. }
        | InputEvent::MouseClick { .. }
        | InputEvent::MouseDown { .. }
        | InputEvent::MouseUp { .. }
        | InputEvent::MouseScroll { .. } => Some(InputDeviceClass::Pointer),
        InputEvent::KeyDown { .. } | InputEvent::KeyUp { .. } | InputEvent::KeyClick { .. } => {
            Some(InputDeviceClass::Keyboard)
        }
        InputEvent::Gamepad { .. } => Some(InputDeviceClass::Gamepad),
        InputEvent::Text { .. } => None,
    }
}

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
            device
                .lock()
                .map_err(|_| anyhow::anyhow!("uinput backend lock poisoned"))?
                .release_all()?;
        }
        Ok(())
    }
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

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MappedKey {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    Minus,
    Equal,
    LeftBrace,
    RightBrace,
    Backslash,
    Semicolon,
    Apostrophe,
    Grave,
    Comma,
    Dot,
    Slash,
    Enter,
    Tab,
    Space,
    Backspace,
    Escape,
    LeftShift,
    LeftCtrl,
    LeftAlt,
    LeftMeta,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
}

#[cfg(target_os = "linux")]
const ALL_MAPPED_KEYS: &[MappedKey] = &[
    MappedKey::A,
    MappedKey::B,
    MappedKey::C,
    MappedKey::D,
    MappedKey::E,
    MappedKey::F,
    MappedKey::G,
    MappedKey::H,
    MappedKey::I,
    MappedKey::J,
    MappedKey::K,
    MappedKey::L,
    MappedKey::M,
    MappedKey::N,
    MappedKey::O,
    MappedKey::P,
    MappedKey::Q,
    MappedKey::R,
    MappedKey::S,
    MappedKey::T,
    MappedKey::U,
    MappedKey::V,
    MappedKey::W,
    MappedKey::X,
    MappedKey::Y,
    MappedKey::Z,
    MappedKey::Num0,
    MappedKey::Num1,
    MappedKey::Num2,
    MappedKey::Num3,
    MappedKey::Num4,
    MappedKey::Num5,
    MappedKey::Num6,
    MappedKey::Num7,
    MappedKey::Num8,
    MappedKey::Num9,
    MappedKey::Minus,
    MappedKey::Equal,
    MappedKey::LeftBrace,
    MappedKey::RightBrace,
    MappedKey::Backslash,
    MappedKey::Semicolon,
    MappedKey::Apostrophe,
    MappedKey::Grave,
    MappedKey::Comma,
    MappedKey::Dot,
    MappedKey::Slash,
    MappedKey::Enter,
    MappedKey::Tab,
    MappedKey::Space,
    MappedKey::Backspace,
    MappedKey::Escape,
    MappedKey::LeftShift,
    MappedKey::LeftCtrl,
    MappedKey::LeftAlt,
    MappedKey::LeftMeta,
    MappedKey::Up,
    MappedKey::Down,
    MappedKey::Left,
    MappedKey::Right,
    MappedKey::Home,
    MappedKey::End,
    MappedKey::PageUp,
    MappedKey::PageDown,
    MappedKey::Delete,
];

#[cfg(any(target_os = "linux", test))]
fn map_key(value: &str) -> Option<MappedKey> {
    match value {
        "Enter" => Some(MappedKey::Enter),
        "Tab" => Some(MappedKey::Tab),
        "Space" => Some(MappedKey::Space),
        "Backspace" => Some(MappedKey::Backspace),
        "Escape" => Some(MappedKey::Escape),
        "Shift" => Some(MappedKey::LeftShift),
        "Control" => Some(MappedKey::LeftCtrl),
        "Alt" => Some(MappedKey::LeftAlt),
        "Meta" => Some(MappedKey::LeftMeta),
        "Up" => Some(MappedKey::Up),
        "Down" => Some(MappedKey::Down),
        "Left" => Some(MappedKey::Left),
        "Right" => Some(MappedKey::Right),
        "Home" => Some(MappedKey::Home),
        "End" => Some(MappedKey::End),
        "PageUp" => Some(MappedKey::PageUp),
        "PageDown" => Some(MappedKey::PageDown),
        "Delete" => Some(MappedKey::Delete),
        _ => map_printable_us_key(value),
    }
}

#[cfg(any(target_os = "linux", test))]
fn map_printable_us_key(value: &str) -> Option<MappedKey> {
    let mut chars = value.chars();
    let character = chars.next()?;
    if chars.next().is_some() || !character.is_ascii() {
        return None;
    }
    Some(match character {
        'a' | 'A' => MappedKey::A,
        'b' | 'B' => MappedKey::B,
        'c' | 'C' => MappedKey::C,
        'd' | 'D' => MappedKey::D,
        'e' | 'E' => MappedKey::E,
        'f' | 'F' => MappedKey::F,
        'g' | 'G' => MappedKey::G,
        'h' | 'H' => MappedKey::H,
        'i' | 'I' => MappedKey::I,
        'j' | 'J' => MappedKey::J,
        'k' | 'K' => MappedKey::K,
        'l' | 'L' => MappedKey::L,
        'm' | 'M' => MappedKey::M,
        'n' | 'N' => MappedKey::N,
        'o' | 'O' => MappedKey::O,
        'p' | 'P' => MappedKey::P,
        'q' | 'Q' => MappedKey::Q,
        'r' | 'R' => MappedKey::R,
        's' | 'S' => MappedKey::S,
        't' | 'T' => MappedKey::T,
        'u' | 'U' => MappedKey::U,
        'v' | 'V' => MappedKey::V,
        'w' | 'W' => MappedKey::W,
        'x' | 'X' => MappedKey::X,
        'y' | 'Y' => MappedKey::Y,
        'z' | 'Z' => MappedKey::Z,
        '0' | ')' => MappedKey::Num0,
        '1' | '!' => MappedKey::Num1,
        '2' | '@' => MappedKey::Num2,
        '3' | '#' => MappedKey::Num3,
        '4' | '$' => MappedKey::Num4,
        '5' | '%' => MappedKey::Num5,
        '6' | '^' => MappedKey::Num6,
        '7' | '&' => MappedKey::Num7,
        '8' | '*' => MappedKey::Num8,
        '9' | '(' => MappedKey::Num9,
        '-' | '_' => MappedKey::Minus,
        '=' | '+' => MappedKey::Equal,
        '[' | '{' => MappedKey::LeftBrace,
        ']' | '}' => MappedKey::RightBrace,
        '\\' | '|' => MappedKey::Backslash,
        ';' | ':' => MappedKey::Semicolon,
        '\'' | '"' => MappedKey::Apostrophe,
        '`' | '~' => MappedKey::Grave,
        ',' | '<' => MappedKey::Comma,
        '.' | '>' => MappedKey::Dot,
        '/' | '?' => MappedKey::Slash,
        ' ' => MappedKey::Space,
        _ => return None,
    })
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GamepadButton {
    A,
    B,
    X,
    Y,
    LeftShoulder,
    RightShoulder,
    Back,
    Start,
    Guide,
    LeftStick,
    RightStick,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GamepadAxis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
    DpadX,
    DpadY,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GamepadReport {
    buttons: [(GamepadButton, bool); 11],
    axes: [(GamepadAxis, i32); 8],
}

#[cfg(any(target_os = "linux", test))]
fn map_gamepad_state(state: &GamepadState) -> GamepadReport {
    GamepadReport {
        buttons: [
            (GamepadButton::A, state.a),
            (GamepadButton::B, state.b),
            (GamepadButton::X, state.x),
            (GamepadButton::Y, state.y),
            (GamepadButton::LeftShoulder, state.left_shoulder),
            (GamepadButton::RightShoulder, state.right_shoulder),
            (GamepadButton::Back, state.back),
            (GamepadButton::Start, state.start),
            (GamepadButton::Guide, state.guide),
            (GamepadButton::LeftStick, state.left_stick),
            (GamepadButton::RightStick, state.right_stick),
        ],
        axes: [
            (GamepadAxis::LeftX, i32::from(state.left_x)),
            (GamepadAxis::LeftY, i32::from(state.left_y)),
            (GamepadAxis::RightX, i32::from(state.right_x)),
            (GamepadAxis::RightY, i32::from(state.right_y)),
            (GamepadAxis::LeftTrigger, i32::from(state.left_trigger)),
            (GamepadAxis::RightTrigger, i32::from(state.right_trigger)),
            (
                GamepadAxis::DpadX,
                i32::from(state.dpad_right) - i32::from(state.dpad_left),
            ),
            (
                GamepadAxis::DpadY,
                i32::from(state.dpad_down) - i32::from(state.dpad_up),
            ),
        ],
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeSet;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};

    use anyhow::{Context, Result, ensure};
    use input_linux::{
        AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime,
        InputId, InputProperty, Key, KeyEvent, KeyState, RelativeAxis, RelativeEvent,
        SynchronizeEvent, UInputHandle,
    };
    use sigil_protocol::{
        GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN, GAMEPAD_TRIGGER_MAX, GamepadState,
        InputEvent as ProtocolInputEvent,
    };

    use super::{
        ALL_MAPPED_KEYS, GAMEPAD_DEVICE_NAME, GAMEPAD_PRODUCT_ID, GamepadAxis, GamepadButton,
        InputDeviceClass, KEYBOARD_DEVICE_NAME, KEYBOARD_PRODUCT_ID, MappedKey,
        POINTER_DEVICE_NAME, POINTER_PRODUCT_ID, PointerReportEvent, UINPUT_VENDOR_ID,
        input_device_class, map_gamepad_state, map_key, pointer_position_sync_report,
    };
    use crate::config::UinputConfig;

    const UINPUT_MAJOR: u32 = 10;
    const UINPUT_MINOR: u32 = 223;
    const MAX_POINTER_EVENTS: usize = 6;
    const MAX_KEYBOARD_EVENTS: usize = ALL_MAPPED_KEYS.len() + 1;
    const GAMEPAD_REPORT_EVENTS: usize = 20;

    pub struct UinputDevice {
        pointer: PointerDevice,
        keyboard: KeyboardDevice,
        gamepad: GamepadDevice,
    }

    impl UinputDevice {
        pub fn open(config: &UinputConfig) -> Result<Self> {
            // Each virtual device owns a distinct validated descriptor. This
            // keeps libinput/udev classification from merging keyboard keys
            // into the conventional relative pointer topology.
            let pointer = PointerDevice::open(config)?;
            let keyboard = KeyboardDevice::open(config)?;
            let gamepad = GamepadDevice::open(config)?;

            Ok(Self {
                pointer,
                keyboard,
                gamepad,
            })
        }

        pub fn apply(&mut self, event: &ProtocolInputEvent) -> Result<()> {
            match input_device_class(event) {
                Some(InputDeviceClass::Pointer) => self.pointer.apply(event),
                Some(InputDeviceClass::Keyboard) => self.keyboard.apply(event),
                Some(InputDeviceClass::Gamepad) => match event {
                    ProtocolInputEvent::Gamepad { state } => self.gamepad.apply(state),
                    _ => unreachable!("input device classification is exhaustive"),
                },
                None => Ok(()),
            }
        }

        pub fn release_all(&mut self) -> Result<()> {
            // Evaluate every release before combining results so one broken
            // descriptor cannot strand held state on either sibling device.
            let pointer_result = self.pointer.release_all();
            let keyboard_result = self.keyboard.release_all();
            let gamepad_result = self.gamepad.neutralize();
            pointer_result.and(keyboard_result).and(gamepad_result)
        }
    }

    struct PointerDevice {
        handle: UInputHandle<File>,
        pressed: BTreeSet<Key>,
    }

    impl PointerDevice {
        fn open(config: &UinputConfig) -> Result<Self> {
            let handle = open_validated_handle(config, "pointer")?;
            handle
                .set_propbit(InputProperty::Pointer)
                .context("enabling pointer property")?;
            handle
                .set_evbit(EventKind::Key)
                .context("enabling pointer button events")?;
            for key in [Key::ButtonLeft, Key::ButtonRight, Key::ButtonMiddle] {
                handle
                    .set_keybit(key)
                    .context("enabling a bounded pointer button capability")?;
            }
            handle
                .set_evbit(EventKind::Relative)
                .context("enabling relative pointer events")?;
            handle
                .set_relbit(RelativeAxis::X)
                .context("enabling relative X events")?;
            handle
                .set_relbit(RelativeAxis::Y)
                .context("enabling relative Y events")?;
            handle
                .set_relbit(RelativeAxis::Wheel)
                .context("enabling vertical wheel events")?;
            handle
                .set_relbit(RelativeAxis::HorizontalWheel)
                .context("enabling horizontal wheel events")?;
            handle
                .create(
                    &InputId {
                        bustype: input_linux::sys::BUS_VIRTUAL,
                        vendor: UINPUT_VENDOR_ID,
                        product: POINTER_PRODUCT_ID,
                        version: 1,
                    },
                    POINTER_DEVICE_NAME,
                    0,
                    &[],
                )
                .context("registering Sigil virtual pointer")?;

            Ok(Self {
                handle,
                pressed: BTreeSet::new(),
            })
        }

        fn apply(&mut self, event: &ProtocolInputEvent) -> Result<()> {
            match event {
                ProtocolInputEvent::MouseMoveRelative { dx, dy } => {
                    let mut events = [synchronize_event(); 3];
                    let mut len = 0;
                    if *dx != 0 {
                        events[len] = relative_event(RelativeAxis::X, *dx);
                        len += 1;
                    }
                    if *dy != 0 {
                        events[len] = relative_event(RelativeAxis::Y, *dy);
                        len += 1;
                    }
                    if len == 0 {
                        return Ok(());
                    }
                    events[len] = synchronize_event();
                    self.emit(&events[..=len])
                }
                ProtocolInputEvent::MousePositionSync { x, y } => {
                    let events = pointer_position_sync_report(*x, *y).map(|event| match event {
                        PointerReportEvent::RelativeX(value) => {
                            relative_event(RelativeAxis::X, value)
                        }
                        PointerReportEvent::RelativeY(value) => {
                            relative_event(RelativeAxis::Y, value)
                        }
                        PointerReportEvent::Synchronize => synchronize_event(),
                    });
                    self.emit(&events)
                }
                ProtocolInputEvent::MouseMove { .. } => anyhow::bail!(
                    "absolute pointer movement is unavailable on the relative-only uinput device"
                ),
                ProtocolInputEvent::MouseClick { b } => {
                    self.click(mouse_button(*b).context("unsupported mouse button")?)
                }
                ProtocolInputEvent::MouseDown { b } => {
                    self.press(mouse_button(*b).context("unsupported mouse button")?)
                }
                ProtocolInputEvent::MouseUp { b } => {
                    self.release(mouse_button(*b).context("unsupported mouse button")?)
                }
                ProtocolInputEvent::MouseScroll { dx, dy } => {
                    let vertical = dy
                        .checked_neg()
                        .context("vertical wheel delta is out of range")?;
                    let mut events = [synchronize_event(); 3];
                    let mut len = 0;
                    if *dx != 0 {
                        events[len] = relative_event(RelativeAxis::HorizontalWheel, *dx);
                        len += 1;
                    }
                    if vertical != 0 {
                        events[len] = relative_event(RelativeAxis::Wheel, vertical);
                        len += 1;
                    }
                    if len == 0 {
                        return Ok(());
                    }
                    events[len] = synchronize_event();
                    self.emit(&events[..=len])
                }
                _ => unreachable!("only pointer events are routed to the pointer device"),
            }
        }

        fn press(&mut self, key: Key) -> Result<()> {
            self.emit(&[key_event(key, KeyState::PRESSED), synchronize_event()])?;
            self.pressed.insert(key);
            Ok(())
        }

        fn release(&mut self, key: Key) -> Result<()> {
            self.emit(&[key_event(key, KeyState::RELEASED), synchronize_event()])?;
            self.pressed.remove(&key);
            Ok(())
        }

        fn click(&mut self, key: Key) -> Result<()> {
            ensure!(
                !self.pressed.contains(&key),
                "click transition is invalid while the key or button is held"
            );
            self.emit(&[
                key_event(key, KeyState::PRESSED),
                synchronize_event(),
                key_event(key, KeyState::RELEASED),
                synchronize_event(),
            ])
        }

        fn release_all(&mut self) -> Result<()> {
            if self.pressed.is_empty() {
                return Ok(());
            }
            let mut events = Vec::with_capacity(self.pressed.len() + 1);
            events.extend(
                self.pressed
                    .iter()
                    .copied()
                    .map(|key| key_event(key, KeyState::RELEASED)),
            );
            events.push(synchronize_event());
            ensure!(
                events.len() <= MAX_POINTER_EVENTS,
                "pointer release set exceeded its static bound"
            );
            self.emit(&events)?;
            self.pressed.clear();
            Ok(())
        }

        fn emit(&self, events: &[input_linux::sys::input_event]) -> Result<()> {
            ensure!(
                !events.is_empty() && events.len() <= MAX_POINTER_EVENTS,
                "pointer event batch is outside its static bound"
            );
            let written = self
                .handle
                .write(events)
                .context("writing nonblocking pointer event batch")?;
            ensure!(
                written == events.len(),
                "uinput accepted only {written} of {} pointer events",
                events.len()
            );
            Ok(())
        }
    }

    impl Drop for PointerDevice {
        fn drop(&mut self) {
            let _ = self.release_all();
            let _ = self.handle.dev_destroy();
        }
    }

    struct KeyboardDevice {
        handle: UInputHandle<File>,
        pressed: BTreeSet<Key>,
    }

    impl KeyboardDevice {
        fn open(config: &UinputConfig) -> Result<Self> {
            let handle = open_validated_handle(config, "keyboard")?;
            handle
                .set_evbit(EventKind::Key)
                .context("enabling keyboard key events")?;
            for key in ALL_MAPPED_KEYS.iter().copied().map(linux_key) {
                handle
                    .set_keybit(key)
                    .context("enabling a bounded keyboard key capability")?;
            }
            handle
                .create(
                    &InputId {
                        bustype: input_linux::sys::BUS_VIRTUAL,
                        vendor: UINPUT_VENDOR_ID,
                        product: KEYBOARD_PRODUCT_ID,
                        version: 1,
                    },
                    KEYBOARD_DEVICE_NAME,
                    0,
                    &[],
                )
                .context("registering Sigil virtual keyboard")?;

            Ok(Self {
                handle,
                pressed: BTreeSet::new(),
            })
        }

        fn apply(&mut self, event: &ProtocolInputEvent) -> Result<()> {
            match event {
                ProtocolInputEvent::KeyDown { k } => {
                    self.press(linux_key(map_key(k).context("unsupported keyboard key")?))
                }
                ProtocolInputEvent::KeyUp { k } => {
                    self.release(linux_key(map_key(k).context("unsupported keyboard key")?))
                }
                ProtocolInputEvent::KeyClick { k } => {
                    self.click(linux_key(map_key(k).context("unsupported keyboard key")?))
                }
                _ => unreachable!("only keyboard events are routed to the keyboard device"),
            }
        }

        fn press(&mut self, key: Key) -> Result<()> {
            self.emit(&[key_event(key, KeyState::PRESSED), synchronize_event()])?;
            self.pressed.insert(key);
            Ok(())
        }

        fn release(&mut self, key: Key) -> Result<()> {
            self.emit(&[key_event(key, KeyState::RELEASED), synchronize_event()])?;
            self.pressed.remove(&key);
            Ok(())
        }

        fn click(&mut self, key: Key) -> Result<()> {
            ensure!(
                !self.pressed.contains(&key),
                "click transition is invalid while the key is held"
            );
            self.emit(&[
                key_event(key, KeyState::PRESSED),
                synchronize_event(),
                key_event(key, KeyState::RELEASED),
                synchronize_event(),
            ])
        }

        fn release_all(&mut self) -> Result<()> {
            if self.pressed.is_empty() {
                return Ok(());
            }
            let mut events = Vec::with_capacity(self.pressed.len() + 1);
            events.extend(
                self.pressed
                    .iter()
                    .copied()
                    .map(|key| key_event(key, KeyState::RELEASED)),
            );
            events.push(synchronize_event());
            ensure!(
                events.len() <= MAX_KEYBOARD_EVENTS,
                "keyboard release set exceeded its static bound"
            );
            self.emit(&events)?;
            self.pressed.clear();
            Ok(())
        }

        fn emit(&self, events: &[input_linux::sys::input_event]) -> Result<()> {
            ensure!(
                !events.is_empty() && events.len() <= MAX_KEYBOARD_EVENTS,
                "keyboard event batch is outside its static bound"
            );
            let written = self
                .handle
                .write(events)
                .context("writing nonblocking keyboard event batch")?;
            ensure!(
                written == events.len(),
                "uinput accepted only {written} of {} keyboard events",
                events.len()
            );
            Ok(())
        }
    }

    impl Drop for KeyboardDevice {
        fn drop(&mut self) {
            let _ = self.release_all();
            let _ = self.handle.dev_destroy();
        }
    }

    struct GamepadDevice {
        handle: UInputHandle<File>,
    }

    impl GamepadDevice {
        fn open(config: &UinputConfig) -> Result<Self> {
            let handle = open_validated_handle(config, "gamepad")?;

            handle
                .set_evbit(EventKind::Key)
                .context("enabling gamepad key events")?;
            for (button, _) in map_gamepad_state(&GamepadState::default()).buttons {
                handle
                    .set_keybit(linux_gamepad_button(button))
                    .context("enabling a bounded gamepad button capability")?;
            }
            handle
                .set_evbit(EventKind::Absolute)
                .context("enabling gamepad absolute-axis events")?;
            for (axis, _) in map_gamepad_state(&GamepadState::default()).axes {
                handle
                    .set_absbit(linux_gamepad_axis(axis))
                    .context("enabling a bounded gamepad axis capability")?;
            }

            let axes = [
                absolute_range(
                    AbsoluteAxis::X,
                    i32::from(GAMEPAD_AXIS_MIN),
                    i32::from(GAMEPAD_AXIS_MAX),
                ),
                absolute_range(
                    AbsoluteAxis::Y,
                    i32::from(GAMEPAD_AXIS_MIN),
                    i32::from(GAMEPAD_AXIS_MAX),
                ),
                absolute_range(
                    AbsoluteAxis::RX,
                    i32::from(GAMEPAD_AXIS_MIN),
                    i32::from(GAMEPAD_AXIS_MAX),
                ),
                absolute_range(
                    AbsoluteAxis::RY,
                    i32::from(GAMEPAD_AXIS_MIN),
                    i32::from(GAMEPAD_AXIS_MAX),
                ),
                absolute_range(AbsoluteAxis::Z, 0, i32::from(GAMEPAD_TRIGGER_MAX)),
                absolute_range(AbsoluteAxis::RZ, 0, i32::from(GAMEPAD_TRIGGER_MAX)),
                absolute_range(AbsoluteAxis::Hat0X, -1, 1),
                absolute_range(AbsoluteAxis::Hat0Y, -1, 1),
            ];
            handle
                .create(
                    &InputId {
                        bustype: input_linux::sys::BUS_VIRTUAL,
                        vendor: UINPUT_VENDOR_ID,
                        product: GAMEPAD_PRODUCT_ID,
                        version: 1,
                    },
                    GAMEPAD_DEVICE_NAME,
                    0,
                    &axes,
                )
                .context("registering Sigil virtual gamepad")?;
            let device = Self { handle };
            device.neutralize()?;
            Ok(device)
        }

        fn apply(&self, state: &GamepadState) -> Result<()> {
            state.validate().context("validating gamepad snapshot")?;
            let report = map_gamepad_state(state);
            let mut events = [synchronize_event(); GAMEPAD_REPORT_EVENTS];
            let mut index = 0;
            for (button, pressed) in report.buttons {
                events[index] = key_event(linux_gamepad_button(button), KeyState::pressed(pressed));
                index += 1;
            }
            for (axis, value) in report.axes {
                events[index] = absolute_event(linux_gamepad_axis(axis), value);
                index += 1;
            }
            events[index] = synchronize_event();
            index += 1;
            ensure!(
                index == GAMEPAD_REPORT_EVENTS,
                "gamepad report violated its static event bound"
            );
            let written = self
                .handle
                .write(&events)
                .context("writing nonblocking gamepad snapshot")?;
            ensure!(
                written == events.len(),
                "uinput accepted only {written} of {} gamepad events",
                events.len()
            );
            Ok(())
        }

        fn neutralize(&self) -> Result<()> {
            self.apply(&GamepadState::default())
                .context("neutralizing virtual gamepad")
        }
    }

    impl Drop for GamepadDevice {
        fn drop(&mut self) {
            let _ = self.neutralize();
            let _ = self.handle.dev_destroy();
        }
    }

    fn open_validated_handle(
        config: &UinputConfig,
        device_class: &str,
    ) -> Result<UInputHandle<File>> {
        let handle = UInputHandle::new(open_validated_device(config)?);
        let version = handle
            .version()
            .with_context(|| format!("querying uinput interface version for {device_class}"))?;
        ensure!(
            version >= 5,
            "uinput interface version {version} is too old for {device_class}"
        );
        Ok(handle)
    }

    fn open_validated_device(config: &UinputConfig) -> Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(&config.device_path).with_context(|| {
            format!(
                "opening configured uinput device {} without following symlinks",
                config.device_path.display()
            )
        })?;
        validate_open_device(&file, config)?;
        Ok(file)
    }

    fn validate_open_device(file: &File, config: &UinputConfig) -> Result<()> {
        let metadata = file.metadata().with_context(|| {
            format!(
                "inspecting opened uinput device {}",
                config.device_path.display()
            )
        })?;
        ensure!(
            metadata.file_type().is_char_device(),
            "configured uinput path is not a character device"
        );
        ensure!(
            metadata.uid() == config.expected_owner_uid,
            "configured uinput owner UID changed"
        );
        ensure!(
            metadata.gid() == config.expected_group_gid,
            "configured uinput group GID changed"
        );
        ensure!(
            metadata.mode() & 0o7777 == config.expected_mode,
            "configured uinput permission mode changed"
        );
        let device = metadata.rdev();
        ensure!(
            libc::major(device) == UINPUT_MAJOR && libc::minor(device) == UINPUT_MINOR,
            "configured character device is not the Linux uinput misc device"
        );
        reject_extended_access_acl(file)?;
        Ok(())
    }

    fn reject_extended_access_acl(file: &File) -> Result<()> {
        let attribute = c"system.posix_acl_access";
        let result = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                attribute.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if result >= 0 {
            anyhow::bail!("configured uinput device must not have an extended access ACL");
        }
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::ENODATA || code == libc::ENOTSUP)
        {
            return Ok(());
        }
        Err(error).context("checking configured uinput device access ACL")
    }

    fn absolute_range(axis: AbsoluteAxis, minimum: i32, maximum: i32) -> AbsoluteInfoSetup {
        AbsoluteInfoSetup {
            axis,
            info: AbsoluteInfo {
                minimum,
                maximum,
                ..AbsoluteInfo::default()
            },
        }
    }

    fn mouse_button(button: u8) -> Option<Key> {
        match button {
            1 => Some(Key::ButtonLeft),
            2 => Some(Key::ButtonRight),
            3 => Some(Key::ButtonMiddle),
            _ => None,
        }
    }

    fn linux_gamepad_button(button: GamepadButton) -> Key {
        match button {
            GamepadButton::A => Key::ButtonSouth,
            GamepadButton::B => Key::ButtonEast,
            GamepadButton::X => Key::ButtonNorth,
            GamepadButton::Y => Key::ButtonWest,
            GamepadButton::LeftShoulder => Key::ButtonTL,
            GamepadButton::RightShoulder => Key::ButtonTR,
            GamepadButton::Back => Key::ButtonSelect,
            GamepadButton::Start => Key::ButtonStart,
            GamepadButton::Guide => Key::ButtonMode,
            GamepadButton::LeftStick => Key::ButtonThumbl,
            GamepadButton::RightStick => Key::ButtonThumbr,
        }
    }

    fn linux_gamepad_axis(axis: GamepadAxis) -> AbsoluteAxis {
        match axis {
            GamepadAxis::LeftX => AbsoluteAxis::X,
            GamepadAxis::LeftY => AbsoluteAxis::Y,
            GamepadAxis::RightX => AbsoluteAxis::RX,
            GamepadAxis::RightY => AbsoluteAxis::RY,
            GamepadAxis::LeftTrigger => AbsoluteAxis::Z,
            GamepadAxis::RightTrigger => AbsoluteAxis::RZ,
            GamepadAxis::DpadX => AbsoluteAxis::Hat0X,
            GamepadAxis::DpadY => AbsoluteAxis::Hat0Y,
        }
    }

    fn key_event(key: Key, state: KeyState) -> input_linux::sys::input_event {
        KeyEvent::new(EventTime::default(), key, state)
            .into_event()
            .into_raw()
    }

    fn absolute_event(axis: AbsoluteAxis, value: i32) -> input_linux::sys::input_event {
        AbsoluteEvent::new(EventTime::default(), axis, value)
            .into_event()
            .into_raw()
    }

    fn relative_event(axis: RelativeAxis, value: i32) -> input_linux::sys::input_event {
        RelativeEvent::new(EventTime::default(), axis, value)
            .into_event()
            .into_raw()
    }

    fn synchronize_event() -> input_linux::sys::input_event {
        SynchronizeEvent::report(EventTime::default())
            .into_event()
            .into_raw()
    }

    fn linux_key(key: MappedKey) -> Key {
        match key {
            MappedKey::A => Key::A,
            MappedKey::B => Key::B,
            MappedKey::C => Key::C,
            MappedKey::D => Key::D,
            MappedKey::E => Key::E,
            MappedKey::F => Key::F,
            MappedKey::G => Key::G,
            MappedKey::H => Key::H,
            MappedKey::I => Key::I,
            MappedKey::J => Key::J,
            MappedKey::K => Key::K,
            MappedKey::L => Key::L,
            MappedKey::M => Key::M,
            MappedKey::N => Key::N,
            MappedKey::O => Key::O,
            MappedKey::P => Key::P,
            MappedKey::Q => Key::Q,
            MappedKey::R => Key::R,
            MappedKey::S => Key::S,
            MappedKey::T => Key::T,
            MappedKey::U => Key::U,
            MappedKey::V => Key::V,
            MappedKey::W => Key::W,
            MappedKey::X => Key::X,
            MappedKey::Y => Key::Y,
            MappedKey::Z => Key::Z,
            MappedKey::Num0 => Key::Num0,
            MappedKey::Num1 => Key::Num1,
            MappedKey::Num2 => Key::Num2,
            MappedKey::Num3 => Key::Num3,
            MappedKey::Num4 => Key::Num4,
            MappedKey::Num5 => Key::Num5,
            MappedKey::Num6 => Key::Num6,
            MappedKey::Num7 => Key::Num7,
            MappedKey::Num8 => Key::Num8,
            MappedKey::Num9 => Key::Num9,
            MappedKey::Minus => Key::Minus,
            MappedKey::Equal => Key::Equal,
            MappedKey::LeftBrace => Key::LeftBrace,
            MappedKey::RightBrace => Key::RightBrace,
            MappedKey::Backslash => Key::Backslash,
            MappedKey::Semicolon => Key::Semicolon,
            MappedKey::Apostrophe => Key::Apostrophe,
            MappedKey::Grave => Key::Grave,
            MappedKey::Comma => Key::Comma,
            MappedKey::Dot => Key::Dot,
            MappedKey::Slash => Key::Slash,
            MappedKey::Enter => Key::Enter,
            MappedKey::Tab => Key::Tab,
            MappedKey::Space => Key::Space,
            MappedKey::Backspace => Key::Backspace,
            MappedKey::Escape => Key::Esc,
            MappedKey::LeftShift => Key::LeftShift,
            MappedKey::LeftCtrl => Key::LeftCtrl,
            MappedKey::LeftAlt => Key::LeftAlt,
            MappedKey::LeftMeta => Key::LeftMeta,
            MappedKey::Up => Key::Up,
            MappedKey::Down => Key::Down,
            MappedKey::Left => Key::Left,
            MappedKey::Right => Key::Right,
            MappedKey::Home => Key::Home,
            MappedKey::End => Key::End,
            MappedKey::PageUp => Key::PageUp,
            MappedKey::PageDown => Key::PageDown,
            MappedKey::Delete => Key::Delete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_input_topology_has_stable_distinct_identities() {
        assert_eq!(UINPUT_VENDOR_ID, 0x5347);
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
    fn protocol_events_route_to_disjoint_virtual_devices() {
        for event in [
            InputEvent::MouseMove { x: 1, y: 2 },
            InputEvent::MouseMoveRelative { dx: 1, dy: 2 },
            InputEvent::MousePositionSync { x: 10, y: 20 },
            InputEvent::MouseClick { b: 1 },
            InputEvent::MouseDown { b: 2 },
            InputEvent::MouseUp { b: 3 },
            InputEvent::MouseScroll { dx: 1, dy: -1 },
        ] {
            assert_eq!(input_device_class(&event), Some(InputDeviceClass::Pointer));
        }
        for event in [
            InputEvent::KeyDown { k: "a".into() },
            InputEvent::KeyUp { k: "a".into() },
            InputEvent::KeyClick { k: "a".into() },
        ] {
            assert_eq!(input_device_class(&event), Some(InputDeviceClass::Keyboard));
        }
        assert_eq!(
            input_device_class(&InputEvent::Gamepad {
                state: GamepadState::default(),
            }),
            Some(InputDeviceClass::Gamepad)
        );
        assert_eq!(
            input_device_class(&InputEvent::Text {
                s: "ignored".into()
            }),
            None
        );
        assert_eq!(input_device_class(&InputEvent::Probe), None);
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
    fn maps_client_named_keys_without_guessing() {
        assert_eq!(map_key("Enter"), Some(MappedKey::Enter));
        assert_eq!(map_key("Control"), Some(MappedKey::LeftCtrl));
        assert_eq!(map_key("PageDown"), Some(MappedKey::PageDown));
        assert_eq!(map_key("F13"), None);
        assert_eq!(map_key(""), None);
    }

    #[test]
    fn maps_us_printable_keys_to_physical_keys() {
        assert_eq!(map_key("a"), Some(MappedKey::A));
        assert_eq!(map_key("A"), Some(MappedKey::A));
        assert_eq!(map_key("!"), Some(MappedKey::Num1));
        assert_eq!(map_key("?"), Some(MappedKey::Slash));
        assert_eq!(map_key("é"), None);
        assert_eq!(map_key("ab"), None);
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
    fn pointer_position_sync_uses_two_ordered_relative_reports() {
        assert_eq!(
            pointer_position_sync_report(1_280, 800),
            [
                PointerReportEvent::RelativeX(-32_767),
                PointerReportEvent::RelativeY(-32_767),
                PointerReportEvent::Synchronize,
                PointerReportEvent::RelativeX(1_280),
                PointerReportEvent::RelativeY(800),
                PointerReportEvent::Synchronize,
            ]
        );
    }

    #[test]
    fn maps_every_gamepad_control_into_one_bounded_report() {
        let state = GamepadState {
            a: true,
            b: true,
            x: true,
            y: true,
            left_shoulder: true,
            right_shoulder: true,
            back: true,
            start: true,
            guide: true,
            left_stick: true,
            right_stick: true,
            dpad_up: true,
            dpad_right: true,
            left_x: -32_767,
            left_y: 32_767,
            right_x: -123,
            right_y: 456,
            left_trigger: 12_345,
            right_trigger: 32_767,
            ..GamepadState::default()
        };
        let report = map_gamepad_state(&state);
        assert_eq!(report.buttons.len(), 11);
        assert!(report.buttons.into_iter().all(|(_, pressed)| pressed));
        assert_eq!(
            report.axes,
            [
                (GamepadAxis::LeftX, -32_767),
                (GamepadAxis::LeftY, 32_767),
                (GamepadAxis::RightX, -123),
                (GamepadAxis::RightY, 456),
                (GamepadAxis::LeftTrigger, 12_345),
                (GamepadAxis::RightTrigger, 32_767),
                (GamepadAxis::DpadX, 1),
                (GamepadAxis::DpadY, -1),
            ]
        );
    }

    #[test]
    fn neutral_gamepad_report_releases_buttons_and_axes() {
        let report = map_gamepad_state(&GamepadState::default());
        assert!(report.buttons.into_iter().all(|(_, pressed)| !pressed));
        assert!(report.axes.into_iter().all(|(_, value)| value == 0));
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
