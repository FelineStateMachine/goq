use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::{Connection, SendStream};
use iroh::protocol::ProtocolHandler;
use sigil_protocol::{
    AUDIO_HEADER_LEN, AudioFlags, AudioPacket, AudioPacketHeader, Capability, ClientHello,
    FrameFlags, HostHello, InputAck, MAX_AUDIO_PAYLOAD_LEN, MediaFrame, MediaFrameHeader,
    read_client_hello, read_input_event, write_host_hello, write_input_ack, write_media_frame,
};
use tracing::{debug, error, info, warn};

use crate::audio::spawn_pipewire_audio;
use crate::clock::SessionClock;
use crate::config::{HostConfig, VideoSource};
use crate::cursor::{PointerPositionTracker, PointerState};
use crate::input::{InputBackend, InputDisposition};
use crate::source::{
    EncodedFrame, EncodedGop, EncodedSource, spawn_gamescope_pipewire_after_static_preflight,
    spawn_test_pattern,
};

const MEDIA_CAPABILITIES: &[Capability] = &[Capability::VideoH264];
const AUDIO_CAPABILITIES: &[Capability] = &[Capability::AudioOpus];
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MEDIA_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_V2_PEER_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_V2_IN_FLIGHT_CAPACITY: usize = 4;
const MEDIA_V2_KEYFRAME_PRIORITY: i32 = 10;
const MEDIA_V2_DELTA_PRIORITY: i32 = 0;
const MEDIA_V2_RESET_CODE: u32 = 0x5356;
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const REJECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_HANDSHAKES: usize = 4;
// Allow one frame of ordinary scheduler/write jitter beyond the frame being
// sent, but never replay a suffix already more than two configured periods old.
const MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS: u64 = 2;
const GAMESCOPE_STARTUP_SAMPLE_WINDOW: Duration = Duration::from_millis(500);
const GAMESCOPE_STARTUP_MIN_OBSERVATION_SPAN: Duration = Duration::from_millis(100);
const GAMESCOPE_STARTUP_TARGET_MIN_FRAMES: u64 = 8;
const GAMESCOPE_STARTUP_TARGET_MIN_FPS: f64 = 45.0;
const SOURCE_REAP_GRACE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct SessionRegistry {
    active: Mutex<Option<ActiveSession>>,
    next_session_id: AtomicU64,
    session_changed: tokio::sync::Notify,
    pending_handshakes: tokio::sync::Semaphore,
}

#[derive(Clone, Copy, Debug)]
struct ActiveSession {
    remote: EndpointId,
    session_id: u64,
    nonce: [u8; 16],
    session_clock: SessionClock,
    media_active: bool,
    input_claimed: bool,
    audio_claimed: bool,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            next_session_id: AtomicU64::new(0),
            session_changed: tokio::sync::Notify::new(),
            pending_handshakes: tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES),
        }
    }
}

impl SessionRegistry {
    fn claim(self: &Arc<Self>, remote: EndpointId, nonce: [u8; 16]) -> Result<SessionLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(current) = *active {
            bail!("host already has active client {}", current.remote);
        }
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session_clock = SessionClock::start();
        *active = Some(ActiveSession {
            remote,
            session_id,
            nonce,
            session_clock,
            media_active: true,
            input_claimed: false,
            audio_claimed: false,
        });
        Ok(SessionLease {
            registry: Arc::clone(self),
            remote,
            session_id,
            session_clock,
        })
    }

    fn claim_input(self: &Arc<Self>, remote: EndpointId, nonce: [u8; 16]) -> Result<InputLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("input connection does not match the active media session")?;
        ensure!(
            !session.input_claimed,
            "active client already has an input stream"
        );
        session.input_claimed = true;
        Ok(InputLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
        })
    }

    fn claim_audio(self: &Arc<Self>, remote: EndpointId, nonce: [u8; 16]) -> Result<AudioLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("audio connection does not match the active media session")?;
        ensure!(
            !session.audio_claimed,
            "active client already has an audio connection"
        );
        session.audio_claimed = true;
        Ok(AudioLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
            session_clock: session.session_clock,
        })
    }

    fn release(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            // Keep the registry occupied until the input handler has observed
            // media shutdown and released all held uinput transitions. This
            // prevents a reconnect from sharing the device with a draining
            // predecessor session.
            session.media_active = false;
            if !session.input_claimed && !session.audio_claimed {
                *active = None;
            }
            drop(active);
            self.session_changed.notify_one();
        }
    }

    fn is_active(&self, remote: EndpointId, session_id: u64) -> bool {
        self.active
            .lock()
            .expect("session registry poisoned")
            .is_some_and(|active| {
                active.media_active && active.remote == remote && active.session_id == session_id
            })
    }

    fn release_input(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.input_claimed = false;
            if !session.media_active && !session.audio_claimed {
                *active = None;
            }
        }
    }

    fn release_audio(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.audio_claimed = false;
            if !session.media_active && !session.input_claimed {
                *active = None;
            }
        }
    }
}

#[derive(Debug)]
struct SessionLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    session_clock: SessionClock,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.registry.release(self.remote, self.session_id);
    }
}

#[derive(Debug)]
struct InputLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
}

#[derive(Debug)]
struct AudioLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    session_clock: SessionClock,
}

#[derive(Debug)]
struct SourceTaskGuard(Option<tokio::task::JoinHandle<Result<()>>>);

impl SourceTaskGuard {
    fn new(task: tokio::task::JoinHandle<Result<()>>) -> Self {
        Self(Some(task))
    }

    async fn abort_and_wait(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }

    async fn wait_or_abort(mut self, grace_timeout: Duration) {
        let Some(mut task) = self.0.take() else {
            return;
        };
        if tokio::time::timeout(grace_timeout, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for SourceTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct StartupCadenceSample {
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    first_observed_at: Option<Instant>,
    last_observed_at: Option<Instant>,
    receiver_open: bool,
    decodable_gop_ready: bool,
}

impl StartupCadenceSample {
    fn observe(&mut self, frame: &EncodedFrame) {
        if self.first_sequence.is_none() {
            self.first_sequence = Some(frame.sequence);
            self.first_observed_at = Some(frame.observed_at);
        }
        if self
            .last_sequence
            .is_none_or(|sequence| frame.sequence >= sequence)
        {
            self.last_sequence = Some(frame.sequence);
            self.last_observed_at = Some(frame.observed_at);
        }
    }

    fn frame_progress(self) -> u64 {
        self.first_sequence
            .zip(self.last_sequence)
            .map_or(0, |(first, last)| last.saturating_sub(first) + 1)
    }

    fn fps(self) -> f64 {
        let frames = self.frame_progress();
        let elapsed = self.observation_span();
        if frames < 2 || elapsed.is_zero() {
            return 0.0;
        }
        (frames - 1) as f64 / elapsed.as_secs_f64()
    }

    fn observation_span(self) -> Duration {
        self.first_observed_at
            .zip(self.last_observed_at)
            .map_or(Duration::ZERO, |(first, last)| {
                last.saturating_duration_since(first)
            })
    }

    fn has_representative_span(self) -> bool {
        self.observation_span() >= GAMESCOPE_STARTUP_MIN_OBSERVATION_SPAN
    }

    fn is_usable(self) -> bool {
        self.receiver_open && self.frame_progress() > 0 && self.decodable_gop_ready
    }

    fn meets_target_cadence(self) -> bool {
        self.receiver_open
            && self.has_representative_span()
            && self.frame_progress() >= GAMESCOPE_STARTUP_TARGET_MIN_FRAMES
            && self.fps() >= GAMESCOPE_STARTUP_TARGET_MIN_FPS
    }
}

fn startup_source_needs_restart(sample: StartupCadenceSample) -> bool {
    !sample.is_usable()
}

async fn sample_startup_cadence(
    receiver: &mut tokio::sync::watch::Receiver<Option<EncodedFrame>>,
    current_gop: &tokio::sync::watch::Receiver<Option<EncodedGop>>,
    window: Duration,
) -> StartupCadenceSample {
    let mut sample = StartupCadenceSample {
        receiver_open: true,
        ..StartupCadenceSample::default()
    };
    if let Some(frame) = receiver.borrow_and_update().as_ref() {
        sample.observe(frame);
    }
    sample.decodable_gop_ready = current_gop.borrow().is_some();
    let deadline = tokio::time::Instant::now() + window;
    while !sample.is_usable() || !sample.has_representative_span() {
        match tokio::time::timeout_at(deadline, receiver.changed()).await {
            Ok(Ok(())) => {
                if let Some(frame) = receiver.borrow_and_update().as_ref() {
                    sample.observe(frame);
                }
                sample.decodable_gop_ready = current_gop.borrow().is_some();
            }
            Ok(Err(_)) => {
                sample.receiver_open = false;
                break;
            }
            Err(_) => break,
        }
    }
    sample
}

async fn reap_encoded_source(source: EncodedSource) {
    reap_encoded_source_with_timeout(source, SOURCE_REAP_GRACE_TIMEOUT).await;
}

async fn reap_encoded_source_with_timeout(source: EncodedSource, grace_timeout: Duration) {
    let EncodedSource {
        frames,
        current_gop,
        task,
        pointer_surface_dimensions: _,
    } = source;
    // Closing both bounded outputs gives the capture task a chance to kill and
    // wait for its GStreamer child itself. Abort is only the bounded fallback.
    drop(frames);
    drop(current_gop);
    SourceTaskGuard::new(task)
        .wait_or_abort(grace_timeout)
        .await;
}

async fn select_gamescope_startup_source(
    config: HostConfig,
    session_clock: SessionClock,
    mut primary: EncodedSource,
) -> Result<EncodedSource> {
    let primary_sample = sample_startup_cadence(
        &mut primary.frames,
        &primary.current_gop,
        GAMESCOPE_STARTUP_SAMPLE_WINDOW,
    )
    .await;
    info!(
        frames = primary_sample.frame_progress(),
        fps = primary_sample.fps(),
        receiver_open = primary_sample.receiver_open,
        decodable_gop_ready = primary_sample.decodable_gop_ready,
        target_cadence = primary_sample.meets_target_cadence(),
        "sampled primary Gamescope capture startup"
    );
    if !startup_source_needs_restart(primary_sample) {
        return Ok(primary);
    }

    // Gamescope's PipeWire export does not reliably fan out full cadence to
    // two simultaneous consumers. Reap an unhealthy startup pipeline before
    // opening its replacement; overlapping them can divide vblank deliveries
    // and permanently strand the selected source at half rate.
    reap_encoded_source(primary).await;
    let mut replacement = spawn_gamescope_pipewire_after_static_preflight(config, session_clock)
        .await
        .context("restarting unhealthy Gamescope capture pipeline")?;
    let replacement_sample = sample_startup_cadence(
        &mut replacement.frames,
        &replacement.current_gop,
        GAMESCOPE_STARTUP_SAMPLE_WINDOW,
    )
    .await;
    info!(
        frames = replacement_sample.frame_progress(),
        fps = replacement_sample.fps(),
        receiver_open = replacement_sample.receiver_open,
        decodable_gop_ready = replacement_sample.decodable_gop_ready,
        target_cadence = replacement_sample.meets_target_cadence(),
        "sampled sequential Gamescope capture startup replacement"
    );
    if !replacement_sample.is_usable() {
        let frames = replacement_sample.frame_progress();
        let fps = replacement_sample.fps();
        let receiver_open = replacement_sample.receiver_open;
        let decodable_gop_ready = replacement_sample.decodable_gop_ready;
        reap_encoded_source(replacement).await;
        bail!(
            "replacement Gamescope capture pipeline remained unhealthy: \
             frames={frames}, fps={fps:.2}, receiver_open={receiver_open}, \
             decodable_gop_ready={decodable_gop_ready}"
        );
    }
    Ok(replacement)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaActivity {
    SourceChanged,
    PeerDisconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaReplayDecision {
    Send { discontinuity: bool },
    SkipUntilKeyframe,
    DiscardStaleSuffix { through_sequence: u64 },
}

#[derive(Debug, PartialEq, Eq)]
enum MediaV2ScheduleDecision {
    Send {
        discontinuity: bool,
        cancel_sequences: Vec<u64>,
    },
    SkipUntilKeyframe,
    EnterResync {
        cancel_sequences: Vec<u64>,
    },
}

#[derive(Debug)]
struct MediaV2Scheduler {
    in_flight: Vec<u64>,
    last_scheduled_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV2Scheduler {
    fn default() -> Self {
        Self {
            in_flight: Vec::with_capacity(MEDIA_V2_IN_FLIGHT_CAPACITY),
            last_scheduled_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV2Scheduler {
    fn schedule(
        &mut self,
        sequence: u64,
        independently_decodable: bool,
    ) -> MediaV2ScheduleDecision {
        if self
            .last_scheduled_sequence
            .is_some_and(|last| sequence <= last)
        {
            return MediaV2ScheduleDecision::SkipUntilKeyframe;
        }

        let sequence_discontinuity = self
            .last_scheduled_sequence
            .is_some_and(|last| last.checked_add(1) != Some(sequence));
        if independently_decodable {
            let cancel_sequences = std::mem::take(&mut self.in_flight);
            let discontinuity = self.discontinuity_pending
                || sequence_discontinuity
                || !cancel_sequences.is_empty();
            self.in_flight.push(sequence);
            self.last_scheduled_sequence = Some(sequence);
            self.waiting_for_keyframe = false;
            self.discontinuity_pending = false;
            return MediaV2ScheduleDecision::Send {
                discontinuity,
                cancel_sequences,
            };
        }

        if self.waiting_for_keyframe {
            return MediaV2ScheduleDecision::SkipUntilKeyframe;
        }
        if sequence_discontinuity || self.in_flight.len() == MEDIA_V2_IN_FLIGHT_CAPACITY {
            return MediaV2ScheduleDecision::EnterResync {
                cancel_sequences: self.enter_resync(),
            };
        }

        self.in_flight.push(sequence);
        self.last_scheduled_sequence = Some(sequence);
        MediaV2ScheduleDecision::Send {
            discontinuity: false,
            cancel_sequences: Vec::new(),
        }
    }

    fn complete(&mut self, sequence: u64) {
        if let Some(index) = self.in_flight.iter().position(|value| *value == sequence) {
            self.in_flight.swap_remove(index);
        }
    }

    fn fail(&mut self, sequence: u64) -> Vec<u64> {
        let Some(index) = self.in_flight.iter().position(|value| *value == sequence) else {
            // A completion from an already-cancelled GOP must not poison the
            // newer GOP which replaced it.
            return Vec::new();
        };
        self.in_flight.swap_remove(index);
        self.enter_resync()
    }

    fn fail_all(&mut self) -> Vec<u64> {
        self.enter_resync()
    }

    fn enter_resync(&mut self) -> Vec<u64> {
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
        std::mem::take(&mut self.in_flight)
    }
}

struct ResetOnDropSendStream(Option<SendStream>);

impl ResetOnDropSendStream {
    fn new(stream: SendStream) -> Self {
        Self(Some(stream))
    }

    fn stream(&self) -> &SendStream {
        self.0.as_ref().expect("send stream guard is armed")
    }

    fn stream_mut(&mut self) -> &mut SendStream {
        self.0.as_mut().expect("send stream guard is armed")
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for ResetOnDropSendStream {
    fn drop(&mut self) {
        if let Some(stream) = self.0.as_mut() {
            let _ = stream.reset(MEDIA_V2_RESET_CODE.into());
        }
    }
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
}

fn maximum_media_replay_age(framerate: u32) -> Duration {
    debug_assert!(framerate > 0);
    let frame_period_nanos = 1_000_000_000_u64.div_ceil(u64::from(framerate.max(1)));
    Duration::from_nanos(frame_period_nanos.saturating_mul(MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS))
}

async fn wait_for_media_activity<T, F>(
    receiver: &mut tokio::sync::watch::Receiver<T>,
    peer_disconnected: Pin<&mut F>,
) -> Result<MediaActivity>
where
    T: Clone + Send + Sync,
    F: Future<Output = ()>,
{
    tokio::select! {
        changed = receiver.changed() => {
            changed.context("encoded source stopped")?;
            Ok(MediaActivity::SourceChanged)
        }
        () = peer_disconnected => Ok(MediaActivity::PeerDisconnected),
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        self.registry.release_input(self.remote, self.session_id);
    }
}

impl Drop for AudioLease {
    fn drop(&mut self) {
        self.registry.release_audio(self.remote, self.session_id);
    }
}

#[derive(Clone, Debug)]
pub struct MediaHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
}

#[derive(Clone, Debug)]
pub struct MediaV2Handler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
}

impl ProtocolHandler for MediaHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media(connection, self.config.clone(), &self.sessions).await {
            warn!(%remote, %error, "media connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for MediaV2Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_v2(connection, self.config.clone(), &self.sessions).await {
            warn!(%remote, %error, "media v2 connection ended");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InputHandler {
    pub backend: InputBackend,
    pub pointer_positions: Option<PointerPositionTracker>,
    pub sessions: Arc<SessionRegistry>,
}

#[derive(Clone, Debug)]
pub struct AudioHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
}

impl ProtocolHandler for AudioHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_audio(connection, self.config.clone(), &self.sessions).await {
            warn!(%remote, %error, "audio connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for InputHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_input(
            connection,
            &self.backend,
            self.pointer_positions.as_ref(),
            &self.sessions,
        )
        .await
        {
            warn!(%remote, %error, "input connection ended");
        }
        Ok(())
    }
}

async fn serve_media(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting media stream")?
        .context("accepting media stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media hello received");

    let lease = match sessions.claim(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, "host already has an active client").await?;
            return Err(error);
        }
    };

    // `serve` has already completed the static executable and encoder
    // preflight. Resolve the live PipeWire node and create the bounded source
    // before accepting the session so plugin discovery can never sit behind an
    // accepted HostHello and the client's media-idle timeout.
    let source = match config.source {
        VideoSource::TestPattern => Ok(spawn_test_pattern(config.clone(), lease.session_clock)),
        VideoSource::GamescopePipewire => {
            let primary = spawn_gamescope_pipewire_after_static_preflight(
                config.clone(),
                lease.session_clock,
            )
            .await?;
            select_gamescope_startup_source(config.clone(), lease.session_clock, primary).await
        }
    };
    let EncodedSource {
        frames: frame_receiver,
        current_gop: mut current_gop_receiver,
        task: source_task,
        pointer_surface_dimensions,
    } = match source {
        Ok(source) => source,
        Err(error) => {
            send_rejection(&mut send, "video source is unavailable").await?;
            return Err(error);
        }
    };
    let source_task = SourceTaskGuard::new(source_task);

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    info!(%remote, session_id = lease.session_id, "media client accepted");

    let session_result: Result<()> = async {
        let maximum_replay_age = maximum_media_replay_age(config.framerate);
        let mut replay_cursor = MediaReplayCursor::default();
        let peer_disconnected = async {
            let result = connection.closed().await;
            debug!(%remote, ?result, "media connection closed");
        };
        tokio::pin!(peer_disconnected);
        loop {
            match wait_for_media_activity(&mut current_gop_receiver, peer_disconnected.as_mut())
                .await?
            {
                MediaActivity::PeerDisconnected => return Ok(()),
                MediaActivity::SourceChanged => {}
            }
            let Some(current_gop) = current_gop_receiver.borrow_and_update().clone() else {
                continue;
            };
            let initial_replay_started_at =
                replay_cursor.last_sequence.is_none().then(Instant::now);
            let replay_through_sequence = current_gop
                .frames
                .last()
                .map(|frame| frame.sequence)
                .context("current GOP snapshot is empty")?;
            for frame in new_current_gop_frames(current_gop, replay_cursor.last_sequence) {
                let discontinuity = match replay_cursor.classify(
                    &frame,
                    replay_through_sequence,
                    initial_replay_started_at,
                    Instant::now(),
                    maximum_replay_age,
                ) {
                    MediaReplayDecision::Send { discontinuity } => discontinuity,
                    MediaReplayDecision::SkipUntilKeyframe => {
                        debug!(
                            sequence = frame.sequence,
                            "waiting for keyframe with codec configuration"
                        );
                        continue;
                    }
                    MediaReplayDecision::DiscardStaleSuffix { through_sequence } => {
                        debug!(
                            sequence = frame.sequence,
                            through_sequence,
                            replay_age_micros = frame.observed_at.elapsed().as_micros(),
                            maximum_replay_age_micros = maximum_replay_age.as_micros(),
                            "discarding stale media suffix and waiting for keyframe"
                        );
                        break;
                    }
                };
                let mut flags = FrameFlags::NONE;
                if frame.keyframe {
                    flags = flags.union(FrameFlags::KEYFRAME);
                }
                if frame.codec_config {
                    flags = flags.union(FrameFlags::CODEC_CONFIG);
                }
                if discontinuity {
                    flags = flags.union(FrameFlags::DISCONTINUITY);
                }
                let width = u16::try_from(config.width).context("width exceeds protocol")?;
                let height = u16::try_from(config.height).context("height exceeds protocol")?;
                let header = MediaFrameHeader::h264(
                    width,
                    height,
                    frame.data.len(),
                    frame.sequence,
                    frame.capture_timestamp_micros,
                    frame.presentation_timestamp_micros,
                    flags,
                )?;
                let media_frame = MediaFrame::new(header, frame.data.as_ref().to_vec())?;
                tokio::time::timeout(
                    MEDIA_WRITE_TIMEOUT,
                    write_media_frame(&mut send, &media_frame),
                )
                .await
                .context("timed out writing media frame")??;
                replay_cursor.commit_sent(&frame);
            }
        }
    }
    .await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media client released");
    session_result
}

async fn serve_media_v2(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting media v2 handshake stream")?
        .context("accepting media v2 handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media v2 hello received");

    let lease = match sessions.claim(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, "host already has an active client").await?;
            return Err(error);
        }
    };

    let source = match config.source {
        VideoSource::TestPattern => Ok(spawn_test_pattern(config.clone(), lease.session_clock)),
        VideoSource::GamescopePipewire => {
            let primary = spawn_gamescope_pipewire_after_static_preflight(
                config.clone(),
                lease.session_clock,
            )
            .await?;
            select_gamescope_startup_source(config.clone(), lease.session_clock, primary).await
        }
    };
    let EncodedSource {
        frames: frame_receiver,
        current_gop: mut current_gop_receiver,
        task: source_task,
        pointer_surface_dimensions,
    } = match source {
        Ok(source) => source,
        Err(error) => {
            send_rejection(&mut send, "video source is unavailable").await?;
            return Err(error);
        }
    };
    let source_task = SourceTaskGuard::new(source_task);

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    send.finish()
        .context("finishing media v2 handshake response")?;
    drop(send);
    drop(recv);
    info!(%remote, session_id = lease.session_id, "media v2 client accepted");

    let session_result =
        run_media_v2_session(&connection, &config, &mut current_gop_receiver, remote).await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media v2 client released");
    session_result
}

async fn run_media_v2_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    remote: EndpointId,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut scheduler = MediaV2Scheduler::default();
    let mut send_tasks = tokio::task::JoinSet::new();

    let result = loop {
        tokio::select! {
            closed = connection.closed() => {
                debug!(%remote, ?closed, "media v2 connection closed");
                break Ok(());
            }
            task = send_tasks.join_next(), if !send_tasks.is_empty() => {
                match task.expect("guarded by non-empty send task set") {
                    Ok((sequence, Ok(()))) => scheduler.complete(sequence),
                    Ok((sequence, Err(error))) => {
                        warn!(sequence, %error, "media v2 object send failed; waiting for keyframe");
                        if !scheduler.fail(sequence).is_empty() {
                            send_tasks.abort_all();
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        warn!(%error, "media v2 object task failed; waiting for keyframe");
                        if !scheduler.fail_all().is_empty() {
                            send_tasks.abort_all();
                        }
                    }
                }
            }
            changed = current_gop_receiver.changed() => {
                if let Err(error) = changed {
                    break Err(error).context("encoded source stopped");
                }
                let Some(current_gop) = current_gop_receiver.borrow_and_update().clone() else {
                    continue;
                };
                let initial_replay_started_at =
                    replay_cursor.last_sequence.is_none().then(Instant::now);
                let replay_through_sequence = current_gop
                    .frames
                    .last()
                    .map(|frame| frame.sequence)
                    .context("current GOP snapshot is empty")?;

                for frame in new_current_gop_frames(current_gop, replay_cursor.last_sequence) {
                    let replay_discontinuity = match replay_cursor.classify(
                        &frame,
                        replay_through_sequence,
                        initial_replay_started_at,
                        Instant::now(),
                        maximum_replay_age,
                    ) {
                        MediaReplayDecision::Send { discontinuity } => discontinuity,
                        MediaReplayDecision::SkipUntilKeyframe => continue,
                        MediaReplayDecision::DiscardStaleSuffix { .. } => break,
                    };
                    let independently_decodable = frame.keyframe && frame.codec_config;
                    let (scheduler_discontinuity, cancel_sequences) =
                        match scheduler.schedule(frame.sequence, independently_decodable) {
                            MediaV2ScheduleDecision::Send {
                                discontinuity,
                                cancel_sequences,
                            } => (discontinuity, cancel_sequences),
                            MediaV2ScheduleDecision::SkipUntilKeyframe => continue,
                            MediaV2ScheduleDecision::EnterResync { cancel_sequences } => {
                                if !cancel_sequences.is_empty() {
                                    send_tasks.abort_all();
                                }
                                continue;
                            }
                        };
                    if !cancel_sequences.is_empty() {
                        debug!(
                            sequence = frame.sequence,
                            ?cancel_sequences,
                            "keyframe superseding media v2 objects"
                        );
                        send_tasks.abort_all();
                    }

                    let media_frame = media_frame_for_encoded(
                        config,
                        &frame,
                        replay_discontinuity || scheduler_discontinuity,
                    )?;
                    let sequence = frame.sequence;
                    let keyframe = independently_decodable;
                    // Reserve stream IDs in encoded-frame order. Opening them
                    // inside the concurrent writer tasks would let task
                    // scheduling reorder the QUIC object sequence even though
                    // each object carries a monotonic media sequence number.
                    let stream = match tokio::time::timeout(
                        MEDIA_WRITE_TIMEOUT,
                        connection.open_uni(),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            warn!(sequence, %error, "opening media v2 object stream failed");
                            if !scheduler.fail(sequence).is_empty() {
                                send_tasks.abort_all();
                            }
                            continue;
                        }
                        Err(_) => {
                            warn!(sequence, "opening media v2 object stream timed out");
                            if !scheduler.fail(sequence).is_empty() {
                                send_tasks.abort_all();
                            }
                            continue;
                        }
                    };
                    let stream = ResetOnDropSendStream::new(stream);
                    if let Err(error) = stream
                        .stream()
                        .set_priority(media_v2_priority(keyframe))
                    {
                        warn!(sequence, %error, "setting media v2 object priority failed");
                        if !scheduler.fail(sequence).is_empty() {
                            send_tasks.abort_all();
                        }
                        continue;
                    }
                    send_tasks.spawn(async move {
                        (
                            sequence,
                            send_media_v2_object(stream, media_frame).await,
                        )
                    });
                    replay_cursor.commit_sent(&frame);
                }
            }
        }
    };

    send_tasks.abort_all();
    while send_tasks.join_next().await.is_some() {}
    result
}

fn media_frame_for_encoded(
    config: &HostConfig,
    frame: &EncodedFrame,
    discontinuity: bool,
) -> Result<MediaFrame> {
    let mut flags = FrameFlags::NONE;
    if frame.keyframe {
        flags = flags.union(FrameFlags::KEYFRAME);
    }
    if frame.codec_config {
        flags = flags.union(FrameFlags::CODEC_CONFIG);
    }
    if discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let width = u16::try_from(config.width).context("width exceeds protocol")?;
    let height = u16::try_from(config.height).context("height exceeds protocol")?;
    let header = MediaFrameHeader::h264(
        width,
        height,
        frame.data.len(),
        frame.sequence,
        frame.capture_timestamp_micros,
        frame.presentation_timestamp_micros,
        flags,
    )?;
    MediaFrame::new(header, frame.data.as_ref().to_vec()).map_err(Into::into)
}

async fn send_media_v2_object(mut stream: ResetOnDropSendStream, frame: MediaFrame) -> Result<()> {
    tokio::time::timeout(
        MEDIA_WRITE_TIMEOUT,
        write_media_frame(stream.stream_mut(), &frame),
    )
    .await
    .context("timed out writing media v2 object")??;
    stream
        .stream_mut()
        .finish()
        .context("finishing media v2 object stream")?;
    match tokio::time::timeout(MEDIA_V2_PEER_ACK_TIMEOUT, stream.stream().stopped())
        .await
        .context("timed out waiting for media v2 object acknowledgement")?
        .context("waiting for media v2 object acknowledgement")?
    {
        None => {
            stream.disarm();
            Ok(())
        }
        Some(code) => bail!("peer stopped media v2 object stream with code {code}"),
    }
}

fn media_v2_priority(keyframe: bool) -> i32 {
    if keyframe {
        MEDIA_V2_KEYFRAME_PRIORITY
    } else {
        MEDIA_V2_DELTA_PRIORITY
    }
}

fn new_current_gop_frames(
    current_gop: EncodedGop,
    last_sequence: Option<u64>,
) -> impl Iterator<Item = EncodedFrame> {
    current_gop
        .frames
        .into_iter()
        .skip_while(move |frame| last_sequence.is_some_and(|last| frame.sequence <= last))
}

async fn serve_input(
    connection: Connection,
    backend: &InputBackend,
    pointer_positions: Option<&PointerPositionTracker>,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting input stream")?
        .context("accepting input stream")?;
    let hello = receive_hello_unconstrained(&mut recv).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "input hello received");

    let supported =
        supported_input_capabilities(backend.capabilities(), pointer_positions.is_some());
    let negotiated = negotiated_input_capabilities(&hello, &supported);

    let ack_enabled = negotiated.contains(&Capability::InputAck);
    let feedback_enabled = negotiated.contains(&Capability::PointerPositionFeedback);
    let visibility_feedback_enabled = negotiated.contains(&Capability::PointerVisibilityFeedback);
    let mut pointer_positions = pointer_positions
        .filter(|_| feedback_enabled)
        .map(PointerPositionTracker::subscribe);
    let lease = match sessions.claim_input(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    write_host_hello(
        &mut send,
        &HostHello::accepted(lease.session_id, negotiated.clone()),
    )
    .await?;
    info!(%remote, session_id = lease.session_id, "input client accepted");

    let session_result: Result<()> = async {
        let mut received_events = 0_u64;
        if let Some(pointer_positions) = pointer_positions.as_ref() {
            let pointer_state = *pointer_positions.borrow();
            let (pointer_position, pointer_visible) =
                pointer_feedback_fields(Some(pointer_state), visibility_feedback_enabled);
            tokio::time::timeout(
                INPUT_ACK_TIMEOUT,
                write_input_ack(
                    &mut send,
                    &InputAck {
                        sequence: received_events,
                        pointer_position,
                        pointer_visible,
                    },
                ),
            )
            .await
            .context("timed out writing initial pointer position")??;
        }
        loop {
            if !sessions.is_active(remote, lease.session_id) {
                debug!(%remote, session_id = lease.session_id, "media session ended; closing input");
                break;
            }
            tokio::select! {
                _ = sessions.session_changed.notified() => continue,
                changed = async {
                    pointer_positions
                        .as_mut()
                        .expect("feedback branch is guarded")
                        .changed()
                        .await
                }, if pointer_positions.is_some() => {
                    changed.context("Xwayland pointer tracker stopped")?;
                    let pointer_state = {
                        let receiver = pointer_positions
                            .as_mut()
                            .expect("feedback branch is guarded");
                        *receiver.borrow_and_update()
                    };
                    let (pointer_position, pointer_visible) =
                        pointer_feedback_fields(Some(pointer_state), visibility_feedback_enabled);
                    tokio::time::timeout(
                        INPUT_ACK_TIMEOUT,
                        write_input_ack(
                            &mut send,
                            &InputAck {
                                sequence: received_events,
                                pointer_position,
                                pointer_visible,
                            },
                        ),
                    )
                    .await
                    .context("timed out writing pointer position feedback")??;
                }
                event = read_input_event(&mut recv) => {
                    let Some(event) = event? else {
                        break;
                    };
                    if !sessions.is_active(remote, lease.session_id) {
                        debug!(%remote, session_id = lease.session_id, "discarding input after media ended");
                        break;
                    }
                    match backend.apply(&event, &negotiated)? {
                        InputDisposition::Probed => {
                            debug!(%remote, "input liveness probe acknowledged");
                        }
                        InputDisposition::Observed => {
                            info!(%remote, event_type = input_event_type(&event), "input event observed");
                        }
                        InputDisposition::Disabled => {
                            debug!(%remote, event_type = input_event_type(&event), "input event ignored because injection is disabled");
                        }
                        #[cfg(target_os = "linux")]
                        InputDisposition::Injected => {
                            debug!(%remote, event_type = input_event_type(&event), "input event injected");
                        }
                        InputDisposition::TextIgnored => {
                            debug!(%remote, event_type = "text", "text input is unsupported and was ignored");
                        }
                    }
                    received_events = received_events.saturating_add(1);
                    if ack_enabled {
                        let pointer_state = pointer_positions
                            .as_ref()
                            .map(|positions| *positions.borrow());
                        let (pointer_position, pointer_visible) =
                            pointer_feedback_fields(pointer_state, visibility_feedback_enabled);
                        tokio::time::timeout(
                            INPUT_ACK_TIMEOUT,
                            write_input_ack(
                                &mut send,
                                &InputAck {
                                    sequence: received_events,
                                    pointer_position,
                                    pointer_visible,
                                },
                            ),
                        )
                        .await
                        .context("timed out writing input acknowledgment")??;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    let reset_result = backend
        .reset_session()
        .context("releasing held input transitions at session end");
    let result = session_result.and(reset_result);
    drop(lease);
    info!(%remote, "input client released");
    result
}

async fn serve_audio(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting audio handshake stream")?
        .context("accepting audio handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::AudioOpus).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "audio hello received");

    if config.audio.is_none() {
        send_rejection(&mut send, "audio is unavailable").await?;
        bail!("audio is not configured");
    }
    let maximum_datagram = connection.max_datagram_size();
    if maximum_datagram.is_none_or(|maximum| maximum < AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD_LEN) {
        send_rejection(&mut send, "peer cannot carry v1 audio datagrams").await?;
        bail!(
            "peer audio datagram limit {:?} is below {}",
            maximum_datagram,
            AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD_LEN
        );
    }
    let lease = match sessions.claim_audio(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    let (mut audio_receiver, audio_task) =
        match spawn_pipewire_audio(config, lease.session_clock).await {
            Ok(source) => source,
            Err(error) => {
                send_rejection(&mut send, "audio source is unavailable").await?;
                return Err(error);
            }
        };
    let audio_task = SourceTaskGuard::new(audio_task);
    write_host_hello(
        &mut send,
        &HostHello::accepted(
            lease.session_id,
            negotiated_capabilities(&hello, AUDIO_CAPABILITIES),
        ),
    )
    .await?;
    send.finish()?;
    info!(%remote, session_id = lease.session_id, "audio client accepted");

    let session_result: Result<()> = async {
        loop {
            if !sessions.is_active(remote, lease.session_id) {
                break;
            }
            tokio::select! {
                _ = sessions.session_changed.notified() => continue,
                packet = audio_receiver.recv() => {
                    let packet = packet.context("audio source stopped")?;
                    let flags = if packet.discontinuity {
                        AudioFlags::DISCONTINUITY
                    } else {
                        AudioFlags::NONE
                    };
                    let header = AudioPacketHeader::opus(
                        packet.payload.len(),
                        packet.sequence,
                        packet.capture_timestamp_us,
                        packet.pts_us,
                        flags,
                    )?;
                    let datagram = AudioPacket::new(header, packet.payload.as_ref().to_vec())?
                        .encode_datagram()?;
                    match connection.send_datagram(datagram.into()) {
                        Ok(()) => {}
                        Err(error) => {
                            // The non-waiting API bounds the QUIC datagram buffer by
                            // evicting stale datagrams. Its errors mean the negotiated
                            // path cannot carry the fixed v1 packet and are terminal.
                            return Err(error).context("sending bounded audio datagram");
                        }
                    }
                }
                result = connection.closed() => {
                    debug!(%remote, ?result, "audio connection closed");
                    break;
                }
            }
        }
        Ok(())
    }
    .await;
    audio_task.abort_and_wait().await;
    drop(lease);
    info!(%remote, "audio client released");
    session_result
}

async fn send_rejection(
    send: &mut iroh::endpoint::SendStream,
    message: impl Into<String>,
) -> Result<()> {
    write_host_hello(send, &HostHello::rejected(message)).await?;
    send.finish()?;
    if tokio::time::timeout(REJECTION_DRAIN_TIMEOUT, send.stopped())
        .await
        .is_err()
    {
        debug!("timed out waiting for peer to acknowledge handshake rejection");
    }
    Ok(())
}

fn negotiated_capabilities(hello: &ClientHello, supported: &[Capability]) -> Vec<Capability> {
    supported
        .iter()
        .copied()
        .filter(|capability| hello.capabilities.contains(capability))
        .collect()
}

fn negotiated_input_capabilities(hello: &ClientHello, supported: &[Capability]) -> Vec<Capability> {
    let mut negotiated = negotiated_capabilities(hello, supported);
    if !negotiated.contains(&Capability::PointerPositionFeedback) {
        negotiated.retain(|capability| *capability != Capability::PointerVisibilityFeedback);
    }
    negotiated
}

fn pointer_feedback_fields(
    pointer_state: Option<PointerState>,
    visibility_feedback_enabled: bool,
) -> (Option<sigil_protocol::PointerPosition>, Option<bool>) {
    match pointer_state {
        Some(state) if visibility_feedback_enabled => (state.position, Some(state.visible)),
        Some(state) => (state.position.filter(|_| state.visible), None),
        None => (None, None),
    }
}

fn supported_input_capabilities(
    backend: &[Capability],
    pointer_feedback_available: bool,
) -> Vec<Capability> {
    let mut supported = backend.to_vec();
    if pointer_feedback_available && supported.contains(&Capability::RelativePointer) {
        supported.push(Capability::PointerPositionFeedback);
        supported.push(Capability::PointerVisibilityFeedback);
    }
    supported
}

fn input_event_type(event: &sigil_protocol::InputEvent) -> &'static str {
    match event {
        sigil_protocol::InputEvent::Probe => "probe",
        sigil_protocol::InputEvent::MouseMove { .. } => "mouse-move",
        sigil_protocol::InputEvent::MouseMoveRelative { .. } => "mouse-move-relative",
        sigil_protocol::InputEvent::MousePositionSync { .. } => "mouse-position-sync",
        sigil_protocol::InputEvent::MouseClick { .. } => "mouse-click",
        sigil_protocol::InputEvent::MouseDown { .. } => "mouse-down",
        sigil_protocol::InputEvent::MouseUp { .. } => "mouse-up",
        sigil_protocol::InputEvent::MouseScroll { .. } => "mouse-scroll",
        sigil_protocol::InputEvent::KeyDown { .. } => "key-down",
        sigil_protocol::InputEvent::KeyUp { .. } => "key-up",
        sigil_protocol::InputEvent::KeyClick { .. } => "key-click",
        sigil_protocol::InputEvent::Text { .. } => "text",
        sigil_protocol::InputEvent::Gamepad { .. } => "gamepad-snapshot",
    }
}

async fn receive_hello<R>(reader: &mut R, required: Capability) -> Result<ClientHello>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_client_hello(reader))
        .await
        .context("timed out waiting for client hello")??
        .context("client closed before hello")?;
    ensure!(
        hello.capabilities.contains(&required),
        "client did not offer required capability {required:?}"
    );
    Ok(hello)
}

async fn receive_hello_unconstrained<R>(reader: &mut R) -> Result<ClientHello>
where
    R: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, read_client_hello(reader))
        .await
        .context("timed out waiting for client hello")??
        .context("client closed before hello")
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        error!(%info, "host panic");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropNotify {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn media_v2_scheduler_is_bounded_and_drops_a_saturated_delta_suffix() {
        let mut scheduler = MediaV2Scheduler::default();
        assert_eq!(
            scheduler.schedule(10, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                cancel_sequences: vec![],
            }
        );
        for sequence in 11..14 {
            assert!(matches!(
                scheduler.schedule(sequence, false),
                MediaV2ScheduleDecision::Send { .. }
            ));
        }
        assert_eq!(scheduler.in_flight.len(), MEDIA_V2_IN_FLIGHT_CAPACITY);

        assert_eq!(
            scheduler.schedule(14, false),
            MediaV2ScheduleDecision::EnterResync {
                cancel_sequences: vec![10, 11, 12, 13],
            }
        );
        assert!(scheduler.in_flight.is_empty());
        assert_eq!(
            scheduler.schedule(15, false),
            MediaV2ScheduleDecision::SkipUntilKeyframe
        );
    }

    #[test]
    fn blocked_old_streams_cannot_prevent_a_keyframe_decision() {
        let mut scheduler = MediaV2Scheduler::default();
        for sequence in 20..24 {
            assert!(matches!(
                scheduler.schedule(sequence, sequence == 20),
                MediaV2ScheduleDecision::Send { .. }
            ));
        }
        assert_eq!(scheduler.in_flight.len(), MEDIA_V2_IN_FLIGHT_CAPACITY);

        assert_eq!(
            scheduler.schedule(30, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![20, 21, 22, 23],
            }
        );
        assert_eq!(scheduler.in_flight, vec![30]);
    }

    #[test]
    fn media_v2_send_failure_cancels_dependents_and_resyncs_on_keyframe() {
        let mut scheduler = MediaV2Scheduler::default();
        assert!(matches!(
            scheduler.schedule(40, true),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(41, false),
            MediaV2ScheduleDecision::Send { .. }
        ));

        assert_eq!(scheduler.fail(40), vec![41]);
        assert_eq!(
            scheduler.schedule(42, false),
            MediaV2ScheduleDecision::SkipUntilKeyframe
        );
        assert_eq!(
            scheduler.schedule(50, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![],
            }
        );
        scheduler.complete(50);
        assert!(scheduler.in_flight.is_empty());
        assert!(matches!(
            scheduler.schedule(51, false),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn cancelled_old_completion_cannot_poison_the_replacement_gop() {
        let mut scheduler = MediaV2Scheduler::default();
        assert!(matches!(
            scheduler.schedule(60, true),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(61, false),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(70, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                ..
            }
        ));

        assert!(scheduler.fail(60).is_empty());
        assert!(matches!(
            scheduler.schedule(71, false),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v2_keyframes_have_strictly_higher_transport_priority() {
        assert!(media_v2_priority(true) > media_v2_priority(false));
    }

    fn endpoint(byte: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[byte; 32]).public()
    }

    fn startup_sample(fps: f64, frames: u64, receiver_open: bool) -> StartupCadenceSample {
        if frames == 0 {
            return StartupCadenceSample {
                receiver_open,
                ..StartupCadenceSample::default()
            };
        }
        let first = Instant::now();
        let span = if frames > 1 && fps > 0.0 {
            Duration::from_secs_f64((frames - 1) as f64 / fps)
        } else {
            Duration::ZERO
        };
        StartupCadenceSample {
            first_sequence: Some(0),
            last_sequence: Some(frames - 1),
            first_observed_at: Some(first),
            last_observed_at: Some(first + span),
            receiver_open,
            decodable_gop_ready: true,
        }
    }

    #[test]
    fn startup_restart_requires_a_live_decodable_source_not_target_cadence() {
        let slow = startup_sample(12.0, 6, true);
        let target = startup_sample(60.0, 8, true);
        assert!(!startup_source_needs_restart(slow));
        assert!(!slow.meets_target_cadence());
        assert!(!startup_source_needs_restart(target));
        assert!(target.meets_target_cadence());
        assert!(startup_source_needs_restart(startup_sample(60.0, 8, false)));
        assert!(startup_source_needs_restart(startup_sample(0.0, 0, true)));
        let mut undecodable = startup_sample(60.0, 8, true);
        undecodable.decodable_gop_ready = false;
        assert!(startup_source_needs_restart(undecodable));
    }

    #[test]
    fn startup_bursts_are_not_sustained_health() {
        let burst = startup_sample(10_000.0, 8, true);
        assert!(burst.fps() > GAMESCOPE_STARTUP_TARGET_MIN_FPS);
        assert!(!burst.has_representative_span());
        assert!(!burst.meets_target_cadence());
        assert!(!startup_source_needs_restart(burst));
    }

    #[tokio::test]
    async fn startup_sampling_preserves_current_decodable_gop() {
        let now = Instant::now();
        let frame = EncodedFrame {
            sequence: 0,
            capture_timestamp_micros: 0,
            presentation_timestamp_micros: 0,
            observed_at: now,
            keyframe: true,
            codec_config: true,
            data: Arc::from([1_u8, 2, 3]),
        };
        let (_frame_sender, mut frame_receiver) = tokio::sync::watch::channel(Some(frame.clone()));
        let (_gop_sender, gop_receiver) = tokio::sync::watch::channel(Some(EncodedGop {
            frames: vec![frame],
            payload_bytes: 3,
        }));

        let sample =
            sample_startup_cadence(&mut frame_receiver, &gop_receiver, Duration::ZERO).await;
        assert_eq!(sample.frame_progress(), 1);
        let current_gop = gop_receiver.borrow().clone().unwrap();
        assert_eq!(current_gop.frames.len(), 1);
        assert!(current_gop.frames[0].keyframe && current_gop.frames[0].codec_config);
    }

    #[tokio::test]
    async fn reaping_source_closes_outputs_before_waiting_for_task_cleanup() {
        let (frame_sender, frame_receiver) = tokio::sync::watch::channel(None);
        let (gop_sender, gop_receiver) = tokio::sync::watch::channel(None);
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            frame_sender.closed().await;
            gop_sender.closed().await;
            let _ = reaped_tx.send(());
            Ok(())
        });
        let source = EncodedSource {
            frames: frame_receiver,
            current_gop: gop_receiver,
            task,
            pointer_surface_dimensions: None,
        };

        reap_encoded_source_with_timeout(source, Duration::from_millis(100)).await;
        reaped_rx
            .await
            .expect("source task did not observe both closed outputs");
    }

    #[tokio::test]
    async fn reaping_stalled_source_aborts_after_bounded_grace() {
        let (_frame_sender, frame_receiver) = tokio::sync::watch::channel(None);
        let (_gop_sender, gop_receiver) = tokio::sync::watch::channel(None);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _notify = DropNotify(Some(reaped_tx));
            let _ = started_tx.send(());
            std::future::pending::<Result<()>>().await
        });
        let source = EncodedSource {
            frames: frame_receiver,
            current_gop: gop_receiver,
            task,
            pointer_surface_dimensions: None,
        };

        started_rx.await.unwrap();
        reap_encoded_source_with_timeout(source, Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_millis(100), reaped_rx)
            .await
            .expect("stalled source task was not aborted and reaped")
            .unwrap();
    }

    #[test]
    fn only_one_remote_can_hold_session() {
        let sessions = Arc::new(SessionRegistry::default());
        let nonce = [7; 16];
        let first = sessions.claim(endpoint(1), nonce).unwrap();
        assert!(sessions.claim(endpoint(2), nonce).is_err());
        assert!(sessions.claim_input(endpoint(1), [8; 16]).is_err());
        let input = sessions.claim_input(endpoint(1), nonce).unwrap();
        assert_eq!(input.session_id, first.session_id);
        assert!(sessions.claim_input(endpoint(1), nonce).is_err());
        let audio = sessions.claim_audio(endpoint(1), nonce).unwrap();
        assert!(sessions.claim_audio(endpoint(1), nonce).is_err());
        drop(input);
        let draining_input = sessions.claim_input(endpoint(1), nonce).unwrap();
        drop(first);
        assert!(sessions.claim(endpoint(2), nonce).is_err());
        drop(draining_input);
        assert!(sessions.claim(endpoint(2), nonce).is_err());
        drop(audio);
        assert!(sessions.claim(endpoint(2), nonce).is_ok());
    }

    #[test]
    fn pending_handshakes_are_bounded() {
        let sessions = SessionRegistry::default();
        let permits: Vec<_> = (0..MAX_PENDING_HANDSHAKES)
            .map(|_| sessions.pending_handshakes.try_acquire().unwrap())
            .collect();
        assert!(sessions.pending_handshakes.try_acquire().is_err());
        drop(permits);
        assert!(sessions.pending_handshakes.try_acquire().is_ok());
    }

    #[test]
    fn capability_negotiation_is_an_exact_intersection() {
        let hello = ClientHello::new(
            "test",
            [0; 16],
            vec![
                Capability::AbsolutePointer,
                Capability::RelativePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::VideoH264,
                Capability::AudioOpus,
            ],
        );
        assert_eq!(
            negotiated_capabilities(
                &hello,
                &[
                    Capability::RelativePointer,
                    Capability::Keyboard,
                    Capability::Gamepad,
                ]
            ),
            vec![
                Capability::RelativePointer,
                Capability::Keyboard,
                Capability::Gamepad,
            ]
        );
        assert!(negotiated_capabilities(&hello, &[Capability::InputAck]).is_empty());
        assert_eq!(
            negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
            vec![Capability::VideoH264]
        );
        assert_eq!(
            negotiated_capabilities(&hello, AUDIO_CAPABILITIES),
            vec![Capability::AudioOpus]
        );
    }

    #[test]
    fn pointer_feedback_is_advertised_only_with_tracker_and_relative_input() {
        assert_eq!(
            supported_input_capabilities(&[Capability::RelativePointer], false),
            vec![Capability::RelativePointer]
        );
        assert_eq!(
            supported_input_capabilities(&[Capability::RelativePointer], true),
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ]
        );
        assert_eq!(
            supported_input_capabilities(&[Capability::InputAck], true),
            vec![Capability::InputAck]
        );
    }

    #[test]
    fn old_pointer_feedback_client_gets_legacy_host_hello_and_ack_shape() {
        let hello = ClientHello::new(
            "old-client",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
            ],
        );
        let supported = supported_input_capabilities(&[Capability::RelativePointer], true);
        let negotiated = negotiated_input_capabilities(&hello, &supported);
        assert_eq!(
            negotiated,
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
            ]
        );
        assert_eq!(
            serde_json::to_string(&HostHello::accepted(7, negotiated)).unwrap(),
            r#"{"version":1,"accepted":true,"session_id":7,"capabilities":["relative_pointer","pointer_position_feedback"],"message":null}"#
        );

        let position = sigil_protocol::PointerPosition { x: 320, y: 200 };
        let (pointer_position, pointer_visible) = pointer_feedback_fields(
            Some(PointerState {
                position: Some(position),
                visible: true,
            }),
            false,
        );
        assert_eq!(
            serde_json::to_string(&InputAck {
                sequence: 1,
                pointer_position,
                pointer_visible,
            })
            .unwrap(),
            r#"{"sequence":1,"pointer_position":{"x":320,"y":200}}"#
        );

        let (pointer_position, pointer_visible) = pointer_feedback_fields(
            Some(PointerState {
                position: Some(position),
                visible: false,
            }),
            false,
        );
        assert_eq!(
            serde_json::to_string(&InputAck {
                sequence: 1,
                pointer_position,
                pointer_visible,
            })
            .unwrap(),
            r#"{"sequence":1}"#
        );
    }

    #[test]
    fn pointer_visibility_feedback_requires_position_feedback() {
        let visibility_only = ClientHello::new(
            "invalid-client",
            [0; 16],
            vec![Capability::PointerVisibilityFeedback],
        );
        let supported = supported_input_capabilities(&[Capability::RelativePointer], true);
        assert!(negotiated_input_capabilities(&visibility_only, &supported).is_empty());

        let upgraded = ClientHello::new(
            "upgraded-client",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ],
        );
        assert_eq!(
            negotiated_input_capabilities(&upgraded, &supported),
            supported
        );

        let position = sigil_protocol::PointerPosition { x: 320, y: 200 };
        assert_eq!(
            pointer_feedback_fields(
                Some(PointerState {
                    position: Some(position),
                    visible: false,
                }),
                true,
            ),
            (Some(position), Some(false))
        );
    }

    #[test]
    fn audio_claim_requires_the_active_remote_and_nonce() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions.claim(endpoint(1), [9; 16]).unwrap();
        assert!(sessions.claim_audio(endpoint(2), [9; 16]).is_err());
        assert!(sessions.claim_audio(endpoint(1), [8; 16]).is_err());
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        drop(media);
        assert!(sessions.claim(endpoint(2), [0; 16]).is_err());
        drop(audio);
        assert!(sessions.claim(endpoint(2), [0; 16]).is_ok());
    }

    #[test]
    fn media_and_audio_leases_share_one_session_clock() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions.claim(endpoint(1), [9; 16]).unwrap();
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        assert_eq!(media.session_clock, audio.session_clock);
    }

    #[test]
    fn current_gop_replay_is_complete_and_skips_already_sent_frames() {
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: std::time::Instant::now(),
            keyframe,
            codec_config: keyframe,
            data: Arc::from([sequence as u8]),
        };

        let gop = || EncodedGop {
            frames: vec![frame(10, true), frame(11, false), frame(12, false)],
            payload_bytes: 3,
        };

        let initial = new_current_gop_frames(gop(), None).collect::<Vec<_>>();
        assert_eq!(
            initial
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert!(initial[0].keyframe && initial[0].codec_config);

        let resumed = new_current_gop_frames(gop(), Some(10)).collect::<Vec<_>>();
        assert_eq!(
            resumed
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );

        let behind_current_gop = new_current_gop_frames(gop(), Some(8)).collect::<Vec<_>>();
        assert_eq!(behind_current_gop[0].sequence, 10);
        assert!(behind_current_gop[0].keyframe && behind_current_gop[0].codec_config);

        assert!(new_current_gop_frames(gop(), Some(12)).next().is_none());
    }

    #[test]
    fn initial_current_gop_replay_preserves_complete_startup_snapshot() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let old_observation = observed_now - Duration::from_secs(1);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
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
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age,
            keyframe: false,
            codec_config: false,
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
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age - Duration::from_nanos(1),
            keyframe: false,
            codec_config: false,
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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: observed_now,
            keyframe,
            codec_config: keyframe,
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

    #[tokio::test]
    async fn no_frame_peer_drop_reaps_source_and_allows_reconnect() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [7; 16];
        let media = sessions.claim(remote, nonce).unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let session_id = media.session_id;

        let input_sessions = Arc::clone(&sessions);
        let input_shutdown = tokio::spawn(async move {
            loop {
                let notified = input_sessions.session_changed.notified();
                if !input_sessions.is_active(remote, session_id) {
                    break;
                }
                notified.await;
            }
            drop(input);
        });

        let (_frame_sender, mut frame_receiver) =
            tokio::sync::watch::channel(Option::<EncodedFrame>::None);
        let (source_started_tx, source_started_rx) = tokio::sync::oneshot::channel();
        let (source_reaped_tx, source_reaped_rx) = tokio::sync::oneshot::channel();
        let source_task = tokio::spawn(async move {
            let _notify = DropNotify(Some(source_reaped_tx));
            let _ = source_started_tx.send(());
            std::future::pending::<Result<()>>().await
        });
        let source_task = SourceTaskGuard::new(source_task);
        source_started_rx.await.unwrap();

        let (peer_alive, peer_disconnected) = tokio::sync::oneshot::channel::<()>();
        let disconnected = async move {
            let _ = peer_disconnected.await;
        };
        tokio::pin!(disconnected);
        drop(peer_alive);

        let activity = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_media_activity(&mut frame_receiver, disconnected.as_mut()),
        )
        .await
        .expect("no-frame media loop ignored peer disconnect")
        .unwrap();
        assert_eq!(activity, MediaActivity::PeerDisconnected);

        source_task.abort_and_wait().await;
        tokio::time::timeout(Duration::from_millis(100), source_reaped_rx)
            .await
            .expect("source task was not reaped after peer disconnect")
            .unwrap();
        drop(media);
        tokio::time::timeout(Duration::from_millis(100), input_shutdown)
            .await
            .expect("input lease did not observe media shutdown")
            .unwrap();

        assert!(sessions.claim(endpoint(2), [8; 16]).is_ok());
    }
}
