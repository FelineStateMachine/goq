use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::Serialize;
use sigil_protocol::{MAX_MEDIA_PAYLOAD_LEN, MAX_VIDEO_DIMENSION, MAX_VIDEO_PIXELS};
use tauri::{AppHandle, Emitter};

pub(crate) fn byte_to_codec(value: u8) -> &'static str {
    match value {
        1 => "h264",
        2 => "h265",
        3 => "av1",
        _ => "h264",
    }
}

#[derive(Serialize, Clone)]
pub struct FramePayload {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub data: String,
    pub keyframe: bool,
    pub codec: String,
    pub capture_timestamp_micros: Option<u64>,
    pub pts_micros: Option<i64>,
    pub discontinuity: bool,
}

#[derive(Serialize, Clone)]
struct FrameErrorPayload {
    generation: u64,
    error: String,
}

pub(crate) fn emit_frame_error(app: &AppHandle, generation: u64, error: impl Into<String>) {
    let _ = app.emit(
        "frame-error",
        FrameErrorPayload {
            generation,
            error: error.into(),
        },
    );
}

// Absorb brief webview→Rust acknowledgment jitter without allowing Tauri IPC
// to grow without bound. Four 60 fps frames cap this handoff at about 67 ms;
// WebCodecs has a separate, stricter decode-queue bound in the frontend.
pub(crate) const CLIENT_FRAME_CHANNEL_CAPACITY: usize = 4;

const FRAME_CHANNEL_MAGIC: [u8; 4] = *b"SGFR";
const FRAME_CHANNEL_VERSION: u8 = 1;
const FRAME_CHANNEL_HEADER_LEN: usize = 40;
const FRAME_CHANNEL_FLAG_KEYFRAME: u8 = 1 << 0;
const FRAME_CHANNEL_FLAG_DISCONTINUITY: u8 = 1 << 1;
const FRAME_CHANNEL_FLAG_CODEC_CONFIG: u8 = 1 << 2;
const FRAME_CHANNEL_OPTIONAL_U64_NONE: u64 = u64::MAX;
const FRAME_CHANNEL_OPTIONAL_I64_NONE: i64 = i64::MIN;

pub(crate) fn close_generation_connection<T>(
    connection: Option<(u64, T)>,
    close: impl FnOnce(T),
) -> Option<u64> {
    connection.map(|(generation, connection)| {
        close(connection);
        generation
    })
}

pub(crate) fn take_generation_owned<T>(
    slot: &mut Option<(u64, T)>,
    expected_generation: u64,
) -> Option<T> {
    if slot
        .as_ref()
        .is_some_and(|(generation, _)| *generation == expected_generation)
    {
        slot.take().map(|(_, value)| value)
    } else {
        None
    }
}

pub(crate) fn take_generation_owned_triple<T, U>(
    slot: &mut Option<(u64, T, U)>,
    expected_generation: u64,
) -> Option<(T, U)> {
    if slot
        .as_ref()
        .is_some_and(|(generation, _, _)| *generation == expected_generation)
    {
        slot.take().map(|(_, value, companion)| (value, companion))
    } else {
        None
    }
}

pub(crate) struct FrameEnvelopeMetadata<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) codec: &'a str,
    pub(crate) keyframe: bool,
    pub(crate) discontinuity: bool,
    pub(crate) codec_config: bool,
    pub(crate) sequence: Option<u64>,
    pub(crate) capture_timestamp_micros: Option<u64>,
    pub(crate) pts_micros: Option<i64>,
}

pub(crate) fn encode_frame_envelope(
    metadata: FrameEnvelopeMetadata<'_>,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    validate_legacy_media_header(metadata.width, metadata.height, payload.len())?;
    if metadata.codec_config && !metadata.keyframe {
        return Err("Frame codec configuration requires a keyframe".to_string());
    }
    if metadata.sequence == Some(FRAME_CHANNEL_OPTIONAL_U64_NONE) {
        return Err("Frame sequence collides with the channel sentinel".to_string());
    }
    if metadata.capture_timestamp_micros == Some(FRAME_CHANNEL_OPTIONAL_U64_NONE) {
        return Err("Capture timestamp collides with the channel sentinel".to_string());
    }
    if metadata.pts_micros == Some(FRAME_CHANNEL_OPTIONAL_I64_NONE) {
        return Err("Frame PTS collides with the channel sentinel".to_string());
    }
    let width = u16::try_from(metadata.width).map_err(|_| {
        format!(
            "Frame width does not fit channel envelope: {}",
            metadata.width
        )
    })?;
    let height = u16::try_from(metadata.height).map_err(|_| {
        format!(
            "Frame height does not fit channel envelope: {}",
            metadata.height
        )
    })?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        format!(
            "Frame payload does not fit channel envelope: {}",
            payload.len()
        )
    })?;
    let codec = match metadata.codec {
        "h264" => 1,
        "h265" => 2,
        "av1" => 3,
        other => return Err(format!("Unsupported frame channel codec: {other}")),
    };
    let mut flags = 0_u8;
    if metadata.keyframe {
        flags |= FRAME_CHANNEL_FLAG_KEYFRAME;
    }
    if metadata.discontinuity {
        flags |= FRAME_CHANNEL_FLAG_DISCONTINUITY;
    }
    if metadata.codec_config {
        flags |= FRAME_CHANNEL_FLAG_CODEC_CONFIG;
    }

    let mut envelope = Vec::with_capacity(FRAME_CHANNEL_HEADER_LEN + payload.len());
    envelope.extend_from_slice(&FRAME_CHANNEL_MAGIC);
    envelope.push(FRAME_CHANNEL_VERSION);
    envelope.push(codec);
    envelope.push(flags);
    envelope.push(0); // Reserved; the parser rejects non-zero values.
    envelope.extend_from_slice(&width.to_be_bytes());
    envelope.extend_from_slice(&height.to_be_bytes());
    envelope.extend_from_slice(&payload_len.to_be_bytes());
    envelope.extend_from_slice(
        &metadata
            .sequence
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_U64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(
        &metadata
            .capture_timestamp_micros
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_U64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(
        &metadata
            .pts_micros
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_I64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(payload);
    debug_assert_eq!(envelope.len(), FRAME_CHANNEL_HEADER_LEN + payload.len());
    Ok(envelope)
}

pub(crate) fn try_reserve_frame_channel_slot(in_flight: &AtomicUsize) -> bool {
    in_flight
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            (current < CLIENT_FRAME_CHANNEL_CAPACITY).then_some(current + 1)
        })
        .is_ok()
}

pub(crate) fn release_frame_channel_slot(in_flight: &AtomicUsize) {
    let _ = in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        current.checked_sub(1)
    });
}

pub(crate) fn release_frame_channel_slot_for_generation(
    in_flight: &AtomicUsize,
    current_generation: u64,
    generation: u64,
) -> bool {
    if generation == 0 || current_generation != generation {
        return false;
    }
    release_frame_channel_slot(in_flight);
    true
}

pub(crate) fn next_media_generation(counter: &AtomicU64) -> Result<u64, String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| "Media connection generation overflowed".to_string())
}

pub(crate) fn validate_legacy_media_header(
    width: u32,
    height: u32,
    payload_len: usize,
) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > u32::from(MAX_VIDEO_DIMENSION)
        || height > u32::from(MAX_VIDEO_DIMENSION)
        || width.saturating_mul(height) > MAX_VIDEO_PIXELS
    {
        return Err(format!("Invalid legacy media dimensions: {width}x{height}"));
    }
    if payload_len == 0 || payload_len > MAX_MEDIA_PAYLOAD_LEN {
        return Err(format!(
            "Invalid legacy media payload length: {payload_len}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_generations_are_nonzero_monotonic_and_checked_for_overflow() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_media_generation(&counter).unwrap(), 1);
        assert_eq!(next_media_generation(&counter).unwrap(), 2);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(next_media_generation(&exhausted).is_err());
    }

    #[test]
    fn binary_frame_envelope_has_exact_stable_layout() {
        let payload = [0, 0, 0, 1, 0x65];
        let envelope = encode_frame_envelope(
            FrameEnvelopeMetadata {
                width: 1280,
                height: 800,
                codec: "h264",
                keyframe: true,
                discontinuity: true,
                codec_config: true,
                sequence: Some(42),
                capture_timestamp_micros: Some(123_456),
                pts_micros: Some(98_765),
            },
            &payload,
        )
        .unwrap();

        assert_eq!(envelope.len(), FRAME_CHANNEL_HEADER_LEN + payload.len());
        assert_eq!(&envelope[0..4], b"SGFR");
        assert_eq!(envelope[4], 1);
        assert_eq!(envelope[5], 1);
        assert_eq!(envelope[6], 0b111);
        assert_eq!(envelope[7], 0);
        assert_eq!(&envelope[8..10], &1280_u16.to_be_bytes());
        assert_eq!(&envelope[10..12], &800_u16.to_be_bytes());
        assert_eq!(&envelope[12..16], &5_u32.to_be_bytes());
        assert_eq!(&envelope[16..24], &42_u64.to_be_bytes());
        assert_eq!(&envelope[24..32], &123_456_u64.to_be_bytes());
        assert_eq!(&envelope[32..40], &98_765_i64.to_be_bytes());
        assert_eq!(&envelope[40..], payload);
    }

    #[test]
    fn binary_frame_envelope_uses_explicit_optional_sentinels() {
        let envelope = encode_frame_envelope(
            FrameEnvelopeMetadata {
                width: 1,
                height: 1,
                codec: "av1",
                keyframe: false,
                discontinuity: false,
                codec_config: false,
                sequence: None,
                capture_timestamp_micros: None,
                pts_micros: None,
            },
            &[1],
        )
        .unwrap();
        assert_eq!(envelope[5], 3);
        assert_eq!(&envelope[16..24], &u64::MAX.to_be_bytes());
        assert_eq!(&envelope[24..32], &u64::MAX.to_be_bytes());
        assert_eq!(&envelope[32..40], &i64::MIN.to_be_bytes());
    }

    #[test]
    fn binary_frame_envelope_rejects_invalid_metadata_before_sending() {
        let metadata = |codec| FrameEnvelopeMetadata {
            width: 1280,
            height: 800,
            codec,
            keyframe: false,
            discontinuity: false,
            codec_config: false,
            sequence: None,
            capture_timestamp_micros: None,
            pts_micros: None,
        };
        assert!(encode_frame_envelope(metadata("vp9"), &[1]).is_err());
        assert!(encode_frame_envelope(metadata("h264"), &[]).is_err());
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    codec_config: true,
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    sequence: Some(u64::MAX),
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    pts_micros: Some(i64::MIN),
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    width: u32::from(MAX_VIDEO_DIMENSION) + 1,
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
    }

    #[test]
    fn frame_channel_slots_are_bounded_and_cannot_underflow() {
        let in_flight = AtomicUsize::new(0);
        release_frame_channel_slot(&in_flight);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);

        for expected in 1..=CLIENT_FRAME_CHANNEL_CAPACITY {
            assert!(try_reserve_frame_channel_slot(&in_flight));
            assert_eq!(in_flight.load(Ordering::SeqCst), expected);
        }
        assert!(!try_reserve_frame_channel_slot(&in_flight));
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            CLIENT_FRAME_CHANNEL_CAPACITY
        );

        for expected in (0..CLIENT_FRAME_CHANNEL_CAPACITY).rev() {
            release_frame_channel_slot(&in_flight);
            assert_eq!(in_flight.load(Ordering::SeqCst), expected);
        }
        release_frame_channel_slot(&in_flight);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frame_acknowledgments_release_only_the_matching_generation() {
        let in_flight = AtomicUsize::new(2);
        let generation = 9;

        assert!(!release_frame_channel_slot_for_generation(
            &in_flight, generation, 8
        ));
        assert!(!release_frame_channel_slot_for_generation(
            &in_flight, generation, 0
        ));
        assert_eq!(in_flight.load(Ordering::SeqCst), 2);
        assert!(release_frame_channel_slot_for_generation(
            &in_flight, generation, 9
        ));
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn global_frame_events_serialize_their_media_generation() {
        let frame = serde_json::to_value(FramePayload {
            generation: 17,
            width: 1280,
            height: 800,
            data: "jpeg".to_string(),
            keyframe: true,
            codec: "h264".to_string(),
            capture_timestamp_micros: Some(1),
            pts_micros: Some(2),
            discontinuity: false,
        })
        .unwrap();
        let error = serde_json::to_value(FrameErrorPayload {
            generation: 17,
            error: "closed".to_string(),
        })
        .unwrap();

        assert_eq!(frame["generation"], 17);
        assert_eq!(error["generation"], 17);
        assert_eq!(error["error"], "closed");
    }

    #[test]
    fn generation_connection_close_retires_exactly_one_owned_handle() {
        let closes = AtomicUsize::new(0);
        let retired = close_generation_connection(Some((7, "media")), |connection| {
            assert_eq!(connection, "media");
            closes.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(retired, Some(7));
        assert_eq!(closes.load(Ordering::SeqCst), 1);

        let absent: Option<(u64, &str)> = None;
        assert_eq!(
            close_generation_connection(absent, |_| {
                closes.fetch_add(1, Ordering::SeqCst);
            }),
            None
        );
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn generation_owned_retirement_cannot_take_a_replacement_session() {
        let mut slot = Some((12, "replacement"));
        assert_eq!(take_generation_owned(&mut slot, 11), None);
        assert_eq!(slot, Some((12, "replacement")));
        assert_eq!(take_generation_owned(&mut slot, 12), Some("replacement"));
        assert_eq!(slot, None);

        let mut feedback = Some((22, "replacement connection", "replacement sender"));
        assert_eq!(take_generation_owned_triple(&mut feedback, 21), None);
        assert_eq!(
            feedback,
            Some((22, "replacement connection", "replacement sender"))
        );
        assert_eq!(
            take_generation_owned_triple(&mut feedback, 22),
            Some(("replacement connection", "replacement sender"))
        );
        assert_eq!(feedback, None);
    }

    #[test]
    fn legacy_media_header_is_bounded_before_allocation() {
        assert!(validate_legacy_media_header(1280, 800, 1024).is_ok());
        assert!(validate_legacy_media_header(0, 800, 1024).is_err());
        assert!(validate_legacy_media_header(1280, 800, 0).is_err());
        assert!(validate_legacy_media_header(1280, 800, MAX_MEDIA_PAYLOAD_LEN + 1).is_err());
        assert!(validate_legacy_media_header(u32::MAX, u32::MAX, 1024).is_err());
    }
}
