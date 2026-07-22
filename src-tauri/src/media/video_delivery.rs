use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use base64::Engine;
use openh264::{formats::YUVSource, nal_units};
use sigil_protocol::{FrameFlags, KeyframeRequestReasonV3, MediaCodec, read_media_frame};
use tauri::{
    AppHandle, Emitter,
    ipc::{Channel, Response},
};

use crate::commands::state::MediaControlRequestSender;
use crate::media::frame_channel::{
    FrameEnvelopeMetadata, FramePayload, byte_to_codec, emit_frame_error, encode_frame_envelope,
    release_frame_channel_slot, try_reserve_frame_channel_slot, validate_legacy_media_header,
};
use crate::media::media_control::try_queue_media_keyframe_request;
use crate::media::metrics::{ClientMediaMetrics, lock_client_media_metrics};
use crate::media::moq_receiver::{
    MoqMediaReadOutcome, MoqMediaReceiver, retire_upstream_moq_generation,
};
use crate::media::network_diagnostics::{
    NetworkLeg, NetworkSessionDiagnostics, lock_network_diagnostics,
};
use crate::media::object_receiver::{
    MediaObjectReadOutcome, MediaObjectReadOutcomeV3, MediaObjectReceiver, MediaObjectReceiverV3,
    MediaObjectSequence, MediaObjectSequenceDecision, MediaObjectSequenceDecisionV3,
    MediaObjectSequenceV3,
};
use crate::media::transport::MediaTransport;

const LEGACY_MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_FRAME_STATS_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct VideoDeliveryRequest {
    pub(crate) app: AppHandle,
    pub(crate) endpoint: iroh::Endpoint,
    pub(crate) frame_recv: iroh::endpoint::RecvStream,
    pub(crate) frame_connection: iroh::endpoint::Connection,
    pub(crate) input_connection: iroh::endpoint::Connection,
    pub(crate) audio_connection: Option<iroh::endpoint::Connection>,
    pub(crate) network_diagnostics: Arc<StdMutex<NetworkSessionDiagnostics>>,
    pub(crate) media_control_requests: Option<MediaControlRequestSender>,
    pub(crate) upstream_moq_media: Option<MoqMediaReceiver>,
    pub(crate) frame_events_in_flight: Arc<AtomicUsize>,
    pub(crate) frame_channel: Channel<Response>,
    pub(crate) media_transport: MediaTransport,
    pub(crate) media_generation: u64,
    pub(crate) connected_audio_generation: Option<u64>,
    pub(crate) use_webcodecs: bool,
}

pub(crate) async fn run_video_delivery(request: VideoDeliveryRequest) {
    let VideoDeliveryRequest {
        app,
        endpoint,
        mut frame_recv,
        frame_connection: frame_connection_for_stats,
        input_connection,
        audio_connection: audio_connection_for_stats,
        network_diagnostics,
        media_control_requests,
        upstream_moq_media,
        frame_events_in_flight,
        frame_channel,
        media_transport,
        media_generation,
        connected_audio_generation,
        use_webcodecs,
    } = request;

    let metrics_started = Instant::now();
    let mut initial_metrics = ClientMediaMetrics::default();
    // Joining a running encoder commonly starts in the middle of a GOP.
    // The initial keyframe wait is a real resync episode, just like a wait
    // entered after frontend backpressure, so account for it from t=0.
    initial_metrics.begin_frontend_resync(Duration::ZERO);
    let metrics = Arc::new(StdMutex::new(initial_metrics));
    let mut previous_sequence: Option<u64> = None;
    let mut frontend_waiting_for_keyframe = true;
    let mut media_objects = (media_transport == MediaTransport::IndependentObjectsV2)
        .then(|| MediaObjectReceiver::new(frame_connection_for_stats.clone()));
    let mut media_object_sequence = MediaObjectSequence::new();
    let mut media_objects_v3 = (media_transport == MediaTransport::GroupedObjectsV3)
        .then(|| MediaObjectReceiverV3::new(frame_connection_for_stats.clone()));
    let mut media_object_sequence_v3 = MediaObjectSequenceV3::new();
    let mut upstream_moq_media = upstream_moq_media;

    let mut decoder = if use_webcodecs {
        None
    } else {
        match openh264::decoder::Decoder::new() {
            Ok(d) => Some(d),
            Err(e) => {
                emit_frame_error(&app, media_generation, format!("Decoder init failed: {e}"));
                if media_transport == MediaTransport::UpstreamMoq {
                    retire_upstream_moq_generation(
                        &app,
                        media_generation,
                        connected_audio_generation,
                    )
                    .await;
                }
                return;
            }
        }
    };

    let (stats_stop, mut stats_stop_rx) = tokio::sync::watch::channel(false);
    let stats_app = app.clone();
    let stats_metrics = Arc::clone(&metrics);
    let stats_in_flight = Arc::clone(&frame_events_in_flight);
    let stats_connection = frame_connection_for_stats.clone();
    let stats_input_connection = input_connection.clone();
    let stats_audio_connection = audio_connection_for_stats.clone();
    let stats_network_diagnostics = Arc::clone(&network_diagnostics);
    let stats_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLIENT_FRAME_STATS_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Tokio intervals tick immediately once. Consume that tick so the
        // first payload represents a full diagnostics interval.
        interval.tick().await;
        let mut last_path_sample = Instant::now() - Duration::from_secs(1);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if last_path_sample.elapsed() >= Duration::from_secs(1) {
                        let mut diagnostics = lock_network_diagnostics(&stats_network_diagnostics);
                        diagnostics.observe_connection(NetworkLeg::Media, &stats_connection);
                        diagnostics.observe_connection(NetworkLeg::Input, &stats_input_connection);
                        if let Some(audio_connection) = stats_audio_connection.as_ref() {
                            diagnostics.observe_connection(NetworkLeg::Audio, audio_connection);
                        }
                        last_path_sample = Instant::now();
                    }
                    let queue_depth = stats_in_flight.load(Ordering::SeqCst);
                    let network_snapshot = lock_network_diagnostics(&stats_network_diagnostics)
                        .snapshot(Instant::now());
                    let payload = lock_client_media_metrics(&stats_metrics).snapshot(
                        metrics_started.elapsed(),
                        queue_depth,
                        network_snapshot,
                        media_generation,
                    );
                    let _ = stats_app.emit("frame-stats", payload);
                }
                changed = stats_stop_rx.changed() => {
                    if changed.is_err() || *stats_stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });

    'frames: loop {
        let (
            w,
            h,
            frame_buf,
            is_keyframe,
            codec,
            sequence,
            capture_timestamp_micros,
            pts_micros,
            discontinuity,
            codec_config,
        ) = match media_transport {
            MediaTransport::UpstreamMoq => {
                let receiver = upstream_moq_media
                    .as_mut()
                    .expect("upstream MoQ receiver must exist for MoQ transport");
                loop {
                    let outcome = match receiver.next().await {
                        Ok(Some(outcome)) => outcome,
                        Ok(None) => {
                            emit_frame_error(
                                &app,
                                media_generation,
                                "Upstream MoQ video track closed",
                            );
                            break 'frames;
                        }
                        Err(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                    };
                    match outcome {
                        MoqMediaReadOutcome::Dropped { reason } => {
                            frontend_waiting_for_keyframe = true;
                            let mut metrics = lock_client_media_metrics(&metrics);
                            metrics.observe_transport_object_drop(false);
                            metrics.begin_frontend_resync(metrics_started.elapsed());
                            try_queue_media_keyframe_request(
                                media_control_requests.as_ref(),
                                reason,
                                receiver.last_frame_sequence(),
                            );
                        }
                        MoqMediaReadOutcome::Malformed(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                        MoqMediaReadOutcome::Frame {
                            frame,
                            discontinuity,
                        } => {
                            let codec = match frame.header.codec {
                                MediaCodec::H264 => "h264".to_string(),
                            };
                            break (
                                u32::from(frame.header.width),
                                u32::from(frame.header.height),
                                frame.payload,
                                frame.header.flags.contains(FrameFlags::KEYFRAME),
                                codec,
                                Some(frame.header.sequence),
                                Some(frame.header.capture_timestamp_us),
                                Some(frame.header.pts_us),
                                discontinuity,
                                frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                            );
                        }
                    }
                }
            }
            MediaTransport::GroupedObjectsV3 => {
                let receiver = media_objects_v3
                    .as_mut()
                    .expect("media v3 receiver must exist for media v3 transport");
                loop {
                    let outcome = match receiver.next().await {
                        Ok(Some(outcome)) => outcome,
                        Ok(None) => {
                            emit_frame_error(&app, media_generation, "Connection closed");
                            break 'frames;
                        }
                        Err(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                    };
                    match outcome {
                        MediaObjectReadOutcomeV3::Dropped { reason, .. } => {
                            let begins_resync = media_object_sequence_v3.note_dropped_object();
                            let mut metrics = lock_client_media_metrics(&metrics);
                            metrics.observe_transport_object_drop(!begins_resync);
                            if begins_resync {
                                frontend_waiting_for_keyframe = true;
                                metrics.begin_frontend_resync(metrics_started.elapsed());
                                try_queue_media_keyframe_request(
                                    media_control_requests.as_ref(),
                                    reason,
                                    media_object_sequence_v3.last_sequence(),
                                );
                            }
                        }
                        MediaObjectReadOutcomeV3::Malformed(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                        MediaObjectReadOutcomeV3::Object { object, .. } => {
                            let was_waiting = media_object_sequence_v3.waiting_for_keyframe();
                            let discontinuity = match media_object_sequence_v3.classify(&object) {
                                MediaObjectSequenceDecisionV3::Deliver { discontinuity } => {
                                    discontinuity
                                }
                                MediaObjectSequenceDecisionV3::DropLate => {
                                    lock_client_media_metrics(&metrics)
                                        .observe_transport_object_drop(true);
                                    continue;
                                }
                                MediaObjectSequenceDecisionV3::DropUntilKeyframe => {
                                    frontend_waiting_for_keyframe = true;
                                    let mut metrics = lock_client_media_metrics(&metrics);
                                    metrics.observe_transport_object_drop(false);
                                    metrics.begin_frontend_resync(metrics_started.elapsed());
                                    if !was_waiting {
                                        try_queue_media_keyframe_request(
                                            media_control_requests.as_ref(),
                                            KeyframeRequestReasonV3::TransportGap,
                                            media_object_sequence_v3.last_sequence(),
                                        );
                                    }
                                    continue;
                                }
                            };
                            let codec = match object.header.codec {
                                MediaCodec::H264 => "h264".to_string(),
                            };
                            break (
                                u32::from(object.header.width),
                                u32::from(object.header.height),
                                object.payload,
                                object.header.flags.contains(FrameFlags::KEYFRAME),
                                codec,
                                Some(object.header.sequence),
                                Some(object.header.capture_timestamp_us),
                                Some(object.header.pts_us),
                                discontinuity,
                                object.header.flags.contains(FrameFlags::CODEC_CONFIG),
                            );
                        }
                    }
                }
            }
            MediaTransport::IndependentObjectsV2 => {
                let receiver = media_objects
                    .as_mut()
                    .expect("media v2 receiver must exist for media v2 transport");
                loop {
                    let outcome = match receiver.next().await {
                        Ok(Some(outcome)) => outcome,
                        Ok(None) => {
                            emit_frame_error(&app, media_generation, "Connection closed");
                            break 'frames;
                        }
                        Err(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                    };
                    match outcome {
                        MediaObjectReadOutcome::Dropped { object_index } => {
                            let begins_resync =
                                media_object_sequence.note_dropped_object(object_index);
                            let mut metrics = lock_client_media_metrics(&metrics);
                            metrics.observe_transport_object_drop(!begins_resync);
                            if begins_resync {
                                frontend_waiting_for_keyframe = true;
                                metrics.begin_frontend_resync(metrics_started.elapsed());
                            }
                        }
                        MediaObjectReadOutcome::Malformed(error) => {
                            emit_frame_error(&app, media_generation, error);
                            break 'frames;
                        }
                        MediaObjectReadOutcome::Frame {
                            object_index,
                            frame,
                        } => {
                            let discontinuity =
                                match media_object_sequence.classify(object_index, &frame) {
                                    MediaObjectSequenceDecision::Deliver { discontinuity } => {
                                        discontinuity
                                    }
                                    MediaObjectSequenceDecision::DropLate => {
                                        lock_client_media_metrics(&metrics)
                                            .observe_transport_object_drop(true);
                                        continue;
                                    }
                                    MediaObjectSequenceDecision::DropUntilKeyframe => {
                                        frontend_waiting_for_keyframe = true;
                                        let mut metrics = lock_client_media_metrics(&metrics);
                                        metrics.observe_transport_object_drop(false);
                                        metrics.begin_frontend_resync(metrics_started.elapsed());
                                        continue;
                                    }
                                };
                            let codec = match frame.header.codec {
                                MediaCodec::H264 => "h264".to_string(),
                            };
                            break (
                                u32::from(frame.header.width),
                                u32::from(frame.header.height),
                                frame.payload,
                                frame.header.flags.contains(FrameFlags::KEYFRAME),
                                codec,
                                Some(frame.header.sequence),
                                Some(frame.header.capture_timestamp_us),
                                Some(frame.header.pts_us),
                                discontinuity,
                                frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                            );
                        }
                    }
                }
            }
            MediaTransport::ReliableStreamV1 => {
                // Gamescope's PipeWire stream is damage-driven: a static
                // screen can legitimately produce no encoded frame for an
                // arbitrary period. Connection closure and parser/source
                // errors remain terminal; frame silence does not.
                let frame = match read_media_frame(&mut frame_recv).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        emit_frame_error(&app, media_generation, "Connection closed");
                        break;
                    }
                    Err(error) => {
                        emit_frame_error(
                            &app,
                            media_generation,
                            format!("Invalid media stream: {error}"),
                        );
                        break;
                    }
                };
                let codec = match frame.header.codec {
                    MediaCodec::H264 => "h264".to_string(),
                };
                (
                    u32::from(frame.header.width),
                    u32::from(frame.header.height),
                    frame.payload,
                    frame.header.flags.contains(FrameFlags::KEYFRAME),
                    codec,
                    Some(frame.header.sequence),
                    Some(frame.header.capture_timestamp_us),
                    Some(frame.header.pts_us),
                    frame.header.flags.contains(FrameFlags::DISCONTINUITY),
                    frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                )
            }
            MediaTransport::LegacyV0 => {
                let mut header = [0u8; 14];
                match tokio::time::timeout(
                    LEGACY_MEDIA_IDLE_TIMEOUT,
                    frame_recv.read_exact(&mut header),
                )
                .await
                {
                    Err(_) => {
                        emit_frame_error(&app, media_generation, "Media stream idle timeout");
                        break;
                    }
                    Ok(Err(_)) => {
                        emit_frame_error(&app, media_generation, "Connection lost");
                        break;
                    }
                    Ok(Ok(_)) => {}
                }
                let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
                let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                let frame_len =
                    u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
                if let Err(error) = validate_legacy_media_header(w, h, frame_len) {
                    emit_frame_error(&app, media_generation, error);
                    break;
                }

                let is_keyframe = header[12] == 1;
                let codec = byte_to_codec(header[13]).to_string();

                let mut frame_buf = vec![0u8; frame_len];
                match tokio::time::timeout(
                    LEGACY_MEDIA_IDLE_TIMEOUT,
                    frame_recv.read_exact(&mut frame_buf),
                )
                .await
                {
                    Err(_) => {
                        emit_frame_error(&app, media_generation, "Media payload idle timeout");
                        break;
                    }
                    Ok(Err(_)) => {
                        emit_frame_error(&app, media_generation, "Connection lost");
                        break;
                    }
                    Ok(Ok(_)) => {}
                }
                (
                    w,
                    h,
                    frame_buf,
                    is_keyframe,
                    codec,
                    None,
                    None,
                    None,
                    false,
                    false,
                )
            }
        };
        lock_client_media_metrics(&metrics).observe_transport_receive(
            metrics_started.elapsed(),
            sequence,
            is_keyframe,
        );

        let sequence_gap = match sequence.zip(previous_sequence) {
            Some((current, previous)) => match sequence_gap(previous, current) {
                Ok(gap) => gap,
                Err(error) => {
                    emit_frame_error(&app, media_generation, error);
                    break;
                }
            },
            None => 0,
        };
        if sequence.is_some() {
            previous_sequence = sequence;
        }
        lock_client_media_metrics(&metrics).observe_sequence_drop(sequence_gap);

        if frontend_waiting_for_keyframe && !is_keyframe {
            lock_client_media_metrics(&metrics).observe_frontend_resync_drop();
            continue;
        }
        if !try_reserve_frame_channel_slot(&frame_events_in_flight) {
            let begins_resync = !frontend_waiting_for_keyframe;
            frontend_waiting_for_keyframe = true;
            let mut metrics = lock_client_media_metrics(&metrics);
            metrics.observe_frontend_queue_drop();
            metrics.begin_frontend_resync(metrics_started.elapsed());
            if begins_resync {
                try_queue_media_keyframe_request(
                    media_control_requests.as_ref(),
                    KeyframeRequestReasonV3::FrontendBackpressure,
                    sequence,
                );
            }
            continue;
        }
        lock_client_media_metrics(&metrics)
            .observe_frontend_queue_depth(frame_events_in_flight.load(Ordering::SeqCst));

        let delivered_to_frontend;
        if use_webcodecs {
            let envelope = match encode_frame_envelope(
                FrameEnvelopeMetadata {
                    width: w,
                    height: h,
                    codec: &codec,
                    keyframe: is_keyframe,
                    discontinuity,
                    codec_config,
                    sequence,
                    capture_timestamp_micros,
                    pts_micros,
                },
                &frame_buf,
            ) {
                Ok(envelope) => envelope,
                Err(error) => {
                    emit_frame_error(&app, media_generation, error);
                    release_frame_channel_slot(&frame_events_in_flight);
                    break;
                }
            };
            let ipc_send_started = Instant::now();
            let send_result = frame_channel.send(Response::new(envelope));
            lock_client_media_metrics(&metrics).observe_frontend_ipc_send_duration(
                metrics_started.elapsed(),
                ipc_send_started.elapsed(),
            );
            if send_result.is_err() {
                release_frame_channel_slot(&frame_events_in_flight);
                break;
            }
            delivered_to_frontend = true;
        } else if let Some(ref mut dec) = decoder {
            let mut emitted = false;
            for nal in nal_units(&frame_buf) {
                if let Ok(Some(yuv)) = dec.decode(nal) {
                    let (yw, yh) = yuv.dimensions();
                    let rgb_len = yuv.rgb8_len();
                    let mut rgb_raw = vec![0u8; rgb_len];
                    yuv.write_rgb8(&mut rgb_raw);

                    let img = match image::RgbImage::from_raw(yw as u32, yh as u32, rgb_raw) {
                        Some(img) => img,
                        None => continue,
                    };
                    let mut jpeg_buf = Vec::with_capacity(30_000);
                    if image::DynamicImage::ImageRgb8(img)
                        .write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
                        .is_err()
                    {
                        continue;
                    }

                    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
                    let ipc_send_started = Instant::now();
                    let send_result = app.emit(
                        "frame",
                        FramePayload {
                            generation: media_generation,
                            width: yw as u32,
                            height: yh as u32,
                            data: b64,
                            keyframe: is_keyframe,
                            codec: codec.clone(),
                            capture_timestamp_micros,
                            pts_micros,
                            discontinuity,
                        },
                    );
                    lock_client_media_metrics(&metrics).observe_frontend_ipc_send_duration(
                        metrics_started.elapsed(),
                        ipc_send_started.elapsed(),
                    );
                    if send_result.is_err() {
                        release_frame_channel_slot(&frame_events_in_flight);
                        break 'frames;
                    }
                    emitted = true;
                    break;
                }
            }
            if !emitted {
                release_frame_channel_slot(&frame_events_in_flight);
            }
            delivered_to_frontend = emitted;
        } else {
            release_frame_channel_slot(&frame_events_in_flight);
            delivered_to_frontend = false;
        }

        if delivered_to_frontend {
            frontend_waiting_for_keyframe = false;
            let mut metrics = lock_client_media_metrics(&metrics);
            let elapsed = metrics_started.elapsed();
            metrics.observe_frontend_send(elapsed);
            metrics.finish_frontend_resync(elapsed);
        }

        tokio::task::yield_now().await;
    }

    let _ = stats_stop.send(true);
    let _ = stats_task.await;
    if media_transport == MediaTransport::UpstreamMoq {
        retire_upstream_moq_generation(&app, media_generation, connected_audio_generation).await;
    }
    drop(endpoint);
}

fn sequence_gap(previous: u64, current: u64) -> Result<u64, String> {
    let expected = previous
        .checked_add(1)
        .ok_or_else(|| format!("Media sequence overflowed after {previous}"))?;
    if current < expected {
        return Err(format!(
            "Non-monotonic media sequence: previous={previous}, current={current}"
        ));
    }
    Ok(current - expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_checks_reject_duplicates_regressions_and_overflow() {
        assert_eq!(sequence_gap(41, 42).unwrap(), 0);
        assert_eq!(sequence_gap(41, 45).unwrap(), 3);
        assert!(sequence_gap(41, 41).is_err());
        assert!(sequence_gap(41, 40).is_err());
        assert!(sequence_gap(u64::MAX, 0).is_err());
    }
}
