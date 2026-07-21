use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, endpoint::presets};
use sigil_protocol::{
    Capability, ClientHello, FrameFlags, GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN, GAMEPAD_TRIGGER_MAX,
    GamepadState, INPUT_ALPN_V1, InputEvent, MEDIA_ALPN_V1, MEDIA_ALPN_V2, MediaFrame,
    PointerSurfaceDimensions, ProtocolError, read_host_hello, read_input_ack, read_media_frame,
    read_media_object, write_client_hello, write_input_event,
};

const MEDIA_OBJECT_CAPACITY: usize = 4;

#[derive(Debug, Parser)]
#[command(name = "sigil-probe", version, about = "Bounded Sigil transport probe")]
struct Args {
    #[arg(long)]
    node_id: EndpointId,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=36_000))]
    frames: u32,
    #[arg(long, default_value_t = 15)]
    timeout_seconds: u64,
    #[arg(long, default_value = "1280x800", value_parser = parse_size)]
    expect_size: (u16, u16),
    /// Exercise the reliable ordered v1 media stream instead of independent
    /// v2 media objects. Intended only for compatibility validation.
    #[arg(long)]
    media_v1: bool,
    /// Require gamepad negotiation and emit one bounded non-neutral snapshot
    /// followed by neutral. Intended for evtest-backed uinput proof.
    #[arg(long)]
    gamepad_smoke: bool,
    /// Require relative-pointer negotiation and emit bounded motion plus one
    /// complete left-click. Intended for libinput/Gamescope-backed proof.
    #[arg(long)]
    pointer_smoke: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaTransport {
    IndependentV2,
    ReliableV1,
}

impl MediaTransport {
    fn alpn(self) -> &'static [u8] {
        match self {
            Self::IndependentV2 => MEDIA_ALPN_V2,
            Self::ReliableV1 => MEDIA_ALPN_V1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::IndependentV2 => "independent-v2",
            Self::ReliableV1 => "reliable-v1",
        }
    }
}

#[derive(Debug)]
enum MediaObjectOutcome {
    Frame { index: u64, frame: MediaFrame },
    Dropped { index: u64 },
    Malformed { index: u64, error: ProtocolError },
}

struct MediaObjectReceiver {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectOutcome>,
    next_index: u64,
    accepting: bool,
    read_timeout: Duration,
}

impl MediaObjectReceiver {
    fn new(connection: iroh::endpoint::Connection, read_timeout: Duration) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            next_index: 0,
            accepting: true,
            read_timeout,
        }
    }

    async fn next(&mut self) -> Result<Option<MediaObjectOutcome>> {
        loop {
            if !self.accepting && self.reads.is_empty() {
                return Ok(None);
            }

            tokio::select! {
                accepted = self.connection.accept_uni(),
                    if self.accepting && self.reads.len() < MEDIA_OBJECT_CAPACITY => {
                    match accepted {
                        Ok(mut stream) => {
                            let index = self.next_index;
                            self.next_index = self
                                .next_index
                                .checked_add(1)
                                .context("media object index overflowed")?;
                            let read_timeout = self.read_timeout;
                            self.reads.spawn(async move {
                                match tokio::time::timeout(read_timeout, read_media_object(&mut stream)).await {
                                    Ok(Ok(frame)) => MediaObjectOutcome::Frame { index, frame },
                                    Ok(Err(ProtocolError::Io(_))) => MediaObjectOutcome::Dropped { index },
                                    Ok(Err(error)) => MediaObjectOutcome::Malformed { index, error },
                                    Err(_) => MediaObjectOutcome::Dropped { index },
                                }
                            });
                        }
                        Err(_) => self.accepting = false,
                    }
                }
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    return match completed.expect("guarded by non-empty media object task set") {
                        Ok(outcome) => Ok(Some(outcome)),
                        Err(error) if error.is_cancelled() => continue,
                        Err(error) => Err(error).context("media object read task failed"),
                    };
                }
            }
        }
    }
}

impl Drop for MediaObjectReceiver {
    fn drop(&mut self) {
        self.reads.abort_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaObjectDecision {
    Deliver,
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
struct MediaObjectSequence {
    completion_watermark: Option<u64>,
    last_sequence: Option<u64>,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequence {
    fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    fn note_drop(&mut self, index: u64) -> bool {
        if self
            .completion_watermark
            .is_some_and(|watermark| index <= watermark)
        {
            return false;
        }
        self.completion_watermark = Some(index);
        self.waiting_for_keyframe = true;
        true
    }

    fn classify(&mut self, index: u64, frame: &MediaFrame) -> MediaObjectDecision {
        if self
            .completion_watermark
            .is_some_and(|watermark| index <= watermark)
        {
            return MediaObjectDecision::DropLate;
        }
        self.completion_watermark = Some(index);

        let independently_decodable = frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG);
        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|last| last.checked_add(1) == Some(frame.header.sequence));
        let sequence_monotonic = self
            .last_sequence
            .is_none_or(|last| frame.header.sequence > last);

        let resync_required =
            !sequence_monotonic || self.waiting_for_keyframe || !sequence_contiguous;
        if resync_required && (!independently_decodable || !sequence_monotonic) {
            self.waiting_for_keyframe = true;
            return MediaObjectDecision::DropUntilKeyframe;
        }

        self.last_sequence = Some(frame.header.sequence);
        self.waiting_for_keyframe = false;
        MediaObjectDecision::Deliver
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.timeout_seconds > 0,
        "--timeout-seconds must be greater than zero"
    );

    let secret = SecretKey::generate();
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).context("generating handshake nonce")?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .context("binding probe endpoint")?;
    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;
    let address = EndpointAddr::new(args.node_id);
    let media_transport = if args.media_v1 {
        MediaTransport::ReliableV1
    } else {
        MediaTransport::IndependentV2
    };

    let media_connection = endpoint
        .connect(address.clone(), media_transport.alpn())
        .await
        .context("connecting media protocol")?;
    let (mut media_send, mut media_recv) = media_connection
        .open_bi()
        .await
        .context("opening media stream")?;
    let media_negotiation = negotiate(
        &mut media_send,
        &mut media_recv,
        nonce,
        vec![Capability::VideoH264],
        Capability::VideoH264,
        "media",
    )
    .await?;
    let session_id = media_negotiation.session_id;
    let mut media_send = Some(media_send);
    let mut media_recv = Some(media_recv);
    if media_transport == MediaTransport::IndependentV2 {
        media_send
            .take()
            .expect("media handshake stream is present")
            .finish()
            .context("finishing media v2 handshake request")?;
        drop(media_recv.take());
    }

    let input_connection = endpoint
        .connect(address, INPUT_ALPN_V1)
        .await
        .context("connecting input protocol")?;
    let (mut input_send, mut input_recv) = input_connection
        .open_bi()
        .await
        .context("opening input stream")?;
    let mut input_offers = vec![Capability::InputAck];
    if args.pointer_smoke {
        input_offers.push(Capability::RelativePointer);
    }
    if args.gamepad_smoke {
        input_offers.push(Capability::Gamepad);
    }
    let input_negotiation = negotiate(
        &mut input_send,
        &mut input_recv,
        nonce,
        input_offers,
        Capability::InputAck,
        "input",
    )
    .await?;
    ensure!(
        session_id == input_negotiation.session_id,
        "media and input session IDs differ"
    );
    let input_started = Instant::now();
    write_input_event(&mut input_send, &InputEvent::Probe)
        .await
        .context("writing input probe")?;
    read_expected_input_ack(&mut input_recv, args.timeout_seconds, 1).await?;
    let input_ack_micros = input_started.elapsed().as_micros();
    let mut expected_ack = 1_u64;

    if args.pointer_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::RelativePointer),
            "host did not accept the required relative pointer capability"
        );
        let [position_sync, relative_motion, click] =
            pointer_smoke_events(media_negotiation.pointer_surface_dimensions)?;
        write_input_event(&mut input_send, &position_sync)
            .await
            .context("writing pointer position synchronization")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        write_input_event(&mut input_send, &relative_motion)
            .await
            .context("writing relative pointer smoke motion")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        write_input_event(&mut input_send, &click)
            .await
            .context("writing pointer smoke click")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
    }

    if args.gamepad_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::Gamepad),
            "host did not accept the required gamepad capability"
        );
        write_input_event(
            &mut input_send,
            &InputEvent::Gamepad {
                state: GamepadState {
                    a: true,
                    right_shoulder: true,
                    dpad_right: true,
                    left_x: GAMEPAD_AXIS_MAX,
                    right_y: GAMEPAD_AXIS_MIN,
                    left_trigger: GAMEPAD_TRIGGER_MAX,
                    right_trigger: GAMEPAD_TRIGGER_MAX,
                    ..GamepadState::default()
                },
            },
        )
        .await
        .context("writing non-neutral gamepad smoke snapshot")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        write_input_event(
            &mut input_send,
            &InputEvent::Gamepad {
                state: GamepadState::default(),
            },
        )
        .await
        .context("writing neutral gamepad smoke snapshot")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
    }

    let started = Instant::now();
    let mut received = 0_u32;
    let mut bytes = 0_u64;
    let mut keyframes = 0_u32;
    let mut gaps = 0_u64;
    let mut media_objects_dropped = 0_u64;
    let mut media_objects_late = 0_u64;
    let mut last_sequence: Option<u64> = None;
    let mut dimensions = None;
    let mut object_receiver = (media_transport == MediaTransport::IndependentV2).then(|| {
        MediaObjectReceiver::new(
            media_connection.clone(),
            Duration::from_secs(args.timeout_seconds),
        )
    });
    let mut object_sequence = MediaObjectSequence::new();

    while received < args.frames {
        let frame = match media_transport {
            MediaTransport::ReliableV1 => tokio::time::timeout(
                Duration::from_secs(args.timeout_seconds),
                read_media_frame(
                    media_recv
                        .as_mut()
                        .expect("v1 media receive stream is present"),
                ),
            )
            .await
            .context("timed out waiting for media frame")??
            .context("host closed the media stream")?,
            MediaTransport::IndependentV2 => loop {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(args.timeout_seconds)
                        .saturating_add(Duration::from_secs(1)),
                    object_receiver
                        .as_mut()
                        .expect("v2 media object receiver is present")
                        .next(),
                )
                .await
                .context("timed out waiting for media object")??
                .context("host closed the media object connection")?;
                match outcome {
                    MediaObjectOutcome::Dropped { index } => {
                        if object_sequence.note_drop(index) {
                            media_objects_dropped = media_objects_dropped.saturating_add(1);
                        } else {
                            media_objects_late = media_objects_late.saturating_add(1);
                        }
                    }
                    MediaObjectOutcome::Malformed { index, error } => {
                        bail!("media object {index} is malformed: {error}");
                    }
                    MediaObjectOutcome::Frame { index, frame } => {
                        match object_sequence.classify(index, &frame) {
                            MediaObjectDecision::Deliver => break frame,
                            MediaObjectDecision::DropLate => {
                                media_objects_late = media_objects_late.saturating_add(1);
                            }
                            MediaObjectDecision::DropUntilKeyframe => {
                                media_objects_dropped = media_objects_dropped.saturating_add(1);
                            }
                        }
                    }
                }
            },
        };

        if received == 0 {
            ensure!(
                frame.header.flags.contains(FrameFlags::KEYFRAME)
                    && frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                "first media frame is not a decodable keyframe with codec configuration"
            );
        }

        let frame_dimensions = (frame.header.width, frame.header.height);
        if let Some(expected) = dimensions {
            ensure!(
                frame_dimensions == expected,
                "media dimensions changed during probe"
            );
        } else {
            dimensions = Some(frame_dimensions);
        }
        if let Some(previous) = last_sequence {
            gaps += sequence_gap(previous, frame.header.sequence)?;
        }
        last_sequence = Some(frame.header.sequence);
        keyframes += u32::from(frame.header.flags.contains(FrameFlags::KEYFRAME));
        bytes = bytes.saturating_add(frame.payload.len() as u64);
        received += 1;
    }

    let Some((width, height)) = dimensions else {
        bail!("probe received no frames");
    };
    ensure!(
        (width, height) == args.expect_size,
        "expected {}x{} but received {width}x{height}",
        args.expect_size.0,
        args.expect_size.1
    );
    ensure!(keyframes > 0, "probe received no H.264 keyframe");
    ensure!(gaps == 0, "probe observed {gaps} media sequence gaps");
    let (path_mode, path_rtt_ms) = selected_path_diagnostics(&media_connection);
    input_send.finish().context("finishing input stream")?;
    if let Some(mut media_send) = media_send {
        media_send
            .finish()
            .context("finishing media request stream")?;
    }
    // Close both protocol connections explicitly. Relying only on endpoint
    // teardown can leave a peer waiting for QUIC idle timeout during repeated
    // short-lived probes, which obscures whether the host released its
    // one-client lease deterministically.
    input_connection.close(0_u32.into(), b"probe complete");
    media_connection.close(0_u32.into(), b"probe complete");
    tokio::time::sleep(Duration::from_millis(50)).await;
    endpoint.close().await;

    println!("probe=ok");
    println!("session_id={session_id}");
    println!("frames={received}");
    println!("dimensions={width}x{height}");
    println!("keyframes={keyframes}");
    println!("sequence_gaps={gaps}");
    println!("media_objects_dropped={media_objects_dropped}");
    println!("media_objects_late={media_objects_late}");
    println!("transport={}", media_transport.label());
    println!("input_ack_micros={input_ack_micros}");
    println!(
        "pointer_smoke={}",
        if args.pointer_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    println!(
        "gamepad_smoke={}",
        if args.gamepad_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    println!("path_mode={path_mode}");
    match path_rtt_ms {
        Some(rtt) => println!("path_rtt_ms={rtt:.3}"),
        None => println!("path_rtt_ms=unknown"),
    }
    println!("encoded_bytes={bytes}");
    println!("elapsed_ms={}", started.elapsed().as_millis());
    Ok(())
}

fn pointer_smoke_events(
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
) -> Result<[InputEvent; 3]> {
    let dimensions = pointer_surface_dimensions
        .context("host did not advertise pointer surface dimensions required by --pointer-smoke")?;
    Ok([
        InputEvent::MousePositionSync {
            x: i32::from(dimensions.width / 2),
            y: i32::from(dimensions.height / 2),
        },
        InputEvent::MouseMoveRelative { dx: 32, dy: 16 },
        InputEvent::MouseClick { b: 1 },
    ])
}

fn sequence_gap(previous: u64, current: u64) -> Result<u64> {
    let Some(expected) = previous.checked_add(1) else {
        bail!("media sequence overflowed after {previous}");
    };
    ensure!(
        current >= expected,
        "non-monotonic media sequence: previous={previous}, current={current}"
    );
    Ok(current - expected)
}

fn selected_path_diagnostics(
    connection: &iroh::endpoint::Connection,
) -> (&'static str, Option<f64>) {
    let paths = connection.paths();
    let Some(path) = paths.iter().find(|path| path.is_selected()) else {
        return ("unknown", None);
    };
    let mode = if path.is_ip() {
        "direct"
    } else if path.is_relay() {
        "relay"
    } else {
        "custom"
    };
    (mode, Some(path.rtt().as_secs_f64() * 1000.0))
}

fn parse_size(value: &str) -> std::result::Result<(u16, u16), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "size must be WIDTHxHEIGHT".to_owned())?;
    let width = width.parse().map_err(|_| "invalid width".to_owned())?;
    let height = height.parse().map_err(|_| "invalid height".to_owned())?;
    if width == 0 || height == 0 {
        return Err("dimensions must be non-zero".to_owned());
    }
    Ok((width, height))
}

async fn negotiate(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    required: Capability,
    name: &str,
) -> Result<Negotiated> {
    write_client_hello(
        send,
        &ClientHello::new("sigil-probe/0.1.0", nonce, capabilities),
    )
    .await
    .with_context(|| format!("writing {name} hello"))?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_host_hello(recv))
        .await
        .with_context(|| format!("timed out waiting for {name} hello"))??
        .with_context(|| format!("host closed during {name} hello"))?;
    if !response.accepted {
        bail!(
            "host rejected {name} stream: {}",
            response.message.as_deref().unwrap_or("unspecified reason")
        );
    }
    ensure!(
        response.capabilities.contains(&required),
        "host accepted {name} without required capability {required:?}"
    );
    let session_id = response
        .session_id
        .with_context(|| format!("host omitted {name} session ID"))?;
    Ok(Negotiated {
        session_id,
        capabilities: response.capabilities,
        pointer_surface_dimensions: response.pointer_surface_dimensions,
    })
}

struct Negotiated {
    session_id: u64,
    capabilities: Vec<Capability>,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

async fn read_expected_input_ack(
    recv: &mut iroh::endpoint::RecvStream,
    timeout_seconds: u64,
    expected_sequence: u64,
) -> Result<()> {
    let input_ack = tokio::time::timeout(
        Duration::from_secs(timeout_seconds.min(5)),
        read_input_ack(recv),
    )
    .await
    .context("timed out waiting for input acknowledgment")??
    .context("host closed before input acknowledgment")?;
    ensure!(
        input_ack.sequence == expected_sequence,
        "unexpected input acknowledgment sequence {}; expected {expected_sequence}",
        input_ack.sequence
    );
    Ok(())
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

    #[test]
    fn pointer_smoke_uses_negotiated_native_surface_center_and_order() {
        let dimensions = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            pointer_smoke_events(Some(dimensions)).unwrap(),
            [
                InputEvent::MousePositionSync { x: 1_280, y: 800 },
                InputEvent::MouseMoveRelative { dx: 32, dy: 16 },
                InputEvent::MouseClick { b: 1 },
            ]
        );
    }

    #[test]
    fn pointer_smoke_requires_negotiated_native_surface_dimensions() {
        let error = pointer_smoke_events(None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("host did not advertise pointer surface dimensions")
        );
    }

    fn media_frame(sequence: u64, flags: FrameFlags) -> MediaFrame {
        let payload = vec![0x65, 0x88, 0x84];
        MediaFrame::new(
            sigil_protocol::MediaFrameHeader::h264(
                1_280,
                800,
                payload.len(),
                sequence,
                sequence * 1_000,
                i64::try_from(sequence * 1_000).unwrap(),
                flags,
            )
            .unwrap(),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn media_objects_wait_for_a_decodable_keyframe_then_deliver_contiguous_frames() {
        let mut sequence = MediaObjectSequence::new();
        let delta = media_frame(1, FrameFlags::NONE);
        let keyframe = media_frame(2, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let next_delta = media_frame(3, FrameFlags::NONE);

        assert_eq!(
            sequence.classify(0, &delta),
            MediaObjectDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(1, &keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(2, &next_delta),
            MediaObjectDecision::Deliver
        );
    }

    #[test]
    fn dropped_objects_force_resync_and_late_completions_cannot_rewind() {
        let mut sequence = MediaObjectSequence::new();
        let first_keyframe = media_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let stale_delta = media_frame(11, FrameFlags::NONE);
        let replacement_keyframe =
            media_frame(20, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));

        assert_eq!(
            sequence.classify(0, &first_keyframe),
            MediaObjectDecision::Deliver
        );
        assert!(sequence.note_drop(1));
        assert_eq!(
            sequence.classify(2, &replacement_keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(1, &stale_delta),
            MediaObjectDecision::DropLate
        );
        assert!(!sequence.note_drop(1));

        let next_delta = media_frame(21, FrameFlags::NONE);
        assert_eq!(
            sequence.classify(3, &next_delta),
            MediaObjectDecision::Deliver
        );
    }

    #[test]
    fn media_sequence_gap_drops_deltas_until_replacement_keyframe() {
        let mut sequence = MediaObjectSequence::new();
        let keyframe = media_frame(40, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let gap_delta = media_frame(42, FrameFlags::NONE);
        let replacement_keyframe =
            media_frame(50, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));

        assert_eq!(
            sequence.classify(0, &keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(1, &gap_delta),
            MediaObjectDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(2, &replacement_keyframe),
            MediaObjectDecision::Deliver
        );
    }
}
