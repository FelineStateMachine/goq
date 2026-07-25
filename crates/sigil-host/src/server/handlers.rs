use std::sync::Mutex;

use super::*;

pub(super) const MEDIA_CAPABILITIES: &[Capability] = &[Capability::VideoH264];
const AUDIO_CAPABILITIES: &[Capability] = &[Capability::AudioOpus];
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const REJECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[must_use = "the guard must remain armed for the lifetime of the input session"]
struct InputSessionGuard<L, F>
where
    F: FnOnce() -> Result<()>,
{
    _lease: L,
    reset: Option<F>,
}

impl<L, F> InputSessionGuard<L, F>
where
    F: FnOnce() -> Result<()>,
{
    fn new(lease: L, reset: F) -> Self {
        Self {
            _lease: lease,
            reset: Some(reset),
        }
    }

    fn finish(mut self) -> Result<()> {
        self.run()
    }

    fn run(&mut self) -> Result<()> {
        let Some(reset) = self.reset.take() else {
            return Ok(());
        };
        reset()
    }
}

impl<L, F> Drop for InputSessionGuard<L, F>
where
    F: FnOnce() -> Result<()>,
{
    fn drop(&mut self) {
        if let Err(error) = self.run() {
            error!(%error, "failed to release held input transitions while dropping session");
        }
    }
}

#[derive(Clone, Debug)]
pub struct MediaV3Handler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct ControlHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct ControlV2Handler {
    pub sessions: Arc<SessionRegistry>,
    pub generations: Arc<MediaGenerationManager>,
    pub authorization: AuthorizationPolicy,
    pub input_operations: Arc<InputOperations>,
    pub host_secret: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct MediaFeedbackHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

/// Upstream MoQ admission guarded by an already-authenticated control lease.
///
/// This deliberately does not use `iroh_moq::Moq::protocol_handler`: that
/// actor makes a completed session globally visible before application-level
/// acceptance. Consuming the exact pending attachment first prevents MoQ from
/// bypassing Sigil's invitation, enrollment, and one-client gate.
#[derive(Clone, Debug)]
pub struct AuthorizedMoqHandler {
    pub sessions: Arc<SessionRegistry>,
    pub origin: Origin,
}

impl ProtocolHandler for MediaV3Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_v3(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media v3 connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for ControlHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_control_moq(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "MoQ control connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for MediaFeedbackHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_feedback(
            connection,
            &self.config,
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media feedback connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for AuthorizedMoqHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        let attachment = match self.sessions.claim_moq(remote) {
            Ok(attachment) => attachment,
            Err(error) => {
                connection.close(MOQ_REJECT_CODE.into(), b"unauthorized MoQ attachment");
                warn!(%remote, %error, "rejected unsolicited MoQ connection");
                return Ok(());
            }
        };
        if let Err(error) = serve_authorized_moq(connection, self.origin, attachment).await {
            warn!(%remote, %error, "authorized MoQ connection ended");
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

#[derive(Debug)]
pub struct InputOperations {
    backend: InputBackend,
    operation: Mutex<()>,
}

impl InputOperations {
    pub fn new(backend: InputBackend) -> Self {
        Self {
            backend,
            operation: Mutex::new(()),
        }
    }

    fn capabilities(&self) -> &'static [Capability] {
        self.backend.capabilities()
    }

    fn apply_v2(
        &self,
        sessions: &SessionRegistry,
        remote: EndpointId,
        binding: &InputEventV2,
        negotiated: &[Capability],
    ) -> Result<InputDisposition> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| anyhow::anyhow!("input operation lock poisoned"))?;
        ensure!(
            sessions.is_v2_focus_owner(
                remote,
                binding.session_id,
                binding.slot,
                binding.focus_generation,
            ),
            "input focus changed before backend application"
        );
        self.backend.apply(&binding.event, negotiated)
    }

    pub(crate) fn reset(&self) -> Result<()> {
        let _operation = match self.operation.lock() {
            Ok(operation) => operation,
            Err(poisoned) => {
                warn!("recovering poisoned input operation lock for reset");
                poisoned.into_inner()
            }
        };
        self.backend.reset_session()
    }

    pub(crate) fn neutralize_focus_transition(
        &self,
        sessions: &SessionRegistry,
        transition_id: u64,
        reason: FocusTransitionReasonV2,
    ) -> Result<()> {
        info!(
            transition_id,
            ?reason,
            "focus transition entered no-inject neutralizing state"
        );
        self.reset()?;
        info!(
            transition_id,
            ?reason,
            "focus transition virtual input neutralized"
        );
        sessions.complete_v2_focus_transition(transition_id)?;
        info!(
            transition_id,
            ?reason,
            "focus transition successor committed after neutralization"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InputV2Handler {
    pub operations: Arc<InputOperations>,
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

impl ProtocolHandler for ControlV2Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_control_moq_v2(
            connection,
            &self.sessions,
            &self.generations,
            &self.authorization,
            &self.input_operations,
            self.host_secret,
        )
        .await
        {
            warn!(%remote, %error, "MoQ control v2 connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for InputV2Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_input_v2(
            connection,
            &self.operations,
            self.pointer_positions.as_ref(),
            &self.sessions,
        )
        .await
        {
            warn!(%remote, %error, "input v2 connection ended");
        }
        Ok(())
    }
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
    ensure!(
        hello.invitation.is_none(),
        "invitations are accepted only on the first media handshake"
    );
    let lease = match sessions.claim_input(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    let session_id = lease.session_id;
    let grants = lease.grants;
    let authorization_revision = lease.authorization_revision;
    // Owning the lease guarantees cancellation or unwinding neutralizes the
    // virtual devices before another input stream can be admitted.
    let session_guard = InputSessionGuard::new(lease, || backend.reset_session());
    let supported =
        supported_input_capabilities(backend.capabilities(), pointer_positions.is_some());
    let negotiated = negotiated_input_capabilities(&hello, &supported, grants);

    let ack_enabled = negotiated.contains(&Capability::InputAck);
    let feedback_enabled = negotiated.contains(&Capability::PointerPositionFeedback);
    let visibility_feedback_enabled = negotiated.contains(&Capability::PointerVisibilityFeedback);
    let mut pointer_positions = pointer_positions
        .filter(|_| feedback_enabled)
        .map(PointerPositionTracker::subscribe);
    write_host_hello(
        &mut send,
        &HostHello::accepted(session_id, negotiated.clone()),
    )
    .await?;
    info!(%remote, session_id, authorization_revision, "input client accepted");

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
            if !sessions.is_active(remote, session_id) {
                debug!(%remote, session_id, "media session ended; closing input");
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
                    if !sessions.is_active(remote, session_id) {
                        debug!(%remote, session_id, "discarding input after media ended");
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
    let reset_result = session_guard
        .finish()
        .context("releasing held input transitions at session end");
    if session_result.is_err()
        && let Err(error) = &reset_result
    {
        error!(%error, "input session and held-transition release both failed");
    }
    let result = session_result.and(reset_result);
    info!(%remote, "input client released");
    result
}

async fn serve_input_v2(
    connection: Connection,
    operations: &InputOperations,
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
        .context("timed out accepting input v2 stream")?
        .context("accepting input v2 stream")?;
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_input_client_hello_v2(&mut recv))
        .await
        .context("timed out waiting for input v2 hello")??
        .context("client closed before input v2 hello")?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, session_id = hello.session_id, focus_generation = hello.focus_generation, "input v2 hello received");
    let lease = match sessions.claim_input_v2(
        remote,
        hello.nonce,
        hello.session_id,
        hello.slot,
        hello.focus_generation,
    ) {
        Ok(lease) => lease,
        Err(error) => {
            write_input_host_hello_v2(&mut send, &InputHostHelloV2::rejected(error.to_string()))
                .await?;
            send.finish()?;
            return Err(error);
        }
    };
    let supported =
        supported_input_capabilities(operations.capabilities(), pointer_positions.is_some());
    let negotiated = negotiated_input_capabilities_v2(&hello, &supported, lease.grants);
    let ack_enabled = negotiated.contains(&Capability::InputAck);
    let feedback_enabled = negotiated.contains(&Capability::PointerPositionFeedback);
    let visibility_feedback_enabled = negotiated.contains(&Capability::PointerVisibilityFeedback);
    let mut pointer_positions = pointer_positions
        .filter(|_| feedback_enabled)
        .map(PointerPositionTracker::subscribe);
    write_input_host_hello_v2(
        &mut send,
        &InputHostHelloV2::accepted(
            lease.session_id,
            lease.slot,
            lease.focus_generation,
            negotiated.clone(),
        ),
    )
    .await?;
    info!(%remote, session_id = lease.session_id, focus_generation = lease.focus_generation, authorization_revision = lease.authorization_revision, "input v2 client accepted");

    let session_id = lease.session_id;
    let slot = lease.slot;
    let focus_generation = lease.focus_generation;
    let guard = InputSessionGuard::new(lease, || operations.reset());
    let session_result: Result<()> = async {
        let mut received_events = 0_u64;
        if let Some(pointer_positions) = pointer_positions.as_ref() {
            let (pointer_position, pointer_visible) = pointer_feedback_fields(
                Some(*pointer_positions.borrow()),
                visibility_feedback_enabled,
            );
            write_input_ack_v2(
                &mut send,
                &InputAckV2 {
                    session_id,
                    slot,
                    focus_generation,
                    ack: InputAck {
                        sequence: 0,
                        pointer_position,
                        pointer_visible,
                    },
                },
            )
            .await?;
        }
        loop {
            if !sessions.is_v2_focus_owner(remote, session_id, slot, focus_generation) {
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
                    let state = *pointer_positions
                        .as_mut()
                        .expect("feedback branch is guarded")
                        .borrow_and_update();
                    let (pointer_position, pointer_visible) =
                        pointer_feedback_fields(Some(state), visibility_feedback_enabled);
                    write_input_ack_v2(
                        &mut send,
                        &InputAckV2 {
                            session_id,
                            slot,
                            focus_generation,
                            ack: InputAck {
                                sequence: received_events,
                                pointer_position,
                                pointer_visible,
                            },
                        },
                    )
                    .await?;
                }
                event = read_input_event_v2(&mut recv) => {
                    let Some(event) = event? else { break; };
                    ensure!(
                        event.session_id == session_id
                            && event.slot == slot
                            && event.focus_generation == focus_generation,
                        "input event changed its accepted focus binding"
                    );
                    let disposition = operations.apply_v2(sessions, remote, &event, &negotiated)?;
                    match disposition {
                        InputDisposition::Probed => debug!(%remote, "input v2 liveness probe acknowledged"),
                        InputDisposition::Observed => info!(%remote, event_type = input_event_type(&event.event), "input v2 event observed"),
                        InputDisposition::Disabled => debug!(%remote, event_type = input_event_type(&event.event), "input v2 event ignored because injection is disabled"),
                        #[cfg(target_os = "linux")]
                        InputDisposition::Injected => debug!(%remote, event_type = input_event_type(&event.event), "input v2 event injected"),
                        InputDisposition::TextIgnored => debug!(%remote, "input v2 text event ignored"),
                    }
                    received_events = received_events.saturating_add(1);
                    if ack_enabled {
                        let pointer_state = pointer_positions.as_ref().map(|positions| *positions.borrow());
                        let (pointer_position, pointer_visible) =
                            pointer_feedback_fields(pointer_state, visibility_feedback_enabled);
                        write_input_ack_v2(
                            &mut send,
                            &InputAckV2 {
                                session_id,
                                slot,
                                focus_generation,
                                ack: InputAck {
                                    sequence: received_events,
                                    pointer_position,
                                    pointer_visible,
                                },
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    let reset_result = guard.finish().context("neutralizing input v2 session");
    let result = session_result.and(reset_result);
    info!(%remote, session_id, focus_generation, "input v2 client released");
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
    ensure!(
        hello.invitation.is_none(),
        "invitations are accepted only on the first media handshake"
    );

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
    ensure!(
        lease.grants.contains(InvitationGrants::VIEW),
        "active Portal session lacks audio view permission"
    );
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
    info!(%remote, session_id = lease.session_id, authorization_revision = lease.authorization_revision, "audio client accepted");

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

pub(super) async fn send_rejection(
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

pub(super) async fn send_rejection_v2(
    send: &mut iroh::endpoint::SendStream,
    message: impl Into<String>,
) -> Result<()> {
    write_host_hello_v2(send, &HostHelloV2::rejected(message)).await?;
    send.finish()?;
    if tokio::time::timeout(REJECTION_DRAIN_TIMEOUT, send.stopped())
        .await
        .is_err()
    {
        debug!("timed out waiting for peer to acknowledge control v2 rejection");
    }
    Ok(())
}

pub(super) fn negotiated_capabilities(
    hello: &ClientHello,
    supported: &[Capability],
) -> Vec<Capability> {
    supported
        .iter()
        .copied()
        .filter(|capability| hello.capabilities.contains(capability))
        .collect()
}

fn negotiated_input_capabilities(
    hello: &ClientHello,
    supported: &[Capability],
    grants: InvitationGrants,
) -> Vec<Capability> {
    let mut negotiated = negotiated_capabilities(hello, supported);
    negotiated.retain(|capability| input_capability_authorized(*capability, grants));
    if !negotiated.contains(&Capability::PointerPositionFeedback) {
        negotiated.retain(|capability| *capability != Capability::PointerVisibilityFeedback);
    }
    negotiated
}

fn negotiated_input_capabilities_v2(
    hello: &InputClientHelloV2,
    supported: &[Capability],
    grants: InvitationGrants,
) -> Vec<Capability> {
    let mut negotiated = supported
        .iter()
        .copied()
        .filter(|capability| hello.capabilities.contains(capability))
        .collect::<Vec<_>>();
    negotiated.retain(|capability| input_capability_authorized(*capability, grants));
    if !negotiated.contains(&Capability::PointerPositionFeedback) {
        negotiated.retain(|capability| *capability != Capability::PointerVisibilityFeedback);
    }
    negotiated
}

fn input_capability_authorized(capability: Capability, grants: InvitationGrants) -> bool {
    match capability {
        Capability::AbsolutePointer
        | Capability::RelativePointer
        | Capability::Keyboard
        | Capability::Text
        | Capability::PointerPositionFeedback
        | Capability::PointerVisibilityFeedback => {
            grants.contains(InvitationGrants::POINTER_KEYBOARD)
        }
        Capability::Gamepad => grants.contains(InvitationGrants::GAMEPAD),
        Capability::InputAck => {
            grants.contains(InvitationGrants::POINTER_KEYBOARD)
                || grants.contains(InvitationGrants::GAMEPAD)
        }
        Capability::VideoH264 | Capability::AudioOpus => false,
    }
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

pub(super) async fn receive_hello<R>(reader: &mut R, required: Capability) -> Result<ClientHello>
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn input_session_guard_resets_before_releasing_lease_on_drop() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [1; 16];
        let _media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let resets = Cell::new(0);
        let reset_saw_lease_held = Cell::new(false);

        {
            let _guard = InputSessionGuard::new(input, || {
                resets.set(resets.get() + 1);
                reset_saw_lease_held.set(sessions.claim_input(remote, nonce).is_err());
                Ok(())
            });
        }

        assert_eq!(resets.get(), 1);
        assert!(reset_saw_lease_held.get());
        assert!(sessions.claim_input(remote, nonce).is_ok());
    }

    #[test]
    fn input_session_guard_finish_propagates_reset_error_once() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [2; 16];
        let _media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let resets = Cell::new(0);

        let error = InputSessionGuard::new(input, || {
            resets.set(resets.get() + 1);
            Err(anyhow::anyhow!("reset failed"))
        })
        .finish()
        .unwrap_err();

        assert_eq!(error.to_string(), "reset failed");
        assert_eq!(resets.get(), 1);
        assert!(sessions.claim_input(remote, nonce).is_ok());
    }

    #[test]
    fn input_session_guard_resets_before_releasing_lease_during_unwind() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [3; 16];
        let _media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let reset_saw_lease_held = Cell::new(false);

        let panic_result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = InputSessionGuard::new(input, || {
                reset_saw_lease_held.set(sessions.claim_input(remote, nonce).is_err());
                Ok(())
            });
            panic!("simulate input task panic");
        }));

        assert!(panic_result.is_err());
        assert!(reset_saw_lease_held.get());
        assert!(sessions.claim_input(remote, nonce).is_ok());
    }

    #[tokio::test]
    async fn input_session_guard_resets_before_releasing_lease_when_task_is_cancelled() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [4; 16];
        let _media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let reset_saw_lease_held = Arc::new(AtomicBool::new(false));
        let task_sessions = Arc::clone(&sessions);
        let task_reset_saw_lease_held = Arc::clone(&reset_saw_lease_held);
        let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(async move {
            let _guard = InputSessionGuard::new(input, move || {
                task_reset_saw_lease_held.store(
                    task_sessions.claim_input(remote, nonce).is_err(),
                    Ordering::SeqCst,
                );
                Ok(())
            });
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });

        armed_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(reset_saw_lease_held.load(Ordering::SeqCst));
        assert!(sessions.claim_input(remote, nonce).is_ok());
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
    fn enrollment_grants_are_a_strict_input_capability_ceiling() {
        let hello = ClientHello::new(
            "test",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::InputAck,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ],
        );
        let supported = hello.capabilities.clone();

        assert!(
            negotiated_input_capabilities(&hello, &supported, InvitationGrants::VIEW).is_empty()
        );
        assert_eq!(
            negotiated_input_capabilities(
                &hello,
                &supported,
                InvitationGrants::VIEW.union(InvitationGrants::POINTER_KEYBOARD),
            ),
            vec![
                Capability::RelativePointer,
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::InputAck,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ]
        );
        assert_eq!(
            negotiated_input_capabilities(
                &hello,
                &supported,
                InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
            ),
            vec![Capability::Gamepad, Capability::InputAck]
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
        let negotiated = negotiated_input_capabilities(&hello, &supported, InvitationGrants::ALL);
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
        assert!(
            negotiated_input_capabilities(&visibility_only, &supported, InvitationGrants::ALL)
                .is_empty()
        );

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
            negotiated_input_capabilities(&upgraded, &supported, InvitationGrants::ALL),
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
}
