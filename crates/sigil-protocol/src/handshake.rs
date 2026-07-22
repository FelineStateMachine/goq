use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::framing::{read_json, write_json};
use crate::{
    MAX_HANDSHAKE_MESSAGE_LEN, MAX_INVITATION_TOKEN_LEN, PROTOCOL_VERSION, ProtocolError, Result,
    SignedInvitation,
};

const MAX_AGENT_LEN: usize = 128;
const MAX_MESSAGE_LEN: usize = 512;
const MAX_CAPABILITIES: usize = 16;
pub const MIN_POINTER_SURFACE_WIDTH: u16 = 64;
pub const MAX_POINTER_SURFACE_WIDTH: u16 = 7_680;
pub const MIN_POINTER_SURFACE_HEIGHT: u16 = 64;
pub const MAX_POINTER_SURFACE_HEIGHT: u16 = 4_320;

/// Negotiable v1 session capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    VideoH264,
    AbsolutePointer,
    RelativePointer,
    Keyboard,
    Text,
    Gamepad,
    InputAck,
    PointerPositionFeedback,
    PointerVisibilityFeedback,
    AudioOpus,
}

/// First control message sent by a connecting client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub version: u16,
    pub agent: String,
    pub nonce: [u8; 16],
    pub capabilities: Vec<Capability>,
    /// One-time enrollment invitation. It is accepted only on the first media
    /// connection and is never sent again after the host persists enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<String>,
}

/// Native coordinate space used by the host compositor for relative-pointer
/// injection. It can differ from the encoded video size when the capture
/// pipeline downscales Gamescope output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerSurfaceDimensions {
    pub width: u16,
    pub height: u16,
}

impl PointerSurfaceDimensions {
    pub fn new(width: u16, height: u16) -> Result<Self> {
        let dimensions = Self { width, height };
        dimensions.validate()?;
        Ok(dimensions)
    }

    pub fn validate(&self) -> Result<()> {
        if !(MIN_POINTER_SURFACE_WIDTH..=MAX_POINTER_SURFACE_WIDTH).contains(&self.width)
            || !(MIN_POINTER_SURFACE_HEIGHT..=MAX_POINTER_SURFACE_HEIGHT).contains(&self.height)
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "pointer surface dimensions",
                reason: "width must be 64..=7680 and height must be 64..=4320",
            });
        }
        Ok(())
    }
}

impl ClientHello {
    pub fn new(agent: impl Into<String>, nonce: [u8; 16], capabilities: Vec<Capability>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            agent: agent.into(),
            nonce,
            capabilities,
            invitation: None,
        }
    }

    pub fn with_invitation(mut self, invitation: impl Into<String>) -> Self {
        self.invitation = Some(invitation.into());
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_version(self.version)?;
        validate_agent(&self.agent)?;
        validate_capabilities(&self.capabilities)?;
        if let Some(invitation) = &self.invitation {
            if invitation.len() > MAX_INVITATION_TOKEN_LEN {
                return Err(ProtocolError::InvalidMessage {
                    message_type: "client hello",
                    reason: "invitation exceeds the bounded token length",
                });
            }
            SignedInvitation::decode(invitation)?;
        }
        Ok(())
    }
}

/// Host response to a client hello. Rejections carry no session id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostHello {
    pub version: u16,
    pub accepted: bool,
    pub session_id: Option<u64>,
    pub capabilities: Vec<Capability>,
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

impl HostHello {
    pub fn accepted(session_id: u64, capabilities: Vec<Capability>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            accepted: true,
            session_id: Some(session_id),
            capabilities,
            message: None,
            pointer_surface_dimensions: None,
        }
    }

    pub fn with_pointer_surface_dimensions(mut self, dimensions: PointerSurfaceDimensions) -> Self {
        self.pointer_surface_dimensions = Some(dimensions);
        self
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            accepted: false,
            session_id: None,
            capabilities: Vec::new(),
            message: Some(message.into()),
            pointer_surface_dimensions: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_version(self.version)?;
        validate_capabilities(&self.capabilities)?;
        if self.accepted != self.session_id.is_some() {
            return Err(ProtocolError::InvalidMessage {
                message_type: "host hello",
                reason: "accepted responses require a session id; rejections forbid one",
            });
        }
        if let Some(message) = &self.message {
            if message.is_empty() || message.len() > MAX_MESSAGE_LEN {
                return Err(ProtocolError::InvalidMessage {
                    message_type: "host hello",
                    reason: "message must contain 1..=512 UTF-8 bytes",
                });
            }
        }
        if !self.accepted && self.message.is_none() {
            return Err(ProtocolError::InvalidMessage {
                message_type: "host hello",
                reason: "rejections require a message",
            });
        }
        if let Some(dimensions) = self.pointer_surface_dimensions {
            dimensions.validate()?;
            if !self.accepted || !self.capabilities.contains(&Capability::VideoH264) {
                return Err(ProtocolError::InvalidMessage {
                    message_type: "host hello",
                    reason: "pointer surface dimensions require accepted H.264 media",
                });
            }
        }
        Ok(())
    }
}

pub async fn read_client_hello<R>(reader: &mut R) -> Result<Option<ClientHello>>
where
    R: AsyncRead + Unpin,
{
    let hello: Option<ClientHello> = read_json(reader, MAX_HANDSHAKE_MESSAGE_LEN).await?;
    if let Some(hello) = &hello {
        hello.validate()?;
    }
    Ok(hello)
}

pub async fn write_client_hello<W>(writer: &mut W, hello: &ClientHello) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    hello.validate()?;
    write_json(writer, hello, MAX_HANDSHAKE_MESSAGE_LEN).await
}

pub async fn read_host_hello<R>(reader: &mut R) -> Result<Option<HostHello>>
where
    R: AsyncRead + Unpin,
{
    let hello: Option<HostHello> = read_json(reader, MAX_HANDSHAKE_MESSAGE_LEN).await?;
    if let Some(hello) = &hello {
        hello.validate()?;
    }
    Ok(hello)
}

pub async fn write_host_hello<W>(writer: &mut W, hello: &HostHello) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    hello.validate()?;
    write_json(writer, hello, MAX_HANDSHAKE_MESSAGE_LEN).await
}

fn validate_version(version: u16) -> Result<()> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion {
            expected: PROTOCOL_VERSION,
            actual: version,
        });
    }
    Ok(())
}

fn validate_agent(agent: &str) -> Result<()> {
    if agent.is_empty() || agent.len() > MAX_AGENT_LEN {
        return Err(ProtocolError::InvalidMessage {
            message_type: "client hello",
            reason: "agent must contain 1..=128 UTF-8 bytes",
        });
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[Capability]) -> Result<()> {
    if capabilities.len() > MAX_CAPABILITIES {
        return Err(ProtocolError::InvalidMessage {
            message_type: "hello",
            reason: "too many capabilities",
        });
    }
    for (index, capability) in capabilities.iter().enumerate() {
        if capabilities[..index].contains(capability) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "hello",
                reason: "duplicate capability",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn client_hello() -> ClientHello {
        ClientHello::new(
            "sigil-client/0.1.0",
            [0x2a; 16],
            vec![
                Capability::VideoH264,
                Capability::Keyboard,
                Capability::Gamepad,
            ],
        )
    }

    #[test]
    fn client_hello_json_is_a_golden_vector() {
        assert_eq!(
            serde_json::to_string(&client_hello()).unwrap(),
            r#"{"version":1,"agent":"sigil-client/0.1.0","nonce":[42,42,42,42,42,42,42,42,42,42,42,42,42,42,42,42],"capabilities":["video_h264","keyboard","gamepad"]}"#
        );
    }

    #[test]
    fn client_hello_carries_only_a_bounded_valid_invitation() {
        let host = SigningKey::from_bytes(&[7; 32]);
        let peer = SigningKey::from_bytes(&[9; 32]);
        let claims = crate::InvitationClaims::new(
            host.verifying_key().to_bytes(),
            peer.verifying_key().to_bytes(),
            100,
            200,
            1,
            [1; 32],
            crate::InvitationGrants::VIEW,
        )
        .unwrap();
        let token = crate::SignedInvitation::issue(claims, &[7; 32])
            .unwrap()
            .encode();
        let hello = ClientHello::new("portal", [0; 16], vec![Capability::VideoH264])
            .with_invitation(token.clone());
        hello.validate().unwrap();
        assert_eq!(hello.invitation.as_deref(), Some(token.as_str()));

        let mut malformed = hello;
        malformed.invitation = Some("goq-invite-v1.invalid".into());
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn relative_pointer_capability_json_is_a_golden_vector() {
        assert_eq!(
            serde_json::to_string(&Capability::RelativePointer).unwrap(),
            r#""relative_pointer""#
        );
        assert_eq!(
            serde_json::to_string(&Capability::PointerPositionFeedback).unwrap(),
            r#""pointer_position_feedback""#
        );
        assert_eq!(
            serde_json::to_string(&Capability::PointerVisibilityFeedback).unwrap(),
            r#""pointer_visibility_feedback""#
        );
    }

    #[test]
    fn media_hello_carries_bounded_pointer_surface_dimensions() {
        let hello = HostHello::accepted(42, vec![Capability::VideoH264])
            .with_pointer_surface_dimensions(PointerSurfaceDimensions::new(2_560, 1_600).unwrap());
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"version":1,"accepted":true,"session_id":42,"capabilities":["video_h264"],"message":null,"pointer_surface_dimensions":{"width":2560,"height":1600}}"#
        );
        assert_eq!(
            serde_json::from_str::<HostHello>(
                r#"{"version":1,"accepted":true,"session_id":42,"capabilities":["video_h264"],"message":null}"#,
            )
            .unwrap()
            .pointer_surface_dimensions,
            None,
            "a new client must accept an old host that omits the optional field",
        );
    }

    #[test]
    fn pointer_surface_dimensions_fail_closed() {
        assert!(PointerSurfaceDimensions::new(63, 800).is_err());
        assert!(PointerSurfaceDimensions::new(1_280, 4_321).is_err());
        assert!(
            HostHello::accepted(42, vec![Capability::Keyboard])
                .with_pointer_surface_dimensions(PointerSurfaceDimensions::new(1_280, 800).unwrap())
                .validate()
                .is_err()
        );
    }

    #[tokio::test]
    async fn both_hello_directions_round_trip() {
        let client = client_hello();
        let host = HostHello::accepted(42, vec![Capability::VideoH264]);
        let (mut sender, mut receiver) = duplex(1024);
        write_client_hello(&mut sender, &client).await.unwrap();
        write_host_hello(&mut sender, &host).await.unwrap();
        sender.shutdown().await.unwrap();

        assert_eq!(
            read_client_hello(&mut receiver).await.unwrap(),
            Some(client)
        );
        assert_eq!(read_host_hello(&mut receiver).await.unwrap(), Some(host));
        assert_eq!(read_host_hello(&mut receiver).await.unwrap(), None);
    }

    #[test]
    fn hello_validation_fails_closed() {
        let mut bad_version = client_hello();
        bad_version.version = 0;
        assert!(matches!(
            bad_version.validate(),
            Err(ProtocolError::UnsupportedVersion { .. })
        ));

        let duplicate = ClientHello::new(
            "client",
            [0; 16],
            vec![Capability::Keyboard, Capability::Keyboard],
        );
        assert!(duplicate.validate().is_err());
        assert!(HostHello::rejected("").validate().is_err());
        assert!(
            HostHello {
                version: PROTOCOL_VERSION,
                accepted: true,
                session_id: None,
                capabilities: vec![],
                message: None,
                pointer_surface_dimensions: None,
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn peer_controlled_handshake_length_is_bounded() {
        let (mut sender, mut receiver) = duplex(16);
        sender
            .write_all(&((MAX_HANDSHAKE_MESSAGE_LEN as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_client_hello(&mut receiver).await,
            Err(ProtocolError::InvalidMessageLength { .. })
        ));
    }
}
