use std::io;

/// Error produced while validating, encoding, or decoding protocol data.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid media magic: {0:02x?}")]
    InvalidMediaMagic([u8; 4]),

    #[error("invalid audio magic: {0:02x?}")]
    InvalidAudioMagic([u8; 4]),

    #[error("invalid invitation magic: {0:02x?}")]
    InvalidInvitationMagic([u8; 4]),

    #[error("unsupported protocol version {actual}; expected {expected}")]
    UnsupportedVersion { expected: u16, actual: u16 },

    #[error("invalid media header length {actual}; expected {expected}")]
    InvalidHeaderLength { expected: u8, actual: u8 },

    #[error("unsupported media codec {0}")]
    UnsupportedCodec(u8),

    #[error("unsupported audio codec {0}")]
    UnsupportedAudioCodec(u8),

    #[error("unsupported media flag bits 0x{0:02x}")]
    UnsupportedFlags(u8),

    #[error("invalid video dimensions {width}x{height}")]
    InvalidDimensions { width: u16, height: u16 },

    #[error("media payload length {actual} is invalid; maximum is {maximum}")]
    InvalidMediaPayloadLength { actual: usize, maximum: usize },

    #[error("audio payload length {actual} is invalid; maximum is {maximum}")]
    InvalidAudioPayloadLength { actual: usize, maximum: usize },

    #[error("message length {actual} is invalid; maximum is {maximum}")]
    InvalidMessageLength { actual: usize, maximum: usize },

    #[error("invalid {message_type}: {reason}")]
    InvalidMessage {
        message_type: &'static str,
        reason: &'static str,
    },

    #[error("invalid JSON message: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
