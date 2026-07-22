use std::sync::atomic::Ordering;
use std::time::Duration;

use iroh::Endpoint;
use iroh_moq::{Moq, MoqSession};
#[cfg(test)]
use moq_net::Track;
use moq_net::{BroadcastConsumer, GroupConsumer, TrackConsumer};
#[cfg(test)]
use sigil_protocol::MOQ_VIDEO_H264_TRACK;
use sigil_protocol::{
    FrameFlags, KeyframeRequestReasonV3, MAX_MEDIA_GROUP_BYTES_V3,
    MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MAX_MEDIA_OBJECT_ID_V3, MediaCodec, MediaFrame,
    decode_media_frame_object, media_moq_broadcast_name,
};
use tauri::{AppHandle, Manager};

use crate::commands::state::AppState;
use crate::media::audio_delivery::cancel_audio_generation;
use crate::media::frame_channel::{take_generation_owned, take_generation_owned_triple};
use crate::media::moq_catalog::subscribe_goq_video_track;
#[cfg(test)]
use crate::media::object_receiver::media_object_frame;
use crate::media::transport::CLIENT_ENDPOINT_CLOSE_TIMEOUT;

const CLIENT_MOQ_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_MOQ_SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
// Match the protocol's maximum single-object delivery horizon. A publisher may
// never hold a partially delivered object open indefinitely.
const CLIENT_MOQ_OBJECT_READ_TIMEOUT: Duration =
    Duration::from_millis(MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS as u64);
// Sigil's external encoder can take 500 ms to reach its next configured IDR.
// Allow another 500 ms for a relay path to deliver the superseding group.
const CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) async fn retire_upstream_moq_generation(
    app: &AppHandle,
    media_generation: u64,
    audio_generation: Option<u64>,
) -> bool {
    let state = app.state::<AppState>();
    let endpoint = {
        // Selection and retirement are one generation-checked transaction.
        // A stale reader must never close a replacement session whose task was
        // installed after an explicit disconnect/reconnect.
        let _connection_serial = state.client_connection_serial.lock().await;
        let media_connection = {
            let mut slot = state.media_connection.lock().await;
            take_generation_owned(&mut slot, media_generation)
        };
        let Some(media_connection) = media_connection else {
            return false;
        };

        {
            let mut control = state.media_control.lock().await;
            let _ = take_generation_owned(&mut control, media_generation);
        }
        {
            let mut delivery = state.frame_delivery.lock().await;
            let _ = take_generation_owned(&mut delivery, media_generation);
        }
        let feedback_connection = {
            let mut feedback = state.media_feedback.lock().await;
            take_generation_owned_triple(&mut feedback, media_generation)
                .map(|(connection, _)| connection)
        };
        *state.input_send.lock().await = None;

        let audio_connection = if let Some(audio_generation) = audio_generation {
            if let Err(error) = cancel_audio_generation(
                &state.audio_connection_generation,
                &state.audio_deliveries,
                audio_generation,
            ) {
                eprintln!(
                    "[client] failed to retire audio generation after upstream MoQ ended: {error}"
                );
            }
            let mut slot = state.audio_connection.lock().await;
            take_generation_owned(&mut slot, audio_generation)
        } else {
            None
        };

        let endpoint = state.client_endpoint.lock().await.take();
        media_connection.close(0_u32.into(), b"upstream MoQ media ended");
        if let Some(feedback_connection) = feedback_connection {
            feedback_connection.close(0_u32.into(), b"upstream MoQ media ended");
        }
        if let Some(audio_connection) = audio_connection {
            audio_connection.close(0_u32.into(), b"upstream MoQ media ended");
        }
        state
            .client_connection_active
            .store(false, Ordering::SeqCst);
        endpoint
    };

    if let Some(endpoint) = endpoint
        && tokio::time::timeout(CLIENT_ENDPOINT_CLOSE_TIMEOUT, endpoint.close())
            .await
            .is_err()
    {
        eprintln!("[client] timed out retiring endpoint after upstream MoQ media ended");
    }
    true
}

#[derive(Debug)]
pub(crate) enum MoqMediaReadOutcome {
    Frame {
        frame: MediaFrame,
        discontinuity: bool,
    },
    Dropped {
        reason: KeyframeRequestReasonV3,
    },
    Malformed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoqGroupRecovery {
    /// Sigil deliberately cancels the old GOP when a configured IDR starts a
    /// replacement. This is normal live-edge supersession, not evidence that
    /// another keyframe request is needed.
    ExpectedSupersession,
    RecoverableGap(KeyframeRequestReasonV3),
}

struct MoqGroupCursor {
    group: GroupConsumer,
    sequence: u64,
    object_count: usize,
    object_bytes: usize,
    group_gap: bool,
    replacement_for_cancelled_group: bool,
}

/// Own every upstream handle for as long as the Portal frame task is alive.
/// In particular, dropping `Moq` aborts its actor and dropping the consumers
/// cancels their subscriptions, so none of these may be scoped only to setup.
struct MoqMediaLifetime {
    _moq: Moq,
    _session: MoqSession,
    _broadcast: BroadcastConsumer,
}

pub(crate) struct MoqMediaReceiver {
    _lifetime: Option<MoqMediaLifetime>,
    track: TrackConsumer,
    current_group: Option<MoqGroupCursor>,
    last_group_sequence: Option<u64>,
    last_frame_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    pending_group_recovery: Option<MoqGroupRecovery>,
}

impl MoqMediaReceiver {
    fn new(
        moq: Moq,
        session: MoqSession,
        broadcast: BroadcastConsumer,
        track: TrackConsumer,
    ) -> Self {
        Self {
            _lifetime: Some(MoqMediaLifetime {
                _moq: moq,
                _session: session,
                _broadcast: broadcast,
            }),
            track,
            current_group: None,
            last_group_sequence: None,
            last_frame_sequence: None,
            waiting_for_keyframe: true,
            pending_group_recovery: None,
        }
    }

    pub(crate) fn last_frame_sequence(&self) -> Option<u64> {
        self.last_frame_sequence
    }

    #[cfg(test)]
    fn for_test(track: TrackConsumer) -> Self {
        Self {
            _lifetime: None,
            track,
            current_group: None,
            last_group_sequence: None,
            last_frame_sequence: None,
            waiting_for_keyframe: true,
            pending_group_recovery: None,
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<MoqMediaReadOutcome>, String> {
        self.next_with_timeouts(
            CLIENT_MOQ_OBJECT_READ_TIMEOUT,
            CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT,
        )
        .await
    }

    async fn next_with_timeouts(
        &mut self,
        object_read_timeout: Duration,
        group_recovery_timeout: Duration,
    ) -> Result<Option<MoqMediaReadOutcome>, String> {
        loop {
            if self.current_group.is_none() {
                let replacement_for_cancelled_group = self.pending_group_recovery.is_some();
                let group = if replacement_for_cancelled_group {
                    match tokio::time::timeout(
                        group_recovery_timeout,
                        self.track.next_group(),
                    )
                    .await
                    {
                        Ok(group) => group,
                        Err(_) => {
                            let recovery = self
                                .pending_group_recovery
                                .take()
                                .expect("pending MoQ recovery reason was present");
                            return match recovery {
                                MoqGroupRecovery::ExpectedSupersession => Err(format!(
                                    "Timed out after {} ms waiting for the MoQ group that supersedes an expected GOP cancellation",
                                    group_recovery_timeout.as_millis()
                                )),
                                MoqGroupRecovery::RecoverableGap(reason) => {
                                    Ok(Some(MoqMediaReadOutcome::Dropped { reason }))
                                }
                            };
                        }
                    }
                } else {
                    self.track.next_group().await
                }
                .map_err(|error| format!("Upstream MoQ video track failed: {error}"))?;
                let Some(group) = group else {
                    return Ok(None);
                };
                self.pending_group_recovery = None;
                let sequence = group.sequence;
                let group_gap =
                    match classify_moq_group_sequence(self.last_group_sequence, sequence) {
                        Ok(group_gap) => group_gap,
                        Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
                    };
                if group_gap {
                    self.waiting_for_keyframe = true;
                }
                self.current_group = Some(MoqGroupCursor {
                    group,
                    sequence,
                    object_count: 0,
                    object_bytes: 0,
                    group_gap,
                    replacement_for_cancelled_group,
                });
            }

            let cursor = self
                .current_group
                .as_mut()
                .expect("MoQ group cursor was initialized");
            let object =
                match tokio::time::timeout(object_read_timeout, cursor.group.read_frame()).await {
                    Err(_) => {
                        let sequence = cursor.sequence;
                        self.last_group_sequence = Some(sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        return Ok(Some(MoqMediaReadOutcome::Dropped {
                            reason: KeyframeRequestReasonV3::DeliveryTimeout,
                        }));
                    }
                    Ok(Ok(object)) => object,
                    Ok(Err(error)) if moq_group_error_is_recoverable(&error) => {
                        let sequence = cursor.sequence;
                        self.last_group_sequence = Some(sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        let reason = moq_group_error_reason(&error);
                        self.pending_group_recovery =
                            Some(if matches!(error, moq_net::Error::Cancel) {
                                MoqGroupRecovery::ExpectedSupersession
                            } else {
                                MoqGroupRecovery::RecoverableGap(reason)
                            });
                        continue;
                    }
                    Ok(Err(error)) => {
                        return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                            "Upstream MoQ group {} failed: {error}",
                            cursor.sequence
                        ))));
                    }
                };
            let Some(object) = object else {
                if cursor.object_count == 0 {
                    if cursor.replacement_for_cancelled_group {
                        self.last_group_sequence = Some(cursor.sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        return Ok(Some(MoqMediaReadOutcome::Dropped {
                            reason: KeyframeRequestReasonV3::TransportGap,
                        }));
                    }
                    return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                        "Upstream MoQ group {} was empty",
                        cursor.sequence
                    ))));
                }
                self.last_group_sequence = Some(cursor.sequence);
                self.current_group = None;
                continue;
            };

            let next_group_bytes = match validate_moq_object_bounds(
                cursor.sequence,
                cursor.object_count,
                cursor.object_bytes,
                object.len(),
            ) {
                Ok(next_group_bytes) => next_group_bytes,
                Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
            };
            let frame = match decode_media_frame_object(&object) {
                Ok(frame) => frame,
                Err(error) => {
                    return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                        "Invalid upstream MoQ media object in group {} object {}: {error}",
                        cursor.sequence, cursor.object_count
                    ))));
                }
            };

            let first_object = cursor.object_count == 0;
            let frame_contiguous = match validate_moq_group_frame(
                cursor.sequence,
                first_object,
                self.last_frame_sequence,
                &frame,
            ) {
                Ok(frame_contiguous) => frame_contiguous,
                Err(_error) if first_object && cursor.replacement_for_cancelled_group => {
                    self.last_group_sequence = Some(cursor.sequence);
                    self.current_group = None;
                    self.waiting_for_keyframe = true;
                    return Ok(Some(MoqMediaReadOutcome::Dropped {
                        reason: KeyframeRequestReasonV3::TransportGap,
                    }));
                }
                Err(_error) if !first_object => {
                    self.last_group_sequence = Some(cursor.sequence);
                    self.current_group = None;
                    self.waiting_for_keyframe = true;
                    return Ok(Some(MoqMediaReadOutcome::Dropped {
                        reason: KeyframeRequestReasonV3::TransportGap,
                    }));
                }
                Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
            };
            let discontinuity = self.waiting_for_keyframe
                || (first_object && cursor.group_gap)
                || (first_object && !frame_contiguous)
                || frame.header.flags.contains(FrameFlags::DISCONTINUITY);
            cursor.object_count += 1;
            cursor.object_bytes = next_group_bytes;
            self.last_frame_sequence = Some(frame.header.sequence);
            self.waiting_for_keyframe = false;
            return Ok(Some(MoqMediaReadOutcome::Frame {
                frame,
                discontinuity,
            }));
        }
    }
}

fn classify_moq_group_sequence(previous: Option<u64>, current: u64) -> Result<bool, String> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    if current <= previous {
        return Err(format!(
            "Upstream MoQ group sequence did not increase: previous={previous}, current={current}"
        ));
    }
    Ok(previous.checked_add(1) != Some(current))
}

fn validate_moq_object_bounds(
    group_sequence: u64,
    object_count: usize,
    object_bytes: usize,
    object_len: usize,
) -> Result<usize, String> {
    let max_objects = MAX_MEDIA_OBJECT_ID_V3 as usize + 1;
    if object_count >= max_objects {
        return Err(format!(
            "Upstream MoQ group {group_sequence} exceeded {max_objects} media objects"
        ));
    }
    let next_group_bytes = object_bytes
        .checked_add(object_len)
        .ok_or_else(|| "Upstream MoQ group byte count overflowed".to_string())?;
    if next_group_bytes > MAX_MEDIA_GROUP_BYTES_V3 {
        return Err(format!(
            "Upstream MoQ group {group_sequence} exceeded the {MAX_MEDIA_GROUP_BYTES_V3} byte limit"
        ));
    }
    Ok(next_group_bytes)
}

fn validate_moq_group_frame(
    group_sequence: u64,
    first_object: bool,
    last_frame_sequence: Option<u64>,
    frame: &MediaFrame,
) -> Result<bool, String> {
    if first_object
        && !(frame.header.codec == MediaCodec::H264
            && frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG))
    {
        return Err(format!(
            "Upstream MoQ group {group_sequence} did not begin with a configured H.264 keyframe"
        ));
    }
    let contiguous = last_frame_sequence
        .is_none_or(|previous| previous.checked_add(1) == Some(frame.header.sequence));
    if !first_object && !contiguous {
        return Err(format!(
            "Upstream MoQ group {group_sequence} contains a non-contiguous access-unit sequence"
        ));
    }
    Ok(contiguous)
}

fn moq_group_error_is_recoverable(error: &moq_net::Error) -> bool {
    matches!(
        error,
        moq_net::Error::Cancel
            | moq_net::Error::Old
            | moq_net::Error::Timeout
            | moq_net::Error::Dropped
            | moq_net::Error::CacheFull
            | moq_net::Error::Remote(0 | 2 | 3 | 24 | 26)
    )
}

fn moq_group_error_reason(error: &moq_net::Error) -> KeyframeRequestReasonV3 {
    if matches!(error, moq_net::Error::Timeout | moq_net::Error::Remote(3)) {
        KeyframeRequestReasonV3::DeliveryTimeout
    } else {
        KeyframeRequestReasonV3::TransportGap
    }
}

pub(crate) async fn open_upstream_moq_media(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    session_id: u64,
) -> Result<(MoqMediaReceiver, iroh::endpoint::Connection), String> {
    let broadcast_name = media_moq_broadcast_name(session_id)
        .map_err(|error| format!("Invalid MoQ media session name: {error}"))?;
    let moq = Moq::new(endpoint.clone());
    let mut session =
        tokio::time::timeout(CLIENT_MOQ_CONNECT_TIMEOUT, moq.connect(address.clone()))
            .await
            .map_err(|_| "Timed out connecting upstream MoQ media session".to_string())?
            .map_err(|error| format!("Failed to connect upstream MoQ media session: {error:#}"))?;
    if session.remote_id() != address.id {
        session.close(1, b"remote identity mismatch");
        return Err(format!(
            "Upstream MoQ connected to unexpected peer {}; expected {}",
            session.remote_id(),
            address.id
        ));
    }
    let diagnostics_connection = session.conn().clone();
    let broadcast = tokio::time::timeout(
        CLIENT_MOQ_SUBSCRIBE_TIMEOUT,
        session.subscribe(&broadcast_name),
    )
    .await
    .map_err(|_| format!("Timed out waiting for upstream MoQ broadcast {broadcast_name}"))?
    .map_err(|error| {
        format!("Failed to subscribe to upstream MoQ broadcast {broadcast_name}: {error}")
    })?;
    let catalog = subscribe_goq_video_track(&broadcast, CLIENT_MOQ_SUBSCRIBE_TIMEOUT).await?;
    eprintln!("[client] moq catalog: {}", catalog.mode.label());
    Ok((
        MoqMediaReceiver::new(moq, session, broadcast, catalog.track),
        diagnostics_connection,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_moq_group_ids_detect_only_real_transport_gaps() {
        assert!(!classify_moq_group_sequence(None, 41).unwrap());
        assert!(!classify_moq_group_sequence(Some(41), 42).unwrap());
        assert!(classify_moq_group_sequence(Some(42), 44).unwrap());
        assert!(classify_moq_group_sequence(Some(44), 44).is_err());
        assert!(classify_moq_group_sequence(Some(44), 43).is_err());
    }

    #[test]
    fn upstream_moq_group_gap_keyframe_is_the_recovery_barrier() {
        let keyframe = media_object_frame(80, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let contiguous = validate_moq_group_frame(9, true, Some(42), &keyframe).unwrap();
        assert!(!contiguous);

        // A native group-id gap puts the decoder into resync, but this exact
        // configured frame 0 exits it immediately. There is deliberately no
        // keyframe-request action on MoqMediaReadOutcome::Frame: requesting a
        // replacement here would recreate the grouped-v3 feedback loop.
        let group_gap = classify_moq_group_sequence(Some(7), 9).unwrap();
        let discontinuity = group_gap || !contiguous;
        assert!(discontinuity);
    }

    #[tokio::test]
    async fn upstream_moq_idr_abort_to_next_frame_zero_has_no_feedback_loop() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());

        let mut prior_group = producer.append_group().unwrap();
        let prior_keyframe =
            media_object_frame(40, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        prior_group
            .write_frame(sigil_protocol::encode_media_frame_object(&prior_keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));

        prior_group.abort(moq_net::Error::Cancel).unwrap();
        let mut replacement = producer.append_group().unwrap();
        let replacement_keyframe =
            media_object_frame(80, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        replacement
            .write_frame(sigil_protocol::encode_media_frame_object(&replacement_keyframe).unwrap())
            .unwrap();
        replacement.finish().unwrap();

        // `next` consumes the expected Cancel internally and returns the
        // replacement frame directly. A Dropped outcome here would enqueue a
        // needless keyframe request and recreate the recovery feedback loop.
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame {
                discontinuity: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn upstream_moq_expected_cancel_without_replacement_is_terminal_without_request() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let mut group = producer.append_group().unwrap();
        let keyframe = media_object_frame(1, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        group
            .write_frame(sigil_protocol::encode_media_frame_object(&keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));
        group.abort(moq_net::Error::Cancel).unwrap();

        let error = receiver
            .next_with_timeouts(Duration::from_millis(25), Duration::from_millis(25))
            .await
            .unwrap_err();
        assert!(error.contains("expected GOP cancellation"));
    }

    #[tokio::test]
    async fn upstream_moq_partial_object_read_has_protocol_bounded_deadline() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let _stalled_group = producer.append_group().unwrap();

        assert!(matches!(
            receiver
                .next_with_timeouts(Duration::from_millis(25), Duration::from_millis(25))
                .await
                .unwrap(),
            Some(MoqMediaReadOutcome::Dropped {
                reason: KeyframeRequestReasonV3::DeliveryTimeout
            })
        ));
        assert_eq!(
            CLIENT_MOQ_OBJECT_READ_TIMEOUT,
            Duration::from_millis(MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS as u64)
        );
        assert!(CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT >= Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn upstream_moq_invalid_cancel_replacement_requests_recovery() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let mut prior = producer.append_group().unwrap();
        let keyframe = media_object_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        prior
            .write_frame(sigil_protocol::encode_media_frame_object(&keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));
        prior.abort(moq_net::Error::Cancel).unwrap();

        let mut invalid_replacement = producer.append_group().unwrap();
        let delta = media_object_frame(11, FrameFlags::NONE);
        invalid_replacement
            .write_frame(sigil_protocol::encode_media_frame_object(&delta).unwrap())
            .unwrap();
        invalid_replacement.finish().unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Dropped {
                reason: KeyframeRequestReasonV3::TransportGap
            })
        ));
    }

    #[test]
    fn upstream_moq_requires_configured_keyframe_zero_and_contiguous_deltas() {
        let delta = media_object_frame(10, FrameFlags::NONE);
        assert!(validate_moq_group_frame(3, true, None, &delta).is_err());

        let unconfigured_keyframe = media_object_frame(10, FrameFlags::KEYFRAME);
        assert!(validate_moq_group_frame(3, true, None, &unconfigured_keyframe).is_err());

        let configured =
            media_object_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        assert!(validate_moq_group_frame(3, true, None, &configured).is_ok());
        assert!(validate_moq_group_frame(3, false, Some(8), &delta).is_err());
        assert!(validate_moq_group_frame(3, false, Some(9), &delta).is_ok());
    }

    #[test]
    fn upstream_moq_bounds_objects_and_group_bytes_before_growth() {
        assert_eq!(validate_moq_object_bounds(1, 0, 0, 40).unwrap(), 40);
        assert!(validate_moq_object_bounds(1, MAX_MEDIA_OBJECT_ID_V3 as usize + 1, 0, 1,).is_err());
        assert!(validate_moq_object_bounds(1, 0, MAX_MEDIA_GROUP_BYTES_V3, 1).is_err());
        assert!(validate_moq_object_bounds(1, 0, usize::MAX, 1).is_err());
    }

    #[test]
    fn upstream_moq_cancellation_is_resync_but_protocol_errors_are_terminal() {
        for recoverable in [
            moq_net::Error::Cancel,
            moq_net::Error::Old,
            moq_net::Error::Timeout,
            moq_net::Error::Dropped,
            moq_net::Error::CacheFull,
            moq_net::Error::Remote(3),
        ] {
            assert!(moq_group_error_is_recoverable(&recoverable));
        }
        assert!(!moq_group_error_is_recoverable(
            &moq_net::Error::ProtocolViolation
        ));
        assert!(!moq_group_error_is_recoverable(&moq_net::Error::WrongSize));
        assert_eq!(
            moq_group_error_reason(&moq_net::Error::Timeout),
            KeyframeRequestReasonV3::DeliveryTimeout
        );
        assert_eq!(
            moq_group_error_reason(&moq_net::Error::Cancel),
            KeyframeRequestReasonV3::TransportGap
        );
    }
}
