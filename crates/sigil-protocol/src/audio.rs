use crate::{
    MAX_AUDIO_PAYLOAD_LEN, OPUS_CHANNELS, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, PROTOCOL_VERSION,
    ProtocolError, Result,
};

const AUDIO_MAGIC: [u8; 4] = *b"SGA1";
pub const AUDIO_HEADER_LEN: usize = 48;
const AUDIO_HEADER_LEN_U8: u8 = AUDIO_HEADER_LEN as u8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    Opus = 1,
}

impl TryFrom<u8> for AudioCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Opus),
            other => Err(ProtocolError::UnsupportedAudioCodec(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioFlags(u8);

impl AudioFlags {
    pub const NONE: Self = Self(0);
    pub const DISCONTINUITY: Self = Self(1 << 0);
    const KNOWN: u8 = Self::DISCONTINUITY.0;

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
}

/// Fixed-width header for one Opus packet carried in one QUIC datagram.
/// Integer fields use network byte order. `capture_timestamp_us` and `pts_us`
/// use the same host-monotonic media-session epoch as video timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacketHeader {
    pub codec: AudioCodec,
    pub flags: AudioFlags,
    pub channels: u8,
    pub sample_rate: u32,
    pub frame_samples: u16,
    pub payload_len: u32,
    pub sequence: u64,
    pub capture_timestamp_us: u64,
    pub pts_us: i64,
}

impl AudioPacketHeader {
    pub fn opus(
        payload_len: usize,
        sequence: u64,
        capture_timestamp_us: u64,
        pts_us: i64,
        flags: AudioFlags,
    ) -> Result<Self> {
        let payload_len =
            u32::try_from(payload_len).map_err(|_| ProtocolError::InvalidAudioPayloadLength {
                actual: payload_len,
                maximum: MAX_AUDIO_PAYLOAD_LEN,
            })?;
        let header = Self {
            codec: AudioCodec::Opus,
            flags,
            channels: OPUS_CHANNELS,
            sample_rate: OPUS_SAMPLE_RATE,
            frame_samples: OPUS_FRAME_SAMPLES,
            payload_len,
            sequence,
            capture_timestamp_us,
            pts_us,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn validate(&self) -> Result<()> {
        if self.channels != OPUS_CHANNELS
            || self.sample_rate != OPUS_SAMPLE_RATE
            || self.frame_samples != OPUS_FRAME_SAMPLES
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio header",
                reason: "v1 requires 48 kHz stereo Opus with 960 samples per packet",
            });
        }
        let payload_len = self.payload_len as usize;
        if payload_len == 0 || payload_len > MAX_AUDIO_PAYLOAD_LEN {
            return Err(ProtocolError::InvalidAudioPayloadLength {
                actual: payload_len,
                maximum: MAX_AUDIO_PAYLOAD_LEN,
            });
        }
        if AudioFlags::from_bits(self.flags.bits()).is_none() {
            return Err(ProtocolError::UnsupportedFlags(self.flags.bits()));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; AUDIO_HEADER_LEN]> {
        self.validate()?;
        let mut wire = [0; AUDIO_HEADER_LEN];
        wire[0..4].copy_from_slice(&AUDIO_MAGIC);
        wire[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        wire[6] = AUDIO_HEADER_LEN_U8;
        wire[7] = self.codec as u8;
        wire[8] = self.flags.bits();
        wire[9] = self.channels;
        // Bytes 10..12 and 18..20 are reserved and remain zero.
        wire[12..16].copy_from_slice(&self.sample_rate.to_be_bytes());
        wire[16..18].copy_from_slice(&self.frame_samples.to_be_bytes());
        wire[20..24].copy_from_slice(&self.payload_len.to_be_bytes());
        wire[24..32].copy_from_slice(&self.sequence.to_be_bytes());
        wire[32..40].copy_from_slice(&self.capture_timestamp_us.to_be_bytes());
        wire[40..48].copy_from_slice(&self.pts_us.to_be_bytes());
        Ok(wire)
    }

    pub fn decode(wire: &[u8; AUDIO_HEADER_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != AUDIO_MAGIC {
            return Err(ProtocolError::InvalidAudioMagic(magic));
        }
        let version = u16::from_be_bytes(wire[4..6].try_into().expect("fixed slice length"));
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: PROTOCOL_VERSION,
                actual: version,
            });
        }
        if wire[6] != AUDIO_HEADER_LEN_U8 {
            return Err(ProtocolError::InvalidHeaderLength {
                expected: AUDIO_HEADER_LEN_U8,
                actual: wire[6],
            });
        }
        if wire[10..12].iter().chain(&wire[18..20]).any(|b| *b != 0) {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio header",
                reason: "reserved fields must be zero",
            });
        }
        let flags =
            AudioFlags::from_bits(wire[8]).ok_or(ProtocolError::UnsupportedFlags(wire[8]))?;
        let header = Self {
            codec: AudioCodec::try_from(wire[7])?,
            flags,
            channels: wire[9],
            sample_rate: u32::from_be_bytes(wire[12..16].try_into().expect("fixed slice length")),
            frame_samples: u16::from_be_bytes(wire[16..18].try_into().expect("fixed slice length")),
            payload_len: u32::from_be_bytes(wire[20..24].try_into().expect("fixed slice length")),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub header: AudioPacketHeader,
    pub payload: Vec<u8>,
}

impl AudioPacket {
    pub fn new(header: AudioPacketHeader, payload: Vec<u8>) -> Result<Self> {
        header.validate()?;
        if payload.len() != header.payload_len as usize {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio packet",
                reason: "payload does not match declared length",
            });
        }
        Ok(Self { header, payload })
    }

    pub fn encode_datagram(&self) -> Result<Vec<u8>> {
        self.header.validate()?;
        if self.payload.len() != self.header.payload_len as usize {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio packet",
                reason: "payload does not match declared length",
            });
        }
        let mut datagram = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        datagram.extend_from_slice(&self.header.encode()?);
        datagram.extend_from_slice(&self.payload);
        Ok(datagram)
    }

    pub fn decode_datagram(datagram: &[u8]) -> Result<Self> {
        if datagram.len() < AUDIO_HEADER_LEN {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio packet",
                reason: "datagram is shorter than the audio header",
            });
        }
        let wire: &[u8; AUDIO_HEADER_LEN] = datagram[..AUDIO_HEADER_LEN]
            .try_into()
            .expect("checked fixed slice length");
        let header = AudioPacketHeader::decode(wire)?;
        if datagram.len() != AUDIO_HEADER_LEN + header.payload_len as usize {
            return Err(ProtocolError::InvalidMessage {
                message_type: "audio packet",
                reason: "datagram does not match declared payload length",
            });
        }
        Self::new(header, datagram[AUDIO_HEADER_LEN..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> AudioPacket {
        let payload = vec![0xf8, 0xff, 0xfe];
        AudioPacket::new(
            AudioPacketHeader::opus(
                payload.len(),
                42,
                123_456,
                120_000,
                AudioFlags::DISCONTINUITY,
            )
            .unwrap(),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn audio_datagram_is_a_golden_vector() {
        let datagram = packet().encode_datagram().unwrap();
        assert_eq!(datagram.len(), AUDIO_HEADER_LEN + 3);
        assert_eq!(&datagram[0..4], b"SGA1");
        assert_eq!(&datagram[4..6], &PROTOCOL_VERSION.to_be_bytes());
        assert_eq!(datagram[6], 48);
        assert_eq!(datagram[7], 1);
        assert_eq!(datagram[8], AudioFlags::DISCONTINUITY.bits());
        assert_eq!(datagram[9], 2);
        assert_eq!(&datagram[12..16], &48_000_u32.to_be_bytes());
        assert_eq!(&datagram[16..18], &960_u16.to_be_bytes());
        assert_eq!(&datagram[20..24], &3_u32.to_be_bytes());
        assert_eq!(&datagram[24..32], &42_u64.to_be_bytes());
        assert_eq!(&datagram[32..40], &123_456_u64.to_be_bytes());
        assert_eq!(&datagram[40..48], &120_000_i64.to_be_bytes());
        assert_eq!(&datagram[48..], &[0xf8, 0xff, 0xfe]);
        assert_eq!(AudioPacket::decode_datagram(&datagram).unwrap(), packet());
    }

    #[test]
    fn rejects_malformed_audio_datagrams_before_payload_use() {
        let valid = packet().encode_datagram().unwrap();
        for index in [0, 4, 6, 7, 8, 9, 10, 12, 16, 18, 20] {
            let mut corrupt = valid.clone();
            corrupt[index] ^= 0xff;
            assert!(
                AudioPacket::decode_datagram(&corrupt).is_err(),
                "accepted corruption at byte {index}"
            );
        }
        assert!(AudioPacket::decode_datagram(&valid[..AUDIO_HEADER_LEN - 1]).is_err());
        assert!(AudioPacket::decode_datagram(&valid[..valid.len() - 1]).is_err());
        let mut trailing = valid;
        trailing.push(0);
        assert!(AudioPacket::decode_datagram(&trailing).is_err());
    }

    #[test]
    fn audio_payload_and_format_are_strictly_bounded() {
        assert!(AudioPacketHeader::opus(0, 0, 0, 0, AudioFlags::NONE).is_err());
        assert!(
            AudioPacketHeader::opus(MAX_AUDIO_PAYLOAD_LEN + 1, 0, 0, 0, AudioFlags::NONE).is_err()
        );
        let mut header = packet().header;
        header.channels = 1;
        assert!(header.validate().is_err());
        header = packet().header;
        header.sample_rate = 44_100;
        assert!(header.validate().is_err());
        header = packet().header;
        header.frame_samples = 480;
        assert!(header.validate().is_err());
    }

    #[test]
    fn declared_and_actual_payload_must_match() {
        let header = AudioPacketHeader::opus(2, 0, 0, 0, AudioFlags::NONE).unwrap();
        assert!(AudioPacket::new(header, vec![1]).is_err());
    }
}
