use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant};

use iroh::Endpoint;
use sigil_protocol::{AUDIO_ALPN_V1, AudioFlags, AudioPacket, AudioPacketHeader, Capability};
use tauri::{
    AppHandle, Emitter, State,
    ipc::{Channel, Response},
};

use super::transport::negotiate_v1;
use crate::commands::state::{AUDIO_DELIVERY_CAPACITY, AppState, AudioDeliveryState};

// Three 20 ms Opus packets cap the Rust→webview handoff at 60 ms. The
// AudioWorklet owns a separate fixed ring and never feeds back into transport.
const AUDIO_CHANNEL_MAGIC: [u8; 4] = *b"SGAC";
const AUDIO_CHANNEL_VERSION: u16 = 1;
const AUDIO_CHANNEL_HEADER_LEN: usize = 24;

pub(crate) fn lock_audio_deliveries(
    deliveries: &StdMutex<AudioDeliveryState>,
) -> StdMutexGuard<'_, AudioDeliveryState> {
    deliveries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn next_audio_generation(counter: &AtomicU64) -> Result<u64, String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| "Audio connection generation overflowed".to_string())
}

fn audio_generation_is_current(counter: &AtomicU64, generation: u64) -> bool {
    counter.load(Ordering::SeqCst) == generation
}

fn emit_audio_event_if_current(
    app: &AppHandle,
    event: &str,
    generation_counter: &AtomicU64,
    deliveries: &StdMutex<AudioDeliveryState>,
    generation: u64,
    payload: impl FnOnce(&AudioDeliveryState) -> serde_json::Value,
) -> bool {
    let deliveries = lock_audio_deliveries(deliveries);
    if generation_counter.load(Ordering::SeqCst) != generation
        || deliveries.generation() != Some(generation)
    {
        return false;
    }
    let _ = app.emit(event, payload(&deliveries));
    true
}

#[derive(Debug, Default)]
struct AudioReorderBuffer {
    expected_sequence: Option<u64>,
    packets: BTreeMap<u64, AudioPacket>,
}

#[derive(Debug)]
struct OrderedAudioPacket {
    packet: AudioPacket,
    discontinuity: bool,
}

impl AudioReorderBuffer {
    const CAPACITY: usize = 3;

    fn insert(&mut self, packet: AudioPacket) -> Result<(Vec<OrderedAudioPacket>, u64), String> {
        let sequence = packet.header.sequence;
        let expected = self.expected_sequence.get_or_insert(sequence);
        if sequence < *expected || self.packets.contains_key(&sequence) {
            return Ok((Vec::new(), 0));
        }
        self.packets.insert(sequence, packet);

        let mut dropped = 0_u64;
        let mut discontinuity = false;
        if !self.packets.contains_key(expected) && self.packets.len() >= Self::CAPACITY {
            let next = *self
                .packets
                .first_key_value()
                .expect("capacity check guarantees one packet")
                .0;
            dropped = next.saturating_sub(*expected);
            *expected = next;
            discontinuity = true;
        }

        let mut ordered = Vec::with_capacity(Self::CAPACITY);
        while let Some(packet) = self.packets.remove(expected) {
            ordered.push(OrderedAudioPacket {
                packet,
                discontinuity,
            });
            discontinuity = false;
            *expected = expected
                .checked_add(1)
                .ok_or_else(|| "Audio sequence overflowed".to_string())?;
        }
        Ok((ordered, dropped))
    }
}

fn encode_audio_channel_packet(
    generation: u64,
    delivery_id: u64,
    packet: AudioPacket,
    force_discontinuity: bool,
) -> Result<Vec<u8>, String> {
    let protocol_packet = if force_discontinuity {
        let header = AudioPacketHeader::opus(
            packet.payload.len(),
            packet.header.sequence,
            packet.header.capture_timestamp_us,
            packet.header.pts_us,
            AudioFlags::DISCONTINUITY,
        )
        .map_err(|error| error.to_string())?;
        AudioPacket::new(header, packet.payload).map_err(|error| error.to_string())?
    } else {
        packet
    };
    let datagram = protocol_packet
        .encode_datagram()
        .map_err(|error| error.to_string())?;
    let mut envelope = Vec::with_capacity(AUDIO_CHANNEL_HEADER_LEN + datagram.len());
    envelope.extend_from_slice(&AUDIO_CHANNEL_MAGIC);
    envelope.extend_from_slice(&AUDIO_CHANNEL_VERSION.to_be_bytes());
    envelope.extend_from_slice(&(AUDIO_CHANNEL_HEADER_LEN as u16).to_be_bytes());
    envelope.extend_from_slice(&generation.to_be_bytes());
    envelope.extend_from_slice(&delivery_id.to_be_bytes());
    envelope.extend_from_slice(&datagram);
    debug_assert_eq!(envelope.len(), AUDIO_CHANNEL_HEADER_LEN + datagram.len());
    Ok(envelope)
}

pub(crate) struct AudioStartRequest {
    pub(crate) address: iroh::EndpointAddr,
    pub(crate) handshake_nonce: [u8; 16],
    pub(crate) media_session_id: Option<u64>,
    pub(crate) audio_supported: bool,
    pub(crate) audio_channel: Channel<Response>,
    pub(crate) audio_deliveries: Arc<StdMutex<AudioDeliveryState>>,
    pub(crate) connection_generation: Arc<AtomicU64>,
    pub(crate) generation: u64,
}

pub(crate) async fn try_start_audio(
    app: AppHandle,
    endpoint: &Endpoint,
    request: AudioStartRequest,
) -> Result<iroh::endpoint::Connection, String> {
    if !request.audio_supported {
        return Err("WebCodecs Opus AudioDecoder is unavailable".to_string());
    }
    let media_session_id = request
        .media_session_id
        .ok_or_else(|| "The connected host protocol does not negotiate audio".to_string())?;
    let audio_connection = tokio::time::timeout(
        Duration::from_secs(3),
        endpoint.connect(request.address, AUDIO_ALPN_V1),
    )
    .await
    .map_err(|_| "Timed out connecting optional audio".to_string())?
    .map_err(|error| format!("Audio connection unavailable: {error}"))?;
    let (mut send, mut recv) = audio_connection
        .open_bi()
        .await
        .map_err(|error| format!("Failed to open audio handshake: {error}"))?;
    let negotiation = negotiate_v1(
        &mut send,
        &mut recv,
        request.handshake_nonce,
        vec![Capability::AudioOpus],
        Some(Capability::AudioOpus),
        "audio",
        None,
    )
    .await?;
    if negotiation.session_id != media_session_id {
        audio_connection.close(1_u32.into(), b"audio session mismatch");
        return Err("Host returned mismatched media and audio sessions".to_string());
    }
    send.finish()
        .map_err(|error| format!("Failed to finish audio handshake: {error}"))?;

    let task_audio_connection = audio_connection.clone();
    tokio::spawn(async move {
        let audio_channel = request.audio_channel;
        let audio_deliveries = request.audio_deliveries;
        let connection_generation = request.connection_generation;
        let generation = request.generation;
        let mut reorder = AudioReorderBuffer::default();
        let mut transport_received_total = 0_u64;
        let mut sequence_dropped_total = 0_u64;
        let mut frontend_dropped_total = 0_u64;
        let mut frontend_sent_total = 0_u64;
        let mut pending_discontinuity = false;
        let mut last_stats = Instant::now();

        loop {
            let datagram = match task_audio_connection.read_datagram().await {
                Ok(datagram) => datagram,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": format!("Audio connection ended: {error}")
                            })
                        },
                    );
                    break;
                }
            };
            if !audio_generation_is_current(&connection_generation, generation) {
                break;
            }
            let packet = match AudioPacket::decode_datagram(&datagram) {
                Ok(packet) => packet,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": format!("Invalid audio packet: {error}")
                            })
                        },
                    );
                    break;
                }
            };
            transport_received_total = transport_received_total.saturating_add(1);
            let (packets, dropped) = match reorder.insert(packet) {
                Ok(result) => result,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": error
                            })
                        },
                    );
                    break;
                }
            };
            sequence_dropped_total = sequence_dropped_total.saturating_add(dropped);
            if dropped > 0 {
                pending_discontinuity = true;
            }

            for ordered in packets {
                let delivery_id = match lock_audio_deliveries(&audio_deliveries).reserve(generation)
                {
                    Ok(Some(delivery_id)) => delivery_id,
                    Ok(None) => {
                        frontend_dropped_total = frontend_dropped_total.saturating_add(1);
                        pending_discontinuity = true;
                        continue;
                    }
                    Err(_) => return,
                };
                let envelope = match encode_audio_channel_packet(
                    generation,
                    delivery_id,
                    ordered.packet,
                    ordered.discontinuity || pending_discontinuity,
                ) {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        lock_audio_deliveries(&audio_deliveries)
                            .release_failed_delivery(generation, delivery_id);
                        emit_audio_event_if_current(
                            &app,
                            "audio-state",
                            &connection_generation,
                            &audio_deliveries,
                            generation,
                            |_| {
                                serde_json::json!({
                                    "generation": generation,
                                    "available": false,
                                    "error": error
                                })
                            },
                        );
                        return;
                    }
                };
                if !audio_generation_is_current(&connection_generation, generation) {
                    lock_audio_deliveries(&audio_deliveries)
                        .release_failed_delivery(generation, delivery_id);
                    return;
                }
                if audio_channel.send(Response::new(envelope)).is_err() {
                    lock_audio_deliveries(&audio_deliveries)
                        .release_failed_delivery(generation, delivery_id);
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": "Audio webview channel closed"
                            })
                        },
                    );
                    return;
                }
                frontend_sent_total = frontend_sent_total.saturating_add(1);
                pending_discontinuity = false;
            }

            if last_stats.elapsed() >= Duration::from_millis(250) {
                if !emit_audio_event_if_current(
                    &app,
                    "audio-stats",
                    &connection_generation,
                    &audio_deliveries,
                    generation,
                    |deliveries| {
                        serde_json::json!({
                            "generation": generation,
                            "transport_received_total": transport_received_total,
                            "sequence_dropped_total": sequence_dropped_total,
                            "frontend_dropped_total": frontend_dropped_total,
                            "frontend_sent_total": frontend_sent_total,
                            "frontend_queue_depth": deliveries.depth(generation).unwrap_or(0),
                            "frontend_queue_capacity": AUDIO_DELIVERY_CAPACITY,
                        })
                    },
                ) {
                    return;
                }
                last_stats = Instant::now();
            }
        }
    });
    Ok(audio_connection)
}

pub(crate) fn iroh_client_ack_audio(
    state: State<'_, AppState>,
    generation: u64,
    delivery_id: u64,
) -> Result<bool, String> {
    lock_audio_deliveries(&state.audio_deliveries).acknowledge(generation, delivery_id)?;
    Ok(true)
}

pub(crate) fn cancel_audio_generation(
    generation_counter: &AtomicU64,
    deliveries: &StdMutex<AudioDeliveryState>,
    expected_generation: u64,
) -> Result<bool, String> {
    let replacement_generation = expected_generation
        .checked_add(1)
        .ok_or_else(|| "Audio connection generation overflowed".to_string())?;
    let mut deliveries = lock_audio_deliveries(deliveries);
    if deliveries.generation() != Some(expected_generation)
        || generation_counter.load(Ordering::SeqCst) != expected_generation
    {
        return Ok(false);
    }
    if generation_counter
        .compare_exchange(
            expected_generation,
            replacement_generation,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return Ok(false);
    }
    if !deliveries.cancel_generation(expected_generation) {
        return Err("Audio delivery generation changed during cancellation".to_string());
    }
    Ok(true)
}

pub(crate) async fn iroh_client_stop_audio(
    state: State<'_, AppState>,
    expected_generation: u64,
) -> Result<bool, String> {
    if !cancel_audio_generation(
        &state.audio_connection_generation,
        &state.audio_deliveries,
        expected_generation,
    )? {
        return Ok(false);
    }

    let connection = {
        let mut audio_connection = state.audio_connection.lock().await;
        if audio_connection
            .as_ref()
            .is_some_and(|(generation, _)| *generation == expected_generation)
        {
            audio_connection.take().map(|(_, connection)| connection)
        } else {
            None
        }
    };
    if let Some(connection) = connection {
        connection.close(0_u32.into(), b"audio stopped by client");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_packet(sequence: u64) -> AudioPacket {
        let payload = vec![0xf8, 0xff, 0xfe];
        AudioPacket::new(
            AudioPacketHeader::opus(
                payload.len(),
                sequence,
                sequence * 20_000,
                sequence as i64 * 20_000,
                AudioFlags::NONE,
            )
            .unwrap(),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn audio_delivery_tokens_are_bounded_unique_and_acknowledged_exactly_once() {
        let mut deliveries = AudioDeliveryState::default();
        deliveries.begin_generation(7).unwrap();

        assert_eq!(deliveries.reserve(7).unwrap(), Some(1));
        assert_eq!(deliveries.reserve(7).unwrap(), Some(2));
        assert_eq!(deliveries.reserve(7).unwrap(), Some(3));
        assert_eq!(deliveries.depth(7), Some(AUDIO_DELIVERY_CAPACITY));
        assert_eq!(deliveries.reserve(7).unwrap(), None);

        deliveries.acknowledge(7, 2).unwrap();
        assert!(deliveries.acknowledge(7, 2).is_err());
        assert_eq!(deliveries.reserve(7).unwrap(), Some(4));
        assert!(deliveries.acknowledge(6, 1).is_err());
        assert!(deliveries.acknowledge(7, 99).is_err());
        assert_eq!(deliveries.depth(7), Some(AUDIO_DELIVERY_CAPACITY));
    }

    #[test]
    fn audio_generation_cancellation_rejects_stale_and_clears_current_tokens() {
        let counter = AtomicU64::new(11);
        let deliveries = StdMutex::new(AudioDeliveryState::default());
        lock_audio_deliveries(&deliveries)
            .begin_generation(11)
            .unwrap();
        assert_eq!(
            lock_audio_deliveries(&deliveries).reserve(11).unwrap(),
            Some(1)
        );

        assert!(!cancel_audio_generation(&counter, &deliveries, 10).unwrap());
        assert_eq!(counter.load(Ordering::SeqCst), 11);
        assert_eq!(lock_audio_deliveries(&deliveries).depth(11), Some(1));

        assert!(cancel_audio_generation(&counter, &deliveries, 11).unwrap());
        assert_eq!(counter.load(Ordering::SeqCst), 12);
        assert_eq!(lock_audio_deliveries(&deliveries).generation(), None);
        assert!(!cancel_audio_generation(&counter, &deliveries, 11).unwrap());
    }

    #[test]
    fn audio_reorder_window_is_bounded_and_marks_skipped_packets() {
        let mut reorder = AudioReorderBuffer::default();
        let (first, dropped) = reorder.insert(audio_packet(10)).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(dropped, 0);

        assert!(reorder.insert(audio_packet(12)).unwrap().0.is_empty());
        assert!(reorder.insert(audio_packet(13)).unwrap().0.is_empty());
        let (ordered, dropped) = reorder.insert(audio_packet(14)).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(
            ordered
                .iter()
                .map(|packet| packet.packet.header.sequence)
                .collect::<Vec<_>>(),
            vec![12, 13, 14]
        );
        assert!(ordered[0].discontinuity);
        assert!(!ordered[1].discontinuity);
        assert!(reorder.packets.len() <= AudioReorderBuffer::CAPACITY);

        assert!(reorder.insert(audio_packet(16)).unwrap().0.is_empty());
        let (ordered, dropped) = reorder.insert(audio_packet(15)).unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(
            ordered
                .iter()
                .map(|packet| packet.packet.header.sequence)
                .collect::<Vec<_>>(),
            vec![15, 16]
        );
        assert!(reorder.insert(audio_packet(16)).unwrap().0.is_empty());
    }

    #[test]
    fn audio_channel_envelope_is_protocol_strict_and_can_force_discontinuity() {
        let encoded = encode_audio_channel_packet(9, 42, audio_packet(7), true).unwrap();
        assert_eq!(&encoded[0..4], b"SGAC");
        assert_eq!(&encoded[4..6], &1_u16.to_be_bytes());
        assert_eq!(&encoded[6..8], &24_u16.to_be_bytes());
        assert_eq!(&encoded[8..16], &9_u64.to_be_bytes());
        assert_eq!(&encoded[16..24], &42_u64.to_be_bytes());
        let decoded = AudioPacket::decode_datagram(&encoded[AUDIO_CHANNEL_HEADER_LEN..]).unwrap();
        assert_eq!(decoded.header.sequence, 7);
        assert!(decoded.header.flags.contains(AudioFlags::DISCONTINUITY));
        assert_eq!(decoded.payload, vec![0xf8, 0xff, 0xfe]);
    }
}
