use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::Connection;
use moq_net::BroadcastConsumer;
use sigil_protocol::{
    ControllerSlot, FocusCommandActionV2, FocusCommandV2, FocusStateV2, FocusTransitionReasonV2,
    InvitationGrants, KeyframeRequestReasonV3, MediaGenerationDescriptorV2, SessionSnapshotV2,
    SignedSubscriptionCapability, SubscriptionTracks, ViewerPresenceId, ViewerPresenceV2,
    media_generation_moq_broadcast_name, media_moq_broadcast_name,
};
use tracing::{debug, info, warn};

use super::focus::{FocusArbiter, FocusCandidate, FocusNeutralization};
use super::{ENCODER_CONTROL_COMMIT_TIMEOUT, VideoDimensions};
use crate::authorization::{AuthorizationMutation, AuthorizedViewer};
use crate::clock::SessionClock;
use crate::source::EncoderControl;

const MAX_PENDING_HANDSHAKES: usize = 4;
// The hard eight-viewer ceiling gives each admitted viewer four independent
// handshake slots (control, MoQ, input, and feedback). The semaphore therefore
// grows linearly with configured concurrency and never with connection churn.
const MAX_TRACKED_ADMISSION_PEERS: usize = crate::config::MAX_VIEWERS as usize * 4;
const ADMISSION_RATE_WINDOW: Duration = Duration::from_secs(10);
const MAX_ADMISSIONS_PER_WINDOW: usize = 12;
const ATTACHMENT_RATE_WINDOW: Duration = Duration::from_secs(5);
const MAX_ATTACHMENTS_PER_WINDOW: usize = 4;
const KEYFRAME_RATE_WINDOW: Duration = Duration::from_secs(2);
const MAX_KEYFRAME_REQUESTS_PER_WINDOW: usize = 8;
const FEEDBACK_CLAIM_RATE_WINDOW: Duration = Duration::from_secs(10);
const MAX_FEEDBACK_CLAIMS_PER_WINDOW: usize = 4;

#[derive(Debug)]
pub struct SessionRegistry {
    active: Mutex<Option<ActiveSession>>,
    pending_moq: Mutex<HashMap<(EndpointId, u64), PendingMoqAttachment>>,
    max_viewers: usize,
    next_session_id: AtomicU64,
    authorization_committed_revision: AtomicU64,
    admission_rates: Mutex<HashMap<EndpointId, RateWindow>>,
    v2_state: Mutex<V2SessionState>,
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
    subscription_capability: Option<SignedSubscriptionCapability>,
    expected_host: Option<[u8; 32]>,
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
    media_generation_id: u64,
    media_broadcast_name: String,
    grants: InvitationGrants,
    viewer_handle: Option<String>,
    authorization_revision: u64,
    authorization_committed_revision: u64,
    media_active: bool,
    input_claimed: bool,
    audio_claimed: bool,
    feedback_claimed: bool,
    media_v3_telemetry: Arc<MediaV3Telemetry>,
    encoder_control: Option<EncoderControl>,
    mode: SessionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMode {
    LegacyExclusive,
    V2Single,
}

#[derive(Debug)]
struct V2SessionState {
    viewers: HashMap<EndpointId, V2ViewerSession>,
    revision: u64,
    focus: FocusArbiter,
    media: Option<MediaGenerationDescriptorV2>,
    live_control_leases: usize,
}

impl V2SessionState {
    fn new(configured_owner: Option<ViewerPresenceId>) -> Self {
        Self {
            viewers: HashMap::new(),
            revision: 0,
            focus: FocusArbiter::new(configured_owner),
            media: None,
            live_control_leases: 0,
        }
    }
}

#[derive(Debug)]
struct V2ViewerSession {
    session: ActiveSession,
    presence_id: ViewerPresenceId,
    authorization_neutralizing: bool,
    snapshots: tokio::sync::watch::Sender<Option<SessionSnapshotV2>>,
    rates: ViewerRateLimits,
}

#[derive(Debug, Default)]
struct ViewerRateLimits {
    attachments: RateWindow,
    keyframes: RateWindow,
    feedback_claims: RateWindow,
}

#[derive(Debug, Default)]
struct RateWindow {
    events: VecDeque<Instant>,
    last_seen: Option<Instant>,
}

impl RateWindow {
    fn check(
        &mut self,
        now: Instant,
        window: Duration,
        maximum: usize,
        label: &'static str,
    ) -> Result<()> {
        while self
            .events
            .front()
            .is_some_and(|event| now.saturating_duration_since(*event) >= window)
        {
            self.events.pop_front();
        }
        self.last_seen = Some(now);
        ensure!(self.events.len() < maximum, "{label} rate limit exceeded");
        self.events.push_back(now);
        Ok(())
    }

    fn expired(&self, now: Instant, window: Duration) -> bool {
        self.last_seen
            .is_none_or(|seen| now.saturating_duration_since(seen) >= window)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionRuntimeStatus {
    pub(crate) mode: &'static str,
    pub(crate) active_viewers: usize,
    pub(crate) media_generation_id: Option<u64>,
    pub(crate) roster_revision: Option<u64>,
    pub(crate) focus_occupied: Option<bool>,
    pub(crate) configured_capacity: usize,
    pub(crate) authorization_revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct FocusCommandEffect {
    pub(crate) snapshot: SessionSnapshotV2,
    pub(crate) neutralization: Option<FocusNeutralization>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorizationSessionEffect {
    pub disconnected: bool,
    pub neutralize_input: bool,
    pub focus_transition_id: Option<u64>,
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
        Self::new(crate::config::DEFAULT_MAX_VIEWERS)
    }
}

impl SessionRegistry {
    pub fn new(max_viewers: u8) -> Self {
        Self::new_with_focus_owner(max_viewers, None)
    }

    pub fn new_with_focus_owner(max_viewers: u8, focus_owner: Option<String>) -> Self {
        assert!(
            (1..=crate::config::MAX_VIEWERS).contains(&max_viewers),
            "validated max_viewers must be between 1 and {}",
            crate::config::MAX_VIEWERS
        );
        let focus_owner = focus_owner
            .map(ViewerPresenceId::new)
            .transpose()
            .expect("validated focus_owner must be an opaque viewer handle");
        Self {
            active: Mutex::new(None),
            pending_moq: Mutex::new(HashMap::with_capacity(crate::config::MAX_VIEWERS.into())),
            max_viewers: usize::from(max_viewers),
            next_session_id: AtomicU64::new(0),
            authorization_committed_revision: AtomicU64::new(1),
            admission_rates: Mutex::new(HashMap::with_capacity(MAX_TRACKED_ADMISSION_PEERS)),
            v2_state: Mutex::new(V2SessionState::new(focus_owner)),
            session_changed: tokio::sync::Notify::new(),
            pending_handshakes: tokio::sync::Semaphore::new(
                MAX_PENDING_HANDSHAKES * usize::from(max_viewers),
            ),
        }
    }

    #[allow(dead_code)]
    pub fn has_session(&self) -> bool {
        if self
            .active
            .lock()
            .expect("session registry poisoned")
            .is_some()
        {
            return true;
        }
        !self
            .v2_state
            .lock()
            .expect("v2 session state poisoned")
            .viewers
            .is_empty()
    }

    pub(crate) fn runtime_status(&self) -> SessionRuntimeStatus {
        if let Some(active) = self
            .active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .filter(|session| session.media_active)
        {
            return SessionRuntimeStatus {
                mode: "legacy_exclusive",
                active_viewers: 1,
                media_generation_id: Some(active.media_generation_id),
                roster_revision: None,
                focus_occupied: None,
                configured_capacity: self.max_viewers,
                authorization_revision: self
                    .authorization_committed_revision
                    .load(Ordering::SeqCst),
            };
        }
        let state = self.v2_state.lock().expect("v2 session state poisoned");
        SessionRuntimeStatus {
            mode: if state.viewers.is_empty() {
                "inactive"
            } else {
                "moq_multi_viewer"
            },
            active_viewers: state.viewers.len(),
            media_generation_id: state.media.as_ref().map(|media| media.generation_id),
            roster_revision: (!state.viewers.is_empty()).then_some(state.revision),
            focus_occupied: (!state.viewers.is_empty())
                .then_some(matches!(state.focus.state(), FocusStateV2::Held { .. })),
            configured_capacity: self.max_viewers,
            authorization_revision: self.authorization_committed_revision.load(Ordering::SeqCst),
        }
    }

    fn check_admission_rate(&self, remote: EndpointId, now: Instant) -> Result<()> {
        let mut rates = self
            .admission_rates
            .lock()
            .expect("viewer admission rate state poisoned");
        rates.retain(|_, rate| !rate.expired(now, ADMISSION_RATE_WINDOW));
        ensure!(
            rates.contains_key(&remote) || rates.len() < MAX_TRACKED_ADMISSION_PEERS,
            "viewer admission rate table is full"
        );
        rates.entry(remote).or_default().check(
            now,
            ADMISSION_RATE_WINDOW,
            MAX_ADMISSIONS_PER_WINDOW,
            "viewer reconnect",
        )
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
        ensure!(
            self.v2_state
                .lock()
                .expect("v2 session state poisoned")
                .live_control_leases
                == 0,
            "host already has an active multi-viewer generation"
        );
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session_clock = SessionClock::start();
        let media_v3_telemetry = Arc::new(MediaV3Telemetry::default());
        *active = Some(ActiveSession {
            remote,
            session_id,
            nonce,
            session_clock,
            media_generation_id: session_id,
            media_broadcast_name: media_moq_broadcast_name(session_id)?,
            grants,
            viewer_handle: None,
            authorization_revision: 1,
            authorization_committed_revision: 1,
            media_active: true,
            input_claimed: false,
            audio_claimed: false,
            feedback_claimed: false,
            media_v3_telemetry: Arc::clone(&media_v3_telemetry),
            encoder_control: None,
            mode: SessionMode::LegacyExclusive,
        });
        Ok(SessionLease {
            registry: Arc::clone(self),
            remote,
            session_id,
            session_clock,
            media_v3_telemetry,
        })
    }

    #[allow(dead_code)]
    pub(super) fn claim_v2(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
        grants: InvitationGrants,
    ) -> Result<V2SessionLease> {
        self.claim_v2_authorized(
            remote,
            nonce,
            AuthorizedViewer {
                handle: "viewer-0000000000000000".to_owned(),
                grants,
                authorization_revision: 1,
                committed_revision: 1,
            },
        )
    }

    pub(crate) fn claim_v2_authorized(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
        authorized: AuthorizedViewer,
    ) -> Result<V2SessionLease> {
        authorized.grants.validate()?;
        ensure!(
            authorized.grants.contains(InvitationGrants::VIEW),
            "viewer lacks view permission"
        );
        ensure!(
            authorized.authorization_revision != 0 && authorized.committed_revision != 0,
            "viewer authorization revision is invalid"
        );
        self.check_admission_rate(remote, Instant::now())?;
        self.authorization_committed_revision
            .fetch_max(authorized.committed_revision, Ordering::SeqCst);
        ensure!(
            self.authorization_committed_revision.load(Ordering::SeqCst)
                == authorized.committed_revision,
            "viewer admission used a stale authorization revision"
        );
        // Hold the legacy registry lock across the whole v2 admission. `claim`
        // takes `active` and then `v2_state`; releasing `active` here first
        // would let a legacy client and a v2 viewer both pass their mutual
        // exclusion check and be admitted together.
        // Hold the legacy registry lock across the whole v2 admission. `claim`
        // takes `active` and then `v2_state`; releasing `active` here first
        // would let a legacy client and a v2 viewer both pass their mutual
        // exclusion check and be admitted together.
        let active = self.active.lock().expect("session registry poisoned");
        ensure!(active.is_none(), "host already has an active legacy client");
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let replacing = state.viewers.contains_key(&remote);
        ensure!(
            replacing || state.viewers.len() < self.max_viewers,
            "multi-viewer capacity of {} is full",
            self.max_viewers
        );
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session_clock = SessionClock::start();
        let media_v3_telemetry = Arc::new(MediaV3Telemetry::default());
        let presence_id = ViewerPresenceId::new(authorized.handle.clone())?;
        ensure!(
            state
                .viewers
                .iter()
                .all(|(peer, viewer)| { *peer == remote || viewer.presence_id != presence_id }),
            "viewer presence id is already active for another peer"
        );
        let replaced = state.viewers.remove(&remote);
        let replaced_candidate = replaced.as_ref().map(|viewer| FocusCandidate {
            presence_id: viewer.presence_id.clone(),
            session_id: viewer.session.session_id,
        });
        let replacement_transition = replaced_candidate
            .as_ref()
            .map(|candidate| {
                state
                    .focus
                    .begin_invalidation(candidate, FocusTransitionReasonV2::Replaced)
            })
            .transpose()?
            .and_then(|mutation| mutation.neutralization);
        if let Some(candidate) = &replaced_candidate {
            state.focus.retire_candidate(candidate);
        }
        let media = state.media.clone().unwrap_or(MediaGenerationDescriptorV2 {
            generation_id: session_id,
            broadcast_name: media_generation_moq_broadcast_name(session_id)?,
        });
        let (snapshots, _receiver) = tokio::sync::watch::channel(None);
        let session = ActiveSession {
            remote,
            session_id,
            nonce,
            session_clock,
            media_generation_id: media.generation_id,
            media_broadcast_name: media.broadcast_name,
            grants: authorized.grants,
            viewer_handle: Some(authorized.handle.clone()),
            authorization_revision: authorized.authorization_revision,
            authorization_committed_revision: authorized.committed_revision,
            media_active: true,
            input_claimed: false,
            audio_claimed: false,
            feedback_claimed: false,
            media_v3_telemetry: Arc::clone(&media_v3_telemetry),
            encoder_control: None,
            mode: SessionMode::V2Single,
        };
        state.viewers.insert(
            remote,
            V2ViewerSession {
                session,
                presence_id,
                authorization_neutralizing: false,
                snapshots,
                rates: ViewerRateLimits::default(),
            },
        );
        state.live_control_leases = state
            .live_control_leases
            .checked_add(1)
            .context("v2 control lease count overflowed")?;
        advance_v2_revision(&mut state)?;
        publish_v2_snapshots(&state)?;
        let snapshot = v2_snapshot_for(&state, remote)?;
        let active_viewers = state.viewers.len();
        let viewer_handle = authorized.handle.clone();
        let replaced_snapshots = replaced.map(|viewer| viewer.snapshots);
        let lease = V2SessionLease {
            registry: Arc::clone(self),
            remote,
            session_id,
            session_clock,
            media_v3_telemetry,
            initial_snapshot: snapshot,
            authorization_revision: authorized.authorization_revision,
            authorization_committed_revision: authorized.committed_revision,
            replaced_snapshots,
            replacement_transition,
        };
        drop(state);
        drop(active);
        self.session_changed.notify_waiters();
        info!(
            viewer_handle,
            session_id,
            active_viewers,
            reason = if replacing {
                "replacement"
            } else {
                "connected"
            },
            "multi-viewer session membership changed"
        );
        Ok(lease)
    }

    pub(super) fn bind_v2_generation(
        &self,
        remote: EndpointId,
        session_id: u64,
        generation_id: u64,
        session_clock: SessionClock,
        _telemetry: Arc<MediaV3Telemetry>,
        encoder_control: Option<EncoderControl>,
    ) -> Result<SessionSnapshotV2> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let descriptor = MediaGenerationDescriptorV2 {
            generation_id,
            broadcast_name: media_generation_moq_broadcast_name(generation_id)?,
        };
        if let Some(current) = &state.media {
            ensure!(
                current == &descriptor,
                "viewer attempted to join a different active media generation"
            );
        } else {
            state.media = Some(descriptor.clone());
        }
        let viewer = state
            .viewers
            .get_mut(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .context("generation binding does not match the active v2 viewer")?;
        ensure!(generation_id != 0, "media generation id must be non-zero");
        viewer.session.session_clock = session_clock;
        viewer.session.media_generation_id = generation_id;
        viewer.session.media_broadcast_name = descriptor.broadcast_name;
        // Keep transport telemetry viewer-scoped. Producer telemetry belongs
        // to the shared generation and is merged only by its adaptive owner.
        viewer.session.encoder_control = encoder_control;
        publish_v2_snapshots(&state)?;
        let snapshot = v2_snapshot_for(&state, remote)?;
        Ok(snapshot)
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
                session.mode == SessionMode::LegacyExclusive
                    && session.media_active
                    && session.remote == remote
                    && session.nonce == nonce
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
            authorization_revision: session.authorization_revision,
        })
    }

    pub(super) fn subscribe_v2_snapshots(
        &self,
        remote: EndpointId,
        session_id: u64,
    ) -> Result<tokio::sync::watch::Receiver<Option<SessionSnapshotV2>>> {
        let state = self.v2_state.lock().expect("v2 session state poisoned");
        let viewer = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .context("snapshot subscription does not match the active v2 session")?;
        Ok(viewer.snapshots.subscribe())
    }

    pub(super) fn v2_revision(&self, remote: EndpointId, session_id: u64) -> Result<u64> {
        let state = self.v2_state.lock().expect("v2 session state poisoned");
        state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .context("revision request does not match the active v2 session")?;
        Ok(state.revision)
    }

    pub(crate) fn apply_focus_command(
        &self,
        remote: EndpointId,
        session_id: u64,
        command: &FocusCommandV2,
    ) -> Result<FocusCommandEffect> {
        command.validate()?;
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let (presence_id, grants, authorization_neutralizing) = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .map(|viewer| {
                (
                    viewer.presence_id.clone(),
                    viewer.session.grants,
                    viewer.authorization_neutralizing,
                )
            })
            .context("focus command does not match the active v2 session")?;
        ensure!(
            command.expected_revision == state.revision,
            "focus command expected stale revision {} instead of {}",
            command.expected_revision,
            state.revision
        );

        let candidate = FocusCandidate {
            presence_id,
            session_id,
        };
        let audit_handle = candidate.presence_id.clone();
        state.focus.check_rate_limit(&candidate, Instant::now())?;
        let mutation = match command.action {
            FocusCommandActionV2::Request => {
                ensure!(
                    !authorization_neutralizing,
                    "input authorization is still neutralizing"
                );
                ensure!(
                    grants.contains(InvitationGrants::POINTER_KEYBOARD)
                        || grants.contains(InvitationGrants::GAMEPAD),
                    "viewer is not input-capable"
                );
                state.focus.request(candidate, Instant::now())?
            }
            FocusCommandActionV2::Approve => state.focus.approve(
                &candidate,
                command
                    .expected_focus_generation
                    .expect("validated approval carries a focus generation"),
                command
                    .expected_proposal_id
                    .expect("validated approval carries a proposal id"),
            )?,
            FocusCommandActionV2::Deny => state.focus.deny(
                &candidate,
                command
                    .expected_focus_generation
                    .expect("validated denial carries a focus generation"),
                command
                    .expected_proposal_id
                    .expect("validated denial carries a proposal id"),
            )?,
            FocusCommandActionV2::Preempt => {
                ensure!(
                    !authorization_neutralizing,
                    "input authorization is still neutralizing"
                );
                ensure!(
                    grants.contains(InvitationGrants::POINTER_KEYBOARD)
                        || grants.contains(InvitationGrants::GAMEPAD),
                    "viewer is not input-capable"
                );
                state.focus.preempt(
                    candidate,
                    command
                        .expected_focus_generation
                        .expect("validated preemption carries a focus generation"),
                )?
            }
            FocusCommandActionV2::Release => {
                let expected_focus_generation = command
                    .expected_focus_generation
                    .expect("validated focus release carries a generation");
                state.focus.release(&candidate, expected_focus_generation)?
            }
        };
        if mutation.changed {
            advance_v2_revision(&mut state)?;
            publish_v2_snapshots(&state)?;
        }
        let snapshot = v2_snapshot_for(&state, remote)?;
        if mutation.changed {
            info!(
                viewer_handle = audit_handle.as_str(),
                session_id,
                action = ?command.action,
                roster_revision = snapshot.revision,
                transition_reason = ?snapshot.transition_reason,
                "slot-0 focus state changed"
            );
        }
        self.session_changed.notify_waiters();
        Ok(FocusCommandEffect {
            snapshot,
            neutralization: mutation.neutralization,
        })
    }

    pub(super) fn invalidate_v2_focus(
        &self,
        remote: EndpointId,
        session_id: u64,
        reason: FocusTransitionReasonV2,
    ) -> Result<Option<FocusNeutralization>> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let Some(presence_id) = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.session_id == session_id)
            .map(|viewer| viewer.presence_id.clone())
        else {
            return Ok(None);
        };
        let mutation = state.focus.begin_invalidation(
            &FocusCandidate {
                presence_id,
                session_id,
            },
            reason,
        )?;
        if mutation.changed {
            advance_v2_revision(&mut state)?;
            publish_v2_snapshots(&state)?;
        }
        self.session_changed.notify_waiters();
        Ok(mutation.neutralization)
    }

    pub(super) fn complete_v2_focus_transition(&self, transition_id: u64) -> Result<()> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let successor_is_valid = state
            .focus
            .transition_successor(transition_id)
            .and_then(|candidate| {
                state.viewers.values().find(|viewer| {
                    viewer.presence_id == candidate.presence_id
                        && viewer.session.session_id == candidate.session_id
                        && viewer.session.media_active
                        && !viewer.authorization_neutralizing
                        && (viewer
                            .session
                            .grants
                            .contains(InvitationGrants::POINTER_KEYBOARD)
                            || viewer.session.grants.contains(InvitationGrants::GAMEPAD))
                })
            })
            .is_some();
        if state
            .focus
            .complete_transition(transition_id, successor_is_valid, Instant::now())?
        {
            advance_v2_revision(&mut state)?;
            publish_v2_snapshots(&state)?;
        }
        drop(state);
        self.session_changed.notify_waiters();
        Ok(())
    }

    pub(super) fn expire_v2_focus(&self) -> Result<Option<FocusNeutralization>> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let now = Instant::now();
        let proposal_expired = state.focus.expire_proposal(now);
        let activation = state.focus.begin_activation_expiry(now)?;
        if proposal_expired || activation.changed {
            advance_v2_revision(&mut state)?;
            publish_v2_snapshots(&state)?;
            self.session_changed.notify_waiters();
        }
        Ok(activation.neutralization)
    }

    pub fn apply_authorization_mutation(
        &self,
        mutation: &AuthorizationMutation,
    ) -> Result<AuthorizationSessionEffect> {
        ensure!(
            mutation.committed_revision != 0 && mutation.authorization_revision != 0,
            "authorization mutation revision is invalid"
        );
        self.authorization_committed_revision
            .fetch_max(mutation.committed_revision, Ordering::SeqCst);

        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let Some(viewer) = state.viewers.get(&mutation.peer).filter(|viewer| {
            viewer.session.viewer_handle.as_deref() == Some(mutation.handle.as_str())
        }) else {
            return Ok(AuthorizationSessionEffect::default());
        };
        ensure!(
            mutation.authorization_revision > viewer.session.authorization_revision,
            "authorization mutation is stale for the active viewer"
        );
        let current_grants = mutation.current_grants.unwrap_or(InvitationGrants::VIEW);
        let input_reduced = mutation
            .previous_grants
            .contains(InvitationGrants::POINTER_KEYBOARD)
            && !current_grants.contains(InvitationGrants::POINTER_KEYBOARD)
            || mutation.previous_grants.contains(InvitationGrants::GAMEPAD)
                && !current_grants.contains(InvitationGrants::GAMEPAD);

        let focus_candidate = FocusCandidate {
            presence_id: viewer.presence_id.clone(),
            session_id: viewer.session.session_id,
        };
        let input_claimed = viewer.session.input_claimed;
        let disconnected = mutation.current_grants.is_none();
        let focus_mutation = if disconnected || input_reduced {
            state
                .focus
                .begin_invalidation(&focus_candidate, FocusTransitionReasonV2::Revoked)?
        } else {
            Default::default()
        };

        if disconnected {
            let viewer = state
                .viewers
                .remove(&mutation.peer)
                .expect("validated authorization viewer disappeared");
            viewer.snapshots.send_replace(None);
            // `release` reclaims focus rate-limit state only for a viewer it
            // still finds in the roster. Revocation removes it here, so retire
            // the candidate now or its timestamps outlive the host.
            state.focus.retire_candidate(&focus_candidate);
            let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
            pending.remove(&(mutation.peer, viewer.session.session_id));
        } else if let Some(grants) = mutation.current_grants {
            let viewer = state
                .viewers
                .get_mut(&mutation.peer)
                .expect("validated authorization viewer disappeared");
            viewer.session.authorization_revision = mutation.authorization_revision;
            viewer.session.authorization_committed_revision = mutation.committed_revision;
            viewer.session.grants = grants;
            if input_reduced {
                viewer.authorization_neutralizing =
                    input_claimed || focus_mutation.neutralization.is_some();
            }
        }
        advance_v2_revision(&mut state)?;
        publish_v2_snapshots(&state)?;
        info!(
            viewer_handle = %mutation.handle,
            authorization_revision = mutation.authorization_revision,
            committed_revision = mutation.committed_revision,
            reason = if disconnected { "view_revoked" } else if input_reduced { "input_reduced" } else { "grants_changed" },
            "live viewer authorization changed"
        );
        drop(state);
        self.session_changed.notify_waiters();
        Ok(AuthorizationSessionEffect {
            disconnected,
            neutralize_input: input_claimed || focus_mutation.neutralization.is_some(),
            focus_transition_id: focus_mutation
                .neutralization
                .map(|transition| transition.transition_id),
        })
    }

    pub fn complete_authorization_neutralization(
        &self,
        remote: EndpointId,
        authorization_revision: u64,
        focus_transition_id: Option<u64>,
    ) -> Result<()> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        if let Some(viewer) = state.viewers.get_mut(&remote)
            && viewer.session.authorization_revision == authorization_revision
        {
            viewer.authorization_neutralizing = false;
        }
        let completed_focus = if let Some(transition_id) = focus_transition_id {
            state
                .focus
                .complete_transition(transition_id, false, Instant::now())?
        } else {
            false
        };
        if completed_focus {
            advance_v2_revision(&mut state)?;
            publish_v2_snapshots(&state)?;
        }
        drop(state);
        self.session_changed.notify_waiters();
        Ok(())
    }

    pub(super) fn claim_input_v2(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
        session_id: u64,
        slot: ControllerSlot,
        focus_generation: u64,
    ) -> Result<InputV2Lease> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let (presence_id, grants, authorization_revision) = state
            .viewers
            .get(&remote)
            .filter(|viewer| {
                viewer.session.media_active
                    && viewer.session.nonce == nonce
                    && viewer.session.session_id == session_id
            })
            .map(|viewer| {
                (
                    viewer.presence_id.clone(),
                    viewer.session.grants,
                    viewer.session.authorization_revision,
                )
            })
            .context("input v2 connection does not match the active session")?;
        ensure!(
            matches!(
                state.focus.state(),
                FocusStateV2::Held {
                    holder,
                    session_id: holder_session_id,
                    slot: holder_slot,
                    focus_generation: holder_generation,
                } if holder == &presence_id
                    && *holder_session_id == session_id
                    && *holder_slot == slot
                    && *holder_generation == focus_generation
            ),
            "input v2 connection does not own the authoritative focus generation"
        );
        state.focus.mark_activated(
            &FocusCandidate {
                presence_id: presence_id.clone(),
                session_id,
            },
            focus_generation,
        );
        let viewer = state
            .viewers
            .get_mut(&remote)
            .expect("validated v2 viewer disappeared");
        ensure!(
            !viewer.session.input_claimed,
            "active viewer already has an input stream"
        );
        viewer.session.input_claimed = true;
        Ok(InputV2Lease {
            registry: Arc::clone(self),
            remote,
            session_id,
            slot,
            focus_generation,
            grants,
            authorization_revision,
        })
    }

    pub(crate) fn is_v2_focus_owner(
        &self,
        remote: EndpointId,
        session_id: u64,
        slot: ControllerSlot,
        focus_generation: u64,
    ) -> bool {
        let state = self.v2_state.lock().expect("v2 session state poisoned");
        let Some(viewer) = state.viewers.get(&remote).filter(|viewer| {
            viewer.session.media_active && viewer.session.session_id == session_id
        }) else {
            return false;
        };
        matches!(
            state.focus.state(),
            FocusStateV2::Held {
                holder,
                session_id: holder_session_id,
                slot: holder_slot,
                focus_generation: holder_generation,
            } if holder == &viewer.presence_id
                && *holder_session_id == viewer.session.session_id
                && *holder_slot == slot
                && *holder_generation == focus_generation
        )
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
        let legacy_control = self
            .active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .and_then(|session| session.encoder_control.clone());
        let control = legacy_control.or_else(|| {
            self.v2_state
                .lock()
                .expect("v2 session state poisoned")
                .viewers
                .get(&remote)
                .filter(|viewer| {
                    viewer.session.media_active && viewer.session.session_id == session_id
                })
                .and_then(|viewer| viewer.session.encoder_control.clone())
        });
        let Some(control) = control else {
            ensure!(
                self.is_active(remote, session_id),
                "adaptive encoder update does not match the active media session"
            );
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
        let legacy_control = self
            .active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .and_then(|session| session.encoder_control.clone());
        let control = legacy_control.or_else(|| {
            self.v2_state
                .lock()
                .expect("v2 session state poisoned")
                .viewers
                .get(&remote)
                .filter(|viewer| {
                    viewer.session.media_active && viewer.session.session_id == session_id
                })
                .and_then(|viewer| viewer.session.encoder_control.clone())
        });
        let Some(control) = control else {
            ensure!(
                self.is_active(remote, session_id),
                "resolution update does not match the active media session"
            );
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
        if let Some(session) = active.as_mut().filter(|session| {
            session.media_active && session.remote == remote && session.nonce == nonce
        }) {
            ensure!(
                session.grants.contains(InvitationGrants::VIEW),
                "active Portal session lacks feedback view permission"
            );
            ensure!(
                !session.feedback_claimed,
                "active client already has a feedback connection"
            );
            session.feedback_claimed = true;
            return Ok(FeedbackLease {
                registry: Arc::clone(self),
                remote,
                session_id: session.session_id,
                telemetry: Arc::clone(&session.media_v3_telemetry),
                encoder_control: session.encoder_control.clone(),
                authorization_revision: session.authorization_revision,
                media_generation_id: session.media_generation_id,
                scope: FeedbackScope::Legacy,
            });
        }
        drop(active);
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let viewer = state
            .viewers
            .get_mut(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.nonce == nonce)
            .context("feedback connection does not match the active media session")?;
        viewer.rates.feedback_claims.check(
            Instant::now(),
            FEEDBACK_CLAIM_RATE_WINDOW,
            MAX_FEEDBACK_CLAIMS_PER_WINDOW,
            "media feedback claim",
        )?;
        let session = &mut viewer.session;
        ensure!(
            session.grants.contains(InvitationGrants::VIEW),
            "active Portal session lacks feedback view permission"
        );
        ensure!(
            session.authorization_committed_revision
                == self.authorization_committed_revision.load(Ordering::SeqCst),
            "feedback claim used a stale committed authorization revision"
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
            authorization_revision: session.authorization_revision,
            media_generation_id: session.media_generation_id,
            scope: FeedbackScope::SharedGeneration,
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
                session.mode == SessionMode::LegacyExclusive
                    && session.media_active
                    && session.remote == remote
                    && session.nonce == nonce
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
            authorization_revision: session.authorization_revision,
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
            !pending.contains_key(&(remote, session_id)),
            "active control session already expects MoQ"
        );
        let (attached, attached_rx) = tokio::sync::oneshot::channel();
        let (closed, closed_rx) = tokio::sync::oneshot::channel();
        pending.insert(
            (remote, session_id),
            PendingMoqAttachment {
                remote,
                session_id,
                broadcast_name,
                broadcast,
                attached,
                closed,
                telemetry,
                subscription_capability: None,
                expected_host: None,
            },
        );
        Ok(MoqAttachmentWait {
            attached: attached_rx,
            closed: closed_rx,
        })
    }

    pub(super) fn expect_moq_v2(
        &self,
        remote: EndpointId,
        session_id: u64,
        broadcast_name: String,
        broadcast: BroadcastConsumer,
        subscription_capability: SignedSubscriptionCapability,
        expected_host: [u8; 32],
    ) -> Result<MoqAttachmentWait> {
        let state = self.v2_state.lock().expect("v2 session state poisoned");
        let session = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .map(|viewer| &viewer.session)
            .context("authenticated MoQ expectation does not match the active v2 session")?;
        ensure!(
            subscription_capability.claims.host_node_id == expected_host
                && subscription_capability.claims.media_generation_id
                    == session.media_generation_id
                && subscription_capability.claims.subscriber_endpoint_id == *remote.as_bytes()
                && subscription_capability
                    .claims
                    .tracks
                    .contains(SubscriptionTracks::VIDEO_H264)
                && subscription_capability.claims.authorization_revision
                    == session.authorization_revision,
            "subscription capability does not match the v2 viewer attachment"
        );
        let telemetry = Arc::clone(&session.media_v3_telemetry);
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        ensure!(
            !pending.contains_key(&(remote, session_id)),
            "active control session already expects MoQ"
        );
        let (attached, attached_rx) = tokio::sync::oneshot::channel();
        let (closed, closed_rx) = tokio::sync::oneshot::channel();
        pending.insert(
            (remote, session_id),
            PendingMoqAttachment {
                remote,
                session_id,
                broadcast_name,
                broadcast,
                attached,
                closed,
                telemetry,
                subscription_capability: Some(subscription_capability),
                expected_host: Some(expected_host),
            },
        );
        Ok(MoqAttachmentWait {
            attached: attached_rx,
            closed: closed_rx,
        })
    }

    pub(super) fn claim_moq(&self, remote: EndpointId) -> Result<ClaimedMoqAttachment> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        self.claim_moq_at(remote, now_unix)
    }

    fn claim_moq_at(&self, remote: EndpointId, now_unix: u64) -> Result<ClaimedMoqAttachment> {
        let legacy = self
            .active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .filter(|session| session.media_active && session.remote == remote)
            .cloned();
        let v2 = {
            let mut state = self.v2_state.lock().expect("v2 session state poisoned");
            state
                .viewers
                .get_mut(&remote)
                .filter(|viewer| viewer.session.media_active)
                .map(|viewer| {
                    viewer.rates.attachments.check(
                        Instant::now(),
                        ATTACHMENT_RATE_WINDOW,
                        MAX_ATTACHMENTS_PER_WINDOW,
                        "MoQ attachment",
                    )?;
                    Ok::<ActiveSession, anyhow::Error>(viewer.session.clone())
                })
                .transpose()?
        };
        let session = legacy
            .or(v2)
            .context("MoQ connection does not match an active control session")?;
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        let attachment = pending
            .get(&(remote, session.session_id))
            .context("active control session is not expecting a MoQ connection")?;
        if session.mode == SessionMode::V2Single {
            let capability = attachment
                .subscription_capability
                .as_ref()
                .context("v2 MoQ attachment has no subscription capability")?;
            let expected_host = attachment
                .expected_host
                .context("v2 MoQ attachment has no expected host identity")?;
            capability
                .verify_binding(
                    expected_host,
                    session.media_generation_id,
                    *remote.as_bytes(),
                    capability.claims.tracks,
                    session.authorization_revision,
                    now_unix,
                )
                .context("verifying endpoint-bound MoQ subscription capability")?;
        } else {
            ensure!(
                attachment.subscription_capability.is_none(),
                "legacy MoQ attachment unexpectedly carries a subscription capability"
            );
        }
        debug_assert_eq!(attachment.remote, session.remote);
        let attachment = pending
            .remove(&(remote, session.session_id))
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

    pub(super) fn admit_v2_keyframe_request(
        &self,
        remote: EndpointId,
        session_id: u64,
    ) -> Result<()> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let viewer = state
            .viewers
            .get_mut(&remote)
            .filter(|viewer| viewer.session.media_active && viewer.session.session_id == session_id)
            .context("keyframe request does not match the active viewer generation")?;
        viewer.rates.keyframes.check(
            Instant::now(),
            KEYFRAME_RATE_WINDOW,
            MAX_KEYFRAME_REQUESTS_PER_WINDOW,
            "keyframe request",
        )
    }

    #[cfg(test)]
    pub(super) fn focus_rate_limit_entries(&self) -> usize {
        self.v2_state
            .lock()
            .expect("v2 session state poisoned")
            .focus
            .tracked_command_candidates()
    }

    fn release(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
            pending.remove(&(remote, session_id));
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
            return;
        }
        drop(active);

        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        state.live_control_leases = state.live_control_leases.saturating_sub(1);
        let Some(viewer) = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.session_id == session_id)
        else {
            return;
        };
        let presence_id = viewer.presence_id.clone();
        let viewer_handle = viewer
            .session
            .viewer_handle
            .clone()
            .unwrap_or_else(|| presence_id.as_str().to_owned());
        let focus_mutation = state.focus.begin_invalidation(
            &FocusCandidate {
                presence_id: presence_id.clone(),
                session_id,
            },
            FocusTransitionReasonV2::Disconnected,
        );
        let viewer = state
            .viewers
            .remove(&remote)
            .expect("validated v2 viewer disappeared");
        viewer.snapshots.send_replace(None);
        self.pending_moq
            .lock()
            .expect("MoQ registry poisoned")
            .remove(&(remote, session_id));
        if let Err(error) = focus_mutation {
            warn!(%error, session_id, "failed to invalidate focus during viewer release");
        }
        state.focus.retire_candidate(&FocusCandidate {
            presence_id,
            session_id,
        });
        let active_viewers = state.viewers.len();
        if state.viewers.is_empty() {
            state.media = None;
        }
        if advance_v2_revision(&mut state).is_ok() {
            let _ = publish_v2_snapshots(&state);
        }
        drop(state);
        self.session_changed.notify_waiters();
        info!(
            viewer_handle,
            session_id,
            active_viewers,
            reason = "disconnected",
            "multi-viewer session membership changed"
        );
    }

    pub(crate) fn is_active(&self, remote: EndpointId, session_id: u64) -> bool {
        if self
            .active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .is_some_and(|active| {
                active.media_active && active.remote == remote && active.session_id == session_id
            })
        {
            return true;
        }
        self.v2_state
            .lock()
            .expect("v2 session state poisoned")
            .viewers
            .get(&remote)
            .is_some_and(|viewer| {
                viewer.session.media_active && viewer.session.session_id == session_id
            })
    }

    pub(super) fn disconnect_v2_viewer(&self, remote: EndpointId, session_id: u64) -> Result<bool> {
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        let Some(viewer) = state
            .viewers
            .get(&remote)
            .filter(|viewer| viewer.session.session_id == session_id)
        else {
            return Ok(false);
        };
        let viewer_handle = viewer
            .session
            .viewer_handle
            .clone()
            .unwrap_or_else(|| viewer.presence_id.as_str().to_owned());
        ensure!(
            !matches!(
                state.focus.state(),
                FocusStateV2::Held {
                    session_id: holder_session_id,
                    ..
                } if *holder_session_id == session_id
            ),
            "focused viewer must be neutralized before adaptive detachment"
        );
        let retired = FocusCandidate {
            presence_id: viewer.presence_id.clone(),
            session_id,
        };
        viewer.snapshots.send_replace(None);
        state.viewers.remove(&remote);
        // Detaching bypasses `release`'s roster lookup, so reclaim this
        // viewer's focus rate-limit timestamps explicitly.
        state.focus.retire_candidate(&retired);
        if state.viewers.is_empty() {
            state.media = None;
        }
        advance_v2_revision(&mut state)?;
        publish_v2_snapshots(&state)?;
        drop(state);
        self.pending_moq
            .lock()
            .expect("MoQ registry poisoned")
            .remove(&(remote, session_id));
        self.session_changed.notify_waiters();
        info!(
            viewer_handle,
            session_id,
            active_viewers = self.runtime_status().active_viewers,
            reason = "adaptive_recovery_detach",
            "multi-viewer session membership changed"
        );
        Ok(true)
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
            return;
        }
        drop(active);
        let mut state = self.v2_state.lock().expect("v2 session state poisoned");
        if let Some(viewer) = state.viewers.get_mut(&remote)
            && viewer.session.session_id == session_id
        {
            viewer.session.input_claimed = false;
        }
        let completed_neutralization = match state.focus.state() {
            FocusStateV2::Neutralizing {
                former_session_id,
                transition_id,
                ..
            } if *former_session_id == session_id => Some(*transition_id),
            _ => None,
        };
        if let Some(transition_id) = completed_neutralization {
            let _ = state
                .focus
                .complete_transition(transition_id, false, Instant::now());
            if advance_v2_revision(&mut state).is_ok() {
                let _ = publish_v2_snapshots(&state);
            }
            drop(state);
            self.session_changed.notify_waiters();
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
            return;
        }
        drop(active);
        if let Some(viewer) = self
            .v2_state
            .lock()
            .expect("v2 session state poisoned")
            .viewers
            .get_mut(&remote)
            && viewer.session.session_id == session_id
        {
            viewer.session.feedback_claimed = false;
        }
    }
}

fn advance_v2_revision(state: &mut V2SessionState) -> Result<()> {
    state.revision = state
        .revision
        .checked_add(1)
        .context("v2 snapshot revision overflowed")?;
    Ok(())
}

fn publish_v2_snapshots(state: &V2SessionState) -> Result<()> {
    for (remote, viewer) in &state.viewers {
        viewer
            .snapshots
            .send_replace(Some(v2_snapshot_for(state, *remote)?));
    }
    Ok(())
}

fn v2_snapshot_for(state: &V2SessionState, remote: EndpointId) -> Result<SessionSnapshotV2> {
    let self_viewer = state
        .viewers
        .get(&remote)
        .context("active v2 viewer is missing from snapshot state")?;
    let mut viewers = state
        .viewers
        .values()
        .map(|viewer| ViewerPresenceV2 {
            presence_id: viewer.presence_id.clone(),
            session_id: viewer.session.session_id,
            input_capable: viewer
                .session
                .grants
                .contains(InvitationGrants::POINTER_KEYBOARD)
                || viewer.session.grants.contains(InvitationGrants::GAMEPAD),
            you: viewer.session.remote == remote,
        })
        .collect::<Vec<_>>();
    viewers.sort_by(|left, right| left.presence_id.cmp(&right.presence_id));
    let snapshot = SessionSnapshotV2 {
        revision: state.revision,
        self_presence_id: self_viewer.presence_id.clone(),
        viewers,
        focus: state.focus.state().clone(),
        focus_proposal: state.focus.proposal().cloned(),
        self_is_focus_owner: state.focus.is_configured_owner(&self_viewer.presence_id),
        transition_reason: state.focus.transition_reason(),
        media: state.media.clone().unwrap_or(MediaGenerationDescriptorV2 {
            generation_id: self_viewer.session.media_generation_id,
            broadcast_name: self_viewer.session.media_broadcast_name.clone(),
        }),
    };
    snapshot.validate()?;
    Ok(snapshot)
}

#[derive(Debug)]
pub(super) struct SessionLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) session_clock: SessionClock,
    pub(super) media_v3_telemetry: Arc<MediaV3Telemetry>,
}

#[derive(Debug)]
pub(crate) struct V2SessionLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(crate) session_id: u64,
    #[allow(dead_code)]
    pub(super) session_clock: SessionClock,
    #[allow(dead_code)]
    pub(super) media_v3_telemetry: Arc<MediaV3Telemetry>,
    #[allow(dead_code)]
    pub(super) initial_snapshot: SessionSnapshotV2,
    pub(super) authorization_revision: u64,
    pub(super) authorization_committed_revision: u64,
    replaced_snapshots: Option<tokio::sync::watch::Sender<Option<SessionSnapshotV2>>>,
    pub(super) replacement_transition: Option<FocusNeutralization>,
}

impl V2SessionLease {
    pub(super) fn retire_replaced_viewer(&mut self) {
        if let Some(snapshots) = self.replaced_snapshots.take() {
            snapshots.send_replace(None);
        }
    }
}

impl Drop for V2SessionLease {
    fn drop(&mut self) {
        self.retire_replaced_viewer();
        self.registry.release(self.remote, self.session_id);
    }
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
    pub(super) authorization_revision: u64,
}

#[derive(Debug)]
pub(super) struct InputV2Lease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) slot: ControllerSlot,
    pub(super) focus_generation: u64,
    pub(super) grants: InvitationGrants,
    pub(super) authorization_revision: u64,
}

#[derive(Debug)]
pub(super) struct AudioLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) session_clock: SessionClock,
    pub(super) grants: InvitationGrants,
    pub(super) authorization_revision: u64,
}

#[derive(Debug)]
pub(super) struct FeedbackLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    pub(super) session_id: u64,
    pub(super) telemetry: Arc<MediaV3Telemetry>,
    pub(super) encoder_control: Option<EncoderControl>,
    pub(super) authorization_revision: u64,
    pub(super) media_generation_id: u64,
    pub(super) scope: FeedbackScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FeedbackScope {
    Legacy,
    SharedGeneration,
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

impl Drop for InputV2Lease {
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

    #[test]
    fn v2_single_viewer_mode_excludes_legacy_and_publishes_revisioned_focus() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [6; 16];
        let media = sessions
            .claim_v2(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        assert_eq!(media.initial_snapshot.revision, 1);
        assert!(matches!(
            media.initial_snapshot.focus,
            FocusStateV2::Vacant { .. }
        ));
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );

        let request = FocusCommandV2 {
            request_id: 1,
            action: FocusCommandActionV2::Request,
            slot: ControllerSlot::ZERO,
            expected_revision: 1,
            expected_focus_generation: None,
            expected_proposal_id: None,
        };
        let granted = sessions
            .apply_focus_command(remote, media.session_id, &request)
            .unwrap();
        assert_eq!(granted.snapshot.revision, 2);
        assert!(granted.neutralization.is_none());
        let focus_generation = granted.snapshot.self_focus_generation().unwrap();
        assert!(sessions.is_v2_focus_owner(
            remote,
            media.session_id,
            ControllerSlot::ZERO,
            focus_generation
        ));
        assert!(
            sessions
                .claim_input_v2(
                    remote,
                    nonce,
                    media.session_id,
                    ControllerSlot::ZERO,
                    focus_generation + 1,
                )
                .is_err()
        );
        let input = sessions
            .claim_input_v2(
                remote,
                nonce,
                media.session_id,
                ControllerSlot::ZERO,
                focus_generation,
            )
            .unwrap();

        let release = FocusCommandV2 {
            request_id: 2,
            action: FocusCommandActionV2::Release,
            slot: ControllerSlot::ZERO,
            expected_revision: 2,
            expected_focus_generation: Some(focus_generation),
            expected_proposal_id: None,
        };
        let released = sessions
            .apply_focus_command(remote, media.session_id, &release)
            .unwrap();
        assert_eq!(released.snapshot.revision, 3);
        let transition = released
            .neutralization
            .expect("release must publish neutralizing state");
        assert!(!sessions.is_v2_focus_owner(
            remote,
            media.session_id,
            ControllerSlot::ZERO,
            focus_generation
        ));
        assert!(
            sessions
                .apply_focus_command(remote, media.session_id, &release)
                .is_err(),
            "a stale release must not mutate the newer snapshot revision"
        );
        sessions
            .complete_v2_focus_transition(transition.transition_id)
            .unwrap();
        assert_eq!(sessions.v2_revision(remote, media.session_id).unwrap(), 4);
        drop(input);
        drop(media);
    }

    fn authorized_viewer(handle: &str, grants: InvitationGrants) -> AuthorizedViewer {
        AuthorizedViewer {
            handle: handle.to_owned(),
            grants,
            authorization_revision: 1,
            committed_revision: 1,
        }
    }

    // Regression: `claim_v2_authorized` used to drop the legacy registry lock
    // before taking the v2 lock, so a legacy client and a v2 viewer could both
    // clear their mutual-exclusion check and be admitted at the same time.
    //
    // The interleaving is too narrow to reproduce by racing threads, so assert
    // the lock discipline that closes it directly: while v2 admission is parked
    // on the v2 lock it must still be holding the legacy registry lock. Under
    // the old ordering it had already released it, and `claim` could walk in.
    #[test]
    fn v2_admission_holds_the_legacy_registry_lock_across_the_v2_mutation() {
        let sessions = Arc::new(SessionRegistry::default());
        let blocked = Arc::clone(&sessions);
        let state_guard = sessions.v2_state.lock().expect("v2 session state poisoned");

        let admission = std::thread::spawn(move || {
            blocked.claim_v2_authorized(
                endpoint(2),
                [2; 16],
                authorized_viewer("viewer-0000000000000002", InvitationGrants::ALL),
            )
        });
        // Generous settle time: the thread is now parked on `v2_state`.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            sessions.active.try_lock().is_err(),
            "v2 admission released the legacy registry lock before mutating v2 state, \
             which lets `claim` admit a legacy client concurrently"
        );

        drop(state_guard);
        // Hold the lease: dropping it would release the viewer and make the
        // exclusion assertion below vacuous.
        let _viewer = admission
            .join()
            .expect("v2 admission thread panicked")
            .unwrap_or_else(|error| {
                panic!("v2 admission must succeed once the v2 lock is free: {error:#}")
            });
        assert!(
            sessions
                .claim(endpoint(1), [1; 16], InvitationGrants::ALL)
                .is_err(),
            "a legacy client must be refused while a v2 viewer holds a control lease"
        );
    }

    #[test]
    fn legacy_and_v2_admission_stay_mutually_exclusive_under_contention() {
        for round in 0..300_u16 {
            let sessions = Arc::new(SessionRegistry::default());
            let legacy_registry = Arc::clone(&sessions);
            let v2_registry = Arc::clone(&sessions);
            let gate = Arc::new(std::sync::Barrier::new(2));
            let legacy_gate = Arc::clone(&gate);
            let v2_gate = Arc::clone(&gate);

            let legacy = std::thread::spawn(move || {
                legacy_gate.wait();
                legacy_registry
                    .claim(endpoint(1), [1; 16], InvitationGrants::ALL)
                    .ok()
            });
            let v2 = std::thread::spawn(move || {
                v2_gate.wait();
                v2_registry
                    .claim_v2_authorized(
                        endpoint(2),
                        [2; 16],
                        authorized_viewer("viewer-0000000000000002", InvitationGrants::ALL),
                    )
                    .ok()
            });
            let legacy = legacy.join().expect("legacy admission thread panicked");
            let v2 = v2.join().expect("v2 admission thread panicked");
            assert!(
                legacy.is_none() || v2.is_none(),
                "round {round} admitted a legacy client and a v2 viewer at once"
            );
            assert!(
                legacy.is_some() || v2.is_some(),
                "round {round} admitted neither client"
            );
        }
    }

    // Regression: focus rate-limit timestamps were reclaimed only by `release`,
    // which cannot find a viewer that revocation or adaptive detachment already
    // removed from the roster.
    #[test]
    fn focus_rate_limit_state_is_reclaimed_on_every_viewer_exit() {
        let request = FocusCommandV2 {
            request_id: 1,
            action: FocusCommandActionV2::Request,
            slot: ControllerSlot::ZERO,
            expected_revision: 1,
            expected_focus_generation: None,
            expected_proposal_id: None,
        };

        // Ordinary disconnect.
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let lease = sessions
            .claim_v2_authorized(
                remote,
                [1; 16],
                authorized_viewer("viewer-0000000000000001", InvitationGrants::ALL),
            )
            .unwrap();
        sessions
            .apply_focus_command(remote, lease.session_id, &request)
            .unwrap();
        assert_eq!(sessions.focus_rate_limit_entries(), 1);
        drop(lease);
        assert_eq!(
            sessions.focus_rate_limit_entries(),
            0,
            "disconnect must reclaim focus rate-limit state"
        );

        // Adaptive slow-viewer detachment.
        let sessions = Arc::new(SessionRegistry::default());
        let holder = endpoint(1);
        let spectator = endpoint(2);
        let holder_lease = sessions
            .claim_v2_authorized(
                holder,
                [1; 16],
                authorized_viewer("viewer-0000000000000001", InvitationGrants::ALL),
            )
            .unwrap();
        let spectator_lease = sessions
            .claim_v2_authorized(
                spectator,
                [2; 16],
                authorized_viewer("viewer-0000000000000002", InvitationGrants::ALL),
            )
            .unwrap();
        let mut spectator_request = request.clone();
        spectator_request.expected_revision = sessions
            .v2_revision(spectator, spectator_lease.session_id)
            .unwrap();
        sessions
            .apply_focus_command(spectator, spectator_lease.session_id, &spectator_request)
            .unwrap();
        assert_eq!(sessions.focus_rate_limit_entries(), 1);
        // The spectator now holds focus, so release it before detaching.
        let mut release = spectator_request.clone();
        release.action = FocusCommandActionV2::Release;
        release.expected_revision = sessions
            .v2_revision(spectator, spectator_lease.session_id)
            .unwrap();
        release.expected_focus_generation = match sessions.v2_state.lock().unwrap().focus.state() {
            FocusStateV2::Held {
                focus_generation, ..
            } => Some(*focus_generation),
            other => panic!("expected held focus, got {other:?}"),
        };
        let effect = sessions
            .apply_focus_command(spectator, spectator_lease.session_id, &release)
            .unwrap();
        if let Some(transition) = effect.neutralization {
            sessions
                .complete_v2_focus_transition(transition.transition_id)
                .unwrap();
        }
        assert!(
            sessions
                .disconnect_v2_viewer(spectator, spectator_lease.session_id)
                .unwrap()
        );
        assert_eq!(
            sessions.focus_rate_limit_entries(),
            0,
            "adaptive detachment must reclaim focus rate-limit state"
        );
        drop(spectator_lease);
        drop(holder_lease);
    }

    #[test]
    fn v2_admits_bounded_viewers_and_publishes_personalized_complete_rosters() {
        let sessions = Arc::new(SessionRegistry::new(3));
        let telemetry = Arc::new(MediaV3Telemetry::default());
        let mut leases = Vec::new();
        for index in 1..=3 {
            let remote = endpoint(index);
            let lease = sessions
                .claim_v2_authorized(
                    remote,
                    [index; 16],
                    authorized_viewer(&format!("viewer-{index}"), InvitationGrants::ALL),
                )
                .unwrap();
            sessions
                .bind_v2_generation(
                    remote,
                    lease.session_id,
                    77,
                    SessionClock::start(),
                    Arc::clone(&telemetry),
                    None,
                )
                .unwrap();
            leases.push(lease);
        }

        for (index, lease) in leases.iter().enumerate() {
            let remote = endpoint(u8::try_from(index + 1).unwrap());
            let snapshots = sessions
                .subscribe_v2_snapshots(remote, lease.session_id)
                .unwrap();
            let snapshot = snapshots.borrow().clone().unwrap();
            assert_eq!(snapshot.viewers.len(), 3);
            assert_eq!(snapshot.media.generation_id, 77);
            assert_eq!(
                snapshot.viewers.iter().filter(|viewer| viewer.you).count(),
                1
            );
            assert_eq!(snapshot.self_viewer().session_id, lease.session_id);
        }

        assert!(
            sessions
                .claim_v2_authorized(
                    endpoint(4),
                    [4; 16],
                    authorized_viewer("viewer-4", InvitationGrants::VIEW),
                )
                .unwrap_err()
                .to_string()
                .contains("capacity")
        );
    }

    #[test]
    fn same_peer_replacement_is_immediate_generation_safe_and_neutralizes_focus() {
        let sessions = Arc::new(SessionRegistry::new(2));
        let remote = endpoint(1);
        let old = sessions
            .claim_v2_authorized(
                remote,
                [1; 16],
                authorized_viewer("viewer-one", InvitationGrants::ALL),
            )
            .unwrap();
        let granted = sessions
            .apply_focus_command(
                remote,
                old.session_id,
                &FocusCommandV2 {
                    request_id: 1,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: 1,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap();
        assert!(granted.snapshot.self_focus_generation().is_some());

        let mut replacement = sessions
            .claim_v2_authorized(
                remote,
                [2; 16],
                authorized_viewer("viewer-one", InvitationGrants::ALL),
            )
            .unwrap();
        let transition = replacement
            .replacement_transition
            .expect("focused replacement must neutralize");
        assert!(matches!(
            replacement.initial_snapshot.focus,
            FocusStateV2::Neutralizing { .. }
        ));
        sessions
            .complete_v2_focus_transition(transition.transition_id)
            .unwrap();
        let replacement_snapshot = sessions
            .subscribe_v2_snapshots(remote, replacement.session_id)
            .unwrap()
            .borrow()
            .clone()
            .unwrap();
        assert!(matches!(
            replacement_snapshot.focus,
            FocusStateV2::Vacant { .. }
        ));
        replacement.retire_replaced_viewer();
        drop(old);
        assert!(sessions.is_active(remote, replacement.session_id));
        assert!(!sessions.is_active(remote, granted.snapshot.self_viewer().session_id));
    }

    #[test]
    fn occupied_focus_retains_one_proposal_without_queueing_additional_viewers() {
        let sessions = Arc::new(SessionRegistry::new(3));
        let first = sessions
            .claim_v2_authorized(
                endpoint(1),
                [1; 16],
                authorized_viewer("viewer-one", InvitationGrants::ALL),
            )
            .unwrap();
        let second = sessions
            .claim_v2_authorized(
                endpoint(2),
                [2; 16],
                authorized_viewer("viewer-two", InvitationGrants::ALL),
            )
            .unwrap();
        let third = sessions
            .claim_v2_authorized(
                endpoint(3),
                [3; 16],
                authorized_viewer("viewer-three", InvitationGrants::ALL),
            )
            .unwrap();
        let revision = third.initial_snapshot.revision;
        sessions
            .apply_focus_command(
                endpoint(1),
                first.session_id,
                &FocusCommandV2 {
                    request_id: 1,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: revision,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap();
        let occupied_revision = sessions
            .v2_revision(endpoint(2), second.session_id)
            .unwrap();
        let proposal = sessions
            .apply_focus_command(
                endpoint(2),
                second.session_id,
                &FocusCommandV2 {
                    request_id: 2,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: occupied_revision,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap();
        assert_eq!(
            proposal.snapshot.focus_proposal.as_ref().unwrap().requester,
            second.initial_snapshot.self_presence_id
        );
        let proposal_revision = proposal.snapshot.revision;
        let error = sessions
            .apply_focus_command(
                endpoint(3),
                third.session_id,
                &FocusCommandV2 {
                    request_id: 3,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: proposal_revision,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("pending handoff proposal"));
        assert_eq!(
            sessions
                .v2_revision(endpoint(2), second.session_id)
                .unwrap(),
            proposal_revision
        );
    }

    #[test]
    fn authorization_reduction_blocks_focus_until_neutralization_completes() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let handle = "viewer-0000000000000001".to_owned();
        let media = sessions
            .claim_v2_authorized(
                remote,
                [4; 16],
                AuthorizedViewer {
                    handle: handle.clone(),
                    grants: InvitationGrants::ALL,
                    authorization_revision: 1,
                    committed_revision: 1,
                },
            )
            .unwrap();
        let granted = sessions
            .apply_focus_command(
                remote,
                media.session_id,
                &FocusCommandV2 {
                    request_id: 1,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: 1,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap();
        assert_eq!(granted.snapshot.revision, 2);

        let effect = sessions
            .apply_authorization_mutation(&AuthorizationMutation {
                handle,
                peer: remote,
                previous_grants: InvitationGrants::ALL,
                current_grants: Some(InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD)),
                authorization_revision: 2,
                committed_revision: 2,
            })
            .unwrap();
        assert!(effect.neutralize_input);
        assert!(
            sessions
                .apply_focus_command(
                    remote,
                    media.session_id,
                    &FocusCommandV2 {
                        request_id: 2,
                        action: FocusCommandActionV2::Request,
                        slot: ControllerSlot::ZERO,
                        expected_revision: 3,
                        expected_focus_generation: None,
                        expected_proposal_id: None,
                    },
                )
                .unwrap_err()
                .to_string()
                .contains("still neutralizing")
        );
        sessions
            .complete_authorization_neutralization(remote, 2, effect.focus_transition_id)
            .unwrap();
        assert!(
            sessions
                .apply_focus_command(
                    remote,
                    media.session_id,
                    &FocusCommandV2 {
                        request_id: 3,
                        action: FocusCommandActionV2::Request,
                        slot: ControllerSlot::ZERO,
                        expected_revision: 4,
                        expected_focus_generation: None,
                        expected_proposal_id: None,
                    },
                )
                .is_ok()
        );
    }

    #[test]
    fn legacy_mode_rejects_v2_coexistence() {
        let sessions = Arc::new(SessionRegistry::default());
        let legacy = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::ALL)
            .unwrap();
        assert!(
            sessions
                .claim_v2(endpoint(2), [2; 16], InvitationGrants::ALL)
                .is_err()
        );
        drop(legacy);
        assert!(
            sessions
                .claim_v2(endpoint(2), [2; 16], InvitationGrants::ALL)
                .is_ok()
        );
    }

    fn test_moq_broadcast() -> (BroadcastProducer, BroadcastConsumer) {
        let producer = Broadcast::new().produce();
        let consumer = producer.consume();
        (producer, consumer)
    }

    fn test_subscription(
        host: &iroh::SecretKey,
        remote: EndpointId,
        generation_id: u64,
        issued_at_unix: u64,
        expires_at_unix: u64,
    ) -> SignedSubscriptionCapability {
        SignedSubscriptionCapability::issue(
            sigil_protocol::SubscriptionClaims::new(
                *host.public().as_bytes(),
                generation_id,
                *remote.as_bytes(),
                SubscriptionTracks::VIDEO_H264,
                1,
                issued_at_unix,
                expires_at_unix,
                [7; 32],
                1,
            )
            .unwrap(),
            &host.to_bytes(),
        )
        .unwrap()
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

    // Regression: `claim_moq` compared the capability's host field against
    // itself, so the host-binding leg of `verify_binding` was a no-op. It only
    // matters once a capability can arrive from anywhere but the host's own
    // issuance path, which is exactly what the relay design contemplates.
    #[test]
    fn v2_moq_attachment_rejects_a_capability_minted_by_another_host() {
        let host = iroh::SecretKey::from_bytes(&[9; 32]);
        let impostor = iroh::SecretKey::from_bytes(&[31; 32]);
        let remote = endpoint(1);
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim_v2(remote, [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let forged = test_subscription(&impostor, remote, lease.session_id, 100, 200);
        let (_producer, consumer) = test_moq_broadcast();
        assert!(
            sessions
                .expect_moq_v2(
                    remote,
                    lease.session_id,
                    media_moq_broadcast_name(lease.session_id).unwrap(),
                    consumer,
                    forged,
                    *host.public().as_bytes(),
                )
                .is_err(),
            "a capability signed by another host must never be attached"
        );
    }

    #[test]
    fn v2_moq_attachment_requires_a_fresh_endpoint_bound_subscription() {
        let host = iroh::SecretKey::from_bytes(&[9; 32]);
        let remote = endpoint(1);
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim_v2(remote, [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let capability = test_subscription(&host, remote, lease.session_id, 100, 200);
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = sessions
            .expect_moq_v2(
                remote,
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
                capability,
                *host.public().as_bytes(),
            )
            .unwrap();
        assert!(sessions.claim_moq_at(endpoint(2), 150).is_err());
        assert!(sessions.claim_moq_at(remote, 150).is_ok());
        assert!(sessions.claim_moq_at(remote, 150).is_err());

        let expired_sessions = Arc::new(SessionRegistry::default());
        let expired_lease = expired_sessions
            .claim_v2(remote, [2; 16], InvitationGrants::VIEW)
            .unwrap();
        let expired = test_subscription(&host, remote, expired_lease.session_id, 100, 200);
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = expired_sessions
            .expect_moq_v2(
                remote,
                expired_lease.session_id,
                media_moq_broadcast_name(expired_lease.session_id).unwrap(),
                consumer,
                expired,
                *host.public().as_bytes(),
            )
            .unwrap();
        assert!(expired_sessions.claim_moq_at(remote, 201).is_err());
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
        let capacity = MAX_PENDING_HANDSHAKES * usize::from(crate::config::DEFAULT_MAX_VIEWERS);
        let permits: Vec<_> = (0..capacity)
            .map(|_| sessions.pending_handshakes.try_acquire().unwrap())
            .collect();
        assert!(sessions.pending_handshakes.try_acquire().is_err());
        drop(permits);
        assert!(sessions.pending_handshakes.try_acquire().is_ok());
    }

    #[test]
    fn per_viewer_reconnect_keyframe_and_feedback_rates_are_bounded() {
        let sessions = Arc::new(SessionRegistry::new(1));
        let remote = endpoint(1);
        let mut lease = sessions
            .claim_v2_authorized(
                remote,
                [1; 16],
                authorized_viewer("viewer-one", InvitationGrants::ALL),
            )
            .unwrap();
        for nonce in 2..=MAX_ADMISSIONS_PER_WINDOW {
            lease = sessions
                .claim_v2_authorized(
                    remote,
                    [u8::try_from(nonce).unwrap(); 16],
                    authorized_viewer("viewer-one", InvitationGrants::ALL),
                )
                .unwrap();
        }
        assert!(
            sessions
                .claim_v2_authorized(
                    remote,
                    [99; 16],
                    authorized_viewer("viewer-one", InvitationGrants::ALL),
                )
                .unwrap_err()
                .to_string()
                .contains("reconnect rate limit")
        );

        for _ in 0..MAX_KEYFRAME_REQUESTS_PER_WINDOW {
            sessions
                .admit_v2_keyframe_request(remote, lease.session_id)
                .unwrap();
        }
        assert!(
            sessions
                .admit_v2_keyframe_request(remote, lease.session_id)
                .unwrap_err()
                .to_string()
                .contains("keyframe request rate limit")
        );

        let active_nonce = [12; 16];
        for _ in 0..MAX_FEEDBACK_CLAIMS_PER_WINDOW {
            let feedback = sessions.claim_feedback(remote, active_nonce).unwrap();
            drop(feedback);
        }
        assert!(sessions.claim_feedback(remote, active_nonce).is_err());
    }

    #[test]
    fn runtime_status_reports_bounded_multi_viewer_focus_without_peer_keys() {
        let sessions = Arc::new(SessionRegistry::new(3));
        let first = sessions
            .claim_v2_authorized(
                endpoint(1),
                [1; 16],
                authorized_viewer("viewer-one", InvitationGrants::ALL),
            )
            .unwrap();
        let second = sessions
            .claim_v2_authorized(
                endpoint(2),
                [2; 16],
                authorized_viewer("viewer-two", InvitationGrants::VIEW),
            )
            .unwrap();
        let status = sessions.runtime_status();
        assert_eq!(status.mode, "moq_multi_viewer");
        assert_eq!(status.active_viewers, 2);
        assert_eq!(status.configured_capacity, 3);
        assert_eq!(status.focus_occupied, Some(false));
        sessions
            .apply_focus_command(
                endpoint(1),
                first.session_id,
                &FocusCommandV2 {
                    request_id: 1,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: second.initial_snapshot.revision,
                    expected_focus_generation: None,
                    expected_proposal_id: None,
                },
            )
            .unwrap();
        assert_eq!(sessions.runtime_status().focus_occupied, Some(true));
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
