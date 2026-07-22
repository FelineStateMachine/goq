//! Shared, platform-independent wire protocol for Sigil.
//!
//! Every allocation driven by a peer-controlled length is checked against a
//! protocol limit first. The version is carried both in the ALPN and in each
//! independently decoded header or handshake so mismatched implementations
//! fail closed.

mod audio;
mod error;
mod feedback;
mod framing;
mod handshake;
mod input;
mod invitation;
mod media;
mod media_v3;
mod moq_catalog;

pub use audio::{AUDIO_HEADER_LEN, AudioCodec, AudioFlags, AudioPacket, AudioPacketHeader};
pub use error::{ProtocolError, Result};
pub use feedback::{
    ADAPTIVE_BITRATE_DECISION_V1_LEN, AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1,
    AdaptiveBitrateStateV1, MEDIA_FEEDBACK_REPORT_V1_LEN, MediaFeedbackFlags,
    MediaFeedbackReportV1, read_adaptive_bitrate_decision_v1, read_media_feedback_report_v1,
    write_adaptive_bitrate_decision_v1, write_media_feedback_report_v1,
};
pub use handshake::{
    Capability, ClientHello, HostHello, MAX_POINTER_SURFACE_HEIGHT, MAX_POINTER_SURFACE_WIDTH,
    MIN_POINTER_SURFACE_HEIGHT, MIN_POINTER_SURFACE_WIDTH, PointerSurfaceDimensions,
    read_client_hello, read_host_hello, write_client_hello, write_host_hello,
};
pub use input::{
    GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN, GAMEPAD_TRIGGER_MAX, GamepadState, InputAck, InputEvent,
    POINTER_POSITION_MAX, POINTER_POSITION_MIN, PointerPosition, RELATIVE_POINTER_DELTA_MAX,
    RELATIVE_POINTER_DELTA_MIN, read_input_ack, read_input_event, write_input_ack,
    write_input_event,
};
pub use invitation::{
    INVITATION_CLOCK_SKEW_SECS, INVITATION_TOKEN_PREFIX, InvitationClaims, InvitationGrants,
    MAX_INVITATION_TOKEN_LEN, MAX_INVITATION_TTL_SECS, SignedInvitation,
};
pub use media::{
    FrameFlags, MEDIA_HEADER_LEN, MediaCodec, MediaFrame, MediaFrameHeader,
    decode_media_frame_object, encode_media_frame_object, read_media_frame, read_media_object,
    write_media_frame,
};
pub use media_v3::{
    KeyframeRequestReasonV3, MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    MAX_MEDIA_OBJECT_ID_V3, MEDIA_CONTROL_REQUEST_V3_LEN, MEDIA_OBJECT_V3_HEADER_LEN,
    MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MediaControlRequestTypeV3, MediaControlRequestV3,
    MediaObjectHeaderV3, MediaObjectV3, read_media_control_request_v3, read_media_object_v3,
    write_media_control_request_v3, write_media_object_v3,
};
pub use moq_catalog::{
    MAX_MOQ_CATALOG_BYTES, MOQ_CATALOG_EXTENSION_VERSION_V1, MOQ_GOP_GROUP_FORMAT_V1,
    MOQ_MEDIA_OBJECT_FORMAT_V1, MOQ_VIDEO_TRACK_PRIORITY, MoqCatalogExtensionV1,
    MoqTrackDescriptorV1, MoqVideoCatalogV1,
};

/// Protocol version encoded in v1 messages.
pub const PROTOCOL_VERSION: u16 = 1;

/// ALPN for the v1 encoded video stream.
pub const MEDIA_ALPN_V1: &[u8] = b"sigil/media/1";
/// ALPN for one encoded video object per host-opened unidirectional stream.
pub const MEDIA_ALPN_V2: &[u8] = b"sigil/media/2";
/// ALPN for custom MoQ-style grouped media objects and keyframe control.
///
/// This is a Sigil protocol and is not IETF MoQ Transport compatible.
pub const MEDIA_ALPN_V3: &[u8] = b"sigil/media/3";
/// ALPN for bounded receiver telemetry and host bitrate decisions.
pub const MEDIA_FEEDBACK_ALPN_V1: &[u8] = b"sigil/media-feedback/1";
/// ALPN for the v1 latency-independent input stream.
pub const INPUT_ALPN_V1: &[u8] = b"sigil/input/1";
/// ALPN for the v1 session-control stream.
pub const CONTROL_ALPN_V1: &[u8] = b"sigil/control/1";
/// Static upstream MoQ track carrying bounded H.264 access-unit objects.
pub const MOQ_VIDEO_H264_TRACK: &str = "video/h264";
/// ALPN for the v1 low-latency Opus datagram connection.
pub const AUDIO_ALPN_V1: &[u8] = b"sigil/audio/1";

/// The inherited frame-stream ALPN, provided only for an explicit migration
/// adapter. New peers should prefer [`MEDIA_ALPN_V2`] and may retain
/// [`MEDIA_ALPN_V1`] only as a compatibility fallback.
pub const LEGACY_FRAME_ALPN_V0: &[u8] = b"sigil/frame-stream/0";
/// The inherited input-stream ALPN, provided only for an explicit migration
/// adapter. New peers must advertise [`INPUT_ALPN_V1`].
pub const LEGACY_INPUT_ALPN_V0: &[u8] = b"sigil/input-stream/0";

/// Maximum encoded video access-unit size accepted from the network (16 MiB).
pub const MAX_MEDIA_PAYLOAD_LEN: usize = 16 * 1024 * 1024;
/// Maximum dimension carried by a v1 video header.
pub const MAX_VIDEO_DIMENSION: u16 = 8_192;
/// Maximum pixel count carried by a v1 video header (8K UHD).
pub const MAX_VIDEO_PIXELS: u32 = 7_680 * 4_320;
/// Maximum length-prefixed JSON input message.
pub const MAX_INPUT_MESSAGE_LEN: usize = 4 * 1024;
/// Maximum length-prefixed JSON handshake message.
pub const MAX_HANDSHAKE_MESSAGE_LEN: usize = 16 * 1024;
/// Maximum Opus payload carried in one QUIC datagram.
pub const MAX_AUDIO_PAYLOAD_LEN: usize = 512;
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: u8 = 2;
pub const OPUS_FRAME_SAMPLES: u16 = 960;

/// Derive the session-scoped upstream MoQ broadcast name advertised by Sigil.
///
/// The authenticated control handshake supplies the non-zero session id, so
/// Portal can derive this application namespace without accepting a
/// peer-controlled path.
pub fn media_moq_broadcast_name(session_id: u64) -> Result<String> {
    if session_id == 0 {
        return Err(ProtocolError::InvalidMessage {
            message_type: "MoQ broadcast name",
            reason: "session id must be non-zero",
        });
    }
    Ok(format!("sigil/session/{session_id}/video"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_values_are_deployment_golden_vectors() {
        assert_eq!(MEDIA_ALPN_V1, b"sigil/media/1");
        assert_eq!(MEDIA_ALPN_V2, b"sigil/media/2");
        assert_eq!(MEDIA_ALPN_V3, b"sigil/media/3");
        assert_eq!(MEDIA_FEEDBACK_ALPN_V1, b"sigil/media-feedback/1");
        assert_eq!(INPUT_ALPN_V1, b"sigil/input/1");
        assert_eq!(CONTROL_ALPN_V1, b"sigil/control/1");
        assert_eq!(AUDIO_ALPN_V1, b"sigil/audio/1");
        assert_eq!(LEGACY_FRAME_ALPN_V0, b"sigil/frame-stream/0");
        assert_eq!(LEGACY_INPUT_ALPN_V0, b"sigil/input-stream/0");
    }

    #[test]
    fn moq_media_namespace_is_session_scoped_and_stable() {
        assert_eq!(MOQ_VIDEO_H264_TRACK, "video/h264");
        assert_eq!(
            media_moq_broadcast_name(42).unwrap(),
            "sigil/session/42/video"
        );
        assert!(media_moq_broadcast_name(0).is_err());
    }
}
