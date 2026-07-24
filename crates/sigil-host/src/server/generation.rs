use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use moq_net::{Broadcast, BroadcastConsumer, BroadcastProducer, Error as MoqError, Track};
use sigil_protocol::{
    MOQ_AUDIO_OPUS_TRACK, MOQ_AUDIO_TRACK_PRIORITY, MOQ_VIDEO_H264_TRACK, MOQ_VIDEO_TRACK_PRIORITY,
    MediaGenerationSigningKey, SignedMediaGenerationCertificate,
    media_generation_moq_broadcast_name,
};
use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use super::moq::{run_generation_audio_publisher, run_generation_video_publisher};
use super::session::{MediaV3Telemetry, SourceTaskGuard};
use crate::audio::spawn_pipewire_audio;
use crate::authorization::unix_timestamp_now;
use crate::clock::SessionClock;
use crate::config::{HostConfig, VideoSource};
use crate::moq_catalog::{GoqCatalogProducer, publish_goq_catalog_v2};
use crate::source::{
    EncodedFrame, EncodedSource, EncoderControl, spawn_gamescope_pipewire_after_static_preflight,
    spawn_test_pattern,
};

const GENERATION_STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MediaGenerationLifecycle {
    Idle,
    Starting,
    Active,
    Stopping,
}

#[derive(Debug)]
struct GenerationState {
    lifecycle: MediaGenerationLifecycle,
    active: Option<ActiveGeneration>,
    leases: usize,
}

impl Default for GenerationState {
    fn default() -> Self {
        Self {
            lifecycle: MediaGenerationLifecycle::Idle,
            active: None,
            leases: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MediaGenerationManager {
    config: HostConfig,
    host_secret: [u8; 32],
    next_generation_id: AtomicU64,
    state: tokio::sync::Mutex<GenerationState>,
    state_changed: Notify,
    starts: AtomicU64,
    stops: AtomicU64,
}

impl MediaGenerationManager {
    pub(crate) fn new(config: HostConfig, host_secret: [u8; 32]) -> Arc<Self> {
        Arc::new(Self {
            config,
            host_secret,
            next_generation_id: AtomicU64::new(0),
            state: tokio::sync::Mutex::new(GenerationState::default()),
            state_changed: Notify::new(),
            starts: AtomicU64::new(0),
            stops: AtomicU64::new(0),
        })
    }

    pub(super) async fn acquire(self: &Arc<Self>) -> Result<MediaGenerationLease> {
        loop {
            let mut state = self.state.lock().await;
            match state.lifecycle {
                MediaGenerationLifecycle::Active => {
                    state.leases = state
                        .leases
                        .checked_add(1)
                        .context("media generation lease count overflowed")?;
                    let shared = state
                        .active
                        .as_ref()
                        .context("active generation has no resources")?
                        .shared
                        .clone();
                    return Ok(MediaGenerationLease::new(self, shared));
                }
                MediaGenerationLifecycle::Idle => {
                    state.lifecycle = MediaGenerationLifecycle::Starting;
                    drop(state);
                    let generation_id = self
                        .next_generation_id
                        .fetch_add(1, Ordering::Relaxed)
                        .checked_add(1)
                        .context("media generation id overflowed")?;
                    match ActiveGeneration::start(&self.config, self.host_secret, generation_id)
                        .await
                    {
                        Ok(active) => {
                            let shared = active.shared.clone();
                            let mut state = self.state.lock().await;
                            state.active = Some(active);
                            state.leases = 1;
                            state.lifecycle = MediaGenerationLifecycle::Active;
                            self.starts.fetch_add(1, Ordering::Relaxed);
                            self.state_changed.notify_waiters();
                            return Ok(MediaGenerationLease::new(self, shared));
                        }
                        Err(error) => {
                            let mut state = self.state.lock().await;
                            state.lifecycle = MediaGenerationLifecycle::Idle;
                            self.state_changed.notify_waiters();
                            return Err(error);
                        }
                    }
                }
                MediaGenerationLifecycle::Starting | MediaGenerationLifecycle::Stopping => {
                    let changed = self.state_changed.notified();
                    drop(state);
                    changed.await;
                }
            }
        }
    }

    async fn release(&self, generation_id: u64) {
        let active = {
            let mut state = self.state.lock().await;
            if state.lifecycle != MediaGenerationLifecycle::Active
                || state
                    .active
                    .as_ref()
                    .map(|active| active.shared.generation_id)
                    != Some(generation_id)
            {
                return;
            }
            state.leases = state.leases.saturating_sub(1);
            if state.leases != 0 {
                return;
            }
            state.lifecycle = MediaGenerationLifecycle::Stopping;
            state.active.take()
        };
        if let Some(active) = active {
            active.stop().await;
            self.stops.fetch_add(1, Ordering::Relaxed);
        }
        let mut state = self.state.lock().await;
        state.lifecycle = MediaGenerationLifecycle::Idle;
        self.state_changed.notify_waiters();
    }

    #[cfg(test)]
    async fn lifecycle(&self) -> MediaGenerationLifecycle {
        self.state.lock().await.lifecycle
    }
}

pub(super) struct GenerationShared {
    pub(super) generation_id: u64,
    pub(super) broadcast_name: String,
    broadcast: Mutex<Option<BroadcastConsumer>>,
    pub(super) certificate: SignedMediaGenerationCertificate,
    pub(super) session_clock: SessionClock,
    pub(super) pointer_surface_dimensions: Option<sigil_protocol::PointerSurfaceDimensions>,
    pub(super) encoder_control: Option<EncoderControl>,
    pub(super) telemetry: Arc<MediaV3Telemetry>,
    pub(super) audio_enabled: bool,
    pub(super) keyframe_requests: watch::Sender<Option<sigil_protocol::MediaControlRequestV3>>,
}

impl std::fmt::Debug for GenerationShared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationShared")
            .field("generation_id", &self.generation_id)
            .field("broadcast_name", &self.broadcast_name)
            .field("audio_enabled", &self.audio_enabled)
            .finish_non_exhaustive()
    }
}

impl GenerationShared {
    pub(super) fn consumer(&self) -> Result<BroadcastConsumer> {
        self.broadcast
            .lock()
            .map_err(|_| anyhow::anyhow!("media generation broadcast lock poisoned"))?
            .as_ref()
            .cloned()
            .context("media generation broadcast is stopping")
    }
}

#[derive(Debug)]
pub(super) struct MediaGenerationLease {
    manager: Weak<MediaGenerationManager>,
    pub(super) shared: Arc<GenerationShared>,
    released: bool,
}

impl MediaGenerationLease {
    fn new(manager: &Arc<MediaGenerationManager>, shared: Arc<GenerationShared>) -> Self {
        Self {
            manager: Arc::downgrade(manager),
            shared,
            released: false,
        }
    }

    pub(super) async fn release(mut self) {
        self.release_inner().await;
        self.released = true;
    }

    async fn release_inner(&self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.release(self.shared.generation_id).await;
        }
    }
}

impl Drop for MediaGenerationLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let generation_id = self.shared.generation_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { manager.release(generation_id).await });
        }
    }
}

struct ActiveGeneration {
    shared: Arc<GenerationShared>,
    shutdown: watch::Sender<bool>,
    video_publisher: tokio::task::JoinHandle<Result<()>>,
    audio_publisher: Option<tokio::task::JoinHandle<Result<()>>>,
    video_source: SourceTaskGuard,
    audio_source: Option<SourceTaskGuard>,
    _frame_receiver: watch::Receiver<Option<EncodedFrame>>,
    catalog: Option<GoqCatalogProducer>,
    broadcast: Option<BroadcastProducer>,
}

impl std::fmt::Debug for ActiveGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveGeneration")
            .field("generation_id", &self.shared.generation_id)
            .field("audio_enabled", &self.shared.audio_enabled)
            .finish_non_exhaustive()
    }
}

impl ActiveGeneration {
    async fn start(config: &HostConfig, host_secret: [u8; 32], generation_id: u64) -> Result<Self> {
        let session_clock = SessionClock::start();
        let source = match config.source {
            VideoSource::TestPattern => Ok(spawn_test_pattern(config.clone(), session_clock)),
            VideoSource::GamescopePipewire => {
                let primary =
                    spawn_gamescope_pipewire_after_static_preflight(config.clone(), session_clock)
                        .await?;
                super::select_gamescope_startup_source(config.clone(), session_clock, primary).await
            }
        }?;
        let EncodedSource {
            frames,
            current_gop,
            task,
            pointer_surface_dimensions,
            encoder_control,
        } = source;
        let video_source = SourceTaskGuard::new(task);

        let mut generation_secret = [0_u8; 32];
        getrandom::fill(&mut generation_secret)
            .context("generating shared media authentication key")?;
        let signing_key = MediaGenerationSigningKey::from_bytes(&generation_secret);
        generation_secret.fill(0);
        let issued_at_unix = unix_timestamp_now()?;
        let certificate = signing_key.certify(
            *iroh::SecretKey::from_bytes(&host_secret)
                .public()
                .as_bytes(),
            &host_secret,
            generation_id,
            issued_at_unix,
            issued_at_unix.saturating_add(60 * 60),
        )?;

        let mut broadcast = Broadcast::new().produce();
        let video_track = broadcast
            .create_track(Track {
                name: MOQ_VIDEO_H264_TRACK.to_owned(),
                priority: MOQ_VIDEO_TRACK_PRIORITY,
            })
            .context("creating generation H.264 track")?;
        let audio_enabled = config.audio.is_some();
        let audio_track = if audio_enabled {
            Some(
                broadcast
                    .create_track(Track {
                        name: MOQ_AUDIO_OPUS_TRACK.to_owned(),
                        priority: MOQ_AUDIO_TRACK_PRIORITY,
                    })
                    .context("creating generation Opus track")?,
            )
        } else {
            None
        };
        let catalog =
            publish_goq_catalog_v2(&mut broadcast, generation_id, &certificate, audio_enabled)?;
        let broadcast_consumer = broadcast.consume();
        let broadcast_name = media_generation_moq_broadcast_name(generation_id)?;
        let telemetry = Arc::new(MediaV3Telemetry::default());
        let (keyframe_requests, keyframes) = watch::channel(None);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let video_publisher = tokio::spawn(run_generation_video_publisher(
            config.clone(),
            current_gop,
            keyframes,
            video_track,
            generation_id,
            signing_key.clone(),
            encoder_control.clone(),
            Arc::clone(&telemetry),
            shutdown_rx.clone(),
        ));

        let (audio_source, audio_publisher) = if let Some(audio_track) = audio_track {
            let (audio_packets, task) =
                match spawn_pipewire_audio(config.clone(), session_clock).await {
                    Ok(source) => source,
                    Err(error) => {
                        video_publisher.abort();
                        video_source.abort_and_wait().await;
                        return Err(error).context("starting generation Opus source");
                    }
                };
            (
                Some(SourceTaskGuard::new(task)),
                Some(tokio::spawn(run_generation_audio_publisher(
                    audio_packets,
                    audio_track,
                    generation_id,
                    signing_key,
                    shutdown_rx,
                ))),
            )
        } else {
            (None, None)
        };

        let shared = Arc::new(GenerationShared {
            generation_id,
            broadcast_name,
            broadcast: Mutex::new(Some(broadcast_consumer)),
            certificate,
            session_clock,
            pointer_surface_dimensions,
            encoder_control,
            telemetry,
            audio_enabled,
            keyframe_requests,
        });
        info!(
            generation_id,
            video_sources = 1,
            video_encoders = 1,
            audio_sources = usize::from(audio_enabled),
            audio_encoders = usize::from(audio_enabled),
            publishers = 1,
            "shared media generation started"
        );
        Ok(Self {
            shared,
            shutdown,
            video_publisher,
            audio_publisher,
            video_source,
            audio_source,
            _frame_receiver: frames,
            catalog: Some(catalog),
            broadcast: Some(broadcast),
        })
    }

    async fn stop(mut self) {
        let generation_id = self.shared.generation_id;
        if let Ok(mut broadcast) = self.shared.broadcast.lock() {
            *broadcast = None;
        }
        self.shutdown.send_replace(true);
        wait_or_abort_task(&mut self.video_publisher, "video publisher").await;
        if let Some(mut audio_publisher) = self.audio_publisher.take() {
            wait_or_abort_task(&mut audio_publisher, "audio publisher").await;
        }
        if let Some(catalog) = self.catalog.take()
            && let Err(error) = catalog.finish()
        {
            warn!(generation_id, %error, "finishing shared media catalog failed");
        }
        if let Some(mut broadcast) = self.broadcast.take() {
            let _ = broadcast.abort(MoqError::Cancel);
        }
        if let Some(audio_source) = self.audio_source.take() {
            audio_source.abort_and_wait().await;
        }
        self.video_source
            .wait_or_abort(super::SOURCE_REAP_GRACE_TIMEOUT)
            .await;
        info!(
            generation_id,
            "shared media generation stopped with all sources cleaned up"
        );
    }
}

async fn wait_or_abort_task(task: &mut tokio::task::JoinHandle<Result<()>>, name: &'static str) {
    match tokio::time::timeout(GENERATION_STOP_TIMEOUT, &mut *task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => warn!(%error, task = name, "media generation task ended with error"),
        Ok(Err(error)) => warn!(%error, task = name, "media generation task failed"),
        Err(_) => {
            debug!(task = name, "aborting stalled media generation task");
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_is_explicit_and_bounded() {
        let state = GenerationState::default();
        assert_eq!(state.lifecycle, MediaGenerationLifecycle::Idle);
        assert_eq!(state.leases, 0);
        assert!(state.active.is_none());
    }

    #[tokio::test]
    async fn manager_begins_idle_without_starting_capture() {
        let manager = MediaGenerationManager::new(super::super::moq_test_config(), [7; 32]);
        assert_eq!(manager.lifecycle().await, MediaGenerationLifecycle::Idle);
        assert_eq!(manager.starts.load(Ordering::Relaxed), 0);
        assert_eq!(manager.stops.load(Ordering::Relaxed), 0);
    }
}
