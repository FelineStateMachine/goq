use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::framing::{read_json, write_json};
use crate::{MAX_INPUT_MESSAGE_LEN, ProtocolError, Result};

const MAX_KEY_LEN: usize = 64;
const MAX_TEXT_LEN: usize = 1_024;

/// Symmetric per-message relative pointer range. Large accumulated movement
/// must be split into bounded messages before it crosses the trust boundary.
pub const RELATIVE_POINTER_DELTA_MIN: i32 = -32_767;
pub const RELATIVE_POINTER_DELTA_MAX: i32 = 32_767;
/// Normalized logical pointer-position range used to periodically resynchronize
/// a relative-only host pointer with the client canvas.
pub const POINTER_POSITION_MIN: i32 = 0;
pub const POINTER_POSITION_MAX: i32 = 32_767;

/// Symmetric normalized stick range. `i16::MIN` is deliberately excluded so
/// negating an axis never overflows and both directions have equal magnitude.
pub const GAMEPAD_AXIS_MIN: i16 = -32_767;
pub const GAMEPAD_AXIS_MAX: i16 = 32_767;
/// Normalized analog-trigger range, from released through fully pressed.
pub const GAMEPAD_TRIGGER_MAX: u16 = 32_767;

/// Complete state for the one virtual gamepad. Each message replaces the
/// previous state; no button or axis has an independent queue.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GamepadState {
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub left_shoulder: bool,
    pub right_shoulder: bool,
    pub back: bool,
    pub start: bool,
    pub guide: bool,
    pub left_stick: bool,
    pub right_stick: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u16,
    pub right_trigger: u16,
}

impl GamepadState {
    pub fn validate(&self) -> Result<()> {
        if [self.left_x, self.left_y, self.right_x, self.right_y]
            .into_iter()
            .any(|axis| !(GAMEPAD_AXIS_MIN..=GAMEPAD_AXIS_MAX).contains(&axis))
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "gamepad state",
                reason: "stick axes must be between -32767 and 32767",
            });
        }
        if self.left_trigger > GAMEPAD_TRIGGER_MAX || self.right_trigger > GAMEPAD_TRIGGER_MAX {
            return Err(ProtocolError::InvalidMessage {
                message_type: "gamepad state",
                reason: "triggers must be between 0 and 32767",
            });
        }
        if (self.dpad_up && self.dpad_down) || (self.dpad_left && self.dpad_right) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "gamepad state",
                reason: "d-pad cannot hold opposing directions",
            });
        }
        Ok(())
    }
}

/// Input messages retain the inherited compact JSON tags during the v1
/// migration. On the v1 ALPN each JSON value is prefixed by a big-endian u32
/// length rather than delimited by a newline.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", deny_unknown_fields)]
pub enum InputEvent {
    /// Content-free liveness probe. A host may accept this only when it also
    /// negotiated `InputAck`; it never reaches an operating-system device.
    #[serde(rename = "ip")]
    Probe,
    #[serde(rename = "mm")]
    MouseMove { x: i32, y: i32 },
    #[serde(rename = "mr")]
    MouseMoveRelative { dx: i32, dy: i32 },
    #[serde(rename = "mp")]
    MousePositionSync { x: i32, y: i32 },
    #[serde(rename = "mc")]
    MouseClick { b: u8 },
    #[serde(rename = "md")]
    MouseDown { b: u8 },
    #[serde(rename = "mu")]
    MouseUp { b: u8 },
    #[serde(rename = "ms")]
    MouseScroll { dx: i32, dy: i32 },
    #[serde(rename = "kd")]
    KeyDown { k: String },
    #[serde(rename = "ku")]
    KeyUp { k: String },
    #[serde(rename = "kt")]
    KeyClick { k: String },
    #[serde(rename = "tx")]
    Text { s: String },
    #[serde(rename = "gp")]
    Gamepad { state: GamepadState },
}

/// Host-observed pointer location in the compositor's native coordinate space.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PointerPosition {
    pub x: i32,
    pub y: i32,
}

impl PointerPosition {
    pub fn new(x: i32, y: i32) -> Result<Self> {
        let position = Self { x, y };
        position.validate()?;
        Ok(position)
    }

    pub fn validate(&self) -> Result<()> {
        if !(POINTER_POSITION_MIN..=POINTER_POSITION_MAX).contains(&self.x)
            || !(POINTER_POSITION_MIN..=POINTER_POSITION_MAX).contains(&self.y)
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "pointer position",
                reason: "coordinates must be between 0 and 32767",
            });
        }
        Ok(())
    }
}

/// Bounded host response used by probes to prove that the input stream is
/// serviced independently while media is active. Pointer coordinates are
/// present only when both peers negotiated `PointerPositionFeedback`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputAck {
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_position: Option<PointerPosition>,
    /// Whether the host compositor is currently presenting its pointer image.
    ///
    /// This is optional for wire compatibility with v1 hosts that encoded
    /// visibility by omitting `pointer_position` while the cursor was hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_visible: Option<bool>,
}

impl InputAck {
    pub fn validate(&self) -> Result<()> {
        if let Some(position) = self.pointer_position {
            position.validate()?;
        }
        if self.pointer_visible == Some(true) && self.pointer_position.is_none() {
            return Err(ProtocolError::InvalidMessage {
                message_type: "input acknowledgment",
                reason: "a visible pointer requires a pointer position",
            });
        }
        Ok(())
    }
}

impl InputEvent {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::MouseMoveRelative { dx, dy }
                if !(RELATIVE_POINTER_DELTA_MIN..=RELATIVE_POINTER_DELTA_MAX).contains(dx)
                    || !(RELATIVE_POINTER_DELTA_MIN..=RELATIVE_POINTER_DELTA_MAX).contains(dy) =>
            {
                Err(ProtocolError::InvalidMessage {
                    message_type: "input event",
                    reason: "relative pointer deltas must be between -32767 and 32767",
                })
            }
            Self::MousePositionSync { x, y }
                if !(POINTER_POSITION_MIN..=POINTER_POSITION_MAX).contains(x)
                    || !(POINTER_POSITION_MIN..=POINTER_POSITION_MAX).contains(y) =>
            {
                Err(ProtocolError::InvalidMessage {
                    message_type: "input event",
                    reason: "pointer synchronization coordinates must be between 0 and 32767",
                })
            }
            Self::MouseClick { b } | Self::MouseDown { b } | Self::MouseUp { b }
                if !(1..=3).contains(b) =>
            {
                Err(ProtocolError::InvalidMessage {
                    message_type: "input event",
                    reason: "mouse button must be in 1..=3",
                })
            }
            Self::KeyDown { k } | Self::KeyUp { k } | Self::KeyClick { k }
                if k.is_empty() || k.len() > MAX_KEY_LEN =>
            {
                Err(ProtocolError::InvalidMessage {
                    message_type: "input event",
                    reason: "key must contain 1..=64 UTF-8 bytes",
                })
            }
            Self::Text { s } if s.is_empty() || s.len() > MAX_TEXT_LEN => {
                Err(ProtocolError::InvalidMessage {
                    message_type: "input event",
                    reason: "text must contain 1..=1024 UTF-8 bytes",
                })
            }
            Self::Gamepad { state } => state.validate(),
            _ => Ok(()),
        }
    }
}

/// Read one v1 length-prefixed input event. Clean EOF returns `None`.
pub async fn read_input_event<R>(reader: &mut R) -> Result<Option<InputEvent>>
where
    R: AsyncRead + Unpin,
{
    let event: Option<InputEvent> = read_json(reader, MAX_INPUT_MESSAGE_LEN).await?;
    if let Some(event) = &event {
        event.validate()?;
    }
    Ok(event)
}

/// Validate and write one v1 length-prefixed input event.
pub async fn write_input_event<W>(writer: &mut W, event: &InputEvent) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    event.validate()?;
    write_json(writer, event, MAX_INPUT_MESSAGE_LEN).await
}

pub async fn read_input_ack<R>(reader: &mut R) -> Result<Option<InputAck>>
where
    R: AsyncRead + Unpin,
{
    let response: Option<InputAck> = read_json(reader, MAX_INPUT_MESSAGE_LEN).await?;
    if let Some(response) = &response {
        response.validate()?;
    }
    Ok(response)
}

pub async fn write_input_ack<W>(writer: &mut W, ack: &InputAck) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    ack.validate()?;
    write_json(writer, ack, MAX_INPUT_MESSAGE_LEN).await
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    #[test]
    fn inherited_json_tags_are_golden() {
        assert_eq!(
            serde_json::to_string(&InputEvent::Probe).unwrap(),
            r#"{"t":"ip"}"#
        );
        assert_eq!(
            serde_json::to_string(&InputEvent::MouseMove { x: 10, y: -20 }).unwrap(),
            r#"{"t":"mm","x":10,"y":-20}"#
        );
        assert_eq!(
            serde_json::to_string(&InputEvent::MouseMoveRelative { dx: 10, dy: -20 }).unwrap(),
            r#"{"t":"mr","dx":10,"dy":-20}"#
        );
        assert_eq!(
            serde_json::to_string(&InputEvent::MousePositionSync { x: 123, y: 456 }).unwrap(),
            r#"{"t":"mp","x":123,"y":456}"#
        );
        assert_eq!(
            serde_json::to_string(&InputEvent::KeyDown { k: "Enter".into() }).unwrap(),
            r#"{"t":"kd","k":"Enter"}"#
        );
        assert_eq!(
            serde_json::to_string(&InputEvent::Text { s: "hi".into() }).unwrap(),
            r#"{"t":"tx","s":"hi"}"#
        );
        let gamepad = InputEvent::Gamepad {
            state: GamepadState {
                a: true,
                dpad_left: true,
                left_x: GAMEPAD_AXIS_MIN,
                right_trigger: GAMEPAD_TRIGGER_MAX,
                ..GamepadState::default()
            },
        };
        assert_eq!(
            serde_json::to_string(&gamepad).unwrap(),
            r#"{"t":"gp","state":{"a":true,"b":false,"x":false,"y":false,"left_shoulder":false,"right_shoulder":false,"back":false,"start":false,"guide":false,"left_stick":false,"right_stick":false,"dpad_up":false,"dpad_down":false,"dpad_left":true,"dpad_right":false,"left_x":-32767,"left_y":0,"right_x":0,"right_y":0,"left_trigger":0,"right_trigger":32767}}"#
        );
    }

    #[tokio::test]
    async fn input_event_round_trips_and_eof_is_clean() {
        let event = InputEvent::MouseScroll { dx: -1, dy: 2 };
        let (mut sender, mut receiver) = duplex(256);
        write_input_event(&mut sender, &event).await.unwrap();
        sender.shutdown().await.unwrap();

        assert_eq!(read_input_event(&mut receiver).await.unwrap(), Some(event));
        assert_eq!(read_input_event(&mut receiver).await.unwrap(), None);
    }

    #[tokio::test]
    async fn full_gamepad_snapshot_round_trips() {
        let event = InputEvent::Gamepad {
            state: GamepadState {
                a: true,
                right_shoulder: true,
                dpad_up: true,
                dpad_right: true,
                left_x: -12_345,
                left_y: 23_456,
                right_x: GAMEPAD_AXIS_MAX,
                right_y: GAMEPAD_AXIS_MIN,
                left_trigger: 1,
                right_trigger: GAMEPAD_TRIGGER_MAX,
                ..GamepadState::default()
            },
        };
        let (mut sender, mut receiver) = duplex(1024);
        write_input_event(&mut sender, &event).await.unwrap();
        sender.shutdown().await.unwrap();
        assert_eq!(read_input_event(&mut receiver).await.unwrap(), Some(event));
    }

    #[tokio::test]
    async fn input_ack_round_trips_and_eof_is_clean() {
        let ack = InputAck {
            sequence: 42,
            pointer_position: Some(PointerPosition::new(1_280, 800).unwrap()),
            pointer_visible: Some(false),
        };
        let (mut sender, mut receiver) = duplex(256);
        write_input_ack(&mut sender, &ack).await.unwrap();
        sender.shutdown().await.unwrap();

        assert_eq!(read_input_ack(&mut receiver).await.unwrap(), Some(ack));
        assert_eq!(read_input_ack(&mut receiver).await.unwrap(), None);
    }

    #[test]
    fn new_reader_accepts_old_ack_and_feedback_is_omitted_when_absent() {
        let old = r#"{"sequence":42}"#;
        let ack: InputAck = serde_json::from_str(old).unwrap();
        assert_eq!(ack.pointer_position, None);
        assert_eq!(ack.pointer_visible, None);
        assert_eq!(serde_json::to_string(&ack).unwrap(), old);
        assert!(
            serde_json::from_str::<InputAck>(r#"{"sequence":42,"pointer_visible":true}"#)
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(PointerPosition::new(-1, 0).is_err());
        assert!(PointerPosition::new(0, POINTER_POSITION_MAX + 1).is_err());
    }

    #[tokio::test]
    async fn peer_controlled_input_length_is_bounded_before_allocation() {
        let (mut sender, mut receiver) = duplex(16);
        sender
            .write_all(&((MAX_INPUT_MESSAGE_LEN as u32) + 1).to_be_bytes())
            .await
            .unwrap();

        assert!(matches!(
            read_input_event(&mut receiver).await,
            Err(ProtocolError::InvalidMessageLength { .. })
        ));
    }

    #[tokio::test]
    async fn peer_controlled_relative_deltas_are_bounded_after_decode() {
        let payload = br#"{"t":"mr","dx":32768,"dy":0}"#;
        let (mut sender, mut receiver) = duplex(256);
        sender
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        sender.write_all(payload).await.unwrap();
        sender.shutdown().await.unwrap();

        assert!(matches!(
            read_input_event(&mut receiver).await,
            Err(ProtocolError::InvalidMessage { .. })
        ));
    }

    #[tokio::test]
    async fn peer_controlled_pointer_sync_coordinates_are_bounded_after_decode() {
        let payload = br#"{"t":"mp","x":32768,"y":0}"#;
        let (mut sender, mut receiver) = duplex(256);
        sender
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        sender.write_all(payload).await.unwrap();
        sender.shutdown().await.unwrap();

        assert!(matches!(
            read_input_event(&mut receiver).await,
            Err(ProtocolError::InvalidMessage { .. })
        ));
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_values() {
        assert!(serde_json::from_str::<InputEvent>(r#"{"t":"mm","x":1,"y":2,"z":3}"#).is_err());
        assert!(
            InputEvent::MouseMoveRelative {
                dx: RELATIVE_POINTER_DELTA_MAX + 1,
                dy: 0,
            }
            .validate()
            .is_err()
        );
        assert!(
            InputEvent::MouseMoveRelative {
                dx: 0,
                dy: RELATIVE_POINTER_DELTA_MIN - 1,
            }
            .validate()
            .is_err()
        );
        assert!(
            InputEvent::MouseMoveRelative {
                dx: RELATIVE_POINTER_DELTA_MIN,
                dy: RELATIVE_POINTER_DELTA_MAX,
            }
            .validate()
            .is_ok()
        );
        assert!(
            InputEvent::MousePositionSync {
                x: POINTER_POSITION_MIN,
                y: POINTER_POSITION_MAX,
            }
            .validate()
            .is_ok()
        );
        assert!(
            InputEvent::MousePositionSync {
                x: POINTER_POSITION_MIN - 1,
                y: 0,
            }
            .validate()
            .is_err()
        );
        assert!(
            InputEvent::MousePositionSync {
                x: 0,
                y: POINTER_POSITION_MAX + 1,
            }
            .validate()
            .is_err()
        );
        assert!(InputEvent::MouseDown { b: 9 }.validate().is_err());
        assert!(InputEvent::KeyUp { k: String::new() }.validate().is_err());
        assert!(
            InputEvent::Text {
                s: "x".repeat(MAX_TEXT_LEN + 1)
            }
            .validate()
            .is_err()
        );

        let mut state = GamepadState {
            left_x: i16::MIN,
            ..GamepadState::default()
        };
        assert!(state.validate().is_err());
        state.left_x = 0;
        state.left_trigger = GAMEPAD_TRIGGER_MAX + 1;
        assert!(state.validate().is_err());
        state.left_trigger = 0;
        state.dpad_up = true;
        state.dpad_down = true;
        assert!(state.validate().is_err());

        assert!(
            serde_json::from_str::<InputEvent>(
                r#"{"t":"gp","state":{"a":false,"b":false,"x":false,"y":false,"left_shoulder":false,"right_shoulder":false,"back":false,"start":false,"guide":false,"left_stick":false,"right_stick":false,"dpad_up":false,"dpad_down":false,"dpad_left":false,"dpad_right":false,"left_x":0,"left_y":0,"right_x":0,"right_y":0,"left_trigger":0,"right_trigger":0,"extra":true}}"#
            )
            .is_err()
        );
    }
}
