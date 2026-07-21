//! Shared, platform-independent wire protocol for Sigil Spark.
//!
//! Every allocation driven by a peer-controlled length is checked against a
//! protocol limit first. The version is carried both in the ALPN and in each
//! independently decoded header or handshake so mismatched implementations
//! fail closed.

mod audio;
mod error;
mod framing;
mod handshake;
mod input;
mod media;

pub use audio::{AUDIO_HEADER_LEN, AudioCodec, AudioFlags, AudioPacket, AudioPacketHeader};
pub use error::{ProtocolError, Result};
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
pub use media::{
    FrameFlags, MEDIA_HEADER_LEN, MediaCodec, MediaFrame, MediaFrameHeader, read_media_frame,
    write_media_frame,
};

/// Protocol version encoded in v1 messages.
pub const PROTOCOL_VERSION: u16 = 1;

/// ALPN for the v1 encoded video stream.
pub const MEDIA_ALPN_V1: &[u8] = b"sigil/media/1";
/// ALPN for the v1 latency-independent input stream.
pub const INPUT_ALPN_V1: &[u8] = b"sigil/input/1";
/// ALPN for the v1 session-control stream.
pub const CONTROL_ALPN_V1: &[u8] = b"sigil/control/1";
/// ALPN for the v1 low-latency Opus datagram connection.
pub const AUDIO_ALPN_V1: &[u8] = b"sigil/audio/1";

/// The inherited frame-stream ALPN, provided only for an explicit migration
/// adapter. New peers must advertise [`MEDIA_ALPN_V1`].
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_values_are_deployment_golden_vectors() {
        assert_eq!(MEDIA_ALPN_V1, b"sigil/media/1");
        assert_eq!(INPUT_ALPN_V1, b"sigil/input/1");
        assert_eq!(CONTROL_ALPN_V1, b"sigil/control/1");
        assert_eq!(AUDIO_ALPN_V1, b"sigil/audio/1");
        assert_eq!(LEGACY_FRAME_ALPN_V0, b"sigil/frame-stream/0");
        assert_eq!(LEGACY_INPUT_ALPN_V0, b"sigil/input-stream/0");
    }
}
