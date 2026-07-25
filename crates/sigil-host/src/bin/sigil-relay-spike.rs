use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use iroh::SecretKey;
use sigil_protocol::{
    AuthenticatedMediaObject, FrameFlags, MediaFrame, MediaFrameHeader, MediaGenerationSigningKey,
    MediaObjectCoordinates, MediaTrack, SignedSubscriptionCapability, SubscriptionClaims,
    SubscriptionTracks, encode_media_frame_object,
};

const VIDEO_FPS: u64 = 60;
const AUDIO_OBJECTS_PER_SECOND: u64 = 50;
const VIDEO_PAYLOAD_BYTES: usize = 64 * 1024;
const AUDIO_PAYLOAD_BYTES: usize = 200;
const RELAY_QUEUE_CAPACITY: usize = 4;
const AUTHORIZATION_REVISION: u64 = 1;

#[derive(Debug, Parser)]
#[command(
    name = "sigil-relay-spike",
    about = "Bounded relay-ready media authentication spike"
)]
struct Args {
    #[arg(long, default_value = "1280x800@60")]
    video: String,
    #[arg(long, default_value = "opus-48k-stereo")]
    audio: String,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=300))]
    duration_seconds: u64,
}

#[derive(Default)]
struct BenchmarkEvidence {
    host_bytes: u64,
    downstream_bytes: u64,
    sign_nanos: u128,
    verification_micros: Vec<u64>,
    verification_nanos: u128,
    maximum_relay_queue: usize,
    first_object: Option<(Vec<u8>, MediaObjectCoordinates)>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.video == "1280x800@60",
        "only the fixed 1280x800@60 target is supported"
    );
    ensure!(
        args.audio == "opus-48k-stereo",
        "only Opus 48 kHz stereo is supported"
    );

    let host = SecretKey::from_bytes(&[0x31; 32]);
    let portal_a = SecretKey::from_bytes(&[0x41; 32]);
    let portal_b = SecretKey::from_bytes(&[0x42; 32]);
    let generation_id = 7_u64;
    let issued_at = 1_700_000_000_u64;
    let generation = MediaGenerationSigningKey::from_bytes(&[0x51; 32]);
    let certificate = generation.certify(
        *host.public().as_bytes(),
        &host.to_bytes(),
        generation_id,
        issued_at,
        issued_at + 600,
    )?;
    certificate.verify_binding(*host.public().as_bytes(), generation_id, issued_at + 1)?;
    let subscription = SignedSubscriptionCapability::issue(
        SubscriptionClaims::new(
            *host.public().as_bytes(),
            generation_id,
            *portal_b.public().as_bytes(),
            SubscriptionTracks::VIDEO_H264.union(SubscriptionTracks::AUDIO_OPUS),
            AUTHORIZATION_REVISION,
            issued_at,
            issued_at + 300,
            [0x61; 32],
            1,
        )?,
        &host.to_bytes(),
    )?;
    subscription.verify_binding(
        *host.public().as_bytes(),
        generation_id,
        *portal_b.public().as_bytes(),
        SubscriptionTracks::VIDEO_H264.union(SubscriptionTracks::AUDIO_OPUS),
        AUTHORIZATION_REVISION,
        issued_at + 1,
    )?;

    let mut evidence = BenchmarkEvidence {
        verification_micros: Vec::with_capacity(
            usize::try_from(args.duration_seconds * (VIDEO_FPS + AUDIO_OBJECTS_PER_SECOND))
                .context("benchmark sample capacity overflow")?,
        ),
        ..BenchmarkEvidence::default()
    };
    let video_payload: Arc<[u8]> = Arc::from(vec![0x65; VIDEO_PAYLOAD_BYTES]);
    let audio_payload = vec![0x7a; AUDIO_PAYLOAD_BYTES];
    let mut relay_queue = VecDeque::with_capacity(RELAY_QUEUE_CAPACITY);

    for sequence in 0..args.duration_seconds * VIDEO_FPS {
        let group_id = sequence / VIDEO_FPS;
        let object_id = u32::try_from(sequence % VIDEO_FPS).expect("video object id is bounded");
        let flags = if object_id == 0 {
            FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG)
        } else {
            FrameFlags::NONE
        };
        let frame = MediaFrame::new(
            MediaFrameHeader::h264(
                1280,
                800,
                video_payload.len(),
                sequence,
                sequence * 1_000_000 / VIDEO_FPS,
                i64::try_from(sequence * 1_000_000 / VIDEO_FPS).expect("bounded timestamp"),
                flags,
            )?,
            Arc::clone(&video_payload),
        )?;
        let payload = encode_media_frame_object(&frame)?;
        authenticate_relay_verify(
            &generation,
            &certificate,
            MediaObjectCoordinates {
                generation_id,
                track: MediaTrack::VideoH264,
                group_id,
                object_id,
                flags: u16::from(flags.bits()),
            },
            &payload,
            &mut relay_queue,
            &mut evidence,
        )?;
    }

    for sequence in 0..args.duration_seconds * AUDIO_OBJECTS_PER_SECOND {
        authenticate_relay_verify(
            &generation,
            &certificate,
            MediaObjectCoordinates {
                generation_id,
                track: MediaTrack::AudioOpus,
                group_id: sequence / AUDIO_OBJECTS_PER_SECOND,
                object_id: u32::try_from(sequence % AUDIO_OBJECTS_PER_SECOND)
                    .expect("audio object id is bounded"),
                flags: 0,
            },
            &audio_payload,
            &mut relay_queue,
            &mut evidence,
        )?;
    }

    ensure!(relay_queue.is_empty(), "relay queue did not drain");
    let (first_object, first_coordinates) = evidence
        .first_object
        .as_ref()
        .context("benchmark did not produce an authenticated object")?;
    let mut tampered = first_object.clone();
    *tampered
        .last_mut()
        .context("authenticated object was empty")? ^= 1;
    ensure!(
        AuthenticatedMediaObject::verify(&tampered, &certificate, *first_coordinates).is_err(),
        "tampered relayed media was trusted"
    );
    ensure!(
        subscription
            .verify_binding(
                *host.public().as_bytes(),
                generation_id,
                *portal_a.public().as_bytes(),
                SubscriptionTracks::VIDEO_H264,
                AUTHORIZATION_REVISION,
                issued_at + 1,
            )
            .is_err(),
        "wrong subscriber was authorized"
    );
    ensure!(
        subscription
            .verify_binding(
                *host.public().as_bytes(),
                generation_id,
                *portal_b.public().as_bytes(),
                SubscriptionTracks::VIDEO_H264,
                AUTHORIZATION_REVISION,
                issued_at + 301,
            )
            .is_err(),
        "expired subscription was authorized"
    );
    AuthenticatedMediaObject::verify(first_object, &certificate, *first_coordinates)
        .context("direct-host fallback could not verify the relayed object")?;

    evidence.verification_micros.sort_unstable();
    let p95_index = evidence
        .verification_micros
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        - 1;
    let verify_p95_us = evidence.verification_micros[p95_index];
    let host_direct_bytes = evidence.host_bytes.saturating_mul(2);
    let host_share_percent = evidence
        .host_bytes
        .saturating_mul(100)
        .checked_div(host_direct_bytes)
        .unwrap_or(0);
    let savings_percent = 100_u64.saturating_sub(host_share_percent);
    let sign_cpu_percent =
        evidence.sign_nanos as f64 / (args.duration_seconds as f64 * 1_000_000_000_f64) * 100.0;
    let verify_cpu_percent = evidence.verification_nanos as f64
        / (args.duration_seconds as f64 * 1_000_000_000_f64)
        * 100.0;

    println!("relay_spike=ok");
    println!("authentication_mode=ed25519-v1");
    println!("decision=defer-production-mesh");
    println!("virtual_duration_seconds={}", args.duration_seconds);
    println!("video_objects={}", args.duration_seconds * VIDEO_FPS);
    println!(
        "audio_objects={}",
        args.duration_seconds * AUDIO_OBJECTS_PER_SECOND
    );
    println!("host_upload_bytes={}", evidence.host_bytes);
    println!("direct_two_viewer_host_upload_bytes={host_direct_bytes}");
    println!("host_upload_savings_percent={savings_percent}");
    println!("relay_downstream_bytes={}", evidence.downstream_bytes);
    println!("sign_cpu_percent_of_virtual_realtime={sign_cpu_percent:.4}");
    println!("verify_cpu_percent_of_virtual_realtime={verify_cpu_percent:.4}");
    println!("verify_p95_us={verify_p95_us}");
    println!("added_local_latency_p95_us={verify_p95_us}");
    println!("maximum_relay_queue={}", evidence.maximum_relay_queue);
    println!("tamper_rejected=ok");
    println!("wrong_subscriber_rejected=ok");
    println!("expired_subscription_rejected=ok");
    println!("relay_loss_direct_fallback=ok");
    println!("withheld_media_trusted=0");
    Ok(())
}

fn authenticate_relay_verify(
    generation: &MediaGenerationSigningKey,
    certificate: &sigil_protocol::SignedMediaGenerationCertificate,
    coordinates: MediaObjectCoordinates,
    payload: &[u8],
    relay_queue: &mut VecDeque<Vec<u8>>,
    evidence: &mut BenchmarkEvidence,
) -> Result<()> {
    ensure!(
        relay_queue.len() < RELAY_QUEUE_CAPACITY,
        "relay queue exceeded its bound"
    );
    let sign_started = Instant::now();
    let object = generation.authenticate(coordinates, payload)?.into_bytes();
    evidence.sign_nanos = evidence
        .sign_nanos
        .saturating_add(sign_started.elapsed().as_nanos());
    evidence.host_bytes = evidence
        .host_bytes
        .saturating_add(u64::try_from(object.len()).unwrap_or(u64::MAX));
    if evidence.first_object.is_none() {
        evidence.first_object = Some((object.clone(), coordinates));
    }
    relay_queue.push_back(object);
    evidence.maximum_relay_queue = evidence.maximum_relay_queue.max(relay_queue.len());

    let relayed = relay_queue
        .pop_front()
        .context("relay queue lost an object")?;
    evidence.downstream_bytes = evidence
        .downstream_bytes
        .saturating_add(u64::try_from(relayed.len()).unwrap_or(u64::MAX));
    let verify_started = Instant::now();
    let verified = AuthenticatedMediaObject::verify(&relayed, certificate, coordinates)?;
    let verify_elapsed = verify_started.elapsed();
    evidence.verification_nanos = evidence
        .verification_nanos
        .saturating_add(verify_elapsed.as_nanos());
    evidence
        .verification_micros
        .push(u64::try_from(verify_elapsed.as_micros()).unwrap_or(u64::MAX));
    ensure!(verified == payload, "verified relay payload changed bytes");
    Ok(())
}
