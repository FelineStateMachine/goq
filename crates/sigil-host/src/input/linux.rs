use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};

use anyhow::{Context, Result, ensure};
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId,
    InputProperty, Key, KeyEvent, KeyState, RelativeAxis, RelativeEvent, SynchronizeEvent,
    UInputHandle,
};
use sigil_protocol::{
    GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN, GAMEPAD_TRIGGER_MAX, GamepadState,
    InputEvent as ProtocolInputEvent,
};

use crate::config::UinputConfig;
use crate::input::acl::{POSIX_ACL_REQUIRED_BYTES, validate_single_user_access_acl};
use crate::input::keymap::{
    ALL_MAPPED_KEYS, GamepadAxis, GamepadButton, InputDeviceClass, PointerReportEvent,
    input_device_class, linux_key, map_gamepad_state, map_key, pointer_position_sync_report,
};
use crate::input::{
    GAMEPAD_DEVICE_NAME, GAMEPAD_PRODUCT_ID, KEYBOARD_DEVICE_NAME, KEYBOARD_PRODUCT_ID,
    POINTER_DEVICE_NAME, POINTER_PRODUCT_ID, UINPUT_BUS_TYPE, UINPUT_DEVICE_VERSION,
    UINPUT_VENDOR_ID,
};

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
                    bustype: UINPUT_BUS_TYPE,
                    vendor: UINPUT_VENDOR_ID,
                    product: POINTER_PRODUCT_ID,
                    version: UINPUT_DEVICE_VERSION,
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
                    PointerReportEvent::RelativeX(value) => relative_event(RelativeAxis::X, value),
                    PointerReportEvent::RelativeY(value) => relative_event(RelativeAxis::Y, value),
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
                    bustype: UINPUT_BUS_TYPE,
                    vendor: UINPUT_VENDOR_ID,
                    product: KEYBOARD_PRODUCT_ID,
                    version: UINPUT_DEVICE_VERSION,
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
                    bustype: UINPUT_BUS_TYPE,
                    vendor: UINPUT_VENDOR_ID,
                    product: GAMEPAD_PRODUCT_ID,
                    version: UINPUT_DEVICE_VERSION,
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

fn open_validated_handle(config: &UinputConfig, device_class: &str) -> Result<UInputHandle<File>> {
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
    validate_access_acl(file, config)?;
    Ok(())
}

fn validate_access_acl(file: &File, config: &UinputConfig) -> Result<()> {
    if let Some(expected_uid) = config.expected_acl_user_uid {
        let effective_uid = unsafe { libc::geteuid() };
        ensure!(
            expected_uid == effective_uid,
            "configured uinput ACL user UID {expected_uid} does not match Sigil effective UID {effective_uid}"
        );
    }

    let attribute = c"system.posix_acl_access";
    let mut bytes = [0_u8; POSIX_ACL_REQUIRED_BYTES];
    let result = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            attribute.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if result >= 0 {
        let Some(expected_uid) = config.expected_acl_user_uid else {
            anyhow::bail!(
                "configured uinput device must use the group-only scheme with no extended access ACL; set uinput.expected_acl_user_uid only for the explicit one-user ACL scheme"
            );
        };
        let length =
            usize::try_from(result).context("converting configured uinput access ACL length")?;
        return validate_single_user_access_acl(
            bytes
                .get(..length)
                .context("configured uinput access ACL exceeded its fixed bound")?,
            expected_uid,
            config.expected_mode,
        )
        .context("validating configured uinput one-user access ACL");
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::ENODATA || code == libc::ENOTSUP)
    {
        ensure!(
            config.expected_acl_user_uid.is_none(),
            "configured uinput device is missing the explicitly required one-user access ACL"
        );
        return Ok(());
    }
    if error.raw_os_error() == Some(libc::ERANGE) {
        if config.expected_acl_user_uid.is_some() {
            anyhow::bail!("configured uinput access ACL exceeds the exact one-user ACL scheme");
        }
        anyhow::bail!(
            "configured uinput device must use the group-only scheme with no extended access ACL"
        );
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
