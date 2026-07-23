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
struct MoqGroupPublisher {
    track: TrackProducer,
    current_group: Option<GroupProducer>,
    cursor: MediaV3GroupCursor,
    object_bytes: usize,
}

impl MoqGroupPublisher {
    fn new(track: TrackProducer) -> Self {
        Self {
            track,
            current_group: None,
            cursor: MediaV3GroupCursor::default(),
            object_bytes: 0,
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
        let object = encode_media_frame_object(&media_frame_for_encoded(
            config,
            frame,
            replay_discontinuity || position.discontinuity,
        )?)?;
        let next_object_bytes = if position.object_id == 0 {
            Some(object.len())
        } else {
            self.object_bytes.checked_add(object.len())
        };
        let Some(next_object_bytes) =
            next_object_bytes.filter(|bytes| *bytes <= MAX_MEDIA_GROUP_BYTES_V3)
        else {
            self.cursor.request_keyframe();
            self.abort_current();
            return Ok(MoqGroupDecision::EnterResync);
        };

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
            group
                .write_frame(object)
                .context("writing configured keyframe to MoQ group")?;
            self.object_bytes = next_object_bytes;
            self.current_group = Some(group);
            return Ok(MoqGroupDecision::Published {
                group_id,
                frame_id: 0,
                cancelled_previous,
            });
        }

        let group = self
            .current_group
            .as_mut()
            .context("MoQ delta frame has no active configured-keyframe group")?;
        let group_id = group.sequence;
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
        recv,
        remote,
        closed,
        track,
        &mut broadcast,
        encoder_control,
        Arc::clone(&lease.media_v3_telemetry),
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

#[allow(clippy::too_many_arguments)]
async fn run_control_moq_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    control_recv: iroh::endpoint::RecvStream,
    remote: EndpointId,
    mut moq_closed: tokio::sync::oneshot::Receiver<()>,
    track: TrackProducer,
    broadcast: &mut BroadcastProducer,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut publisher = MoqGroupPublisher::new(track);
    let (control_sender, mut control_requests) = tokio::sync::watch::channel(None);
    let mut control_task = tokio::spawn(forward_media_v3_control_requests(
        control_recv,
        control_sender,
    ));
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
    MediaFrame::new(header, frame.data.as_ref().to_vec()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::super::{media_v3_encoded_frame, moq_test_config};
    use super::*;

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
