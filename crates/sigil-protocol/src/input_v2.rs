use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::framing::{read_json, write_json};
use crate::{
    Capability, InputAck, InputEvent, MAX_INPUT_MESSAGE_LEN, PROTOCOL_VERSION_V2, ProtocolError,
    Result,
};

const MAX_AGENT_LEN: usize = 128;
const MAX_MESSAGE_LEN: usize = 512;
const MAX_CAPABILITIES: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControllerSlot(u8);

impl ControllerSlot {
    pub const ZERO: Self = Self(0);

    pub fn new(value: u8) -> Result<Self> {
        if value != 0 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "controller slot",
                reason: "only controller slot 0 is supported",
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    pub fn validate(self) -> Result<()> {
        Self::new(self.0).map(|_| ())
    }
}

impl Serialize for ControllerSlot {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for ControllerSlot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputClientHelloV2 {
    pub version: u16,
    pub agent: String,
    pub nonce: [u8; 16],
    pub session_id: u64,
    pub slot: ControllerSlot,
    pub focus_generation: u64,
    pub capabilities: Vec<Capability>,
}

impl InputClientHelloV2 {
    pub fn new(
        agent: impl Into<String>,
        nonce: [u8; 16],
        session_id: u64,
        slot: ControllerSlot,
        focus_generation: u64,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
            agent: agent.into(),
            nonce,
            session_id,
            slot,
            focus_generation,
            capabilities,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_version(self.version)?;
        if self.agent.is_empty() || self.agent.len() > MAX_AGENT_LEN {
            return invalid(
                "input client hello v2",
                "agent must contain 1..=128 UTF-8 bytes",
            );
        }
        validate_binding(self.session_id, self.slot, self.focus_generation)?;
        validate_capabilities(&self.capabilities)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputHostHelloV2 {
    pub version: u16,
    pub accepted: bool,
    pub session_id: Option<u64>,
    pub slot: Option<ControllerSlot>,
    pub focus_generation: Option<u64>,
    pub capabilities: Vec<Capability>,
    pub message: Option<String>,
}

impl InputHostHelloV2 {
    pub fn accepted(
        session_id: u64,
        slot: ControllerSlot,
        focus_generation: u64,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
            accepted: true,
            session_id: Some(session_id),
            slot: Some(slot),
            focus_generation: Some(focus_generation),
            capabilities,
            message: None,
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
            accepted: false,
            session_id: None,
            slot: None,
            focus_generation: None,
            capabilities: Vec::new(),
            message: Some(message.into()),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_version(self.version)?;
        validate_capabilities(&self.capabilities)?;
        if self.accepted {
            let session_id = self.session_id.unwrap_or(0);
            let focus_generation = self.focus_generation.unwrap_or(0);
            let slot = self.slot.ok_or(ProtocolError::InvalidMessage {
                message_type: "input host hello v2",
                reason: "accepted responses require a controller slot",
            })?;
            validate_binding(session_id, slot, focus_generation)?;
            if self.message.is_some() {
                return invalid("input host hello v2", "accepted responses forbid a message");
            }
        } else if self.session_id.is_some()
            || self.slot.is_some()
            || self.focus_generation.is_some()
            || self
                .message
                .as_ref()
                .is_none_or(|message| message.is_empty() || message.len() > MAX_MESSAGE_LEN)
        {
            return invalid(
                "input host hello v2",
                "rejections require a bounded message and forbid focus binding",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEventV2 {
    pub session_id: u64,
    pub slot: ControllerSlot,
    pub focus_generation: u64,
    pub event: InputEvent,
}

impl InputEventV2 {
    pub fn validate(&self) -> Result<()> {
        validate_binding(self.session_id, self.slot, self.focus_generation)?;
        self.event.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputAckV2 {
    pub session_id: u64,
    pub slot: ControllerSlot,
    pub focus_generation: u64,
    pub ack: InputAck,
}

impl InputAckV2 {
    pub fn validate(&self) -> Result<()> {
        validate_binding(self.session_id, self.slot, self.focus_generation)?;
        self.ack.validate()
    }
}

macro_rules! json_io {
    ($read:ident, $write:ident, $ty:ty) => {
        pub async fn $read<R>(reader: &mut R) -> Result<Option<$ty>>
        where
            R: AsyncRead + Unpin,
        {
            let value: Option<$ty> = read_json(reader, MAX_INPUT_MESSAGE_LEN).await?;
            if let Some(value) = &value {
                value.validate()?;
            }
            Ok(value)
        }

        pub async fn $write<W>(writer: &mut W, value: &$ty) -> Result<()>
        where
            W: AsyncWrite + Unpin,
        {
            value.validate()?;
            write_json(writer, value, MAX_INPUT_MESSAGE_LEN).await
        }
    };
}

json_io!(
    read_input_client_hello_v2,
    write_input_client_hello_v2,
    InputClientHelloV2
);
json_io!(
    read_input_host_hello_v2,
    write_input_host_hello_v2,
    InputHostHelloV2
);
json_io!(read_input_event_v2, write_input_event_v2, InputEventV2);
json_io!(read_input_ack_v2, write_input_ack_v2, InputAckV2);

fn validate_binding(session_id: u64, slot: ControllerSlot, focus_generation: u64) -> Result<()> {
    if session_id == 0 || focus_generation == 0 {
        return invalid(
            "input v2 focus binding",
            "session and focus generations must be non-zero",
        );
    }
    slot.validate()
}

fn validate_version(version: u16) -> Result<()> {
    if version != PROTOCOL_VERSION_V2 {
        return Err(ProtocolError::UnsupportedVersion {
            expected: PROTOCOL_VERSION_V2,
            actual: version,
        });
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[Capability]) -> Result<()> {
    if capabilities.len() > MAX_CAPABILITIES {
        return invalid("input hello v2", "too many capabilities");
    }
    for (index, capability) in capabilities.iter().enumerate() {
        if capabilities[..index].contains(capability) {
            return invalid("input hello v2", "duplicate capability");
        }
        if matches!(capability, Capability::VideoH264 | Capability::AudioOpus) {
            return invalid("input hello v2", "media capability is invalid on input v2");
        }
    }
    Ok(())
}

fn invalid<T>(message_type: &'static str, reason: &'static str) -> Result<T> {
    Err(ProtocolError::InvalidMessage {
        message_type,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;
    use crate::GamepadState;

    #[test]
    fn controller_slot_accepts_only_slot_zero() {
        assert_eq!(ControllerSlot::new(0).unwrap(), ControllerSlot::ZERO);
        assert!(ControllerSlot::new(1).is_err());
        assert!(serde_json::from_str::<ControllerSlot>("1").is_err());
    }

    #[test]
    fn input_v2_gamepad_json_carries_slot_and_focus_generation() {
        let event = InputEventV2 {
            session_id: 7,
            slot: ControllerSlot::ZERO,
            focus_generation: 9,
            event: InputEvent::Gamepad {
                state: GamepadState::default(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.starts_with(r#"{"session_id":7,"slot":0,"focus_generation":9,"event":{"t":"gp""#)
        );
        event.validate().unwrap();
    }

    #[tokio::test]
    async fn input_v2_hello_event_and_ack_round_trip() {
        let hello = InputClientHelloV2::new(
            "portal/0.1.0",
            [3; 16],
            7,
            ControllerSlot::ZERO,
            9,
            vec![Capability::Gamepad, Capability::InputAck],
        );
        let event = InputEventV2 {
            session_id: 7,
            slot: ControllerSlot::ZERO,
            focus_generation: 9,
            event: InputEvent::Probe,
        };
        let ack = InputAckV2 {
            session_id: 7,
            slot: ControllerSlot::ZERO,
            focus_generation: 9,
            ack: InputAck {
                sequence: 1,
                pointer_position: None,
                pointer_visible: None,
            },
        };
        let (mut sender, mut receiver) = duplex(2048);
        write_input_client_hello_v2(&mut sender, &hello)
            .await
            .unwrap();
        write_input_event_v2(&mut sender, &event).await.unwrap();
        write_input_ack_v2(&mut sender, &ack).await.unwrap();
        sender.shutdown().await.unwrap();
        assert_eq!(
            read_input_client_hello_v2(&mut receiver).await.unwrap(),
            Some(hello)
        );
        assert_eq!(
            read_input_event_v2(&mut receiver).await.unwrap(),
            Some(event)
        );
        assert_eq!(read_input_ack_v2(&mut receiver).await.unwrap(), Some(ack));
    }

    #[test]
    fn input_v2_rejects_zero_generations_unknown_fields_and_media_capabilities() {
        let mut hello =
            InputClientHelloV2::new("portal", [0; 16], 0, ControllerSlot::ZERO, 1, vec![]);
        assert!(hello.validate().is_err());
        hello.session_id = 1;
        hello.capabilities = vec![Capability::VideoH264];
        assert!(hello.validate().is_err());
        assert!(
            serde_json::from_str::<InputEventV2>(
                r#"{"session_id":1,"slot":0,"focus_generation":1,"event":{"t":"ip"},"extra":true}"#
            )
            .is_err()
        );
    }
}
