use super::*;

const MOQ_ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const MOQ_REJECT_CODE: u32 = 0x534d;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoqGroupDecision {
    Published {
        group_id: u64,
        frame_id: u32,
        cancelled_previous: bool,
    },
    SkipUntilKeyframe,
    EnterResync,
}

/// Owns the single bounded live MoQ track. One configured H.264 GOP maps to
/// one native MoQ group; its application frame sequence remains inside the
/// encoded object envelope and is never reused as the transport group id.
pub(super) struct MoqGroupPublisher {
    track: TrackProducer,
    current_group: Option<GroupProducer>,
    cursor: MediaV3GroupCursor,
    object_bytes: usize,
    authentication: Option<MoqObjectAuthentication>,
}

struct MoqObjectAuthentication {
    generation_id: u64,
    signing_key: MediaGenerationSigningKey,
}

impl MoqGroupPublisher {
    fn new(track: TrackProducer) -> Self {
        Self {
            track,
            current_group: None,
            cursor: MediaV3GroupCursor::default(),
            object_bytes: 0,
            authentication: None,
        }
    }

    fn authenticated(
        track: TrackProducer,
        generation_id: u64,
        signing_key: MediaGenerationSigningKey,
    ) -> Self {
        Self {
            track,
            current_group: None,
            cursor: MediaV3GroupCursor::default(),
            object_bytes: 0,
            authentication: Some(MoqObjectAuthentication {
                generation_id,
                signing_key,
            }),
        }
    }

    fn publish(
        &mut self,
        config: &HostConfig,
        frame: &EncodedFrame,
        replay_discontinuity: bool,
    ) -> Result<MoqGroupDecision> {
        let position = match self.cursor.classify(frame) {
            MediaV3GroupDecision::Send(position) => position,
            MediaV3GroupDecision::SkipUntilKeyframe => {
                return Ok(MoqGroupDecision::SkipUntilKeyframe);
            }
            MediaV3GroupDecision::EnterResync => {
                self.abort_current();
                return Ok(MoqGroupDecision::EnterResync);
            }
        };
        let media_frame = media_frame_for_encoded(
            config,
            frame,
            replay_discontinuity || position.discontinuity,
        )?;
        let payload = encode_media_frame_object(&media_frame)?;

        if position.object_id == 0 {
            // A new independently-decodable GOP supersedes the previous one.
            // Actively aborting it cancels a slow subscriber rather than
            // retaining a playable history behind the live edge.
            let cancelled_previous = self.abort_current().is_some();
            let mut group = self
                .track
                .append_group()
                .context("creating sequential MoQ video group")?;
            let group_id = group.sequence;
            let object = self.authenticate_object(
                group_id,
                position.object_id,
                u16::from(media_frame.header.flags.bits()),
                payload,
            )?;
            let object_len = object.len();
            if object.len() > MAX_MEDIA_GROUP_BYTES_V3 {
                let _ = group.abort(MoqError::Cancel);
                self.cursor.request_keyframe();
                return Ok(MoqGroupDecision::EnterResync);
            }
            group
                .write_frame(object)
                .context("writing configured keyframe to MoQ group")?;
            self.object_bytes = object_len;
            self.current_group = Some(group);
            return Ok(MoqGroupDecision::Published {
                group_id,
                frame_id: 0,
                cancelled_previous,
            });
        }

        let group_id = self
            .current_group
            .as_ref()
            .map(|group| group.sequence)
            .context("MoQ delta frame has no active configured-keyframe group")?;
        let object = self.authenticate_object(
            group_id,
            position.object_id,
            u16::from(media_frame.header.flags.bits()),
            payload,
        )?;
        let Some(next_object_bytes) = self
            .object_bytes
            .checked_add(object.len())
            .filter(|bytes| *bytes <= MAX_MEDIA_GROUP_BYTES_V3)
        else {
            self.cursor.request_keyframe();
            self.abort_current();
            return Ok(MoqGroupDecision::EnterResync);
        };
        let group = self
            .current_group
            .as_mut()
            .context("MoQ delta frame lost its configured-keyframe group")?;
        group
            .write_frame(object)
            .context("writing delta access unit to MoQ group")?;
        self.object_bytes = next_object_bytes;
        Ok(MoqGroupDecision::Published {
            group_id,
            frame_id: position.object_id,
            cancelled_previous: false,
        })
    }

    fn authenticate_object(
        &self,
        group_id: u64,
        object_id: u32,
        flags: u16,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let Some(authentication) = &self.authentication else {
            return Ok(payload);
        };
        authentication
            .signing_key
            .authenticate(
                MediaObjectCoordinates {
                    generation_id: authentication.generation_id,
                    track: MediaTrack::VideoH264,
                    group_id,
                    object_id,
                    flags,
                },
                &payload,
            )
            .map(|object| object.into_bytes())
            .map_err(Into::into)
    }

    fn request_keyframe(&mut self) -> Option<u64> {
        self.cursor.request_keyframe();
        self.abort_current()
    }

    fn abort_current(&mut self) -> Option<u64> {
        self.object_bytes = 0;
        let mut group = self.current_group.take()?;
        let group_id = group.sequence;
        let _ = group.abort(MoqError::Cancel);
        Some(group_id)
    }

    fn abort(mut self) {
        self.abort_current();
        let _ = self.track.abort(MoqError::Cancel);
    }
}

const MOQ_AUDIO_PACKETS_PER_GROUP: usize = 5;
const MAX_MOQ_AUDIO_GROUP_BYTES: usize =
    MOQ_AUDIO_PACKETS_PER_GROUP * (AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD_LEN + 256);

struct MoqAudioPublisher {
    track: TrackProducer,
    current_group: Option<GroupProducer>,
    object_count: usize,
    object_bytes: usize,
    generation_id: u64,
    signing_key: MediaGenerationSigningKey,
}

impl MoqAudioPublisher {
    fn new(
        track: TrackProducer,
        generation_id: u64,
        signing_key: MediaGenerationSigningKey,
    ) -> Self {
        Self {
            track,
            current_group: None,
            object_count: 0,
            object_bytes: 0,
            generation_id,
            signing_key,
        }
    }

    fn publish(&mut self, packet: crate::audio::EncodedAudioPacket) -> Result<()> {
        let flags = if packet.discontinuity {
            AudioFlags::DISCONTINUITY
        } else {
            AudioFlags::NONE
        };
        let payload = AudioPacket::new(
            AudioPacketHeader::opus(
                packet.payload.len(),
                packet.sequence,
                packet.capture_timestamp_us,
                packet.pts_us,
                flags,
            )?,
            packet.payload.as_ref().to_vec(),
        )?
        .encode_datagram()?;

        if self.current_group.is_none()
            || self.object_count >= MOQ_AUDIO_PACKETS_PER_GROUP
            || packet.discontinuity
        {
            self.finish_group()?;
            self.current_group = Some(
                self.track
                    .append_group()
                    .context("creating bounded MoQ Opus group")?,
            );
        }
        let group = self
            .current_group
            .as_mut()
            .context("Opus packet has no active MoQ group")?;
        let object_id = u32::try_from(self.object_count).context("audio object id overflowed")?;
        let object = self.signing_key.authenticate(
            MediaObjectCoordinates {
                generation_id: self.generation_id,
                track: MediaTrack::AudioOpus,
                group_id: group.sequence,
                object_id,
                flags: u16::from(flags.bits()),
            },
            &payload,
        )?;
        let next_bytes = self
            .object_bytes
            .checked_add(object.as_bytes().len())
            .filter(|bytes| *bytes <= MAX_MOQ_AUDIO_GROUP_BYTES)
            .context("bounded MoQ Opus group exceeded its byte limit")?;
        group
            .write_frame(object.into_bytes())
            .context("writing authenticated Opus object")?;
        self.object_count += 1;
        self.object_bytes = next_bytes;
        if self.object_count == MOQ_AUDIO_PACKETS_PER_GROUP {
            self.finish_group()?;
        }
        Ok(())
    }

    fn finish_group(&mut self) -> Result<()> {
        if let Some(mut group) = self.current_group.take() {
            group.finish().context("finishing bounded MoQ Opus group")?;
        }
        self.object_count = 0;
        self.object_bytes = 0;
        Ok(())
    }

    fn abort(mut self) {
        if let Some(mut group) = self.current_group.take() {
            let _ = group.abort(MoqError::Cancel);
        }
        let _ = self.track.abort(MoqError::Cancel);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_generation_video_publisher(
    config: HostConfig,
    mut current_gop_receiver: tokio::sync::watch::Receiver<Option<EncodedGop>>,
    mut control_requests: tokio::sync::watch::Receiver<Option<MediaControlRequestV3>>,
    track: TrackProducer,
    generation_id: u64,
    signing_key: MediaGenerationSigningKey,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut publisher = MoqGroupPublisher::authenticated(track, generation_id, signing_key);
    let mut forced_idr = ForcedIdrCoordinator::new(encoder_control, Arc::clone(&telemetry));
    let log_identity = iroh::SecretKey::from_bytes(&[1; 32]).public();

    let result = async {
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return Ok(());
                    }
                }
                changed = control_requests.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                    let Some(request) = *control_requests.borrow_and_update() else {
                        continue;
                    };
                    let through_sequence = current_gop_receiver
                        .borrow()
                        .as_ref()
                        .and_then(|gop| gop.frames.last())
                        .map(|frame| frame.sequence);
                    let cancelled_group = apply_moq_keyframe_request(
                        &mut publisher,
                        &mut replay_cursor,
                        through_sequence,
                        request.reason,
                    );
                    if cancelled_group.is_some() {
                        telemetry.scheduler_cancellations.fetch_add(1, Ordering::Relaxed);
                    }
                    let disposition = forced_idr.request(request.reason);
                    debug!(generation_id, request_id = request.request_id, ?disposition, ?cancelled_group, "shared generation accepted keyframe request");
                }
                acknowledgement = forced_idr.acknowledgements.join_next(),
                    if forced_idr.pending_revision.is_some() =>
                {
                    forced_idr.complete(acknowledgement, log_identity, "shared-iroh-moq");
                }
                changed = current_gop_receiver.changed() => {
                    changed.context("shared encoded video source stopped")?;
                    let Some(current_gop) = current_gop_receiver.borrow_and_update().clone() else {
                        continue;
                    };
                    publish_generation_gop(
                        &config,
                        &mut publisher,
                        &mut replay_cursor,
                        current_gop,
                        maximum_replay_age,
                        &telemetry,
                    )?;
                }
            }
        }
    }
    .await;
    forced_idr
        .abort_and_drain(log_identity, "shared-iroh-moq")
        .await;
    publisher.abort();
    result
}

fn publish_generation_gop(
    config: &HostConfig,
    publisher: &mut MoqGroupPublisher,
    replay_cursor: &mut MediaReplayCursor,
    current_gop: EncodedGop,
    maximum_replay_age: Duration,
    telemetry: &MediaV3Telemetry,
) -> Result<()> {
    let initial_replay_started_at = replay_cursor.last_sequence.is_none().then(Instant::now);
    let replay_through_sequence = current_gop
        .frames
        .last()
        .map(|frame| frame.sequence)
        .context("shared current GOP snapshot is empty")?;
    for frame in new_current_gop_frames(current_gop, replay_cursor.last_sequence) {
        let replay_discontinuity = match replay_cursor.classify(
            &frame,
            replay_through_sequence,
            initial_replay_started_at,
            Instant::now(),
            maximum_replay_age,
        ) {
            MediaReplayDecision::Send { discontinuity } => discontinuity,
            MediaReplayDecision::SkipUntilKeyframe
            | MediaReplayDecision::DiscardStaleSuffix { .. } => {
                if publisher.request_keyframe().is_some() {
                    telemetry
                        .scheduler_cancellations
                        .fetch_add(1, Ordering::Relaxed);
                }
                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                break;
            }
        };
        match publisher
            .publish(config, &frame, replay_discontinuity)
            .inspect_err(|_| {
                telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
            })? {
            MoqGroupDecision::Published {
                cancelled_previous, ..
            } => {
                if cancelled_previous {
                    telemetry
                        .scheduler_cancellations
                        .fetch_add(1, Ordering::Relaxed);
                }
                replay_cursor.commit_sent(&frame);
            }
            MoqGroupDecision::SkipUntilKeyframe | MoqGroupDecision::EnterResync => {
                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                break;
            }
        }
    }
    Ok(())
}

pub(super) async fn run_generation_audio_publisher(
    mut packets: tokio::sync::mpsc::Receiver<crate::audio::EncodedAudioPacket>,
    track: TrackProducer,
    generation_id: u64,
    signing_key: MediaGenerationSigningKey,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut publisher = MoqAudioPublisher::new(track, generation_id, signing_key);
    let result = async {
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() {
                        return Ok(());
                    }
                }
                packet = packets.recv() => {
                    let packet = packet.context("shared encoded Opus source stopped")?;
                    publisher.publish(packet)?;
                }
            }
        }
    }
    .await;
    publisher.abort();
    result
}

fn apply_moq_keyframe_request(
    publisher: &mut MoqGroupPublisher,
    replay_cursor: &mut MediaReplayCursor,
    through_sequence: Option<u64>,
    reason: KeyframeRequestReasonV3,
) -> Option<u64> {
    // The bounded current group is already the late joiner's decodable replay.
    // Aborting it on Join can strand a static source until its next natural IDR.
    if reason == KeyframeRequestReasonV3::Join {
        return None;
    }
    let cancelled_group = publisher.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    cancelled_group
}

pub(super) async fn serve_authorized_moq(
    connection: Connection,
    origin: Origin,
    attachment: ClaimedMoqAttachment,
) -> Result<()> {
    let ClaimedMoqAttachment {
        session_id,
        broadcast_name,
        broadcast,
        attached,
        closed,
        telemetry,
    } = attachment;
    let result: Result<()> = async {
        let web_transport = web_transport_iroh::Session::raw(connection);
        let session = tokio::time::timeout(
            MOQ_ATTACHMENT_TIMEOUT,
            iroh_moq::MoqSession::session_accept(web_transport, origin),
        )
        .await
        .context("timed out completing authorized MoQ handshake")?
        .context("completing authorized MoQ handshake")?;
        let broadcast_closed = broadcast.clone();
        session.publish(&broadcast_name, broadcast);
        ensure!(
            attached.send(()).is_ok(),
            "control session ended before MoQ attachment completed"
        );
        info!(
            remote = %session.remote_id(),
            session_id,
            %broadcast_name,
            track = MOQ_VIDEO_H264_TRACK,
            "authorized MoQ media attachment accepted"
        );
        let mut telemetry_interval = tokio::time::interval(Duration::from_secs(1));
        telemetry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                reason = session.closed() => {
                    debug!(remote = %session.remote_id(), ?reason, "MoQ media session closed");
                    break;
                }
                reason = broadcast_closed.closed() => {
                    debug!(remote = %session.remote_id(), ?reason, "control-owned MoQ broadcast closed");
                    session.close(0, b"control session ended");
                    break;
                }
                _ = telemetry_interval.tick() => {
                    telemetry.record_selected_path(session.conn());
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = closed.send(());
    result
}

pub(super) async fn serve_control_moq(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
    authorization: &AuthorizationPolicy,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting MoQ control stream")?
        .context("accepting MoQ control stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "MoQ control hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing MoQ control peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized MoQ control peer lacks view permission"
    );
    let lease = match sessions.claim(remote, hello.nonce, grants) {
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
        encoder_control,
    } = match source {
        Ok(source) => source,
        Err(error) => {
            send_rejection(&mut send, "video source is unavailable").await?;
            return Err(error);
        }
    };
    let source_task = SourceTaskGuard::new(source_task);
    sessions.install_encoder_control(remote, lease.session_id, encoder_control.clone())?;

    let mut broadcast = Broadcast::new().produce();
    let track = broadcast
        .create_track(Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        })
        .context("creating static MoQ H.264 track")?;
    let catalog = publish_goq_catalog(&mut broadcast)?;
    let broadcast_name = media_moq_broadcast_name(lease.session_id)?;
    let attachment = sessions.expect_moq(
        remote,
        lease.session_id,
        broadcast_name.clone(),
        broadcast.consume(),
    )?;

    let mut control_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        control_hello = control_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &control_hello).await?;
    send.finish().context("finishing MoQ control response")?;
    drop(send);
    info!(
        %remote,
        session_id = lease.session_id,
        %broadcast_name,
        "MoQ control client accepted; awaiting authorized media attachment"
    );

    let MoqAttachmentWait {
        mut attached,
        closed,
    } = attachment;
    tokio::time::timeout(MOQ_ATTACHMENT_TIMEOUT, async {
        tokio::select! {
            result = &mut attached => {
                result.context("authorized MoQ handler ended before attachment")
            }
            reason = connection.closed() => {
                Err(anyhow::anyhow!("control connection closed before MoQ attachment: {reason:?}"))
            }
        }
    })
    .await
    .context("timed out waiting for authorized MoQ attachment")??;

    let session_result = run_control_moq_session(
        &connection,
        &config,
        &mut current_gop_receiver,
        MoqControlReader::V1(recv),
        remote,
        closed,
        track,
        &mut broadcast,
        encoder_control,
        Arc::clone(&lease.media_v3_telemetry),
        None,
    )
    .await;
    let catalog_result = catalog.finish();

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "MoQ control client released");
    match session_result {
        Err(error) => Err(error),
        Ok(()) => catalog_result,
    }
}

pub(super) async fn serve_control_moq_v2(
    connection: Connection,
    sessions: &Arc<SessionRegistry>,
    generations: &Arc<MediaGenerationManager>,
    authorization: &AuthorizationPolicy,
    input_operations: &Arc<InputOperations>,
    host_secret: [u8; 32],
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting MoQ control v2 stream")?
        .context("accepting MoQ control v2 stream")?;
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_client_hello_v2(&mut recv))
        .await
        .context("timed out waiting for control v2 hello")??
        .context("client closed before control v2 hello")?;
    drop(handshake_permit);
    ensure!(
        hello.capabilities.contains(&Capability::VideoH264),
        "client did not offer required capability VideoH264"
    );
    debug!(%remote, agent = %hello.agent, "MoQ control v2 hello received");

    let authorized = match authorization.authorize_or_redeem_viewer(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(authorized) => authorized,
        Err(error) => {
            send_rejection_v2(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing MoQ control v2 peer"));
        }
    };
    ensure!(
        authorized.grants.contains(InvitationGrants::VIEW),
        "authorized MoQ control v2 peer lacks view permission"
    );
    let mut lease = match sessions.claim_v2_authorized(remote, hello.nonce, authorized) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection_v2(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };

    let generation = match generations.acquire().await {
        Ok(generation) => generation,
        Err(error) => {
            send_rejection_v2(&mut send, "shared media generation is unavailable").await?;
            return Err(error);
        }
    };
    let shared = Arc::clone(&generation.shared);
    let mut initial_snapshot = sessions.bind_v2_generation(
        remote,
        lease.session_id,
        shared.generation_id,
        shared.session_clock,
        Arc::clone(&shared.telemetry),
        shared.encoder_control.clone(),
    )?;
    if let Some(transition) = lease.replacement_transition {
        input_operations.neutralize_focus_transition(
            sessions,
            transition.transition_id,
            FocusTransitionReasonV2::Replaced,
        )?;
        initial_snapshot = sessions
            .subscribe_v2_snapshots(remote, lease.session_id)?
            .borrow()
            .clone()
            .context("replacement completion did not publish a session snapshot")?;
    }
    // Keep the predecessor's generation lease alive until this viewer has
    // acquired the shared generation, then retire only that replaced control
    // stream. Survivors and the producer remain untouched.
    lease.retire_replaced_viewer();
    let issued_at_unix = unix_timestamp_now()?;
    let mut subscription_nonce = [0_u8; 32];
    getrandom::fill(&mut subscription_nonce).context("generating subscription capability nonce")?;
    let subscription_tracks = if shared.audio_enabled {
        SubscriptionTracks::ALL
    } else {
        SubscriptionTracks::VIDEO_H264
    };
    let subscription_capability = SignedSubscriptionCapability::issue(
        SubscriptionClaims::new(
            shared.certificate.claims.host_node_id,
            shared.generation_id,
            *remote.as_bytes(),
            subscription_tracks,
            lease.authorization_revision,
            issued_at_unix,
            issued_at_unix.saturating_add(15 * 60),
            subscription_nonce,
            1,
        )?,
        &host_secret,
    )?;
    let subscription_token = subscription_capability.encode();

    let broadcast_name = shared.broadcast_name.clone();
    let attachment = sessions.expect_moq_v2(
        remote,
        lease.session_id,
        broadcast_name.clone(),
        shared.consumer()?,
        subscription_capability,
    )?;

    let mut supported = vec![Capability::VideoH264];
    if shared.audio_enabled {
        supported.push(Capability::AudioOpus);
    }
    let negotiated = supported
        .iter()
        .copied()
        .filter(|capability| hello.capabilities.contains(capability))
        .collect();
    let mut control_hello = HostHelloV2::accepted(lease.session_id, negotiated, initial_snapshot)
        .with_media_subscription_capability(subscription_token);
    if let Some(dimensions) = shared.pointer_surface_dimensions {
        control_hello = control_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello_v2(&mut send, &control_hello).await?;
    info!(
        %remote,
        session_id = lease.session_id,
        authorization_revision = lease.authorization_revision,
        authorization_committed_revision = lease.authorization_committed_revision,
        %broadcast_name,
        "MoQ control v2 client accepted; awaiting authorized media attachment"
    );

    let MoqAttachmentWait {
        mut attached,
        closed,
    } = attachment;
    tokio::time::timeout(MOQ_ATTACHMENT_TIMEOUT, async {
        tokio::select! {
            result = &mut attached => result.context("authorized MoQ handler ended before v2 attachment"),
            reason = connection.closed() => Err(anyhow::anyhow!("control v2 connection closed before MoQ attachment: {reason:?}")),
        }
    })
    .await
    .context("timed out waiting for authorized MoQ v2 attachment")??;

    let session_result = run_generation_control_session(
        &connection,
        send,
        recv,
        Arc::clone(sessions),
        Arc::clone(input_operations),
        remote,
        lease.session_id,
        closed,
        shared.keyframe_requests.clone(),
    )
    .await;

    if let Some(transition) = sessions.invalidate_v2_focus(
        remote,
        lease.session_id,
        FocusTransitionReasonV2::Disconnected,
    )? {
        input_operations.neutralize_focus_transition(
            sessions,
            transition.transition_id,
            FocusTransitionReasonV2::Disconnected,
        )?;
    }
    drop(lease);
    generation.release().await;
    info!(%remote, "MoQ control v2 client released");
    session_result
}

#[allow(clippy::too_many_arguments)]
async fn run_generation_control_session(
    connection: &Connection,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    sessions: Arc<SessionRegistry>,
    input_operations: Arc<InputOperations>,
    remote: EndpointId,
    session_id: u64,
    mut moq_closed: tokio::sync::oneshot::Receiver<()>,
    keyframes: tokio::sync::watch::Sender<Option<MediaControlRequestV3>>,
) -> Result<()> {
    let mut control = tokio::spawn(forward_control_v2_requests(
        send,
        recv,
        sessions,
        input_operations,
        remote,
        session_id,
        keyframes,
    ));
    tokio::select! {
        result = &mut control => result.context("shared-generation control task failed")?,
        reason = connection.closed() => {
            debug!(%remote, ?reason, "shared-generation control connection closed");
            Ok(())
        }
        result = &mut moq_closed => {
            debug!(%remote, ?result, "shared-generation MoQ attachment closed");
            Ok(())
        }
    }
}

enum MoqControlReader {
    V1(iroh::endpoint::RecvStream),
    V2 {
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        sessions: Arc<SessionRegistry>,
        input_operations: Arc<InputOperations>,
        session_id: u64,
    },
}

async fn forward_control_v2_requests(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    sessions: Arc<SessionRegistry>,
    input_operations: Arc<InputOperations>,
    remote: EndpointId,
    session_id: u64,
    keyframes: tokio::sync::watch::Sender<Option<MediaControlRequestV3>>,
) -> Result<()> {
    let mut snapshots = sessions.subscribe_v2_snapshots(remote, session_id)?;
    let mut focus_deadlines = tokio::time::interval(Duration::from_millis(250));
    focus_deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = focus_deadlines.tick() => {
                if let Some(transition) = sessions.expire_v2_focus()? {
                    input_operations.neutralize_focus_transition(
                        &sessions,
                        transition.transition_id,
                        FocusTransitionReasonV2::ActivationExpired,
                    )?;
                }
            }
            changed = snapshots.changed() => {
                changed.context("v2 session snapshot publisher stopped")?;
                let Some(snapshot) = snapshots.borrow_and_update().clone() else {
                    break;
                };
                write_server_control_v2(
                    &mut send,
                    &ServerControlEnvelopeV2::Snapshot { snapshot },
                )
                .await?;
            }
            command = read_client_control_v2(&mut recv) => {
                let Some(command) = command? else { break; };
                match command {
                    ClientControlEnvelopeV2::Focus { command } => {
                        let request_id = command.request_id;
                        let outcome = sessions.apply_focus_command(remote, session_id, &command);
                        let result = match outcome {
                            Ok(effect) => {
                                if let Some(transition) = effect.neutralization {
                                    input_operations.neutralize_focus_transition(
                                        &sessions,
                                        transition.transition_id,
                                        effect.snapshot.transition_reason,
                                    )?;
                                }
                                FocusCommandResultV2 {
                                    request_id,
                                    accepted: true,
                                    revision: sessions.v2_revision(remote, session_id)?,
                                    message: None,
                                }
                            }
                            Err(error) => FocusCommandResultV2 {
                                request_id,
                                accepted: false,
                                revision: sessions.v2_revision(remote, session_id)?,
                                message: Some(error.to_string()),
                            },
                        };
                        write_server_control_v2(
                            &mut send,
                            &ServerControlEnvelopeV2::FocusResult { result },
                        )
                        .await?;
                    }
                    ClientControlEnvelopeV2::Keyframe {
                        request_id,
                        last_sequence,
                        reason,
                    } => {
                        keyframes.send_replace(Some(MediaControlRequestV3::request_keyframe(
                            request_id,
                            last_sequence,
                            reason.into(),
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_control_moq_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    control_reader: MoqControlReader,
    remote: EndpointId,
    mut moq_closed: tokio::sync::oneshot::Receiver<()>,
    track: TrackProducer,
    broadcast: &mut BroadcastProducer,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
    authentication: Option<(u64, MediaGenerationSigningKey)>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut publisher = match authentication {
        Some((generation_id, signing_key)) => {
            MoqGroupPublisher::authenticated(track, generation_id, signing_key)
        }
        None => MoqGroupPublisher::new(track),
    };
    let (control_sender, mut control_requests) = tokio::sync::watch::channel(None);
    let terminate_when_control_ends = matches!(&control_reader, MoqControlReader::V2 { .. });
    let mut control_task = match control_reader {
        MoqControlReader::V1(control_recv) => tokio::spawn(forward_media_v3_control_requests(
            control_recv,
            control_sender,
        )),
        MoqControlReader::V2 {
            send,
            recv,
            sessions,
            input_operations,
            session_id,
        } => tokio::spawn(forward_control_v2_requests(
            send,
            recv,
            sessions,
            input_operations,
            remote,
            session_id,
            control_sender,
        )),
    };
    let mut control_task_finished = false;
    let mut control_receiver_open = true;
    let mut forced_idr = ForcedIdrCoordinator::new(encoder_control, Arc::clone(&telemetry));

    let result = async {
        loop {
            tokio::select! {
                biased;
                control_result = &mut control_task, if !control_task_finished => {
                    control_task_finished = true;
                    match control_result {
                        Ok(Ok(())) => {
                            debug!(%remote, "MoQ keyframe-control stream closed");
                            if terminate_when_control_ends {
                                return Ok(());
                            }
                        }
                        Ok(Err(error)) => {
                            return Err(error).context("reading MoQ keyframe-control stream");
                        }
                        Err(error) => {
                            return Err(error).context("MoQ keyframe-control task failed");
                        }
                    }
                }
                changed = control_requests.changed(), if control_receiver_open => {
                    if changed.is_err() {
                        control_receiver_open = false;
                        continue;
                    }
                    let Some(request) = *control_requests.borrow_and_update() else {
                        continue;
                    };
                    let through_sequence = current_gop_receiver
                        .borrow()
                        .as_ref()
                        .and_then(|gop| gop.frames.last())
                        .map(|frame| frame.sequence);
                    let cancelled_group = apply_moq_keyframe_request(
                        &mut publisher,
                        &mut replay_cursor,
                        through_sequence,
                        request.reason,
                    );
                    if cancelled_group.is_some() {
                        telemetry
                            .scheduler_cancellations
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let forced_idr_disposition = forced_idr.request(request.reason);
                    if let ForcedIdrDisposition::Failed { error } = &forced_idr_disposition {
                        warn!(
                            %remote,
                            request_id = request.request_id,
                            ?request.reason,
                            %error,
                            "forced-IDR request failed; retaining natural-IDR fallback"
                        );
                    }
                    debug!(
                        %remote,
                        request_id = request.request_id,
                        ?request.reason,
                        advisory_last_sequence = ?request.last_sequence,
                        coalesced = cancelled_group.is_none(),
                        ?cancelled_group,
                        ?forced_idr_disposition,
                        "accepted MoQ keyframe request"
                    );
                }
                acknowledgement = forced_idr.acknowledgements.join_next(),
                    if forced_idr.pending_revision.is_some() =>
                {
                    forced_idr.complete(acknowledgement, remote, "iroh-moq");
                }
                reason = connection.closed() => {
                    debug!(%remote, ?reason, "MoQ control connection closed");
                    return Ok(());
                }
                result = &mut moq_closed => {
                    debug!(%remote, ?result, "authorized MoQ media attachment closed");
                    return Ok(());
                }
                changed = current_gop_receiver.changed() => {
                    if let Err(error) = changed {
                        return Err(error).context("encoded source stopped");
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
                            MediaReplayDecision::SkipUntilKeyframe => {
                                if publisher.request_keyframe().is_some() {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                            MediaReplayDecision::DiscardStaleSuffix { through_sequence } => {
                                let cancelled_group = publisher.request_keyframe();
                                if cancelled_group.is_some() {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                debug!(
                                    %remote,
                                    through_sequence,
                                    ?cancelled_group,
                                    "cancelled stale MoQ media suffix"
                                );
                                break;
                            }
                        };
                        let decision = publisher
                            .publish(config, &frame, replay_discontinuity)
                            .inspect_err(|_error| {
                                telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            })?;
                        match decision {
                            MoqGroupDecision::Published {
                                group_id,
                                frame_id,
                                cancelled_previous,
                            } => {
                                if cancelled_previous {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                debug!(
                                    sequence = frame.sequence,
                                    group_id,
                                    frame_id,
                                    cancelled_previous,
                                    "published upstream MoQ video frame"
                                );
                                replay_cursor.commit_sent(&frame);
                            }
                            MoqGroupDecision::SkipUntilKeyframe => {
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                            MoqGroupDecision::EnterResync => {
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    .await;

    forced_idr.abort_and_drain(remote, "iroh-moq").await;
    publisher.abort();
    let _ = broadcast.abort(MoqError::Cancel);
    if !control_task_finished {
        control_task.abort();
        let _ = control_task.await;
    }
    result
}

pub(super) fn media_frame_for_encoded(
    _config: &HostConfig,
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
    if discontinuity || frame.discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let header = MediaFrameHeader::h264(
        frame.width,
        frame.height,
        frame.data.len(),
        frame.sequence,
        frame.capture_timestamp_micros,
        frame.presentation_timestamp_micros,
        flags,
    )?;
    MediaFrame::new(header, frame.data.clone()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::super::{media_v3_encoded_frame, moq_test_config};
    use super::*;

    fn authenticated_publisher(
        track: TrackProducer,
    ) -> (
        MoqGroupPublisher,
        sigil_protocol::SignedMediaGenerationCertificate,
    ) {
        let host = iroh::SecretKey::from_bytes(&[7; 32]);
        let signing_key = MediaGenerationSigningKey::from_bytes(&[9; 32]);
        let certificate = signing_key
            .certify(
                *host.public().as_bytes(),
                &host.to_bytes(),
                42,
                1_700_000_000,
                1_700_000_600,
            )
            .unwrap();
        (
            MoqGroupPublisher::authenticated(track, 42, signing_key),
            certificate,
        )
    }

    #[tokio::test]
    async fn upstream_moq_groups_are_sequential_and_cancel_the_superseded_gop() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();

        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(100, true, true, 4), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 0,
                cancelled_previous: false,
            }
        );
        let mut first_group = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(first_group.sequence, 0);
        assert!(first_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            publisher
                .publish(
                    &config,
                    &media_v3_encoded_frame(101, false, false, 4),
                    false,
                )
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 1,
                cancelled_previous: false,
            }
        );
        assert!(first_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(200, true, true, 4), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 1,
                frame_id: 0,
                cancelled_previous: true,
            }
        );
        assert!(first_group.finished().await.is_err());
        let mut second_group = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(second_group.sequence, 1);
        let object = second_group.read_frame().await.unwrap().unwrap();
        let frame = sigil_protocol::decode_media_frame_object(&object).unwrap();
        assert_eq!(frame.header.sequence, 200);
        assert!(frame.header.flags.contains(FrameFlags::DISCONTINUITY));
    }

    #[tokio::test]
    async fn control_v2_objects_are_signed_after_final_moq_coordinates_are_known() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let (mut publisher, certificate) = authenticated_publisher(track);
        let config = moq_test_config();
        let encoded = media_v3_encoded_frame(100, true, true, 4);
        publisher.publish(&config, &encoded, false).unwrap();
        let mut group = consumer.recv_group().await.unwrap().unwrap();
        let object = group.read_frame().await.unwrap().unwrap();
        let payload = sigil_protocol::AuthenticatedMediaObject::verify(
            &object,
            &certificate,
            MediaObjectCoordinates {
                generation_id: 42,
                track: MediaTrack::VideoH264,
                group_id: group.sequence,
                object_id: 0,
                flags: u16::from(FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG).bits()),
            },
        )
        .unwrap();
        assert_eq!(
            sigil_protocol::decode_media_frame_object(payload)
                .unwrap()
                .header
                .sequence,
            100
        );
    }

    #[tokio::test]
    async fn upstream_moq_late_join_preserves_active_static_group() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let mut replay_cursor = MediaReplayCursor::default();
        let config = moq_test_config();
        let keyframe = media_v3_encoded_frame(10, true, true, 1);
        let first_delta = media_v3_encoded_frame(11, false, false, 1);
        let next_delta = media_v3_encoded_frame(12, false, false, 1);

        publisher.publish(&config, &keyframe, false).unwrap();
        replay_cursor.commit_sent(&keyframe);
        let mut active_group = consumer.recv_group().await.unwrap().unwrap();
        assert!(active_group.read_frame().await.unwrap().is_some());

        publisher.publish(&config, &first_delta, false).unwrap();
        replay_cursor.commit_sent(&first_delta);
        assert!(active_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            apply_moq_keyframe_request(
                &mut publisher,
                &mut replay_cursor,
                Some(first_delta.sequence),
                KeyframeRequestReasonV3::Join,
            ),
            None
        );
        assert_eq!(replay_cursor.last_sequence, Some(first_delta.sequence));
        assert!(!replay_cursor.waiting_for_keyframe);
        assert_eq!(
            publisher.publish(&config, &next_delta, false).unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 2,
                cancelled_previous: false,
            }
        );
        assert!(active_group.read_frame().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn upstream_moq_resync_aborts_current_group_and_waits_for_configured_idr() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();

        publisher
            .publish(&config, &media_v3_encoded_frame(10, true, true, 1), false)
            .unwrap();
        let mut cancelled = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(publisher.request_keyframe(), Some(0));
        assert!(cancelled.finished().await.is_err());
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(11, false, false, 1), false,)
                .unwrap(),
            MoqGroupDecision::SkipUntilKeyframe
        );
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(20, true, true, 1), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 1,
                frame_id: 0,
                cancelled_previous: false,
            }
        );
    }

    #[tokio::test]
    async fn upstream_moq_group_counts_envelope_bytes_before_upstream_cache_eviction() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();
        publisher
            .publish(&config, &media_v3_encoded_frame(10, true, true, 1), false)
            .unwrap();
        let mut cancelled = consumer.recv_group().await.unwrap().unwrap();

        // Payload-only accounting would accept this next one-byte access unit,
        // but its fixed application envelope would overflow moq-net's 32 MiB
        // group cache and silently evict the keyframe.
        publisher.object_bytes = MAX_MEDIA_GROUP_BYTES_V3 - 1;
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(11, false, false, 1), false,)
                .unwrap(),
            MoqGroupDecision::EnterResync
        );
        assert!(cancelled.finished().await.is_err());
        assert_eq!(publisher.object_bytes, 0);
    }
}
