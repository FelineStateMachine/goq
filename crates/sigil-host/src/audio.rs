use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use tokio::net::UdpSocket;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::debug;

use crate::clock::{self, SessionClock};
use crate::config::HostConfig;
use crate::source::{
    log_stderr_chunks, preflight_gstreamer_element, resolve_pipewire_node_by_properties,
    validate_executable,
};
use sigil_protocol::{MAX_AUDIO_PAYLOAD_LEN, OPUS_SAMPLE_RATE};

const RTP_PAYLOAD_TYPE: u8 = 111;
const AUDIO_QUEUE_CAPACITY: usize = 2;
const RTP_DATAGRAM_CAPACITY: usize = 2_048;

#[derive(Clone, Debug)]
pub struct EncodedAudioPacket {
    pub sequence: u64,
    /// Post-encode observation in the shared media-session clock.
    pub capture_timestamp_us: u64,
    /// First-packet common-clock anchor plus elapsed RTP samples.
    pub pts_us: i64,
    pub discontinuity: bool,
    pub payload: Arc<[u8]>,
}

pub type EncodedAudioSource = (
    mpsc::Receiver<EncodedAudioPacket>,
    tokio::task::JoinHandle<Result<()>>,
);

pub async fn preflight_audio_static(config: &HostConfig) -> Result<()> {
    config.validate()?;
    let audio = config
        .audio
        .as_ref()
        .context("audio configuration is missing")?;
    let pipewire = config
        .gamescope_pipewire
        .as_ref()
        .context("gamescope_pipewire configuration is missing")?;
    validate_executable("pw_dump_path", &pipewire.pw_dump_path)?;
    validate_executable("gst_launch_path", &pipewire.gst_launch_path)?;
    validate_executable("gst_inspect_path", &pipewire.gst_inspect_path)?;
    for element in [
        "pipewiresrc",
        "queue",
        "audioconvert",
        "audioresample",
        "opusenc",
        "rtpopuspay",
        "udpsink",
    ] {
        preflight_gstreamer_element(&pipewire.gst_inspect_path, element).await?;
    }
    ensure!(
        audio.bitrate_bps == 96_000,
        "audio config changed after validation"
    );
    Ok(())
}

pub async fn preflight_audio(config: &HostConfig) -> Result<String> {
    preflight_audio_static(config).await?;
    let audio = config
        .audio
        .as_ref()
        .context("audio configuration is missing")?;
    let pipewire = config
        .gamescope_pipewire
        .as_ref()
        .context("gamescope_pipewire configuration is missing")?;
    resolve_pipewire_node_by_properties(
        &pipewire.pw_dump_path,
        &audio.node_name,
        &audio.media_class,
        &audio.match_properties,
    )
    .await
}

pub async fn spawn_pipewire_audio(
    config: HostConfig,
    session_clock: SessionClock,
) -> Result<EncodedAudioSource> {
    let audio = config
        .audio
        .as_ref()
        .context("audio configuration is missing")?;
    let pipewire = config
        .gamescope_pipewire
        .as_ref()
        .context("gamescope_pipewire configuration is missing")?;
    let target_object = resolve_pipewire_node_by_properties(
        &pipewire.pw_dump_path,
        &audio.node_name,
        &audio.media_class,
        &audio.match_properties,
    )
    .await?;
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .context("binding bounded audio RTP receiver")?;
    let port = socket
        .local_addr()
        .context("reading audio RTP address")?
        .port();
    let mut ssrc_bytes = [0_u8; 4];
    getrandom::fill(&mut ssrc_bytes).context("generating audio RTP SSRC")?;
    let ssrc = u32::from_be_bytes(ssrc_bytes);
    let args = audio_pipeline_args(&config, &target_object, port, ssrc)?;
    let (sender, receiver) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
    let task = tokio::spawn(async move {
        run_pipewire_audio(config, session_clock, args, socket, ssrc, sender).await
    });
    Ok((receiver, task))
}

async fn run_pipewire_audio(
    config: HostConfig,
    session_clock: SessionClock,
    args: Vec<OsString>,
    socket: UdpSocket,
    expected_ssrc: u32,
    sender: mpsc::Sender<EncodedAudioPacket>,
) -> Result<()> {
    let pipewire = config
        .gamescope_pipewire
        .as_ref()
        .context("gamescope_pipewire configuration is missing")?;
    let mut command = Command::new(&pipewire.gst_launch_path);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        format!(
            "starting audio pipeline with {}",
            pipewire.gst_launch_path.display()
        )
    })?;
    let stderr = child
        .stderr
        .take()
        .context("audio GStreamer stderr was not piped")?;
    let stderr_task = tokio::spawn(log_stderr_chunks(stderr, "gstreamer-audio"));

    let result = receive_rtp(socket, expected_ssrc, session_clock, sender).await;
    let _ = child.kill().await;
    let status = child.wait().await.ok();
    stderr_task.abort();
    match (result, status) {
        (Err(error), Some(status)) => {
            Err(error).with_context(|| format!("audio GStreamer pipeline exited with {status}"))
        }
        (result, _) => result,
    }
}

async fn receive_rtp(
    socket: UdpSocket,
    expected_ssrc: u32,
    session_clock: SessionClock,
    sender: mpsc::Sender<EncodedAudioPacket>,
) -> Result<()> {
    let mut datagram = [0_u8; RTP_DATAGRAM_CAPACITY];
    let mut sequence = 0_u64;
    let mut previous_rtp_sequence: Option<u16> = None;
    let mut rtp_clock = RtpClock::default();
    let mut pending_discontinuity = false;

    loop {
        let (length, source) = socket
            .recv_from(&mut datagram)
            .await
            .context("receiving audio RTP")?;
        ensure!(
            source.ip().is_loopback(),
            "audio RTP arrived from a non-loopback address"
        );
        let packet = parse_rtp_opus(&datagram[..length], expected_ssrc)?;
        let rtp_discontinuity = previous_rtp_sequence
            .is_some_and(|previous| previous.wrapping_add(1) != packet.sequence);
        previous_rtp_sequence = Some(packet.sequence);
        let observed_us = session_clock.now_micros();
        let pts_us = rtp_clock.pts_us(packet.timestamp, observed_us)?;
        let encoded = EncodedAudioPacket {
            sequence,
            capture_timestamp_us: observed_us,
            pts_us,
            discontinuity: pending_discontinuity || rtp_discontinuity,
            payload: Arc::from(packet.payload),
        };
        sequence = sequence.saturating_add(1);
        match sender.try_send(encoded) {
            Ok(()) => pending_discontinuity = false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                pending_discontinuity = true;
                debug!("dropping encoded audio packet because bounded queue is full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
    }
}

fn audio_pipeline_args(
    config: &HostConfig,
    target_object: &str,
    port: u16,
    ssrc: u32,
) -> Result<Vec<OsString>> {
    let audio = config
        .audio
        .as_ref()
        .context("audio configuration is missing")?;
    ensure!(
        !target_object.is_empty()
            && target_object.len() <= 32
            && target_object.bytes().all(|byte| byte.is_ascii_digit()),
        "resolved PipeWire audio object.serial is invalid"
    );
    let args = [
        "--quiet".to_owned(),
        "pipewiresrc".to_owned(),
        "do-timestamp=true".to_owned(),
        "min-buffers=1".to_owned(),
        "max-buffers=3".to_owned(),
        format!("target-object={target_object}"),
        "!".to_owned(),
        "queue".to_owned(),
        "max-size-buffers=3".to_owned(),
        "max-size-bytes=0".to_owned(),
        "max-size-time=0".to_owned(),
        "leaky=downstream".to_owned(),
        "!".to_owned(),
        "audioconvert".to_owned(),
        "!".to_owned(),
        "audioresample".to_owned(),
        "!".to_owned(),
        "audio/x-raw,format=S16LE,rate=48000,channels=2".to_owned(),
        "!".to_owned(),
        "opusenc".to_owned(),
        "audio-type=restricted-lowdelay".to_owned(),
        format!("bitrate={}", audio.bitrate_bps),
        "bitrate-type=cbr".to_owned(),
        "frame-size=20".to_owned(),
        "dtx=false".to_owned(),
        "inband-fec=false".to_owned(),
        format!("max-payload-size={MAX_AUDIO_PAYLOAD_LEN}"),
        "!".to_owned(),
        "rtpopuspay".to_owned(),
        format!("pt={RTP_PAYLOAD_TYPE}"),
        "mtu=600".to_owned(),
        format!("ssrc={ssrc}"),
        "!".to_owned(),
        "udpsink".to_owned(),
        "host=127.0.0.1".to_owned(),
        format!("port={port}"),
        "sync=false".to_owned(),
        "async=false".to_owned(),
    ];
    Ok(args.into_iter().map(OsString::from).collect())
}

#[derive(Debug, PartialEq, Eq)]
struct RtpOpusPacket<'a> {
    sequence: u16,
    timestamp: u32,
    payload: &'a [u8],
}

fn parse_rtp_opus(datagram: &[u8], expected_ssrc: u32) -> Result<RtpOpusPacket<'_>> {
    ensure!(
        datagram.len() >= 12,
        "RTP datagram is shorter than its fixed header"
    );
    ensure!(datagram[0] >> 6 == 2, "unsupported RTP version");
    let padding = datagram[0] & 0x20 != 0;
    let extension = datagram[0] & 0x10 != 0;
    let csrc_count = usize::from(datagram[0] & 0x0f);
    ensure!(
        datagram[1] & 0x7f == RTP_PAYLOAD_TYPE,
        "unexpected RTP payload type"
    );
    ensure!(
        u32::from_be_bytes(datagram[8..12].try_into().expect("fixed slice length"))
            == expected_ssrc,
        "unexpected RTP SSRC"
    );
    let mut payload_start = 12_usize
        .checked_add(
            csrc_count
                .checked_mul(4)
                .context("RTP CSRC length overflow")?,
        )
        .context("RTP header length overflow")?;
    ensure!(payload_start <= datagram.len(), "truncated RTP CSRC list");
    if extension {
        ensure!(
            payload_start + 4 <= datagram.len(),
            "truncated RTP extension header"
        );
        let words = usize::from(u16::from_be_bytes(
            datagram[payload_start + 2..payload_start + 4]
                .try_into()
                .expect("fixed slice length"),
        ));
        payload_start = payload_start
            .checked_add(4)
            .and_then(|value| value.checked_add(words.checked_mul(4)?))
            .context("RTP extension length overflow")?;
        ensure!(
            payload_start <= datagram.len(),
            "truncated RTP extension data"
        );
    }
    let padding_len = if padding {
        usize::from(*datagram.last().context("RTP padding byte is missing")?)
    } else {
        0
    };
    ensure!(!padding || padding_len > 0, "RTP padding length is zero");
    ensure!(
        padding_len <= datagram.len().saturating_sub(payload_start),
        "invalid RTP padding"
    );
    let payload_end = datagram.len() - padding_len;
    let payload = &datagram[payload_start..payload_end];
    ensure!(!payload.is_empty(), "RTP Opus payload is empty");
    ensure!(
        payload.len() <= MAX_AUDIO_PAYLOAD_LEN,
        "RTP Opus payload exceeds protocol limit"
    );
    Ok(RtpOpusPacket {
        sequence: u16::from_be_bytes(datagram[2..4].try_into().expect("fixed slice length")),
        timestamp: u32::from_be_bytes(datagram[4..8].try_into().expect("fixed slice length")),
        payload,
    })
}

#[derive(Default)]
struct RtpClock {
    previous: Option<u32>,
    elapsed_samples: u64,
    anchor_micros: Option<u64>,
}

impl RtpClock {
    fn pts_us(&mut self, timestamp: u32, observed_micros: u64) -> Result<i64> {
        if let Some(previous) = self.previous {
            let delta = timestamp.wrapping_sub(previous);
            // One packet may be absent, but a jump beyond ten seconds is a
            // reset or malicious timestamp rather than useful audio timing.
            ensure!(
                delta <= OPUS_SAMPLE_RATE * 10,
                "RTP timestamp advanced beyond the bounded clock window"
            );
            self.elapsed_samples = self.elapsed_samples.saturating_add(u64::from(delta));
        }
        self.previous = Some(timestamp);
        let anchor_micros = *self.anchor_micros.get_or_insert(observed_micros);
        Ok(clock::anchored_timestamp_micros(
            anchor_micros,
            self.elapsed_samples,
            OPUS_SAMPLE_RATE,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{
        GamescopeEncoderBackend, GamescopePipewireConfig, InputMode, PipewireAudioConfig,
        VaapiRateControl, VideoSource,
    };
    use crate::source::resolve_pipewire_node_exact;

    fn config() -> HostConfig {
        HostConfig {
            identity_path: PathBuf::from("/tmp/id"),
            state_path: PathBuf::from("/tmp/state"),
            source: VideoSource::GamescopePipewire,
            width: 1280,
            height: 800,
            framerate: 60,
            codec: "h264".into(),
            input_mode: InputMode::Disabled,
            uinput: None,
            ffmpeg_path: "ffmpeg".into(),
            gamescope_pipewire: Some(GamescopePipewireConfig {
                xwayland_display: None,
                node_name: "gamescope".into(),
                media_class: "Video/Source".into(),
                match_properties: BTreeMap::new(),
                pw_dump_path: "/usr/bin/pw-dump".into(),
                gst_launch_path: "/usr/bin/gst-launch-1.0".into(),
                gst_inspect_path: "/usr/bin/gst-inspect-1.0".into(),
                encoder_backend: GamescopeEncoderBackend::ExternalGstLaunch,
                vaapi_encoder: "vah264enc".into(),
                vaapi_render_node: "/dev/dri/renderD128".into(),
                rate_control: VaapiRateControl::Cbr,
                bitrate_kbps: Some(12_000),
                quantizer: None,
            }),
            audio: Some(PipewireAudioConfig {
                node_name: "sigil-game-audio".into(),
                media_class: "Audio/Sink".into(),
                match_properties: BTreeMap::new(),
                bitrate_bps: 96_000,
            }),
        }
    }

    fn rtp(sequence: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut datagram = vec![0x80, RTP_PAYLOAD_TYPE];
        datagram.extend_from_slice(&sequence.to_be_bytes());
        datagram.extend_from_slice(&timestamp.to_be_bytes());
        datagram.extend_from_slice(&ssrc.to_be_bytes());
        datagram.extend_from_slice(payload);
        datagram
    }

    #[test]
    fn pipeline_is_fixed_bounded_and_loopback_only() {
        let args: Vec<String> = audio_pipeline_args(&config(), "50", 34_567, 123)
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        for required in [
            "target-object=50",
            "max-size-buffers=3",
            "max-size-bytes=0",
            "max-size-time=0",
            "leaky=downstream",
            "audio/x-raw,format=S16LE,rate=48000,channels=2",
            "audio-type=restricted-lowdelay",
            "bitrate=96000",
            "bitrate-type=cbr",
            "frame-size=20",
            "dtx=false",
            "inband-fec=false",
            "max-payload-size=512",
            "host=127.0.0.1",
            "port=34567",
            "ssrc=123",
            "sync=false",
            "async=false",
        ] {
            assert!(args.contains(&required.to_owned()), "missing {required}");
        }
        assert!(audio_pipeline_args(&config(), "gamescope", 1, 1).is_err());
    }

    #[test]
    fn resolves_exactly_one_audio_sink_without_accepting_a_microphone() {
        let dump = br#"[
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"sigil-game-audio","media.class":"Audio/Sink",
            "device.profile.name":"stereo","object.serial":50}}},
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"sigil-game-audio","media.class":"Audio/Source",
            "device.profile.name":"stereo","object.serial":51}}}
        ]"#;
        let properties = BTreeMap::from([("device.profile.name".into(), "stereo".into())]);
        assert_eq!(
            resolve_pipewire_node_exact(dump, "sigil-game-audio", "Audio/Sink", &properties)
                .unwrap(),
            "50"
        );

        let duplicated = String::from_utf8(dump.to_vec())
            .unwrap()
            .replace("]", ",{\"type\":\"PipeWire:Interface:Node\",\"info\":{\"props\":{\"node.name\":\"sigil-game-audio\",\"media.class\":\"Audio/Sink\",\"device.profile.name\":\"stereo\",\"object.serial\":52}}}]");
        assert!(
            resolve_pipewire_node_exact(
                duplicated.as_bytes(),
                "sigil-game-audio",
                "Audio/Sink",
                &properties
            )
            .is_err()
        );
    }

    #[test]
    fn parses_strict_bounded_rtp_and_rejects_wrong_identity() {
        let datagram = rtp(7, 960, 123, &[1, 2, 3]);
        assert_eq!(
            parse_rtp_opus(&datagram, 123).unwrap(),
            RtpOpusPacket {
                sequence: 7,
                timestamp: 960,
                payload: &[1, 2, 3],
            }
        );
        assert!(parse_rtp_opus(&datagram, 124).is_err());
        let mut wrong_payload_type = datagram.clone();
        wrong_payload_type[1] = 110;
        assert!(parse_rtp_opus(&wrong_payload_type, 123).is_err());
        let mut wrong_version = datagram;
        wrong_version[0] = 0;
        assert!(parse_rtp_opus(&wrong_version, 123).is_err());
    }

    #[test]
    fn rtp_parser_handles_csrc_extension_and_padding_safely() {
        let mut datagram = vec![0xb1, RTP_PAYLOAD_TYPE]; // padding, extension, one CSRC
        datagram.extend_from_slice(&1_u16.to_be_bytes());
        datagram.extend_from_slice(&2_u32.to_be_bytes());
        datagram.extend_from_slice(&3_u32.to_be_bytes());
        datagram.extend_from_slice(&4_u32.to_be_bytes()); // CSRC
        datagram.extend_from_slice(&0xbede_u16.to_be_bytes());
        datagram.extend_from_slice(&1_u16.to_be_bytes());
        datagram.extend_from_slice(&[0, 0, 0, 0]);
        datagram.extend_from_slice(&[9, 8, 7]);
        datagram.extend_from_slice(&[0, 0, 3]);
        assert_eq!(parse_rtp_opus(&datagram, 3).unwrap().payload, &[9, 8, 7]);

        for length in 0..12 {
            assert!(parse_rtp_opus(&datagram[..length], 3).is_err());
        }
        let mut bad_padding = datagram;
        *bad_padding.last_mut().unwrap() = u8::MAX;
        assert!(parse_rtp_opus(&bad_padding, 3).is_err());
    }

    #[test]
    fn rtp_clock_handles_wrap_and_rejects_implausible_jumps() {
        let mut clock = RtpClock::default();
        assert_eq!(clock.pts_us(u32::MAX - 479, 1_000_000).unwrap(), 1_000_000);
        assert_eq!(clock.pts_us(480, 9_000_000).unwrap(), 1_020_000);
        assert_eq!(clock.pts_us(1_440, 9_000_000).unwrap(), 1_040_000);
        assert!(
            clock
                .pts_us(1_440 + OPUS_SAMPLE_RATE * 11, 9_000_000)
                .is_err()
        );
    }

    #[test]
    fn rejects_oversized_and_empty_rtp_payloads() {
        assert!(parse_rtp_opus(&rtp(0, 0, 1, &[]), 1).is_err());
        assert!(parse_rtp_opus(&rtp(0, 0, 1, &vec![0; MAX_AUDIO_PAYLOAD_LEN + 1]), 1).is_err());
    }
}
