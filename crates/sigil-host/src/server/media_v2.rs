use super::*;

const MEDIA_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_V2_PEER_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_V2_IN_FLIGHT_CAPACITY: usize = 4;
const MEDIA_V2_KEYFRAME_PRIORITY: i32 = 10;
const MEDIA_V2_DELTA_PRIORITY: i32 = 0;
const MEDIA_V2_RESET_CODE: u32 = 0x5356;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaActivity {
    SourceChanged,
    PeerDisconnected,
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

pub(super) async fn serve_media(
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
        .context("timed out accepting media stream")?
        .context("accepting media stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized media peer lacks view permission"
    );

    let lease = match sessions.claim(remote, hello.nonce, grants) {
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
    let _encoder_control = encoder_control;

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
                let media_frame = media_frame_for_encoded(&config, &frame, discontinuity)?;
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

pub(super) async fn serve_media_v2(
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
        .context("timed out accepting media v2 handshake stream")?
        .context("accepting media v2 handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media v2 hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media v2 peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized media peer lacks view permission"
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
    let _encoder_control = encoder_control;

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

#[cfg(test)]
mod tests {
    use super::super::{DropNotify, endpoint};
    use super::*;

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

    #[tokio::test]
    async fn no_frame_peer_drop_reaps_source_and_allows_reconnect() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [7; 16];
        let media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
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

        assert!(
            sessions
                .claim(endpoint(2), [8; 16], InvitationGrants::ALL)
                .is_ok()
        );
    }
}
