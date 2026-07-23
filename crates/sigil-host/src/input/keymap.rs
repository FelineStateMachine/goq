#[cfg(target_os = "linux")]
use input_linux::Key;
#[cfg(any(target_os = "linux", test))]
use sigil_protocol::GamepadState;
#[cfg(any(target_os = "linux", test))]
use sigil_protocol::InputEvent;

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputDeviceClass {
    Pointer,
    Keyboard,
    Gamepad,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerReportEvent {
    RelativeX(i32),
    RelativeY(i32),
    Synchronize,
}

#[cfg(any(target_os = "linux", test))]
/// Emulate an absolute position on the relative-only appliance pointer.
///
/// This is correct only when Gamescope clamps the first report at the surface
/// origin and applies neither acceleration nor scaling to either report. The
/// hardware UAT verifies both the synchronized position and one subsequent
/// relative delta through the Xwayland pointer tracker.
pub(crate) fn pointer_position_sync_report(x: i32, y: i32) -> [PointerReportEvent; 6] {
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
pub(crate) fn input_device_class(event: &InputEvent) -> Option<InputDeviceClass> {
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

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MappedKey {
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
    NonUsBackslash,
    Ro,
    Yen,
    Enter,
    Tab,
    Space,
    Backspace,
    Escape,
    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftMeta,
    RightMeta,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    PrintScreen,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

#[cfg(any(target_os = "linux", test))]
pub(crate) const ALL_MAPPED_KEYS: &[MappedKey] = &[
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
    MappedKey::NonUsBackslash,
    MappedKey::Ro,
    MappedKey::Yen,
    MappedKey::Enter,
    MappedKey::Tab,
    MappedKey::Space,
    MappedKey::Backspace,
    MappedKey::Escape,
    MappedKey::LeftShift,
    MappedKey::RightShift,
    MappedKey::LeftCtrl,
    MappedKey::RightCtrl,
    MappedKey::LeftAlt,
    MappedKey::RightAlt,
    MappedKey::LeftMeta,
    MappedKey::RightMeta,
    MappedKey::Up,
    MappedKey::Down,
    MappedKey::Left,
    MappedKey::Right,
    MappedKey::Home,
    MappedKey::End,
    MappedKey::PageUp,
    MappedKey::PageDown,
    MappedKey::Insert,
    MappedKey::Delete,
    MappedKey::PrintScreen,
    MappedKey::F1,
    MappedKey::F2,
    MappedKey::F3,
    MappedKey::F4,
    MappedKey::F5,
    MappedKey::F6,
    MappedKey::F7,
    MappedKey::F8,
    MappedKey::F9,
    MappedKey::F10,
    MappedKey::F11,
    MappedKey::F12,
];

#[cfg(any(target_os = "linux", test))]
pub(crate) fn map_key(value: &str) -> Option<MappedKey> {
    match value {
        "KeyA" => Some(MappedKey::A),
        "KeyB" => Some(MappedKey::B),
        "KeyC" => Some(MappedKey::C),
        "KeyD" => Some(MappedKey::D),
        "KeyE" => Some(MappedKey::E),
        "KeyF" => Some(MappedKey::F),
        "KeyG" => Some(MappedKey::G),
        "KeyH" => Some(MappedKey::H),
        "KeyI" => Some(MappedKey::I),
        "KeyJ" => Some(MappedKey::J),
        "KeyK" => Some(MappedKey::K),
        "KeyL" => Some(MappedKey::L),
        "KeyM" => Some(MappedKey::M),
        "KeyN" => Some(MappedKey::N),
        "KeyO" => Some(MappedKey::O),
        "KeyP" => Some(MappedKey::P),
        "KeyQ" => Some(MappedKey::Q),
        "KeyR" => Some(MappedKey::R),
        "KeyS" => Some(MappedKey::S),
        "KeyT" => Some(MappedKey::T),
        "KeyU" => Some(MappedKey::U),
        "KeyV" => Some(MappedKey::V),
        "KeyW" => Some(MappedKey::W),
        "KeyX" => Some(MappedKey::X),
        "KeyY" => Some(MappedKey::Y),
        "KeyZ" => Some(MappedKey::Z),
        "Digit0" => Some(MappedKey::Num0),
        "Digit1" => Some(MappedKey::Num1),
        "Digit2" => Some(MappedKey::Num2),
        "Digit3" => Some(MappedKey::Num3),
        "Digit4" => Some(MappedKey::Num4),
        "Digit5" => Some(MappedKey::Num5),
        "Digit6" => Some(MappedKey::Num6),
        "Digit7" => Some(MappedKey::Num7),
        "Digit8" => Some(MappedKey::Num8),
        "Digit9" => Some(MappedKey::Num9),
        "Minus" => Some(MappedKey::Minus),
        "Equal" => Some(MappedKey::Equal),
        "BracketLeft" => Some(MappedKey::LeftBrace),
        "BracketRight" => Some(MappedKey::RightBrace),
        "Backslash" => Some(MappedKey::Backslash),
        "IntlBackslash" => Some(MappedKey::NonUsBackslash),
        "IntlRo" => Some(MappedKey::Ro),
        "IntlYen" => Some(MappedKey::Yen),
        "Semicolon" => Some(MappedKey::Semicolon),
        "Quote" => Some(MappedKey::Apostrophe),
        "Backquote" => Some(MappedKey::Grave),
        "Comma" => Some(MappedKey::Comma),
        "Period" => Some(MappedKey::Dot),
        "Slash" => Some(MappedKey::Slash),
        "Enter" => Some(MappedKey::Enter),
        "Tab" => Some(MappedKey::Tab),
        "Space" => Some(MappedKey::Space),
        "Backspace" => Some(MappedKey::Backspace),
        "Escape" => Some(MappedKey::Escape),
        "ShiftLeft" => Some(MappedKey::LeftShift),
        "ShiftRight" => Some(MappedKey::RightShift),
        "ControlLeft" => Some(MappedKey::LeftCtrl),
        "ControlRight" => Some(MappedKey::RightCtrl),
        "AltLeft" => Some(MappedKey::LeftAlt),
        "AltRight" => Some(MappedKey::RightAlt),
        "MetaLeft" => Some(MappedKey::LeftMeta),
        "MetaRight" => Some(MappedKey::RightMeta),
        "Shift" => Some(MappedKey::LeftShift),
        "Control" => Some(MappedKey::LeftCtrl),
        "Alt" => Some(MappedKey::LeftAlt),
        "Meta" => Some(MappedKey::LeftMeta),
        "ArrowUp" => Some(MappedKey::Up),
        "ArrowDown" => Some(MappedKey::Down),
        "ArrowLeft" => Some(MappedKey::Left),
        "ArrowRight" => Some(MappedKey::Right),
        "Up" => Some(MappedKey::Up),
        "Down" => Some(MappedKey::Down),
        "Left" => Some(MappedKey::Left),
        "Right" => Some(MappedKey::Right),
        "Home" => Some(MappedKey::Home),
        "End" => Some(MappedKey::End),
        "PageUp" => Some(MappedKey::PageUp),
        "PageDown" => Some(MappedKey::PageDown),
        "Insert" => Some(MappedKey::Insert),
        "Delete" => Some(MappedKey::Delete),
        "PrintScreen" => Some(MappedKey::PrintScreen),
        "F1" => Some(MappedKey::F1),
        "F2" => Some(MappedKey::F2),
        "F3" => Some(MappedKey::F3),
        "F4" => Some(MappedKey::F4),
        "F5" => Some(MappedKey::F5),
        "F6" => Some(MappedKey::F6),
        "F7" => Some(MappedKey::F7),
        "F8" => Some(MappedKey::F8),
        "F9" => Some(MappedKey::F9),
        "F10" => Some(MappedKey::F10),
        "F11" => Some(MappedKey::F11),
        "F12" => Some(MappedKey::F12),
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
pub(crate) enum GamepadButton {
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
pub(crate) enum GamepadAxis {
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
pub(crate) struct GamepadReport {
    pub(crate) buttons: [(GamepadButton, bool); 11],
    pub(crate) axes: [(GamepadAxis, i32); 8],
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn map_gamepad_state(state: &GamepadState) -> GamepadReport {
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
pub(crate) fn linux_key(key: MappedKey) -> Key {
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
        MappedKey::NonUsBackslash => Key::NonUsBackslashAndPipe,
        MappedKey::Ro => Key::Ro,
        MappedKey::Yen => Key::Yen,
        MappedKey::Enter => Key::Enter,
        MappedKey::Tab => Key::Tab,
        MappedKey::Space => Key::Space,
        MappedKey::Backspace => Key::Backspace,
        MappedKey::Escape => Key::Esc,
        MappedKey::LeftShift => Key::LeftShift,
        MappedKey::RightShift => Key::RightShift,
        MappedKey::LeftCtrl => Key::LeftCtrl,
        MappedKey::RightCtrl => Key::RightCtrl,
        MappedKey::LeftAlt => Key::LeftAlt,
        MappedKey::RightAlt => Key::RightAlt,
        MappedKey::LeftMeta => Key::LeftMeta,
        MappedKey::RightMeta => Key::RightMeta,
        MappedKey::Up => Key::Up,
        MappedKey::Down => Key::Down,
        MappedKey::Left => Key::Left,
        MappedKey::Right => Key::Right,
        MappedKey::Home => Key::Home,
        MappedKey::End => Key::End,
        MappedKey::PageUp => Key::PageUp,
        MappedKey::PageDown => Key::PageDown,
        MappedKey::Insert => Key::Insert,
        MappedKey::Delete => Key::Delete,
        MappedKey::PrintScreen => Key::Sysrq,
        MappedKey::F1 => Key::F1,
        MappedKey::F2 => Key::F2,
        MappedKey::F3 => Key::F3,
        MappedKey::F4 => Key::F4,
        MappedKey::F5 => Key::F5,
        MappedKey::F6 => Key::F6,
        MappedKey::F7 => Key::F7,
        MappedKey::F8 => Key::F8,
        MappedKey::F9 => Key::F9,
        MappedKey::F10 => Key::F10,
        MappedKey::F11 => Key::F11,
        MappedKey::F12 => Key::F12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn maps_client_named_keys_without_guessing() {
        assert_eq!(map_key("Enter"), Some(MappedKey::Enter));
        assert_eq!(map_key("Control"), Some(MappedKey::LeftCtrl));
        assert_eq!(map_key("PageDown"), Some(MappedKey::PageDown));
        assert_eq!(map_key("F13"), None);
        assert_eq!(map_key("CapsLock"), None);
        assert_eq!(map_key("Numpad1"), None);
        assert_eq!(map_key(""), None);
    }

    #[test]
    fn maps_bounded_browser_physical_keys_for_games_and_layouts() {
        for (value, expected) in [
            ("KeyQ", MappedKey::Q),
            ("Digit2", MappedKey::Num2),
            ("BracketLeft", MappedKey::LeftBrace),
            ("IntlBackslash", MappedKey::NonUsBackslash),
            ("IntlRo", MappedKey::Ro),
            ("IntlYen", MappedKey::Yen),
            ("ShiftLeft", MappedKey::LeftShift),
            ("ShiftRight", MappedKey::RightShift),
            ("ControlLeft", MappedKey::LeftCtrl),
            ("ControlRight", MappedKey::RightCtrl),
            ("AltLeft", MappedKey::LeftAlt),
            ("AltRight", MappedKey::RightAlt),
            ("MetaLeft", MappedKey::LeftMeta),
            ("MetaRight", MappedKey::RightMeta),
            ("ArrowUp", MappedKey::Up),
            ("Insert", MappedKey::Insert),
            ("PrintScreen", MappedKey::PrintScreen),
            ("F1", MappedKey::F1),
            ("F12", MappedKey::F12),
        ] {
            assert_eq!(map_key(value), Some(expected), "{value}");
            assert!(ALL_MAPPED_KEYS.contains(&expected), "{value}");
        }
        let unique = ALL_MAPPED_KEYS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ALL_MAPPED_KEYS.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extended_physical_keys_translate_to_exact_linux_evdev_codes() {
        for (mapped, expected) in [
            (MappedKey::F1, input_linux::Key::F1),
            (MappedKey::F12, input_linux::Key::F12),
            (MappedKey::Insert, input_linux::Key::Insert),
            (MappedKey::PrintScreen, input_linux::Key::Sysrq),
            (MappedKey::RightShift, input_linux::Key::RightShift),
            (MappedKey::RightCtrl, input_linux::Key::RightCtrl),
            (MappedKey::RightAlt, input_linux::Key::RightAlt),
            (MappedKey::RightMeta, input_linux::Key::RightMeta),
            (
                MappedKey::NonUsBackslash,
                input_linux::Key::NonUsBackslashAndPipe,
            ),
            (MappedKey::Ro, input_linux::Key::Ro),
            (MappedKey::Yen, input_linux::Key::Yen),
        ] {
            assert_eq!(linux_key(mapped), expected);
        }
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
}
