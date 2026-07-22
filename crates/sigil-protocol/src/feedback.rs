//! Bounded receiver feedback and adaptive-bitrate decisions.
//!
//! The feedback ALPN uses one direction for receiver reports and the other for
//! host decisions. Both records are fixed width so a peer cannot influence an
//! allocation or leave a decoder waiting for a peer-declared payload length.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{PROTOCOL_VERSION, ProtocolError, Result};

const MEDIA_FEEDBACK_REPORT_V1_MAGIC: [u8; 4] = *b"SGR1";
const ADAPTIVE_BITRATE_DECISION_V1_MAGIC: [u8; 4] = *b"SGD1";

/// Size of one receiver feedback report in bytes.
pub const MEDIA_FEEDBACK_REPORT_V1_LEN: usize = 64;
const MEDIA_FEEDBACK_REPORT_V1_LEN_U16: u16 = MEDIA_FEEDBACK_REPORT_V1_LEN as u16;
/// Size of one host adaptive-bitrate decision in bytes.
pub const ADAPTIVE_BITRATE_DECISION_V1_LEN: usize = 40;
const ADAPTIVE_BITRATE_DECISION_V1_LEN_U16: u16 = ADAPTIVE_BITRATE_DECISION_V1_LEN as u16;

const MIN_FEEDBACK_INTERVAL_MS: u16 = 250;
const MAX_FEEDBACK_INTERVAL_MS: u16 = 5_000;
const MAX_FEEDBACK_QUEUE_CAPACITY: u8 = 16;
const OPTIONAL_SEQUENCE_NONE: u64 = u64::MAX;
const OPTIONAL_LATENCY_NONE: u16 = u16::MAX;
const MAX_FEEDBACK_LATENCY_MS: u16 = 60_000;
const MIN_ADAPTIVE_BITRATE_KBPS: u32 = 1_000;
const MAX_ADAPTIVE_BITRATE_KBPS: u32 = 100_000;

/// State flags reported by Portal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaFeedbackFlags(u8);

impl MediaFeedbackFlags {
    pub const NONE: Self = Self(0);
    pub const RESYNC_ACTIVE: Self = Self(1 << 0);

    const KNOWN: u8 = Self::RESYNC_ACTIVE.0;

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

/// One bounded snapshot of receiver-side video pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaFeedbackReportV1 {
    pub report_id: u64,
    pub interval_ms: u16,
    pub flags: MediaFeedbackFlags,
    pub last_sequence: Option<u64>,
    pub transport_dropped_delta: u32,
    pub frontend_dropped_delta: u32,
    pub decoder_dropped_delta: u32,
    pub presenter_dropped_delta: u32,
    pub frontend_queue_depth: u8,
    pub frontend_queue_capacity: u8,
    pub decode_queue_depth: u8,
    pub decode_queue_capacity: u8,
    pub presenter_queue_depth: u8,
    pub presenter_queue_capacity: u8,
    pub transport_delivery_p95_ms: Option<u16>,
    pub decode_p95_ms: Option<u16>,
    pub presentation_p95_ms: Option<u16>,
}

impl MediaFeedbackReportV1 {
    pub fn validate(&self) -> Result<()> {
        if self.report_id == 0 {
            return Err(invalid_report("report ID must be nonzero"));
        }
        if !(MIN_FEEDBACK_INTERVAL_MS..=MAX_FEEDBACK_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(invalid_report("interval must be 250..=5000 milliseconds"));
        }
        if MediaFeedbackFlags::from_bits(self.flags.bits()).is_none() {
            return Err(invalid_report("unsupported feedback flag bits"));
        }
        if self.last_sequence == Some(OPTIONAL_SEQUENCE_NONE) {
            return Err(invalid_report(
                "the all-ones last sequence is reserved for none",
            ));
        }
        validate_queue(self.frontend_queue_depth, self.frontend_queue_capacity)?;
        validate_queue(self.decode_queue_depth, self.decode_queue_capacity)?;
        validate_queue(self.presenter_queue_depth, self.presenter_queue_capacity)?;
        for latency in [
            self.transport_delivery_p95_ms,
            self.decode_p95_ms,
            self.presentation_p95_ms,
        ] {
            if latency.is_some_and(|latency| latency > MAX_FEEDBACK_LATENCY_MS) {
                return Err(invalid_report(
                    "p95 latency must be at most 60000 milliseconds",
                ));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; MEDIA_FEEDBACK_REPORT_V1_LEN]> {
        self.validate()?;
        let mut wire = [0_u8; MEDIA_FEEDBACK_REPORT_V1_LEN];
        wire[0..4].copy_from_slice(&MEDIA_FEEDBACK_REPORT_V1_MAGIC);
        wire[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        wire[6..8].copy_from_slice(&MEDIA_FEEDBACK_REPORT_V1_LEN_U16.to_be_bytes());
        wire[8..16].copy_from_slice(&self.report_id.to_be_bytes());
        wire[16..18].copy_from_slice(&self.interval_ms.to_be_bytes());
        wire[18] = self.flags.bits();
        // Byte 19 is reserved and remains zero.
        wire[20..28].copy_from_slice(
            &self
                .last_sequence
                .unwrap_or(OPTIONAL_SEQUENCE_NONE)
                .to_be_bytes(),
        );
        wire[28..32].copy_from_slice(&self.transport_dropped_delta.to_be_bytes());
        wire[32..36].copy_from_slice(&self.frontend_dropped_delta.to_be_bytes());
        wire[36..40].copy_from_slice(&self.decoder_dropped_delta.to_be_bytes());
        wire[40..44].copy_from_slice(&self.presenter_dropped_delta.to_be_bytes());
        wire[44] = self.frontend_queue_depth;
        wire[45] = self.frontend_queue_capacity;
        wire[46] = self.decode_queue_depth;
        wire[47] = self.decode_queue_capacity;
        wire[48] = self.presenter_queue_depth;
        wire[49] = self.presenter_queue_capacity;
        wire[50..52].copy_from_slice(
            &self
                .transport_delivery_p95_ms
                .unwrap_or(OPTIONAL_LATENCY_NONE)
                .to_be_bytes(),
        );
        wire[52..54].copy_from_slice(
            &self
                .decode_p95_ms
                .unwrap_or(OPTIONAL_LATENCY_NONE)
                .to_be_bytes(),
        );
        wire[54..56].copy_from_slice(
            &self
                .presentation_p95_ms
                .unwrap_or(OPTIONAL_LATENCY_NONE)
                .to_be_bytes(),
        );
        // Bytes 56..64 are reserved and remain zero.
        Ok(wire)
    }

    pub fn decode(wire: &[u8; MEDIA_FEEDBACK_REPORT_V1_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != MEDIA_FEEDBACK_REPORT_V1_MAGIC {
            return Err(invalid_report("invalid report magic"));
        }
        validate_fixed_prefix(
            wire,
            MEDIA_FEEDBACK_REPORT_V1_LEN_U16,
            "media feedback report",
        )?;
        if wire[19] != 0 || wire[56..64].iter().any(|byte| *byte != 0) {
            return Err(invalid_report("reserved fields must be zero"));
        }
        let flags = MediaFeedbackFlags::from_bits(wire[18])
            .ok_or_else(|| invalid_report("unsupported feedback flag bits"))?;
        let last_sequence =
            u64::from_be_bytes(wire[20..28].try_into().expect("fixed slice length"));
        let report = Self {
            report_id: u64::from_be_bytes(wire[8..16].try_into().expect("fixed slice length")),
            interval_ms: u16::from_be_bytes(wire[16..18].try_into().expect("fixed slice length")),
            flags,
            last_sequence: (last_sequence != OPTIONAL_SEQUENCE_NONE).then_some(last_sequence),
            transport_dropped_delta: u32::from_be_bytes(
                wire[28..32].try_into().expect("fixed slice length"),
            ),
            frontend_dropped_delta: u32::from_be_bytes(
                wire[32..36].try_into().expect("fixed slice length"),
            ),
            decoder_dropped_delta: u32::from_be_bytes(
                wire[36..40].try_into().expect("fixed slice length"),
            ),
            presenter_dropped_delta: u32::from_be_bytes(
                wire[40..44].try_into().expect("fixed slice length"),
            ),
            frontend_queue_depth: wire[44],
            frontend_queue_capacity: wire[45],
            decode_queue_depth: wire[46],
            decode_queue_capacity: wire[47],
            presenter_queue_depth: wire[48],
            presenter_queue_capacity: wire[49],
            transport_delivery_p95_ms: decode_optional_latency(wire, 50),
            decode_p95_ms: decode_optional_latency(wire, 52),
            presentation_p95_ms: decode_optional_latency(wire, 54),
        };
        report.validate()?;
        Ok(report)
    }
}

/// Direction selected by the host controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AdaptiveBitrateStateV1 {
    Hold = 0,
    Decrease = 1,
    Increase = 2,
}

impl TryFrom<u8> for AdaptiveBitrateStateV1 {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Hold),
            1 => Ok(Self::Decrease),
            2 => Ok(Self::Increase),
            _ => Err(invalid_decision("unsupported controller state")),
        }
    }
}

/// Bounded reasons contributing to a bitrate decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdaptiveBitrateReasonFlagsV1(u16);

impl AdaptiveBitrateReasonFlagsV1 {
    pub const NONE: Self = Self(0);
    pub const RTT_INFLATION: Self = Self(1 << 0);
    pub const LOSS_OR_CANCELLATION: Self = Self(1 << 1);
    pub const SENDER_BACKPRESSURE: Self = Self(1 << 2);
    pub const RECEIVER_QUEUE: Self = Self(1 << 3);
    pub const DECODE_BACKLOG: Self = Self(1 << 4);
    pub const DELIVERY_LATENCY: Self = Self(1 << 5);
    pub const CLEAN_RECOVERY: Self = Self(1 << 6);
    pub const FEEDBACK_STALE: Self = Self(1 << 7);

    const KNOWN: u16 = Self::RTT_INFLATION.0
        | Self::LOSS_OR_CANCELLATION.0
        | Self::SENDER_BACKPRESSURE.0
        | Self::RECEIVER_QUEUE.0
        | Self::DECODE_BACKLOG.0
        | Self::DELIVERY_LATENCY.0
        | Self::CLEAN_RECOVERY.0
        | Self::FEEDBACK_STALE.0;

    pub const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One bounded host controller decision corresponding to a receiver report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveBitrateDecisionV1 {
    pub decision_id: u64,
    pub report_id: u64,
    pub target_kbps: u32,
    pub floor_kbps: u32,
    pub ceiling_kbps: u32,
    pub state: AdaptiveBitrateStateV1,
    pub reasons: AdaptiveBitrateReasonFlagsV1,
    pub applied: bool,
}

impl AdaptiveBitrateDecisionV1 {
    pub fn validate(&self) -> Result<()> {
        if self.decision_id == 0 {
            return Err(invalid_decision("decision ID must be nonzero"));
        }
        if self.report_id == 0 {
            return Err(invalid_decision("report ID must be nonzero"));
        }
        for bitrate in [self.target_kbps, self.floor_kbps, self.ceiling_kbps] {
            if !(MIN_ADAPTIVE_BITRATE_KBPS..=MAX_ADAPTIVE_BITRATE_KBPS).contains(&bitrate) {
                return Err(invalid_decision(
                    "bitrates must be between 1000 and 100000 kilobits per second",
                ));
            }
        }
        if self.floor_kbps > self.target_kbps || self.target_kbps > self.ceiling_kbps {
            return Err(invalid_decision(
                "bitrate floor must not exceed target and target must not exceed ceiling",
            ));
        }
        if AdaptiveBitrateReasonFlagsV1::from_bits(self.reasons.bits()).is_none() {
            return Err(invalid_decision("unsupported decision reason bits"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; ADAPTIVE_BITRATE_DECISION_V1_LEN]> {
        self.validate()?;
        let mut wire = [0_u8; ADAPTIVE_BITRATE_DECISION_V1_LEN];
        wire[0..4].copy_from_slice(&ADAPTIVE_BITRATE_DECISION_V1_MAGIC);
        wire[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        wire[6..8].copy_from_slice(&ADAPTIVE_BITRATE_DECISION_V1_LEN_U16.to_be_bytes());
        wire[8..16].copy_from_slice(&self.decision_id.to_be_bytes());
        wire[16..24].copy_from_slice(&self.report_id.to_be_bytes());
        wire[24..28].copy_from_slice(&self.target_kbps.to_be_bytes());
        wire[28..32].copy_from_slice(&self.floor_kbps.to_be_bytes());
        wire[32..36].copy_from_slice(&self.ceiling_kbps.to_be_bytes());
        wire[36] = self.state as u8;
        wire[37] = u8::from(self.applied);
        wire[38..40].copy_from_slice(&self.reasons.bits().to_be_bytes());
        Ok(wire)
    }

    pub fn decode(wire: &[u8; ADAPTIVE_BITRATE_DECISION_V1_LEN]) -> Result<Self> {
        let magic: [u8; 4] = wire[0..4].try_into().expect("fixed slice length");
        if magic != ADAPTIVE_BITRATE_DECISION_V1_MAGIC {
            return Err(invalid_decision("invalid decision magic"));
        }
        validate_fixed_prefix(
            wire,
            ADAPTIVE_BITRATE_DECISION_V1_LEN_U16,
            "adaptive bitrate decision",
        )?;
        if wire[37] > 1 {
            return Err(invalid_decision("applied must be encoded as zero or one"));
        }
        let reason_bits = u16::from_be_bytes(wire[38..40].try_into().expect("fixed slice length"));
        let decision = Self {
            decision_id: u64::from_be_bytes(wire[8..16].try_into().expect("fixed slice length")),
            report_id: u64::from_be_bytes(wire[16..24].try_into().expect("fixed slice length")),
            target_kbps: u32::from_be_bytes(wire[24..28].try_into().expect("fixed slice length")),
            floor_kbps: u32::from_be_bytes(wire[28..32].try_into().expect("fixed slice length")),
            ceiling_kbps: u32::from_be_bytes(wire[32..36].try_into().expect("fixed slice length")),
            state: AdaptiveBitrateStateV1::try_from(wire[36])?,
            reasons: AdaptiveBitrateReasonFlagsV1::from_bits(reason_bits)
                .ok_or_else(|| invalid_decision("unsupported decision reason bits"))?,
            applied: wire[37] == 1,
        };
        decision.validate()?;
        Ok(decision)
    }
}

/// Read one fixed-width report. Clean EOF before the next record returns `None`.
pub async fn read_media_feedback_report_v1<R>(
    reader: &mut R,
) -> Result<Option<MediaFeedbackReportV1>>
where
    R: AsyncRead + Unpin,
{
    let mut wire = [0_u8; MEDIA_FEEDBACK_REPORT_V1_LEN];
    if reader.read(&mut wire[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut wire[1..]).await?;
    MediaFeedbackReportV1::decode(&wire).map(Some)
}

/// Validate and write one fixed-width report.
pub async fn write_media_feedback_report_v1<W>(
    writer: &mut W,
    report: &MediaFeedbackReportV1,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&report.encode()?).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one fixed-width decision. Clean EOF before the next record returns `None`.
pub async fn read_adaptive_bitrate_decision_v1<R>(
    reader: &mut R,
) -> Result<Option<AdaptiveBitrateDecisionV1>>
where
    R: AsyncRead + Unpin,
{
    let mut wire = [0_u8; ADAPTIVE_BITRATE_DECISION_V1_LEN];
    if reader.read(&mut wire[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut wire[1..]).await?;
    AdaptiveBitrateDecisionV1::decode(&wire).map(Some)
}

/// Validate and write one fixed-width decision.
pub async fn write_adaptive_bitrate_decision_v1<W>(
    writer: &mut W,
    decision: &AdaptiveBitrateDecisionV1,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&decision.encode()?).await?;
    writer.flush().await?;
    Ok(())
}

fn validate_queue(depth: u8, capacity: u8) -> Result<()> {
    if !(1..=MAX_FEEDBACK_QUEUE_CAPACITY).contains(&capacity) {
        return Err(invalid_report("queue capacity must be 1..=16"));
    }
    if depth > capacity {
        return Err(invalid_report("queue depth must not exceed capacity"));
    }
    Ok(())
}

fn decode_optional_latency<const N: usize>(wire: &[u8; N], offset: usize) -> Option<u16> {
    let value = u16::from_be_bytes(
        wire[offset..offset + 2]
            .try_into()
            .expect("fixed slice length"),
    );
    (value != OPTIONAL_LATENCY_NONE).then_some(value)
}

fn validate_fixed_prefix<const N: usize>(
    wire: &[u8; N],
    expected_len: u16,
    message_type: &'static str,
) -> Result<()> {
    let version = u16::from_be_bytes(wire[4..6].try_into().expect("fixed slice length"));
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion {
            expected: PROTOCOL_VERSION,
            actual: version,
        });
    }
    let actual_len = u16::from_be_bytes(wire[6..8].try_into().expect("fixed slice length"));
    if actual_len != expected_len {
        return Err(ProtocolError::InvalidMessage {
            message_type,
            reason: "fixed record length does not match the protocol",
        });
    }
    Ok(())
}

fn invalid_report(reason: &'static str) -> ProtocolError {
    ProtocolError::InvalidMessage {
        message_type: "media feedback report",
        reason,
    }
}

fn invalid_decision(reason: &'static str) -> ProtocolError {
    ProtocolError::InvalidMessage {
        message_type: "adaptive bitrate decision",
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn report() -> MediaFeedbackReportV1 {
        MediaFeedbackReportV1 {
            report_id: 0x0102_0304_0506_0708,
            interval_ms: 1_000,
            flags: MediaFeedbackFlags::RESYNC_ACTIVE,
            last_sequence: Some(0x1112_1314_1516_1718),
            transport_dropped_delta: 0x0102_0304,
            frontend_dropped_delta: 0x1112_1314,
            decoder_dropped_delta: 0x2122_2324,
            presenter_dropped_delta: 0x3132_3334,
            frontend_queue_depth: 1,
            frontend_queue_capacity: 4,
            decode_queue_depth: 2,
            decode_queue_capacity: 2,
            presenter_queue_depth: 1,
            presenter_queue_capacity: 2,
            transport_delivery_p95_ms: Some(17),
            decode_p95_ms: Some(33),
            presentation_p95_ms: None,
        }
    }

    fn decision() -> AdaptiveBitrateDecisionV1 {
        AdaptiveBitrateDecisionV1 {
            decision_id: 0x0102_0304_0506_0708,
            report_id: 0x1112_1314_1516_1718,
            target_kbps: 12_000,
            floor_kbps: 4_000,
            ceiling_kbps: 20_000,
            state: AdaptiveBitrateStateV1::Decrease,
            reasons: AdaptiveBitrateReasonFlagsV1::LOSS_OR_CANCELLATION
                .union(AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG),
            applied: true,
        }
    }

    #[test]
    fn feedback_report_golden_vector() {
        assert_eq!(
            report().encode().unwrap(),
            [
                0x53, 0x47, 0x52, 0x31, // SGR1
                0x00, 0x01, 0x00, 0x40, // version, fixed length
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // report ID
                0x03, 0xe8, 0x01, 0x00, // interval, flags, reserved
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // last sequence
                0x01, 0x02, 0x03, 0x04, // transport drops
                0x11, 0x12, 0x13, 0x14, // frontend drops
                0x21, 0x22, 0x23, 0x24, // decoder drops
                0x31, 0x32, 0x33, 0x34, // presenter drops
                0x01, 0x04, 0x02, 0x02, 0x01, 0x02, // queue pairs
                0x00, 0x11, 0x00, 0x21, 0xff, 0xff, // p95 latencies
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            ]
        );
    }

    #[test]
    fn adaptive_bitrate_decision_golden_vector() {
        assert_eq!(
            decision().encode().unwrap(),
            [
                0x53, 0x47, 0x44, 0x31, // SGD1
                0x00, 0x01, 0x00, 0x28, // version, fixed length
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // decision ID
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // report ID
                0x00, 0x00, 0x2e, 0xe0, // target: 12000
                0x00, 0x00, 0x0f, 0xa0, // floor: 4000
                0x00, 0x00, 0x4e, 0x20, // ceiling: 20000
                0x01, 0x01, 0x00, 0x12, // decrease, applied, reasons
            ]
        );
    }

    #[test]
    fn records_round_trip() {
        let report = report();
        assert_eq!(
            MediaFeedbackReportV1::decode(&report.encode().unwrap()).unwrap(),
            report
        );
        let decision = decision();
        assert_eq!(
            AdaptiveBitrateDecisionV1::decode(&decision.encode().unwrap()).unwrap(),
            decision
        );
    }

    #[test]
    fn feedback_report_rejects_out_of_bounds_fields() {
        let mut value = report();
        value.report_id = 0;
        assert!(value.validate().is_err());
        value = report();
        value.interval_ms = 249;
        assert!(value.validate().is_err());
        value = report();
        value.interval_ms = 5_001;
        assert!(value.validate().is_err());
        value = report();
        value.last_sequence = Some(u64::MAX);
        assert!(value.validate().is_err());
        value = report();
        value.frontend_queue_capacity = 0;
        assert!(value.validate().is_err());
        value = report();
        value.decode_queue_capacity = 17;
        assert!(value.validate().is_err());
        value = report();
        value.presenter_queue_depth = 3;
        assert!(value.validate().is_err());
        value = report();
        value.decode_p95_ms = Some(60_001);
        assert!(value.validate().is_err());
    }

    #[test]
    fn feedback_report_rejects_malformed_wire_fields() {
        let valid = report().encode().unwrap();
        for (offset, value) in [
            (0, 0),  // magic
            (5, 2),  // version
            (7, 63), // length
            (18, 2), // flags
            (19, 1), // reserved
            (63, 1), // trailing reserved
        ] {
            let mut malformed = valid;
            malformed[offset] = value;
            assert!(
                MediaFeedbackReportV1::decode(&malformed).is_err(),
                "offset {offset}"
            );
        }

        let mut invalid_latency = valid;
        invalid_latency[52..54].copy_from_slice(&60_001_u16.to_be_bytes());
        assert!(MediaFeedbackReportV1::decode(&invalid_latency).is_err());
    }

    #[test]
    fn adaptive_bitrate_decision_rejects_out_of_bounds_fields() {
        let mut value = decision();
        value.decision_id = 0;
        assert!(value.validate().is_err());
        value = decision();
        value.report_id = 0;
        assert!(value.validate().is_err());
        value = decision();
        value.floor_kbps = 999;
        assert!(value.validate().is_err());
        value = decision();
        value.ceiling_kbps = 100_001;
        assert!(value.validate().is_err());
        value = decision();
        value.target_kbps = value.floor_kbps - 1;
        assert!(value.validate().is_err());
        value = decision();
        value.target_kbps = value.ceiling_kbps + 1;
        assert!(value.validate().is_err());
    }

    #[test]
    fn adaptive_bitrate_decision_rejects_malformed_wire_fields() {
        let valid = decision().encode().unwrap();
        for (offset, value) in [
            (0, 0),  // magic
            (5, 2),  // version
            (7, 39), // length
            (36, 3), // state
            (37, 2), // applied
            (38, 1), // unknown high reason bit
        ] {
            let mut malformed = valid;
            malformed[offset] = value;
            assert!(
                AdaptiveBitrateDecisionV1::decode(&malformed).is_err(),
                "offset {offset}"
            );
        }
    }

    #[tokio::test]
    async fn async_report_records_round_trip_and_clean_eof() {
        let first = report();
        let mut second = report();
        second.report_id += 1;
        let (mut writer, mut reader) = duplex(256);
        write_media_feedback_report_v1(&mut writer, &first)
            .await
            .unwrap();
        write_media_feedback_report_v1(&mut writer, &second)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();

        assert_eq!(
            read_media_feedback_report_v1(&mut reader).await.unwrap(),
            Some(first)
        );
        assert_eq!(
            read_media_feedback_report_v1(&mut reader).await.unwrap(),
            Some(second)
        );
        assert_eq!(
            read_media_feedback_report_v1(&mut reader).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn async_decision_records_round_trip_and_clean_eof() {
        let expected = decision();
        let (mut writer, mut reader) = duplex(128);
        write_adaptive_bitrate_decision_v1(&mut writer, &expected)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();

        assert_eq!(
            read_adaptive_bitrate_decision_v1(&mut reader)
                .await
                .unwrap(),
            Some(expected)
        );
        assert_eq!(
            read_adaptive_bitrate_decision_v1(&mut reader)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn truncated_fixed_records_are_io_errors() {
        let (mut report_writer, mut report_reader) = duplex(64);
        report_writer
            .write_all(&report().encode().unwrap()[..63])
            .await
            .unwrap();
        report_writer.shutdown().await.unwrap();
        let error = read_media_feedback_report_v1(&mut report_reader)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProtocolError::Io(error) if error.kind() == ErrorKind::UnexpectedEof)
        );

        let (mut decision_writer, mut decision_reader) = duplex(40);
        decision_writer
            .write_all(&decision().encode().unwrap()[..39])
            .await
            .unwrap();
        decision_writer.shutdown().await.unwrap();
        let error = read_adaptive_bitrate_decision_v1(&mut decision_reader)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProtocolError::Io(error) if error.kind() == ErrorKind::UnexpectedEof)
        );
    }
}
