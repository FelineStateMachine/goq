use crate::{
    MAX_MEDIA_PAYLOAD_LEN, MAX_VIDEO_DIMENSION, MAX_VIDEO_PIXELS, PROTOCOL_VERSION, ProtocolError,
    Result,
};

const MEDIA_MAGIC: [u8; 4] = *b"SGV1";
/// Size of a v1 media header in bytes.
pub const MEDIA_HEADER_LEN: usize = 48;
const MEDIA_HEADER_LEN_U8: u8 = MEDIA_HEADER_LEN as u8;

/// Encoded media codec. V1 deliberately supports only H.264 video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaCodec {
    H264 = 1,
}

impl TryFrom<u8> for MediaCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::H264),
            other => Err(ProtocolError::UnsupportedCodec(other)),
        }
    }
}

/// Flags attached to one encoded video access unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameFlags(u8);

impl FrameFlags {
    pub const NONE: Self = Self(0);
    pub const KEYFRAME: Self = Self(1 << 0);
    pub const CODEC_CONFIG: Self = Self(1 << 1);
    pub const DISCONTINUITY: Self = Self(1 << 2);

    const KNOWN: u8 = Self::KEYFRAME.0 | Self::CODEC_CONFIG.0 | Self::DISCONTINUITY.0;

    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Fixed-width v1 header. Integer fields use network byte order.
///
/// `capture_timestamp_us` is microseconds from one host-monotonic media-session
/// epoch shared by every stream. A source that cannot preserve capture
/// metadata may use its post-encode observation time, but must not present that
/// value as capture or glass-to-glass latency. `pts_us` is the presentation
/// timestamp in the same host timebase and may be negative for encoder preroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaFrameHeader {
    pub codec: MediaCodec,
    pub flags: FrameFlags,
    pub width: u16,
    pub height: u16,
    pub payload_len: u32,
    pub sequence: u64,
    pub capture_timestamp_us: u64,
    pub pts_us: i64,
}

impl MediaFrameHeader {
    pub fn h264(
        width: u16,
        height: u16,
        payload_len: usize,
        sequence: u64,
        capture_timestamp_us: u64,
        pts_us: i64,
        flags: FrameFlags,
    ) -> Result<Self> {
        let payload_len =
            u32::try_from(payload_len).map_err(|_| ProtocolError::InvalidMediaPayloadLength {
                actual: payload_len,
                maximum: MAX_MEDIA_PAYLOAD_LEN,
            })?;
        let header = Self {
            codec: MediaCodec::H264,
            flags,
            width,
            height,
            payload_len,
            sequence,
            capture_timestamp_us,
            pts_us,
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
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; MEDIA_HEADER_LEN]> {
        self.validate()?;
        let mut wire = [0; MEDIA_HEADER_LEN];
        wire[0..4].copy_from_slice(&MEDIA_MAGIC);
        wire[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        wire[6] = MEDIA_HEADER_LEN_U8;
        wire[7] = self.codec as u8;
        wire[8] = self.flags.bits();
        // Bytes 9..12 are reserved and must remain zero.
        wire[12..14].copy_from_slice(&self.width.to_be_bytes());
        wire[14..16].copy_from_slice(&self.height.to_be_bytes());
        wire[16..20].copy_from_slice(&self.payload_len.to_be_bytes());
        // Bytes 20..24 are reserved and must remain zero.
        wire[24..32].copy_from_slice(&self.sequence.to_be_bytes());
        wire[32..40].copy_from_slice(&self.capture_timestamp_us.to_be_bytes());
        wire[40..48].copy_from_slice(&self.pts_us.to_be_bytes());
        Ok(wire)
    }

    pub fn decode(wire: &[u8; MEDIA_HEADER_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != MEDIA_MAGIC {
            return Err(ProtocolError::InvalidMediaMagic(magic));
        }
        let version = u16::from_be_bytes(wire[4..6].try_into().expect("fixed slice length"));
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: PROTOCOL_VERSION,
                actual: version,
            });
        }
        if wire[6] != MEDIA_HEADER_LEN_U8 {
            return Err(ProtocolError::InvalidHeaderLength {
                expected: MEDIA_HEADER_LEN_U8,
                actual: wire[6],
            });
        }
        if wire[9..12]
            .iter()
            .chain(&wire[20..24])
            .any(|byte| *byte != 0)
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "media header",
                reason: "reserved fields must be zero",
            });
        }
        let flags =
            FrameFlags::from_bits(wire[8]).ok_or(ProtocolError::UnsupportedFlags(wire[8]))?;
        let header = Self {
            codec: MediaCodec::try_from(wire[7])?,
            flags,
            width: u16::from_be_bytes(wire[12..14].try_into().expect("fixed slice length")),
            height: u16::from_be_bytes(wire[14..16].try_into().expect("fixed slice length")),
            payload_len: u32::from_be_bytes(wire[16..20].try_into().expect("fixed slice length")),
            sequence: u64::from_be_bytes(wire[24..32].try_into().expect("fixed slice length")),
            capture_timestamp_us: u64::from_be_bytes(
                wire[32..40].try_into().expect("fixed slice length"),
            ),
            pts_us: i64::from_be_bytes(wire[40..48].try_into().expect("fixed slice length")),
        };
        header.validate()?;
        Ok(header)
    }
}

/// One complete encoded access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub header: MediaFrameHeader,
    pub payload: Vec<u8>,
}

/// Encode one complete application media frame for an object transport such
/// as MoQ. The MoQ group and frame boundaries carry delivery semantics; this
/// envelope carries only Sigil's bounded codec metadata and compressed access
/// unit.
pub fn encode_media_frame_object(frame: &MediaFrame) -> Result<Vec<u8>> {
    frame.header.validate()?;
    if frame.payload.len() != frame.header.payload_len as usize {
        return Err(ProtocolError::InvalidMessage {
            message_type: "media frame object",
            reason: "payload does not match declared length",
        });
    }
    let object_len = MEDIA_HEADER_LEN.checked_add(frame.payload.len()).ok_or(
        ProtocolError::InvalidMediaPayloadLength {
            actual: frame.payload.len(),
            maximum: MAX_MEDIA_PAYLOAD_LEN,
        },
    )?;
    let mut object = Vec::with_capacity(object_len);
    object.extend_from_slice(&frame.header.encode()?);
    object.extend_from_slice(&frame.payload);
    Ok(object)
}

/// Decode exactly one application media frame from an object transport.
/// Header validation, including the declared payload bound, occurs before the
/// payload is copied into its owned buffer.
pub fn decode_media_frame_object(object: &[u8]) -> Result<MediaFrame> {
    if object.len() < MEDIA_HEADER_LEN {
        return Err(ProtocolError::InvalidMessage {
            message_type: "media frame object",
            reason: "object ended before the media header",
        });
    }
    let wire: &[u8; MEDIA_HEADER_LEN] = object[..MEDIA_HEADER_LEN]
        .try_into()
        .expect("fixed media header slice");
    let header = MediaFrameHeader::decode(wire)?;
    let expected_len = MEDIA_HEADER_LEN
        .checked_add(header.payload_len as usize)
        .ok_or(ProtocolError::InvalidMediaPayloadLength {
            actual: header.payload_len as usize,
            maximum: MAX_MEDIA_PAYLOAD_LEN,
        })?;
    if object.len() != expected_len {
        return Err(ProtocolError::InvalidMessage {
            message_type: "media frame object",
            reason: "object length does not match declared payload",
        });
    }
    MediaFrame::new(header, object[MEDIA_HEADER_LEN..].to_vec())
}

impl MediaFrame {
    pub fn new(header: MediaFrameHeader, payload: Vec<u8>) -> Result<Self> {
        header.validate()?;
        if payload.len() != header.payload_len as usize {
            return Err(ProtocolError::InvalidMessage {
                message_type: "media frame",
                reason: "payload does not match declared length",
            });
        }
        Ok(Self { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> MediaFrameHeader {
        MediaFrameHeader::h264(
            1280,
            800,
            4,
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
            -2,
            FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG),
        )
        .unwrap()
    }

    #[test]
    fn media_header_golden_vector() {
        assert_eq!(
            header().encode().unwrap(),
            [
                0x53, 0x47, 0x56, 0x31, // SGV1
                0x00, 0x01, 0x30, 0x01, // version, length, H.264
                0x03, 0x00, 0x00, 0x00, // flags, reserved
                0x05, 0x00, 0x03, 0x20, // 1280x800
                0x00, 0x00, 0x00, 0x04, // payload length
                0x00, 0x00, 0x00, 0x00, // reserved
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
                0x17, 0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
            ]
        );
    }

    #[test]
    fn header_round_trips() {
        let expected = header();
        assert_eq!(
            MediaFrameHeader::decode(&expected.encode().unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn malformed_headers_fail_closed() {
        let valid = header().encode().unwrap();
        let cases: &[(usize, u8)] = &[
            (0, 0),   // magic
            (5, 2),   // version
            (6, 47),  // header length
            (7, 9),   // codec
            (8, 128), // flags
            (9, 1),   // reserved
        ];
        for &(offset, value) in cases {
            let mut malformed = valid;
            malformed[offset] = value;
            assert!(
                MediaFrameHeader::decode(&malformed).is_err(),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn dimensions_and_payload_are_bounded() {
        assert!(MediaFrameHeader::h264(0, 800, 1, 0, 0, 0, FrameFlags::NONE).is_err());
        assert!(MediaFrameHeader::h264(8192, 8192, 1, 0, 0, 0, FrameFlags::NONE).is_err());
        assert!(MediaFrameHeader::h264(1, 1, 0, 0, 0, 0, FrameFlags::NONE).is_err());
        assert!(
            MediaFrameHeader::h264(1, 1, MAX_MEDIA_PAYLOAD_LEN + 1, 0, 0, 0, FrameFlags::NONE,)
                .is_err()
        );
    }

    #[test]
    fn object_transport_envelope_round_trips_exactly() {
        let frame = MediaFrame::new(header(), vec![0, 0, 0, 1]).unwrap();
        let object = encode_media_frame_object(&frame).unwrap();
        assert_eq!(object.len(), MEDIA_HEADER_LEN + frame.payload.len());
        assert_eq!(decode_media_frame_object(&object).unwrap(), frame);

        let mut trailing = object.clone();
        trailing.push(0);
        assert!(decode_media_frame_object(&trailing).is_err());
        assert!(decode_media_frame_object(&object[..MEDIA_HEADER_LEN - 1]).is_err());
    }
}
