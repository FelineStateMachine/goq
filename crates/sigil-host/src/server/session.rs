use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::Connection;
use moq_net::BroadcastConsumer;
use sigil_protocol::{InvitationGrants, KeyframeRequestReasonV3};
use tracing::{debug, warn};

use super::{ENCODER_CONTROL_COMMIT_TIMEOUT, VideoDimensions};
use crate::clock::SessionClock;
use crate::source::EncoderControl;

const MAX_PENDING_HANDSHAKES: usize = 4;

#[derive(Debug)]
pub struct SessionRegistry {
    active: Mutex<Option<ActiveSession>>,
    pending_moq: Mutex<Option<PendingMoqAttachment>>,
    next_session_id: AtomicU64,
    pub(super) session_changed: tokio::sync::Notify,
    pub(super) pending_handshakes: tokio::sync::Semaphore,
}

struct PendingMoqAttachment {
    remote: EndpointId,
    session_id: u64,
    broadcast_name: String,
    broadcast: BroadcastConsumer,
    attached: tokio::sync::oneshot::Sender<()>,
    closed: tokio::sync::oneshot::Sender<()>,
    telemetry: Arc<MediaV3Telemetry>,
}

impl std::fmt::Debug for PendingMoqAttachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingMoqAttachment")
            .field("remote", &self.remote)
            .field("session_id", &self.session_id)
            .field("broadcast_name", &self.broadcast_name)
            .finish_non_exhaustive()
    }
}

pub(super) struct ClaimedMoqAttachment {
    pub(super) session_id: u64,
    pub(super) broadcast_name: String,
    pub(super) broadcast: BroadcastConsumer,
    pub(super) attached: tokio::sync::oneshot::Sender<()>,
    pub(super) closed: tokio::sync::oneshot::Sender<()>,
    pub(super) telemetry: Arc<MediaV3Telemetry>,
}

pub(super) struct MoqAttachmentWait {
    pub(super) attached: tokio::sync::oneshot::Receiver<()>,
    pub(super) closed: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Clone, Debug)]
struct ActiveSession {
    remote: EndpointId,
    session_id: u64,
    nonce: [u8; 16],
    session_clock: SessionClock,
    grants: InvitationGrants,
    media_active: bool,
    input_claimed: bool,
    audio_claimed: bool,
    feedback_claimed: bool,
    media_v3_telemetry: Arc<MediaV3Telemetry>,
    encoder_control: Option<EncoderControl>,
}

#[derive(Debug, Default)]
pub(super) struct MediaV3Telemetry {
    pub(super) scheduler_cancellations: AtomicU64,
    pub(super) send_failures: AtomicU64,
    selected_path_rtt_micros: AtomicU64,
    selected_path_lost_packets: AtomicU64,
    selected_path_congestion_events: AtomicU64,
    keyframe_control_requests: AtomicU64,
    encoder_force_requests: AtomicU64,
    encoder_force_acknowledgements: AtomicU64,
    encoder_force_failures: AtomicU64,
    last_encoder_force_ack_micros: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MediaV3TelemetrySnapshot {
    pub(super) scheduler_cancellations: u64,
    pub(super) send_failures: u64,
    pub(super) selected_path_rtt_micros: u64,
    pub(super) selected_path_lost_packets: u64,
    pub(super) selected_path_congestion_events: u64,
}

#[derive(Clone, Debug)]
pub(super) struct AdaptiveEncoderProposal {
    pub(super) control: EncoderControl,
    pub(super) target_kbps: u32,
    pub(super) bitrate_revision: u64,
    pub(super) force_keyframe_revision: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct ResolutionEncoderProposal {
    pub(super) control: EncoderControl,
    pub(super) target: VideoDimensions,
    pub(super) revision: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ForcedIdrDisposition {
    JoinReplay,
    Unavailable,
    Requested { revision: u64 },
    Coalesced { revision: u64 },
    Failed { error: String },
}

pub(super) struct ForcedIdrAcknowledgement {
    requested_revision: u64,
    elapsed: Duration,
    result: Result<crate::source::EncoderControlStatus>,
}

pub(super) struct ForcedIdrCoordinator {
    control: Option<EncoderControl>,
    pub(super) pending_revision: Option<u64>,
    pub(super) acknowledgements: tokio::task::JoinSet<ForcedIdrAcknowledgement>,
    telemetry: Arc<MediaV3Telemetry>,
}

impl ForcedIdrCoordinator {
    pub(super) fn new(control: Option<EncoderControl>, telemetry: Arc<MediaV3Telemetry>) -> Self {
        Self {
            control,
            pending_revision: None,
            acknowledgements: tokio::task::JoinSet::new(),
            telemetry,
        }
    }

    pub(super) fn request(&mut self, reason: KeyframeRequestReasonV3) -> ForcedIdrDisposition {
        self.telemetry
            .keyframe_control_requests
            .fetch_add(1, Ordering::Relaxed);
        if reason == KeyframeRequestReasonV3::Join {
            return ForcedIdrDisposition::JoinReplay;
        }
        if let Some(revision) = self.pending_revision {
            return ForcedIdrDisposition::Coalesced { revision };
        }
        let Some(control) = self.control.clone() else {
            return ForcedIdrDisposition::Unavailable;
        };
        let revision = match control.request_force_keyframe() {
            Ok(revision) => revision,
            Err(error) => {
                self.telemetry
                    .encoder_force_failures
                    .fetch_add(1, Ordering::Relaxed);
                return ForcedIdrDisposition::Failed {
                    error: error.to_string(),
                };
            }
        };
        self.pending_revision = Some(revision);
        self.telemetry
            .encoder_force_requests
            .fetch_add(1, Ordering::Relaxed);
        self.acknowledgements.spawn(async move {
            let started_at = Instant::now();
            let result = control
                .wait_for_recovery_keyframe_acknowledged(revision, ENCODER_CONTROL_COMMIT_TIMEOUT)
                .await;
            ForcedIdrAcknowledgement {
                requested_revision: revision,
                elapsed: started_at.elapsed(),
                result,
            }
        });
        ForcedIdrDisposition::Requested { revision }
    }

    pub(super) fn complete(
        &mut self,
        result: Option<Result<ForcedIdrAcknowledgement, tokio::task::JoinError>>,
        remote: EndpointId,
        transport: &'static str,
    ) {
        let pending_revision = self.pending_revision.take();
        match result {
            Some(Ok(acknowledgement)) => match acknowledgement.result {
                Ok(status) => {
                    let elapsed_micros =
                        u64::try_from(acknowledgement.elapsed.as_micros()).unwrap_or(u64::MAX);
                    self.telemetry
                        .encoder_force_acknowledgements
                        .fetch_add(1, Ordering::Relaxed);
                    self.telemetry
                        .last_encoder_force_ack_micros
                        .store(elapsed_micros, Ordering::Relaxed);
                    debug!(
                        %remote,
                        transport,
                        requested_revision = acknowledgement.requested_revision,
                        acknowledged_revision = ?status.acknowledged_force_keyframe_revision,
                        elapsed_micros,
                        "forced-IDR recovery acknowledged"
                    );
                }
                Err(error) => {
                    self.telemetry
                        .encoder_force_failures
                        .fetch_add(1, Ordering::Relaxed);
                    warn!(
                        %remote,
                        transport,
                        requested_revision = acknowledgement.requested_revision,
                        %error,
                        "forced-IDR recovery was not acknowledged; retaining natural-IDR fallback"
                    );
                }
            },
            Some(Err(error)) => {
                self.telemetry
                    .encoder_force_failures
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    %remote,
                    transport,
                    ?pending_revision,
                    %error,
                    "forced-IDR acknowledgement task failed; retaining natural-IDR fallback"
                );
            }
            None => {
                self.telemetry
                    .encoder_force_failures
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    %remote,
                    transport,
                    ?pending_revision,
                    "forced-IDR acknowledgement task ended without a result"
                );
            }
        }
    }

    pub(super) async fn abort_and_drain(&mut self, remote: EndpointId, transport: &'static str) {
        self.pending_revision = None;
        self.acknowledgements.abort_all();
        while self.acknowledgements.join_next().await.is_some() {}
        debug!(
            %remote,
            transport,
            keyframe_control_requests = self
                .telemetry
                .keyframe_control_requests
                .load(Ordering::Relaxed),
            encoder_force_requests = self
                .telemetry
                .encoder_force_requests
                .load(Ordering::Relaxed),
            encoder_force_acknowledgements = self
                .telemetry
                .encoder_force_acknowledgements
                .load(Ordering::Relaxed),
            encoder_force_failures = self
                .telemetry
                .encoder_force_failures
                .load(Ordering::Relaxed),
            last_encoder_force_ack_micros = self
                .telemetry
                .last_encoder_force_ack_micros
                .load(Ordering::Relaxed),
            "forced-IDR recovery session summary"
        );
    }
}

impl MediaV3Telemetry {
    pub(super) fn snapshot(&self) -> MediaV3TelemetrySnapshot {
        MediaV3TelemetrySnapshot {
            scheduler_cancellations: self.scheduler_cancellations.load(Ordering::Relaxed),
            send_failures: self.send_failures.load(Ordering::Relaxed),
            selected_path_rtt_micros: self.selected_path_rtt_micros.load(Ordering::Relaxed),
            selected_path_lost_packets: self.selected_path_lost_packets.load(Ordering::Relaxed),
            selected_path_congestion_events: self
                .selected_path_congestion_events
                .load(Ordering::Relaxed),
        }
    }

    pub(super) fn record_selected_path(&self, connection: &Connection) {
        let paths = connection.paths();
        let Some(path) = paths.iter().find(|path| path.is_selected()) else {
            return;
        };
        let stats = path.stats();
        self.selected_path_rtt_micros.store(
            u64::try_from(stats.rtt.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.selected_path_lost_packets
            .store(stats.lost_packets, Ordering::Relaxed);
        self.selected_path_congestion_events
            .store(stats.congestion_events, Ordering::Relaxed);
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            pending_moq: Mutex::new(None),
            next_session_id: AtomicU64::new(0),
            session_changed: tokio::sync::Notify::new(),
            pending_handshakes: tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES),
        }
    }
}

impl SessionRegistry {
    pub fn has_session(&self) -> bool {
        self.active
            .lock()
            .expect("session registry poisoned")
            .is_some()
    }

    pub(super) fn claim(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
        grants: InvitationGrants,
    ) -> Result<SessionLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(current) = active.as_ref() {
            bail!("host already has active client {}", current.remote);
        }
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session_clock = SessionClock::start();
        let media_v3_telemetry = Arc::new(MediaV3Telemetry::default());
        *active = Some(ActiveSession {
            remote,
            session_id,
            nonce,
            session_clock,
            grants,
            media_active: true,
            input_claimed: false,
            audio_claimed: false,
            feedback_claimed: false,
            media_v3_telemetry: Arc::clone(&media_v3_telemetry),
            encoder_control: None,
        });
        Ok(SessionLease {
            registry: Arc::clone(self),
            remote,
            session_id,
            session_clock,
            media_v3_telemetry,
        })
    }

    pub(super) fn claim_input(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
    ) -> Result<InputLease> {
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
            grants: session.grants,
        })
    }

    pub(super) fn install_encoder_control(
        &self,
        remote: EndpointId,
        session_id: u64,
        encoder_control: Option<EncoderControl>,
    ) -> Result<()> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .context("encoder control does not match the active media session")?;
        ensure!(
            session.encoder_control.is_none(),
            "active media session already has encoder control"
        );
        session.encoder_control = encoder_control;
        Ok(())
    }

    pub(super) fn propose_adaptive_encoder_update(
        &self,
        remote: EndpointId,
        session_id: u64,
        target_kbps: u32,
        force_keyframe: bool,
    ) -> Result<Option<AdaptiveEncoderProposal>> {
        let active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .context("adaptive encoder update does not match the active media session")?;
        let Some(control) = session.encoder_control.clone() else {
            return Ok(None);
        };
        let bitrate_revision = control.request_bitrate_kbps(target_kbps)?;
        let force_keyframe_revision = force_keyframe
            .then(|| control.request_force_keyframe())
            .transpose()?;
        Ok(Some(AdaptiveEncoderProposal {
            control,
            target_kbps,
            bitrate_revision,
            force_keyframe_revision,
        }))
    }

    pub(super) fn propose_resolution_update(
        &self,
        remote: EndpointId,
        session_id: u64,
        target: VideoDimensions,
    ) -> Result<Option<ResolutionEncoderProposal>> {
        let active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .context("resolution update does not match the active media session")?;
        let Some(control) = session.encoder_control.clone() else {
            return Ok(None);
        };
        let revision = control.request_resolution(target.width, target.height)?;
        Ok(Some(ResolutionEncoderProposal {
            control,
            target,
            revision,
        }))
    }

    pub(super) fn claim_feedback(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
    ) -> Result<FeedbackLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("feedback connection does not match the active media session")?;
        ensure!(
            session.grants.contains(InvitationGrants::VIEW),
            "active Portal session lacks feedback view permission"
        );
        ensure!(
            !session.feedback_claimed,
            "active client already has a feedback connection"
        );
        session.feedback_claimed = true;
        Ok(FeedbackLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
            telemetry: Arc::clone(&session.media_v3_telemetry),
            encoder_control: session.encoder_control.clone(),
        })
    }

    pub(super) fn claim_audio(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
    ) -> Result<AudioLease> {
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
            grants: session.grants,
        })
    }

    pub(super) fn expect_moq(
        &self,
        remote: EndpointId,
        session_id: u64,
        broadcast_name: String,
        broadcast: BroadcastConsumer,
    ) -> Result<MoqAttachmentWait> {
        let active = self.active.lock().expect("session registry poisoned");
        let telemetry = active
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .map(|session| Arc::clone(&session.media_v3_telemetry))
            .context("MoQ expectation does not match the active control session")?;
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        ensure!(
            pending.is_none(),
            "active control session already expects MoQ"
        );
        let (attached, attached_rx) = tokio::sync::oneshot::channel();
        let (closed, closed_rx) = tokio::sync::oneshot::channel();
        *pending = Some(PendingMoqAttachment {
            remote,
            session_id,
            broadcast_name,
            broadcast,
            attached,
            closed,
            telemetry,
        });
        Ok(MoqAttachmentWait {
            attached: attached_rx,
            closed: closed_rx,
        })
    }

    pub(super) fn claim_moq(&self, remote: EndpointId) -> Result<ClaimedMoqAttachment> {
        let active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_ref()
            .filter(|session| session.media_active && session.remote == remote)
            .context("MoQ connection does not match the active control session")?;
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        let attachment = pending
            .as_ref()
            .filter(|attachment| {
                attachment.remote == remote && attachment.session_id == session.session_id
            })
            .context("active control session is not expecting a MoQ connection")?;
        debug_assert_eq!(attachment.remote, session.remote);
        let attachment = pending
            .take()
            .expect("validated pending MoQ attachment disappeared");
        Ok(ClaimedMoqAttachment {
            session_id: attachment.session_id,
            broadcast_name: attachment.broadcast_name,
            broadcast: attachment.broadcast,
            attached: attachment.attached,
            closed: attachment.closed,
            telemetry: attachment.telemetry,
        })
    }

    fn release(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
            if pending.as_ref().is_some_and(|attachment| {
                attachment.remote == remote && attachment.session_id == session_id
            }) {
                *pending = None;
            }
            // Keep the registry occupied until the input handler has observed
            // media shutdown and released all held uinput transitions. This
            // prevents a reconnect from sharing the device with a draining
            // predecessor session.
            session.media_active = false;
            session.encoder_control = None;
            if !session.input_claimed && !session.audio_claimed && !session.feedback_claimed {
                *active = None;
            }
            drop(active);
            self.session_changed.notify_waiters();
        }
    }

    pub(super) fn is_active(&self, remote: EndpointId, session_id: u64) -> bool {
        self.active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
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
            if !session.media_active && !session.audio_claimed && !session.feedback_claimed {
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
            if !session.media_active && !session.input_claimed && !session.feedback_claimed {
                *active = None;
            }
        }
    }

    fn release_feedback(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.feedback_claimed = false;
            if !session.media_active && !session.input_claimed && !session.audio_claimed {
                *active = None;
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SessionLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) session_clock: SessionClock,
    pub(super) media_v3_telemetry: Arc<MediaV3Telemetry>,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.registry.release(self.remote, self.session_id);
    }
}

#[derive(Debug)]
pub(super) struct InputLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) grants: InvitationGrants,
}

#[derive(Debug)]
pub(super) struct AudioLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) session_clock: SessionClock,
    pub(super) grants: InvitationGrants,
}

#[derive(Debug)]
pub(super) struct FeedbackLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) telemetry: Arc<MediaV3Telemetry>,
    pub(super) encoder_control: Option<EncoderControl>,
}

#[derive(Debug)]
pub(super) struct SourceTaskGuard(Option<tokio::task::JoinHandle<Result<()>>>);

impl SourceTaskGuard {
    pub(super) fn new(task: tokio::task::JoinHandle<Result<()>>) -> Self {
        Self(Some(task))
    }

    pub(super) async fn abort_and_wait(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }

    pub(super) async fn wait_or_abort(mut self, grace_timeout: Duration) {
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

impl Drop for FeedbackLease {
    fn drop(&mut self) {
        self.registry.release_feedback(self.remote, self.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::endpoint;
    use super::*;

    use moq_net::{Broadcast, BroadcastProducer};
    use sigil_protocol::media_moq_broadcast_name;

    #[tokio::test]
    async fn forced_idr_recovery_is_one_slot_join_safe_and_rearms_after_ack() {
        let telemetry = Arc::new(MediaV3Telemetry::default());
        let harness = crate::source::EncoderControlTestHarness::new();
        let mut coordinator =
            ForcedIdrCoordinator::new(Some(harness.control.clone()), Arc::clone(&telemetry));

        assert_eq!(
            coordinator.request(KeyframeRequestReasonV3::Join),
            ForcedIdrDisposition::JoinReplay
        );
        assert_eq!(harness.requested_force_keyframe_revision(), None);

        let requested_revision = match coordinator.request(KeyframeRequestReasonV3::DecoderReset) {
            ForcedIdrDisposition::Requested { revision } => revision,
            disposition => panic!("unexpected forced-IDR disposition: {disposition:?}"),
        };
        assert_eq!(
            harness.requested_force_keyframe_revision(),
            Some(requested_revision)
        );
        assert_eq!(
            coordinator.request(KeyframeRequestReasonV3::TransportGap),
            ForcedIdrDisposition::Coalesced {
                revision: requested_revision
            }
        );
        assert_eq!(telemetry.encoder_force_requests.load(Ordering::Relaxed), 1);

        let newer_revision = harness.control.request_force_keyframe().unwrap();
        harness.status.send_modify(|status| {
            status.requested_force_keyframe_revision = Some(newer_revision);
            status.acknowledged_force_keyframe_revision = Some(newer_revision);
        });
        let acknowledgement = coordinator.acknowledgements.join_next().await;
        coordinator.complete(acknowledgement, endpoint(1), "test");
        assert_eq!(coordinator.pending_revision, None);
        assert_eq!(
            telemetry
                .encoder_force_acknowledgements
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(telemetry.encoder_force_failures.load(Ordering::Relaxed), 0);

        assert!(matches!(
            coordinator.request(KeyframeRequestReasonV3::DecoderReset),
            ForcedIdrDisposition::Requested { revision } if revision > newer_revision
        ));
        coordinator.abort_and_drain(endpoint(1), "test").await;

        let fallback_telemetry = Arc::new(MediaV3Telemetry::default());
        let mut fallback = ForcedIdrCoordinator::new(None, fallback_telemetry);
        assert_eq!(
            fallback.request(KeyframeRequestReasonV3::DecoderReset),
            ForcedIdrDisposition::Unavailable
        );
        assert!(fallback.acknowledgements.is_empty());
    }

    #[test]
    fn only_one_remote_can_hold_session() {
        let sessions = Arc::new(SessionRegistry::default());
        assert!(!sessions.has_session());
        let nonce = [7; 16];
        let first = sessions
            .claim(endpoint(1), nonce, InvitationGrants::ALL)
            .unwrap();
        assert!(sessions.has_session());
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        assert!(sessions.claim_input(endpoint(1), [8; 16]).is_err());
        let input = sessions.claim_input(endpoint(1), nonce).unwrap();
        assert_eq!(input.session_id, first.session_id);
        assert!(sessions.claim_input(endpoint(1), nonce).is_err());
        let audio = sessions.claim_audio(endpoint(1), nonce).unwrap();
        assert!(sessions.claim_audio(endpoint(1), nonce).is_err());
        drop(input);
        let draining_input = sessions.claim_input(endpoint(1), nonce).unwrap();
        drop(first);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        drop(draining_input);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        drop(audio);
        assert!(!sessions.has_session());
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_ok()
        );
    }

    fn test_moq_broadcast() -> (BroadcastProducer, BroadcastConsumer) {
        let producer = Broadcast::new().produce();
        let consumer = producer.consume();
        (producer, consumer)
    }

    #[test]
    fn moq_attachment_requires_exact_active_control_remote_and_is_single_use() {
        let sessions = Arc::new(SessionRegistry::default());
        assert!(sessions.claim_moq(endpoint(1)).is_err());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();

        // A wrong peer cannot consume the exact pending attachment.
        assert!(sessions.claim_moq(endpoint(2)).is_err());
        let attachment = sessions.claim_moq(endpoint(1)).unwrap();
        assert_eq!(attachment.session_id, lease.session_id);
        assert_eq!(
            attachment.broadcast_name,
            media_moq_broadcast_name(lease.session_id).unwrap()
        );
        // The pending token was atomically consumed before the MoQ handshake.
        assert!(sessions.claim_moq(endpoint(1)).is_err());
    }

    #[test]
    fn competing_moq_connections_cannot_both_claim_one_control_attachment() {
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let contenders = (0..2)
            .map(|_| {
                let sessions = Arc::clone(&sessions);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    sessions.claim_moq(endpoint(1)).is_ok()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let claimed = contenders
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(claimed, 1);
    }

    #[tokio::test]
    async fn releasing_control_clears_an_unclaimed_moq_attachment() {
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();
        drop(lease);
        assert!(sessions.claim_moq(endpoint(1)).is_err());
        assert!(wait.attached.await.is_err());
        assert!(wait.closed.await.is_err());
    }

    #[test]
    fn feedback_attaches_only_to_exact_active_view_session() {
        let sessions = Arc::new(SessionRegistry::default());
        let nonce = [9; 16];
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());

        let no_view = sessions
            .claim(endpoint(1), nonce, InvitationGrants::GAMEPAD)
            .unwrap();
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());
        drop(no_view);

        let media = sessions
            .claim(endpoint(1), nonce, InvitationGrants::VIEW)
            .unwrap();
        assert!(sessions.claim_feedback(endpoint(2), nonce).is_err());
        assert!(sessions.claim_feedback(endpoint(1), [8; 16]).is_err());
        let feedback = sessions.claim_feedback(endpoint(1), nonce).unwrap();
        assert_eq!(feedback.session_id, media.session_id);
        assert!(feedback.encoder_control.is_none());
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());

        drop(media);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::VIEW)
                .is_err(),
            "feedback teardown must keep the draining session isolated"
        );
        drop(feedback);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::VIEW)
                .is_ok()
        );
    }

    #[test]
    fn adaptive_encoder_proposals_are_bound_to_the_exact_active_generation() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let media = sessions
            .claim(remote, [3; 16], InvitationGrants::VIEW)
            .unwrap();
        let harness = crate::source::EncoderControlTestHarness::new();

        assert!(
            sessions
                .install_encoder_control(
                    endpoint(2),
                    media.session_id,
                    Some(harness.control.clone())
                )
                .is_err()
        );
        sessions
            .install_encoder_control(remote, media.session_id, Some(harness.control.clone()))
            .unwrap();
        let feedback = sessions.claim_feedback(remote, [3; 16]).unwrap();
        assert!(feedback.encoder_control.is_some());
        let proposal = sessions
            .propose_adaptive_encoder_update(remote, media.session_id, 8_000, true)
            .unwrap()
            .unwrap();
        assert_eq!(proposal.target_kbps, 8_000);
        assert!(proposal.force_keyframe_revision > Some(proposal.bitrate_revision));

        let old_session_id = media.session_id;
        drop(media);
        assert!(
            sessions
                .propose_adaptive_encoder_update(remote, old_session_id, 7_000, false)
                .is_err(),
            "a draining generation must not issue another encoder proposal"
        );
        drop(feedback);
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
    fn session_substreams_inherit_the_exact_enrollment_grant() {
        let sessions = Arc::new(SessionRegistry::default());
        let grants = InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD);
        let media = sessions.claim(endpoint(1), [3; 16], grants).unwrap();
        let input = sessions.claim_input(endpoint(1), [3; 16]).unwrap();
        let audio = sessions.claim_audio(endpoint(1), [3; 16]).unwrap();
        assert_eq!(input.grants, grants);
        assert_eq!(audio.grants, grants);
        drop(input);
        drop(audio);
        drop(media);
    }

    #[test]
    fn audio_claim_requires_the_active_remote_and_nonce() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions
            .claim(endpoint(1), [9; 16], InvitationGrants::ALL)
            .unwrap();
        assert!(sessions.claim_audio(endpoint(2), [9; 16]).is_err());
        assert!(sessions.claim_audio(endpoint(1), [8; 16]).is_err());
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        drop(media);
        assert!(
            sessions
                .claim(endpoint(2), [0; 16], InvitationGrants::ALL)
                .is_err()
        );
        drop(audio);
        assert!(
            sessions
                .claim(endpoint(2), [0; 16], InvitationGrants::ALL)
                .is_ok()
        );
    }

    #[test]
    fn media_and_audio_leases_share_one_session_clock() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions
            .claim(endpoint(1), [9; 16], InvitationGrants::ALL)
            .unwrap();
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        assert_eq!(media.session_clock, audio.session_clock);
    }
}
