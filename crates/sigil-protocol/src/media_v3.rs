//! Custom MoQ-style media objects used by `sigil/media/3`.
//!
//! This wire format borrows MoQ's group, object, publisher-priority, and
//! delivery-timeout semantics. It is intentionally not an implementation of
//! the IETF MoQ Transport protocol.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    FrameFlags, MAX_MEDIA_PAYLOAD_LEN, MAX_VIDEO_DIMENSION, MAX_VIDEO_PIXELS, MediaCodec,
    ProtocolError, Result,
};

const MEDIA_OBJECT_V3_MAGIC: [u8; 4] = *b"SGO3";
const MEDIA_CONTROL_V3_MAGIC: [u8; 4] = *b"SGC3";
const MEDIA_V3_VERSION: u16 = 3;

/// Size of a v3 media-object header in bytes.
pub const MEDIA_OBJECT_V3_HEADER_LEN: usize = 64;
const MEDIA_OBJECT_V3_HEADER_LEN_U16: u16 = MEDIA_OBJECT_V3_HEADER_LEN as u16;
/// Size of a v3 media-control request in bytes.
pub const MEDIA_CONTROL_REQUEST_V3_LEN: usize = 28;
const MEDIA_CONTROL_REQUEST_V3_LEN_U16: u16 = MEDIA_CONTROL_REQUEST_V3_LEN as u16;

/// Lowest delivery timeout a v3 peer may place on an object.
pub const MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS: u32 = 16;
/// Highest delivery timeout a v3 peer may place on an object.
pub const MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS: u32 = 1_000;
/// Highest object identifier in a v3 group.
pub const MAX_MEDIA_OBJECT_ID_V3: u32 = 255;
/// Maximum aggregate encoded payload retained or accepted for one v3 group.
pub const MAX_MEDIA_GROUP_BYTES_V3: usize = 32 * 1024 * 1024;

/// Fixed-width header for one independently streamed v3 encoded access unit.
///
/// Integer fields use network byte order. A lower `publisher_priority` value
/// denotes a higher priority. `group_id` identifies one encoded GOP and
/// `object_id` identifies the access unit within that GOP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaObjectHeaderV3 {
    pub codec: MediaCodec,
    pub publisher_priority: u8,
    pub flags: FrameFlags,
    pub width: u16,
    pub height: u16,
    pub payload_len: u32,
    pub object_id: u32,
    pub group_id: u64,
    pub sequence: u64,
    pub capture_timestamp_us: u64,
    pub pts_us: i64,
    pub delivery_timeout_ms: u32,
}

impl MediaObjectHeaderV3 {
    #[allow(clippy::too_many_arguments)]
    pub fn h264(
        width: u16,
        height: u16,
        payload_len: usize,
        publisher_priority: u8,
        flags: FrameFlags,
        object_id: u32,
        group_id: u64,
        sequence: u64,
        capture_timestamp_us: u64,
        pts_us: i64,
        delivery_timeout_ms: u32,
    ) -> Result<Self> {
        let payload_len =
            u32::try_from(payload_len).map_err(|_| ProtocolError::InvalidMediaPayloadLength {
                actual: payload_len,
                maximum: MAX_MEDIA_PAYLOAD_LEN,
            })?;
        let header = Self {
            codec: MediaCodec::H264,
            publisher_priority,
            flags,
            width,
            height,
            payload_len,
            object_id,
            group_id,
            sequence,
            capture_timestamp_us,
            pts_us,
            delivery_timeout_ms,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn validate(&self) -> Result<()> {
        let pixels = u32::from(self.width) * u32::from(self.height);
        if self.width == 0
            || self.height == 0
            || self.width > MAX_VIDEO_DIMENSION
            || self.height > MAX_VIDEO_DIMENSION
            || pixels > MAX_VIDEO_PIXELS
        {
            return Err(ProtocolError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }

        let payload_len = self.payload_len as usize;
        if payload_len == 0 || payload_len > MAX_MEDIA_PAYLOAD_LEN {
            return Err(ProtocolError::InvalidMediaPayloadLength {
                actual: payload_len,
                maximum: MAX_MEDIA_PAYLOAD_LEN,
            });
        }
        if FrameFlags::from_bits(self.flags.bits()).is_none() {
            return Err(ProtocolError::UnsupportedFlags(self.flags.bits()));
        }
        if self.object_id > MAX_MEDIA_OBJECT_ID_V3 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "object id must be 0..=255",
            });
        }
        if !(MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS..=MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS)
            .contains(&self.delivery_timeout_ms)
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "delivery timeout must be 16..=1000 milliseconds",
            });
        }

        let keyframe = self.flags.contains(FrameFlags::KEYFRAME);
        let codec_config = self.flags.contains(FrameFlags::CODEC_CONFIG);
        if self.object_id == 0 && !(keyframe && codec_config) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "object zero must be a keyframe carrying codec configuration",
            });
        }
        if self.object_id != 0
            && (keyframe || codec_config || self.flags.contains(FrameFlags::DISCONTINUITY))
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "nonzero objects must not carry recovery-boundary flags",
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; MEDIA_OBJECT_V3_HEADER_LEN]> {
        self.validate()?;
        let mut wire = [0_u8; MEDIA_OBJECT_V3_HEADER_LEN];
        wire[0..4].copy_from_slice(&MEDIA_OBJECT_V3_MAGIC);
        wire[4..6].copy_from_slice(&MEDIA_V3_VERSION.to_be_bytes());
        wire[6..8].copy_from_slice(&MEDIA_OBJECT_V3_HEADER_LEN_U16.to_be_bytes());
        wire[8] = self.codec as u8;
        wire[9] = self.publisher_priority;
        wire[10] = self.flags.bits();
        // Byte 11 is reserved and must remain zero.
        wire[12..14].copy_from_slice(&self.width.to_be_bytes());
        wire[14..16].copy_from_slice(&self.height.to_be_bytes());
        wire[16..20].copy_from_slice(&self.payload_len.to_be_bytes());
        wire[20..24].copy_from_slice(&self.object_id.to_be_bytes());
        wire[24..32].copy_from_slice(&self.group_id.to_be_bytes());
        wire[32..40].copy_from_slice(&self.sequence.to_be_bytes());
        wire[40..48].copy_from_slice(&self.capture_timestamp_us.to_be_bytes());
        wire[48..56].copy_from_slice(&self.pts_us.to_be_bytes());
        wire[56..60].copy_from_slice(&self.delivery_timeout_ms.to_be_bytes());
        // Bytes 60..64 are reserved and must remain zero.
        Ok(wire)
    }

    pub fn decode(wire: &[u8; MEDIA_OBJECT_V3_HEADER_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != MEDIA_OBJECT_V3_MAGIC {
            return Err(ProtocolError::InvalidMediaMagic(magic));
        }
        let version = u16::from_be_bytes(wire[4..6].try_into().expect("fixed slice length"));
        if version != MEDIA_V3_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: MEDIA_V3_VERSION,
                actual: version,
            });
        }
        let header_len = u16::from_be_bytes(wire[6..8].try_into().expect("fixed slice length"));
        if header_len != MEDIA_OBJECT_V3_HEADER_LEN_U16 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "header length must be 64 bytes",
            });
        }
        if wire[11] != 0 || wire[60..64].iter().any(|byte| *byte != 0) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object header",
                reason: "reserved fields must be zero",
            });
        }
        let flags =
            FrameFlags::from_bits(wire[10]).ok_or(ProtocolError::UnsupportedFlags(wire[10]))?;
        let header = Self {
            codec: MediaCodec::try_from(wire[8])?,
            publisher_priority: wire[9],
            flags,
            width: u16::from_be_bytes(wire[12..14].try_into().expect("fixed slice length")),
            height: u16::from_be_bytes(wire[14..16].try_into().expect("fixed slice length")),
            payload_len: u32::from_be_bytes(wire[16..20].try_into().expect("fixed slice length")),
            object_id: u32::from_be_bytes(wire[20..24].try_into().expect("fixed slice length")),
            group_id: u64::from_be_bytes(wire[24..32].try_into().expect("fixed slice length")),
            sequence: u64::from_be_bytes(wire[32..40].try_into().expect("fixed slice length")),
            capture_timestamp_us: u64::from_be_bytes(
                wire[40..48].try_into().expect("fixed slice length"),
            ),
            pts_us: i64::from_be_bytes(wire[48..56].try_into().expect("fixed slice length")),
            delivery_timeout_ms: u32::from_be_bytes(
                wire[56..60].try_into().expect("fixed slice length"),
            ),
        };
        header.validate()?;
        Ok(header)
    }
}

/// One complete v3 media object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaObjectV3 {
    pub header: MediaObjectHeaderV3,
    pub payload: Vec<u8>,
}

impl MediaObjectV3 {
    pub fn new(header: MediaObjectHeaderV3, payload: Vec<u8>) -> Result<Self> {
        header.validate()?;
        if payload.len() != header.payload_len as usize {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object",
                reason: "payload does not match declared length",
            });
        }
        Ok(Self { header, payload })
    }
}

/// Read exactly one complete v3 media object followed by a clean stream EOF.
pub async fn read_media_object_v3<R>(reader: &mut R) -> Result<MediaObjectV3>
where
    R: AsyncRead + Unpin,
{
    let mut wire = [0_u8; MEDIA_OBJECT_V3_HEADER_LEN];
    reader.read_exact(&mut wire).await?;
    let header = MediaObjectHeaderV3::decode(&wire)?;

    // Header validation, including the peer-controlled payload bound, occurs
    // before this allocation.
    let mut payload = vec![0_u8; header.payload_len as usize];
    reader.read_exact(&mut payload).await?;
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing).await? != 0 {
        return Err(ProtocolError::InvalidMessage {
            message_type: "v3 media object",
            reason: "trailing bytes after the media object",
        });
    }
    Ok(MediaObjectV3 { header, payload })
}

/// Validate and write one complete v3 media object.
pub async fn write_media_object_v3<W>(writer: &mut W, object: &MediaObjectV3) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    object.header.validate()?;
    if object.payload.len() != object.header.payload_len as usize {
        return Err(ProtocolError::InvalidMessage {
            message_type: "v3 media object",
            reason: "payload does not match declared length",
        });
    }
    writer.write_all(&object.header.encode()?).await?;
    writer.write_all(&object.payload).await?;
    writer.flush().await?;
    Ok(())
}

/// V3 media-control request type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaControlRequestTypeV3 {
    RequestKeyframe = 1,
}

impl TryFrom<u8> for MediaControlRequestTypeV3 {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::RequestKeyframe),
            _ => Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "unknown request type",
            }),
        }
    }
}

/// Why Portal is asking Sigil for a new independently decodable frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyframeRequestReasonV3 {
    Join = 1,
    TransportGap = 2,
    DeliveryTimeout = 3,
    DecoderReset = 4,
    FrontendBackpressure = 5,
}

impl TryFrom<u8> for KeyframeRequestReasonV3 {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Join),
            2 => Ok(Self::TransportGap),
            3 => Ok(Self::DeliveryTimeout),
            4 => Ok(Self::DecoderReset),
            5 => Ok(Self::FrontendBackpressure),
            _ => Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "unknown keyframe request reason",
            }),
        }
    }
}

/// Fixed-width client-to-host keyframe request for the v3 control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaControlRequestV3 {
    pub request_type: MediaControlRequestTypeV3,
    pub reason: KeyframeRequestReasonV3,
    pub request_id: u64,
    /// Last globally sequenced object accepted by the client, or `None` when
    /// no object has been accepted yet.
    pub last_sequence: Option<u64>,
}

impl MediaControlRequestV3 {
    pub const fn request_keyframe(
        request_id: u64,
        last_sequence: Option<u64>,
        reason: KeyframeRequestReasonV3,
    ) -> Self {
        Self {
            request_type: MediaControlRequestTypeV3::RequestKeyframe,
            reason,
            request_id,
            last_sequence,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.request_type != MediaControlRequestTypeV3::RequestKeyframe {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "unknown request type",
            });
        }
        if self.last_sequence == Some(u64::MAX) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "the all-ones last sequence is reserved for none",
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; MEDIA_CONTROL_REQUEST_V3_LEN]> {
        self.validate()?;
        let mut wire = [0_u8; MEDIA_CONTROL_REQUEST_V3_LEN];
        wire[0..4].copy_from_slice(&MEDIA_CONTROL_V3_MAGIC);
        wire[4..6].copy_from_slice(&MEDIA_V3_VERSION.to_be_bytes());
        wire[6..8].copy_from_slice(&MEDIA_CONTROL_REQUEST_V3_LEN_U16.to_be_bytes());
        wire[8] = self.request_type as u8;
        wire[9] = self.reason as u8;
        // Bytes 10..12 are reserved and must remain zero.
        wire[12..20].copy_from_slice(&self.request_id.to_be_bytes());
        wire[20..28].copy_from_slice(&self.last_sequence.unwrap_or(u64::MAX).to_be_bytes());
        Ok(wire)
    }

    pub fn decode(wire: &[u8; MEDIA_CONTROL_REQUEST_V3_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != MEDIA_CONTROL_V3_MAGIC {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "invalid control magic",
            });
        }
        let version = u16::from_be_bytes(wire[4..6].try_into().expect("fixed slice length"));
        if version != MEDIA_V3_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: MEDIA_V3_VERSION,
                actual: version,
            });
        }
        let request_len = u16::from_be_bytes(wire[6..8].try_into().expect("fixed slice length"));
        if request_len != MEDIA_CONTROL_REQUEST_V3_LEN_U16 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "request length must be 28 bytes",
            });
        }
        if wire[10..12].iter().any(|byte| *byte != 0) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "v3 media control request",
                reason: "reserved fields must be zero",
            });
        }
        let last_sequence =
            u64::from_be_bytes(wire[20..28].try_into().expect("fixed slice length"));
        let request = Self {
            request_type: MediaControlRequestTypeV3::try_from(wire[8])?,
            reason: KeyframeRequestReasonV3::try_from(wire[9])?,
            request_id: u64::from_be_bytes(wire[12..20].try_into().expect("fixed slice length")),
            last_sequence: (last_sequence != u64::MAX).then_some(last_sequence),
        };
        request.validate()?;
        Ok(request)
    }
}

/// Read one fixed-width v3 media-control request. Clean EOF returns `None`.
pub async fn read_media_control_request_v3<R>(
    reader: &mut R,
) -> Result<Option<MediaControlRequestV3>>
where
    R: AsyncRead + Unpin,
{
    let mut wire = [0_u8; MEDIA_CONTROL_REQUEST_V3_LEN];
    let read = reader.read(&mut wire[..1]).await?;
    if read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut wire[1..]).await?;
    MediaControlRequestV3::decode(&wire).map(Some)
}

/// Validate and write one fixed-width v3 media-control request.
pub async fn write_media_control_request_v3<W>(
    writer: &mut W,
    request: &MediaControlRequestV3,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&request.encode()?).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn keyframe_header() -> MediaObjectHeaderV3 {
        MediaObjectHeaderV3::h264(
            1280,
            800,
            4,
            32,
            FrameFlags::KEYFRAME
                .union(FrameFlags::CODEC_CONFIG)
                .union(FrameFlags::DISCONTINUITY),
            0,
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
            0x2122_2324_2526_2728,
            -2,
            100,
        )
        .unwrap()
    }

    #[test]
    fn media_object_v3_header_golden_vector() {
        assert_eq!(
            keyframe_header().encode().unwrap(),
            [
                0x53, 0x47, 0x4f, 0x33, // SGO3
                0x00, 0x03, 0x00, 0x40, // version, header length
                0x01, 0x20, 0x07, 0x00, // H.264, priority, flags, reserved
                0x05, 0x00, 0x03, 0x20, // 1280x800
                0x00, 0x00, 0x00, 0x04, // payload length
                0x00, 0x00, 0x00, 0x00, // object id
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // group id
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // sequence
                0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, // capture timestamp
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, // PTS
                0x00, 0x00, 0x00, 0x64, // delivery timeout
                0x00, 0x00, 0x00, 0x00, // reserved
            ]
        );
    }

    #[test]
    fn media_object_v3_header_round_trips() {
        let expected = keyframe_header();
        assert_eq!(
            MediaObjectHeaderV3::decode(&expected.encode().unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn media_object_v3_malformed_headers_fail_closed() {
        let valid = keyframe_header().encode().unwrap();
        let cases: &[(usize, u8)] = &[
            (0, 0),    // magic
            (5, 2),    // version
            (7, 63),   // header length
            (8, 9),    // codec
            (10, 128), // flags
            (11, 1),   // reserved
            (12, 0),   // zero width
            (59, 15),  // delivery timeout below minimum
            (60, 1),   // reserved
        ];
        for &(offset, value) in cases {
            let mut malformed = valid;
            malformed[offset] = value;
            assert!(
                MediaObjectHeaderV3::decode(&malformed).is_err(),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn media_object_v3_enforces_group_boundaries_and_limits() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        assert!(
            MediaObjectHeaderV3::h264(
                1280,
                800,
                1,
                0,
                FrameFlags::NONE,
                255,
                1,
                1,
                1,
                1,
                MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
            )
            .is_ok()
        );
        assert!(
            MediaObjectHeaderV3::h264(1280, 800, 1, 0, FrameFlags::NONE, 0, 1, 1, 1, 1, 16)
                .is_err()
        );
        assert!(
            MediaObjectHeaderV3::h264(1280, 800, 1, 0, FrameFlags::KEYFRAME, 0, 1, 1, 1, 1, 16)
                .is_err()
        );
        assert!(MediaObjectHeaderV3::h264(1280, 800, 1, 0, keyframe, 256, 1, 1, 1, 1, 16).is_err());
        assert!(
            MediaObjectHeaderV3::h264(1280, 800, 1, 0, FrameFlags::NONE, 1, 1, 2, 2, 2, 16).is_ok()
        );
        assert!(
            MediaObjectHeaderV3::h264(
                1280,
                800,
                1,
                0,
                FrameFlags::DISCONTINUITY,
                1,
                1,
                2,
                2,
                2,
                16,
            )
            .is_err()
        );
        assert!(
            MediaObjectHeaderV3::h264(1280, 800, 1, 0, FrameFlags::NONE, 1, 1, 2, 2, 2, 15)
                .is_err()
        );
        assert!(
            MediaObjectHeaderV3::h264(1280, 800, 1, 0, FrameFlags::NONE, 1, 1, 2, 2, 2, 1_001)
                .is_err()
        );
        assert!(
            MediaObjectHeaderV3::h264(
                1280,
                800,
                1,
                u8::MAX,
                FrameFlags::NONE,
                1,
                1,
                2,
                2,
                2,
                MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
            )
            .is_ok()
        );
        assert!(MediaObjectHeaderV3::h264(1280, 800, 0, 0, keyframe, 0, 1, 1, 1, 1, 16).is_err());
        assert!(
            MediaObjectHeaderV3::h264(
                1280,
                800,
                MAX_MEDIA_PAYLOAD_LEN + 1,
                0,
                keyframe,
                0,
                1,
                1,
                1,
                1,
                16
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn media_object_v3_round_trips_with_exact_eof() {
        let expected = MediaObjectV3::new(keyframe_header(), vec![0, 0, 0, 1]).unwrap();
        let (mut sender, mut receiver) = duplex(256);
        write_media_object_v3(&mut sender, &expected).await.unwrap();
        sender.shutdown().await.unwrap();
        assert_eq!(read_media_object_v3(&mut receiver).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn media_object_v3_rejects_empty_truncated_and_trailing_streams() {
        let (mut empty_sender, mut empty_receiver) = duplex(16);
        empty_sender.shutdown().await.unwrap();
        assert!(matches!(
            read_media_object_v3(&mut empty_receiver).await,
            Err(ProtocolError::Io(ref error)) if error.kind() == ErrorKind::UnexpectedEof
        ));

        let mut header = keyframe_header();
        header.payload_len = 2;
        let (mut short_sender, mut short_receiver) = duplex(128);
        short_sender
            .write_all(&header.encode().unwrap())
            .await
            .unwrap();
        short_sender.write_all(&[1]).await.unwrap();
        short_sender.shutdown().await.unwrap();
        assert!(matches!(
            read_media_object_v3(&mut short_receiver).await,
            Err(ProtocolError::Io(ref error)) if error.kind() == ErrorKind::UnexpectedEof
        ));

        let object = MediaObjectV3::new(keyframe_header(), vec![0, 0, 0, 1]).unwrap();
        let (mut sender, mut receiver) = duplex(256);
        write_media_object_v3(&mut sender, &object).await.unwrap();
        sender.write_all(&[0xff]).await.unwrap();
        sender.shutdown().await.unwrap();
        assert!(matches!(
            read_media_object_v3(&mut receiver).await,
            Err(ProtocolError::InvalidMessage {
                message_type: "v3 media object",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn media_object_v3_rejects_oversized_payload_before_body_read() {
        let mut wire = keyframe_header().encode().unwrap();
        wire[16..20].copy_from_slice(&((MAX_MEDIA_PAYLOAD_LEN as u32) + 1).to_be_bytes());
        let (mut sender, mut receiver) = duplex(128);
        sender.write_all(&wire).await.unwrap();
        assert!(matches!(
            read_media_object_v3(&mut receiver).await,
            Err(ProtocolError::InvalidMediaPayloadLength { .. })
        ));
    }

    fn control_request(last_sequence: Option<u64>) -> MediaControlRequestV3 {
        MediaControlRequestV3::request_keyframe(
            0x0102_0304_0506_0708,
            last_sequence,
            KeyframeRequestReasonV3::FrontendBackpressure,
        )
    }

    #[test]
    fn media_control_request_v3_golden_vector() {
        assert_eq!(
            control_request(Some(0x1112_1314_1516_1718))
                .encode()
                .unwrap(),
            [
                0x53, 0x47, 0x43, 0x33, // SGC3
                0x00, 0x03, 0x00, 0x1c, // version, request length
                0x01, 0x05, 0x00, 0x00, // type, reason, reserved
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // request id
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // last sequence
            ]
        );
    }

    #[test]
    fn media_control_request_v3_round_trips_and_preserves_none() {
        for expected in [control_request(Some(42)), control_request(None)] {
            assert_eq!(
                MediaControlRequestV3::decode(&expected.encode().unwrap()).unwrap(),
                expected
            );
        }
        assert_eq!(&control_request(None).encode().unwrap()[20..28], &[0xff; 8]);
        assert!(
            MediaControlRequestV3::request_keyframe(
                1,
                Some(u64::MAX),
                KeyframeRequestReasonV3::Join,
            )
            .encode()
            .is_err()
        );
    }

    #[test]
    fn media_control_request_v3_rejects_malformed_and_unknown_fields() {
        let valid = control_request(Some(1)).encode().unwrap();
        let cases: &[(usize, u8)] = &[
            (0, 0),  // magic
            (5, 2),  // version
            (7, 27), // request length
            (8, 2),  // unknown type
            (9, 6),  // unknown reason
            (10, 1), // reserved
        ];
        for &(offset, value) in cases {
            let mut malformed = valid;
            malformed[offset] = value;
            assert!(
                MediaControlRequestV3::decode(&malformed).is_err(),
                "offset {offset}"
            );
        }
    }

    #[tokio::test]
    async fn media_control_request_v3_reads_fixed_messages_and_clean_eof() {
        let expected = control_request(Some(9));
        let (mut sender, mut receiver) = duplex(128);
        write_media_control_request_v3(&mut sender, &expected)
            .await
            .unwrap();
        sender.shutdown().await.unwrap();
        assert_eq!(
            read_media_control_request_v3(&mut receiver).await.unwrap(),
            Some(expected)
        );
        assert_eq!(
            read_media_control_request_v3(&mut receiver).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn media_control_request_v3_rejects_truncation() {
        let wire = control_request(Some(9)).encode().unwrap();
        let (mut sender, mut receiver) = duplex(128);
        sender.write_all(&wire[..27]).await.unwrap();
        sender.shutdown().await.unwrap();
        assert!(matches!(
            read_media_control_request_v3(&mut receiver).await,
            Err(ProtocolError::Io(ref error)) if error.kind() == ErrorKind::UnexpectedEof
        ));
    }
}
