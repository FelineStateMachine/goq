use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::framing::{read_json, write_json};
use crate::{
    Capability, ControllerSlot, KeyframeRequestReasonV3, MAX_INVITATION_TOKEN_LEN,
    PROTOCOL_VERSION_V2, PointerSurfaceDimensions, ProtocolError, Result, SignedInvitation,
    SignedSubscriptionCapability,
};

const MAX_AGENT_LEN: usize = 128;
const MAX_REJECTION_MESSAGE_LEN: usize = 512;
const MAX_CAPABILITIES: usize = 16;
const MAX_PRESENCE_ID_LEN: usize = 32;
const MAX_BROADCAST_NAME_LEN: usize = 128;

pub const MAX_SESSION_ROSTER_VIEWERS: usize = 8;
pub const MAX_CONTROL_V2_MESSAGE_LEN: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHelloV2 {
    pub version: u16,
    pub agent: String,
    pub nonce: [u8; 16],
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<String>,
}

impl ClientHelloV2 {
    pub fn new(agent: impl Into<String>, nonce: [u8; 16], capabilities: Vec<Capability>) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
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
        if self.agent.is_empty() || self.agent.len() > MAX_AGENT_LEN {
            return invalid("client hello v2", "agent must contain 1..=128 UTF-8 bytes");
        }
        validate_capabilities(&self.capabilities)?;
        if let Some(invitation) = &self.invitation {
            if invitation.len() > MAX_INVITATION_TOKEN_LEN {
                return invalid(
                    "client hello v2",
                    "invitation exceeds the bounded token length",
                );
            }
            SignedInvitation::decode(invitation)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ViewerPresenceId(String);

impl ViewerPresenceId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_PRESENCE_ID_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return invalid(
                "viewer presence id",
                "presence id must contain 1..=32 ASCII letters, digits, or hyphens",
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<()> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPresenceV2 {
    pub presence_id: ViewerPresenceId,
    pub session_id: u64,
    pub input_capable: bool,
    pub you: bool,
}

impl ViewerPresenceV2 {
    pub fn validate(&self) -> Result<()> {
        self.presence_id.validate()?;
        if self.session_id == 0 {
            return invalid("viewer presence", "session id must be non-zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusTransitionReasonV2 {
    Initial,
    Requested,
    Released,
    Disconnected,
    Replaced,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FocusStateV2 {
    Vacant {
        slot: ControllerSlot,
    },
    Held {
        slot: ControllerSlot,
        holder: ViewerPresenceId,
        session_id: u64,
        focus_generation: u64,
    },
    Neutralizing {
        slot: ControllerSlot,
        former_holder: ViewerPresenceId,
        former_session_id: u64,
        former_focus_generation: u64,
        transition_id: u64,
    },
}

impl FocusStateV2 {
    pub fn slot(&self) -> ControllerSlot {
        match self {
            Self::Vacant { slot } | Self::Held { slot, .. } | Self::Neutralizing { slot, .. } => {
                *slot
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.slot().validate()?;
        match self {
            Self::Vacant { .. } => Ok(()),
            Self::Held {
                holder,
                session_id,
                focus_generation,
                ..
            } => {
                holder.validate()?;
                if *session_id == 0 || *focus_generation == 0 {
                    return invalid(
                        "focus state",
                        "held focus requires non-zero session and focus generations",
                    );
                }
                Ok(())
            }
            Self::Neutralizing {
                former_holder,
                former_session_id,
                former_focus_generation,
                transition_id,
                ..
            } => {
                former_holder.validate()?;
                if *former_session_id == 0 || *former_focus_generation == 0 || *transition_id == 0 {
                    return invalid(
                        "focus state",
                        "neutralizing focus requires non-zero generations and transition id",
                    );
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGenerationDescriptorV2 {
    pub generation_id: u64,
    pub broadcast_name: String,
}

impl MediaGenerationDescriptorV2 {
    pub fn validate(&self) -> Result<()> {
        if self.generation_id == 0 {
            return invalid("media generation", "generation id must be non-zero");
        }
        if self.broadcast_name.is_empty()
            || self.broadcast_name.len() > MAX_BROADCAST_NAME_LEN
            || !self.broadcast_name.is_ascii()
        {
            return invalid(
                "media generation",
                "broadcast name must contain 1..=128 ASCII bytes",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshotV2 {
    pub revision: u64,
    pub self_presence_id: ViewerPresenceId,
    pub viewers: Vec<ViewerPresenceV2>,
    pub focus: FocusStateV2,
    pub transition_reason: FocusTransitionReasonV2,
    pub media: MediaGenerationDescriptorV2,
}

impl SessionSnapshotV2 {
    pub fn validate(&self) -> Result<()> {
        if self.revision == 0 {
            return invalid("session snapshot", "revision must be non-zero");
        }
        self.self_presence_id.validate()?;
        if self.viewers.is_empty() || self.viewers.len() > MAX_SESSION_ROSTER_VIEWERS {
            return invalid("session snapshot", "roster must contain 1..=8 viewers");
        }
        let mut identities = BTreeSet::new();
        let mut self_entries = 0;
        for viewer in &self.viewers {
            viewer.validate()?;
            if !identities.insert(viewer.presence_id.clone()) {
                return invalid("session snapshot", "roster contains a duplicate identity");
            }
            if viewer.you {
                self_entries += 1;
                if viewer.presence_id != self.self_presence_id {
                    return invalid(
                        "session snapshot",
                        "the you marker does not match self identity",
                    );
                }
            }
        }
        if self_entries != 1 || !identities.contains(&self.self_presence_id) {
            return invalid(
                "session snapshot",
                "roster requires exactly one matching you entry",
            );
        }
        self.focus.validate()?;
        if let FocusStateV2::Held {
            holder, session_id, ..
        } = &self.focus
            && !self
                .viewers
                .iter()
                .any(|viewer| &viewer.presence_id == holder && viewer.session_id == *session_id)
        {
            return invalid("session snapshot", "focus holder is absent from the roster");
        }
        self.media.validate()
    }

    pub fn self_viewer(&self) -> &ViewerPresenceV2 {
        self.viewers
            .iter()
            .find(|viewer| viewer.presence_id == self.self_presence_id)
            .expect("validated snapshots contain the self viewer")
    }

    pub fn self_focus_generation(&self) -> Option<u64> {
        match &self.focus {
            FocusStateV2::Held {
                holder,
                session_id,
                focus_generation,
                ..
            } if holder == &self.self_presence_id
                && *session_id == self.self_viewer().session_id =>
            {
                Some(*focus_generation)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusCommandActionV2 {
    Request,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusCommandV2 {
    pub request_id: u64,
    pub action: FocusCommandActionV2,
    pub slot: ControllerSlot,
    pub expected_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_focus_generation: Option<u64>,
}

impl FocusCommandV2 {
    pub fn validate(&self) -> Result<()> {
        if self.request_id == 0 || self.expected_revision == 0 {
            return invalid(
                "focus command",
                "request id and expected revision must be non-zero",
            );
        }
        self.slot.validate()?;
        match self.action {
            FocusCommandActionV2::Request if self.expected_focus_generation.is_some() => invalid(
                "focus command",
                "focus requests must not carry an expected focus generation",
            ),
            FocusCommandActionV2::Release
                if self
                    .expected_focus_generation
                    .is_none_or(|generation| generation == 0) =>
            {
                invalid(
                    "focus command",
                    "focus release requires a non-zero expected focus generation",
                )
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusCommandResultV2 {
    pub request_id: u64,
    pub accepted: bool,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FocusCommandResultV2 {
    pub fn validate(&self) -> Result<()> {
        if self.request_id == 0 || self.revision == 0 {
            return invalid(
                "focus command result",
                "request id and revision must be non-zero",
            );
        }
        if let Some(message) = &self.message
            && (message.is_empty() || message.len() > MAX_REJECTION_MESSAGE_LEN)
        {
            return invalid(
                "focus command result",
                "message must contain 1..=512 UTF-8 bytes",
            );
        }
        if self.accepted && self.message.is_some() {
            return invalid(
                "focus command result",
                "accepted results must not carry a message",
            );
        }
        if !self.accepted && self.message.is_none() {
            return invalid("focus command result", "rejected results require a message");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientControlEnvelopeV2 {
    Focus {
        command: FocusCommandV2,
    },
    Keyframe {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_sequence: Option<u64>,
        reason: ControlKeyframeReasonV2,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKeyframeReasonV2 {
    Join,
    DecoderReset,
    TransportGap,
    DeliveryTimeout,
    FrontendBackpressure,
}

impl From<ControlKeyframeReasonV2> for KeyframeRequestReasonV3 {
    fn from(value: ControlKeyframeReasonV2) -> Self {
        match value {
            ControlKeyframeReasonV2::Join => Self::Join,
            ControlKeyframeReasonV2::DecoderReset => Self::DecoderReset,
            ControlKeyframeReasonV2::TransportGap => Self::TransportGap,
            ControlKeyframeReasonV2::DeliveryTimeout => Self::DeliveryTimeout,
            ControlKeyframeReasonV2::FrontendBackpressure => Self::FrontendBackpressure,
        }
    }
}

impl From<KeyframeRequestReasonV3> for ControlKeyframeReasonV2 {
    fn from(value: KeyframeRequestReasonV3) -> Self {
        match value {
            KeyframeRequestReasonV3::Join => Self::Join,
            KeyframeRequestReasonV3::DecoderReset => Self::DecoderReset,
            KeyframeRequestReasonV3::TransportGap => Self::TransportGap,
            KeyframeRequestReasonV3::DeliveryTimeout => Self::DeliveryTimeout,
            KeyframeRequestReasonV3::FrontendBackpressure => Self::FrontendBackpressure,
        }
    }
}

impl ClientControlEnvelopeV2 {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Focus { command } => command.validate(),
            Self::Keyframe { request_id, .. } if *request_id == 0 => {
                invalid("client control envelope v2", "request id must be non-zero")
            }
            Self::Keyframe { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerControlEnvelopeV2 {
    Snapshot { snapshot: SessionSnapshotV2 },
    FocusResult { result: FocusCommandResultV2 },
}

impl ServerControlEnvelopeV2 {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Snapshot { snapshot } => snapshot.validate(),
            Self::FocusResult { result } => result.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostHelloV2 {
    pub version: u16,
    pub accepted: bool,
    pub session_id: Option<u64>,
    pub capabilities: Vec<Capability>,
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SessionSnapshotV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_subscription_capability: Option<String>,
}

impl HostHelloV2 {
    pub fn accepted(
        session_id: u64,
        capabilities: Vec<Capability>,
        snapshot: SessionSnapshotV2,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
            accepted: true,
            session_id: Some(session_id),
            capabilities,
            message: None,
            pointer_surface_dimensions: None,
            snapshot: Some(snapshot),
            media_subscription_capability: None,
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION_V2,
            accepted: false,
            session_id: None,
            capabilities: Vec::new(),
            message: Some(message.into()),
            pointer_surface_dimensions: None,
            snapshot: None,
            media_subscription_capability: None,
        }
    }

    pub fn with_pointer_surface_dimensions(mut self, dimensions: PointerSurfaceDimensions) -> Self {
        self.pointer_surface_dimensions = Some(dimensions);
        self
    }

    pub fn with_media_subscription_capability(mut self, capability: impl Into<String>) -> Self {
        self.media_subscription_capability = Some(capability.into());
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_version(self.version)?;
        validate_capabilities(&self.capabilities)?;
        if self.accepted {
            let session_id = self.session_id.filter(|id| *id != 0).ok_or_else(|| {
                ProtocolError::InvalidMessage {
                    message_type: "host hello v2",
                    reason: "accepted responses require a non-zero session id",
                }
            })?;
            if self.message.is_some() {
                return invalid("host hello v2", "accepted responses forbid a message");
            }
            let snapshot = self
                .snapshot
                .as_ref()
                .ok_or(ProtocolError::InvalidMessage {
                    message_type: "host hello v2",
                    reason: "accepted responses require an initial snapshot",
                })?;
            snapshot.validate()?;
            if snapshot.self_viewer().session_id != session_id {
                return invalid(
                    "host hello v2",
                    "snapshot session does not match host hello",
                );
            }
            if let Some(capability) = &self.media_subscription_capability {
                let capability = SignedSubscriptionCapability::decode(capability)?;
                if capability.claims.media_generation_id != snapshot.media.generation_id {
                    return invalid(
                        "host hello v2",
                        "subscription capability generation does not match the snapshot",
                    );
                }
            }
        } else if self.session_id.is_some()
            || self.snapshot.is_some()
            || self.media_subscription_capability.is_some()
            || self.message.as_ref().is_none_or(|message| {
                message.is_empty() || message.len() > MAX_REJECTION_MESSAGE_LEN
            })
        {
            return invalid(
                "host hello v2",
                "rejections require a bounded message and forbid session state",
            );
        }
        if let Some(dimensions) = self.pointer_surface_dimensions {
            dimensions.validate()?;
            if !self.accepted || !self.capabilities.contains(&Capability::VideoH264) {
                return invalid(
                    "host hello v2",
                    "pointer dimensions require accepted H.264 media",
                );
            }
        }
        Ok(())
    }
}

macro_rules! json_io {
    ($read:ident, $write:ident, $ty:ty) => {
        pub async fn $read<R>(reader: &mut R) -> Result<Option<$ty>>
        where
            R: AsyncRead + Unpin,
        {
            let value: Option<$ty> = read_json(reader, MAX_CONTROL_V2_MESSAGE_LEN).await?;
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
            write_json(writer, value, MAX_CONTROL_V2_MESSAGE_LEN).await
        }
    };
}

json_io!(read_client_hello_v2, write_client_hello_v2, ClientHelloV2);
json_io!(read_host_hello_v2, write_host_hello_v2, HostHelloV2);
json_io!(
    read_client_control_v2,
    write_client_control_v2,
    ClientControlEnvelopeV2
);
json_io!(
    read_server_control_v2,
    write_server_control_v2,
    ServerControlEnvelopeV2
);

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
        return invalid("hello v2", "too many capabilities");
    }
    for (index, capability) in capabilities.iter().enumerate() {
        if capabilities[..index].contains(capability) {
            return invalid("hello v2", "duplicate capability");
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
    use crate::media_moq_broadcast_name;

    fn snapshot(revision: u64) -> SessionSnapshotV2 {
        let presence = ViewerPresenceId::new("viewer-0001").unwrap();
        SessionSnapshotV2 {
            revision,
            self_presence_id: presence.clone(),
            viewers: vec![ViewerPresenceV2 {
                presence_id: presence,
                session_id: 7,
                input_capable: true,
                you: true,
            }],
            focus: FocusStateV2::Vacant {
                slot: ControllerSlot::ZERO,
            },
            transition_reason: FocusTransitionReasonV2::Initial,
            media: MediaGenerationDescriptorV2 {
                generation_id: 7,
                broadcast_name: media_moq_broadcast_name(7).unwrap(),
            },
        }
    }

    #[test]
    fn control_v2_snapshot_json_is_a_golden_vector() {
        assert_eq!(
            serde_json::to_string(&snapshot(1)).unwrap(),
            r#"{"revision":1,"self_presence_id":"viewer-0001","viewers":[{"presence_id":"viewer-0001","session_id":7,"input_capable":true,"you":true}],"focus":{"state":"vacant","slot":0},"transition_reason":"initial","media":{"generation_id":7,"broadcast_name":"sigil/session/7/video"}}"#
        );
    }

    #[test]
    fn snapshots_reject_zero_revision_duplicate_identity_and_unsupported_slot() {
        assert!(snapshot(0).validate().is_err());
        let mut duplicate = snapshot(1);
        duplicate.viewers.push(duplicate.viewers[0].clone());
        assert!(duplicate.validate().is_err());
        assert!(serde_json::from_str::<ControllerSlot>("1").is_err());
    }

    #[test]
    fn focus_commands_are_revision_and_generation_bound() {
        let request = FocusCommandV2 {
            request_id: 1,
            action: FocusCommandActionV2::Request,
            slot: ControllerSlot::ZERO,
            expected_revision: 1,
            expected_focus_generation: None,
        };
        request.validate().unwrap();
        let mut release = request.clone();
        release.action = FocusCommandActionV2::Release;
        assert!(release.validate().is_err());
        release.expected_focus_generation = Some(2);
        release.validate().unwrap();
    }

    #[tokio::test]
    async fn v2_hello_and_envelopes_round_trip_with_clean_eof() {
        let hello = ClientHelloV2::new("portal/0.1.0", [4; 16], vec![Capability::VideoH264]);
        let host = HostHelloV2::accepted(7, vec![Capability::VideoH264], snapshot(1));
        let command = ClientControlEnvelopeV2::Focus {
            command: FocusCommandV2 {
                request_id: 2,
                action: FocusCommandActionV2::Request,
                slot: ControllerSlot::ZERO,
                expected_revision: 1,
                expected_focus_generation: None,
            },
        };
        let (mut sender, mut receiver) = duplex(4096);
        write_client_hello_v2(&mut sender, &hello).await.unwrap();
        write_host_hello_v2(&mut sender, &host).await.unwrap();
        write_client_control_v2(&mut sender, &command)
            .await
            .unwrap();
        sender.shutdown().await.unwrap();
        assert_eq!(
            read_client_hello_v2(&mut receiver).await.unwrap(),
            Some(hello)
        );
        assert_eq!(read_host_hello_v2(&mut receiver).await.unwrap(), Some(host));
        assert_eq!(
            read_client_control_v2(&mut receiver).await.unwrap(),
            Some(command)
        );
        assert_eq!(read_client_control_v2(&mut receiver).await.unwrap(), None);
    }

    #[tokio::test]
    async fn peer_controlled_v2_length_is_bounded_before_allocation() {
        let (mut sender, mut receiver) = duplex(16);
        sender
            .write_all(&((MAX_CONTROL_V2_MESSAGE_LEN as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_client_control_v2(&mut receiver).await,
            Err(ProtocolError::InvalidMessageLength { .. })
        ));
    }
}
