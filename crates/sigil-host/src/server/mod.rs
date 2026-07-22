use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::{Connection, SendStream};
use iroh::protocol::ProtocolHandler;
use moq_net::{
    Broadcast, BroadcastProducer, Error as MoqError, GroupProducer, Origin, Track, TrackProducer,
};
use sigil_protocol::{
    AUDIO_HEADER_LEN, AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1,
    AdaptiveBitrateStateV1, AudioFlags, AudioPacket, AudioPacketHeader, Capability, ClientHello,
    FrameFlags, HostHello, InputAck, InvitationGrants, KeyframeRequestReasonV3,
    MAX_AUDIO_PAYLOAD_LEN, MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    MAX_MEDIA_OBJECT_ID_V3, MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MOQ_VIDEO_H264_TRACK,
    MOQ_VIDEO_TRACK_PRIORITY, MediaControlRequestV3, MediaFeedbackFlags, MediaFeedbackReportV1,
    MediaFrame, MediaFrameHeader, MediaObjectHeaderV3, MediaObjectV3, encode_media_frame_object,
    media_moq_broadcast_name, read_client_hello, read_input_event, read_media_control_request_v3,
    read_media_feedback_report_v1, write_adaptive_bitrate_decision_v1, write_host_hello,
    write_input_ack, write_media_frame, write_media_object_v3,
};
use tracing::{debug, error, info, warn};

use crate::audio::spawn_pipewire_audio;
use crate::authorization::{AuthorizationPolicy, unix_timestamp_now};
use crate::clock::SessionClock;
use crate::config::{GamescopeEncoderBackend, HostConfig, VaapiRateControl, VideoSource};
use crate::cursor::{PointerPositionTracker, PointerState};
use crate::input::{InputBackend, InputDisposition};
use crate::moq_catalog::publish_goq_catalog;
use crate::source::{
    EncodedFrame, EncodedGop, EncodedSource, EncoderControl,
    spawn_gamescope_pipewire_after_static_preflight, spawn_test_pattern,
};

// Allow one frame of ordinary scheduler/write jitter beyond the frame being
// sent, but never replay a suffix already more than two configured periods old.
const MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS: u64 = 2;
const SOURCE_REAP_GRACE_TIMEOUT: Duration = Duration::from_secs(1);
const ENCODER_CONTROL_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

mod adaptive;
mod handlers;
mod media_v2;
mod media_v3;
mod moq;
mod session;
mod startup;

#[allow(unused_imports)]
pub(crate) use adaptive::MotionResolutionPolicy;
pub(crate) use adaptive::VideoDimensions;
use adaptive::serve_media_feedback;
pub use handlers::{
    AudioHandler, AuthorizedMoqHandler, ControlHandler, InputHandler, MediaFeedbackHandler,
    MediaHandler, MediaV2Handler, MediaV3Handler,
};
use handlers::{
    HANDSHAKE_TIMEOUT, MEDIA_CAPABILITIES, negotiated_capabilities, receive_hello, send_rejection,
};
use media_v2::{media_frame_for_encoded, serve_media, serve_media_v2};
use media_v3::{
    MediaV3GroupCursor, forward_media_v3_control_requests, new_current_gop_frames, serve_media_v3,
};
use moq::{MOQ_REJECT_CODE, serve_authorized_moq, serve_control_moq};

pub use session::SessionRegistry;
use session::{
    ClaimedMoqAttachment, ForcedIdrCoordinator, ForcedIdrDisposition, MediaV3Telemetry,
    MoqAttachmentWait, SourceTaskGuard,
};
use startup::select_gamescope_startup_source;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaReplayDecision {
    Send { discontinuity: bool },
    SkipUntilKeyframe,
    DiscardStaleSuffix { through_sequence: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MediaV3ObjectPosition {
    group_id: u64,
    object_id: u32,
    discontinuity: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaV3GroupDecision {
    Send(MediaV3ObjectPosition),
    SkipUntilKeyframe,
    EnterResync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MediaReplayCursor {
    last_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaReplayCursor {
    fn default() -> Self {
        Self {
            last_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaReplayCursor {
    fn classify(
        &mut self,
        frame: &EncodedFrame,
        replay_through_sequence: u64,
        initial_replay_started_at: Option<Instant>,
        observed_now: Instant,
        maximum_replay_age: Duration,
    ) -> MediaReplayDecision {
        let replay_age = observed_now.saturating_duration_since(frame.observed_at);
        let initial_replay_within_budget = initial_replay_started_at.is_some_and(|started_at| {
            observed_now.saturating_duration_since(started_at) <= maximum_replay_age
        });
        if !initial_replay_within_budget && replay_age > maximum_replay_age {
            self.last_sequence = Some(replay_through_sequence);
            self.waiting_for_keyframe = true;
            self.discontinuity_pending = true;
            return MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: replay_through_sequence,
            };
        }

        let sequence_discontinuity = self
            .last_sequence
            .is_some_and(|previous| previous.checked_add(1) != Some(frame.sequence));
        if sequence_discontinuity {
            self.waiting_for_keyframe = true;
            self.discontinuity_pending = true;
        }
        if self.waiting_for_keyframe && !(frame.keyframe && frame.codec_config) {
            return MediaReplayDecision::SkipUntilKeyframe;
        }
        MediaReplayDecision::Send {
            discontinuity: self.discontinuity_pending,
        }
    }

    fn commit_sent(&mut self, frame: &EncodedFrame) {
        self.last_sequence = Some(frame.sequence);
        self.waiting_for_keyframe = false;
        self.discontinuity_pending = false;
    }

    fn enter_resync_through(&mut self, through_sequence: Option<u64>) {
        if let Some(through_sequence) = through_sequence {
            self.last_sequence = Some(
                self.last_sequence
                    .map_or(through_sequence, |last| last.max(through_sequence)),
            );
        }
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
    }
}

fn maximum_media_replay_age(framerate: u32) -> Duration {
    debug_assert!(framerate > 0);
    let frame_period_nanos = 1_000_000_000_u64.div_ceil(u64::from(framerate.max(1)));
    Duration::from_nanos(frame_period_nanos.saturating_mul(MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS))
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        error!(%info, "host panic");
    }));
}

#[cfg(test)]
fn endpoint(byte: u8) -> EndpointId {
    iroh::SecretKey::from_bytes(&[byte; 32]).public()
}

#[cfg(test)]
fn moq_test_config() -> HostConfig {
    HostConfig {
        identity_path: "identity".into(),
        state_path: "state".into(),
        source: VideoSource::TestPattern,
        width: Some(1280),
        height: Some(800),
        framerate: 60,
        codec: "h264".to_owned(),
        input_mode: crate::config::InputMode::Disabled,
        uinput: None,
        ffmpeg_path: "ffmpeg".into(),
        gamescope_pipewire: None,
        audio: None,
    }
}

#[cfg(test)]
fn media_v3_encoded_frame(
    sequence: u64,
    keyframe: bool,
    codec_config: bool,
    payload_len: usize,
) -> EncodedFrame {
    EncodedFrame {
        sequence,
        width: 1_280,
        height: 800,
        capture_timestamp_micros: sequence,
        presentation_timestamp_micros: sequence as i64,
        observed_at: Instant::now(),
        keyframe,
        codec_config,
        discontinuity: false,
        data: Arc::from(vec![sequence as u8; payload_len]),
    }
}

#[cfg(test)]
struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for DropNotify {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_current_gop_replay_preserves_complete_startup_snapshot() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let old_observation = observed_now - Duration::from_secs(1);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor::default();

        let keyframe = frame(10, true);
        assert_eq!(
            cursor.classify(
                &keyframe,
                12,
                Some(observed_now),
                observed_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&keyframe);

        let delta = frame(11, false);
        assert_eq!(
            cursor.classify(
                &delta,
                12,
                Some(observed_now),
                observed_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
    }

    #[test]
    fn stalled_initial_current_gop_discards_its_remaining_suffix() {
        let replay_started_at = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let old_observation = replay_started_at - Duration::from_secs(1);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor::default();

        let keyframe = frame(10, true);
        assert_eq!(
            cursor.classify(
                &keyframe,
                12,
                Some(replay_started_at),
                replay_started_at,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&keyframe);

        let stalled_now = replay_started_at + maximum_replay_age + Duration::from_nanos(1);
        let delta = frame(11, false);
        assert_eq!(
            cursor.classify(
                &delta,
                12,
                Some(replay_started_at),
                stalled_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: 12,
            }
        );
        assert_eq!(cursor.last_sequence, Some(12));
        assert!(cursor.waiting_for_keyframe);
        assert!(cursor.discontinuity_pending);
    }

    #[test]
    fn fresh_current_gop_suffix_replays_normally() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        assert_eq!(maximum_replay_age, Duration::from_nanos(33_333_334));
        let frame = EncodedFrame {
            sequence: 11,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age,
            keyframe: false,
            codec_config: false,
            discontinuity: false,
            data: Arc::from([11]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(10),
            waiting_for_keyframe: false,
            discontinuity_pending: false,
        };

        assert_eq!(
            cursor.classify(&frame, 12, None, observed_now, maximum_replay_age),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&frame);
        assert_eq!(cursor.last_sequence, Some(11));
    }

    #[test]
    fn stale_current_gop_suffix_is_discarded_as_one_bounded_unit() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let frame = EncodedFrame {
            sequence: 11,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age - Duration::from_nanos(1),
            keyframe: false,
            codec_config: false,
            discontinuity: false,
            data: Arc::from([11]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(10),
            waiting_for_keyframe: false,
            discontinuity_pending: false,
        };

        assert_eq!(
            cursor.classify(&frame, 13, None, observed_now, maximum_replay_age),
            MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: 13,
            }
        );
        assert_eq!(cursor.last_sequence, Some(13));
        assert!(cursor.waiting_for_keyframe);
        assert!(cursor.discontinuity_pending);
    }

    #[test]
    fn stale_suffix_recovers_only_on_idr_marked_discontinuity() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: observed_now,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(13),
            waiting_for_keyframe: true,
            discontinuity_pending: true,
        };

        let delta = frame(14, false);
        assert_eq!(
            cursor.classify(&delta, 14, None, observed_now, maximum_replay_age),
            MediaReplayDecision::SkipUntilKeyframe
        );
        assert_eq!(cursor.last_sequence, Some(13));

        let idr = frame(15, true);
        assert_eq!(
            cursor.classify(&idr, 15, None, observed_now, maximum_replay_age),
            MediaReplayDecision::Send {
                discontinuity: true,
            }
        );
        cursor.commit_sent(&idr);
        assert_eq!(cursor.last_sequence, Some(15));
        assert!(!cursor.waiting_for_keyframe);
        assert!(!cursor.discontinuity_pending);

        let next_delta = frame(16, false);
        assert_eq!(
            cursor.classify(&next_delta, 16, None, observed_now, maximum_replay_age,),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
    }
}
