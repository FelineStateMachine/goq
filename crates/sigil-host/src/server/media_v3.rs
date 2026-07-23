use super::*;

const MEDIA_V3_IN_FLIGHT_CAPACITY: usize = 4;
const MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY: u8 = 0;
const MEDIA_V3_DELTA_PUBLISHER_PRIORITY: u8 = 128;
const MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY: i32 = 10;
const MEDIA_V3_DELTA_TRANSPORT_PRIORITY: i32 = 0;
// Wire-visible application reset code. Keep stable across compatible v3 releases.
const MEDIA_V3_RESET_CODE: u32 = 0x5357;
const MEDIA_V3_DELIVERY_FRAME_PERIODS: u64 = 4;
const MEDIA_V3_MAX_CONTROL_REQUESTS_PER_SECOND: u32 = 10;
const MEDIA_V3_CONTROL_REQUEST_INTERVAL: Duration =
    Duration::from_millis(1_000 / MEDIA_V3_MAX_CONTROL_REQUESTS_PER_SECOND as u64);

#[derive(Debug)]
pub(super) struct MediaV3GroupCursor {
    group_id: Option<u64>,
    last_sequence: Option<u64>,
    last_object_id: Option<u32>,
    payload_bytes: usize,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV3GroupCursor {
    fn default() -> Self {
        Self {
            group_id: None,
            last_sequence: None,
            last_object_id: None,
            payload_bytes: 0,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV3GroupCursor {
    pub(super) fn classify(&mut self, frame: &EncodedFrame) -> MediaV3GroupDecision {
        let independently_decodable = frame.keyframe && frame.codec_config;
        if independently_decodable {
            if frame.data.len() > MAX_MEDIA_GROUP_BYTES_V3 {
                self.enter_resync();
                return MediaV3GroupDecision::EnterResync;
            }
            let sequence_discontinuity = self
                .last_sequence
                .is_some_and(|last| last.checked_add(1) != Some(frame.sequence));
            let position = MediaV3ObjectPosition {
                group_id: frame.sequence,
                object_id: 0,
                discontinuity: self.discontinuity_pending || sequence_discontinuity,
            };
            self.group_id = Some(frame.sequence);
            self.last_sequence = Some(frame.sequence);
            self.last_object_id = Some(0);
            self.payload_bytes = frame.data.len();
            self.waiting_for_keyframe = false;
            self.discontinuity_pending = false;
            return MediaV3GroupDecision::Send(position);
        }

        if self.waiting_for_keyframe {
            return MediaV3GroupDecision::SkipUntilKeyframe;
        }
        if frame.keyframe || frame.codec_config {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        }
        let Some(group_id) = self.group_id else {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        };
        let contiguous =
            self.last_sequence.and_then(|last| last.checked_add(1)) == Some(frame.sequence);
        let object_id = self.last_object_id.and_then(|last| last.checked_add(1));
        let payload_bytes = self.payload_bytes.checked_add(frame.data.len());
        let (Some(object_id), Some(payload_bytes)) = (object_id, payload_bytes) else {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        };
        if !contiguous
            || object_id > MAX_MEDIA_OBJECT_ID_V3
            || payload_bytes > MAX_MEDIA_GROUP_BYTES_V3
        {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        }

        self.last_sequence = Some(frame.sequence);
        self.last_object_id = Some(object_id);
        self.payload_bytes = payload_bytes;
        MediaV3GroupDecision::Send(MediaV3ObjectPosition {
            group_id,
            object_id,
            discontinuity: false,
        })
    }

    pub(super) fn request_keyframe(&mut self) {
        self.enter_resync();
    }

    fn enter_resync(&mut self) {
        self.group_id = None;
        self.last_object_id = None;
        self.payload_bytes = 0;
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MediaV3ScheduleDecision {
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
struct MediaV3Scheduler {
    in_flight: Vec<u64>,
    last_scheduled_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV3Scheduler {
    fn default() -> Self {
        Self {
            in_flight: Vec::with_capacity(MEDIA_V3_IN_FLIGHT_CAPACITY),
            last_scheduled_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV3Scheduler {
    fn schedule(
        &mut self,
        sequence: u64,
        independently_decodable: bool,
    ) -> MediaV3ScheduleDecision {
        if self
            .last_scheduled_sequence
            .is_some_and(|last| sequence <= last)
        {
            return MediaV3ScheduleDecision::SkipUntilKeyframe;
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
            return MediaV3ScheduleDecision::Send {
                discontinuity,
                cancel_sequences,
            };
        }

        if self.waiting_for_keyframe {
            return MediaV3ScheduleDecision::SkipUntilKeyframe;
        }
        if sequence_discontinuity || self.in_flight.len() == MEDIA_V3_IN_FLIGHT_CAPACITY {
            return MediaV3ScheduleDecision::EnterResync {
                cancel_sequences: self.enter_resync(),
            };
        }

        self.in_flight.push(sequence);
        self.last_scheduled_sequence = Some(sequence);
        MediaV3ScheduleDecision::Send {
            discontinuity: false,
            cancel_sequences: Vec::new(),
        }
    }

    fn complete(&mut self, sequence: u64) {
        if let Some(index) = self.in_flight.iter().position(|value| *value == sequence) {
            self.in_flight.swap_remove(index);
        }
    }

    fn fail(&mut self, sequence: u64) -> Option<Vec<u64>> {
        let index = self.in_flight.iter().position(|value| *value == sequence)?;
        self.in_flight.swap_remove(index);
        Some(self.enter_resync())
    }

    fn fail_all(&mut self) -> Vec<u64> {
        self.enter_resync()
    }

    fn request_keyframe(&mut self) -> Vec<u64> {
        if self.waiting_for_keyframe {
            self.discontinuity_pending = true;
            return Vec::new();
        }
        self.enter_resync()
    }

    fn enter_resync(&mut self) -> Vec<u64> {
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
        std::mem::take(&mut self.in_flight)
    }
}

fn apply_media_v3_keyframe_request(
    scheduler: &mut MediaV3Scheduler,
    group_cursor: &mut MediaV3GroupCursor,
    replay_cursor: &mut MediaReplayCursor,
    through_sequence: Option<u64>,
    reason: KeyframeRequestReasonV3,
) -> (bool, Vec<u64>) {
    // Every v3 session already begins by replaying the bounded current GOP
    // from object zero. A Join arriving after that replay was scheduled must
    // not cancel the only decodable image on a damage-driven static source.
    if reason == KeyframeRequestReasonV3::Join || scheduler.waiting_for_keyframe {
        return (false, Vec::new());
    }
    let cancel_sequences = scheduler.request_keyframe();
    group_cursor.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    (true, cancel_sequences)
}

fn apply_media_v3_send_failure(
    scheduler: &mut MediaV3Scheduler,
    group_cursor: &mut MediaV3GroupCursor,
    replay_cursor: &mut MediaReplayCursor,
    sequence: u64,
    through_sequence: Option<u64>,
) -> Option<Vec<u64>> {
    let cancel_sequences = scheduler.fail(sequence)?;
    group_cursor.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    Some(cancel_sequences)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaV3ControlDecision {
    Accept,
    Pace(Duration),
}

#[derive(Debug, Default)]
struct MediaV3ControlGate {
    last_request_id: Option<u64>,
    last_accepted_at: Option<Instant>,
}

impl MediaV3ControlGate {
    fn accept(
        &mut self,
        request: MediaControlRequestV3,
        now: Instant,
    ) -> Result<MediaV3ControlDecision> {
        ensure!(
            self.last_request_id
                .is_none_or(|last| request.request_id > last),
            "v3 media control request IDs must be strictly increasing"
        );
        self.last_request_id = Some(request.request_id);
        if let Some(last) = self.last_accepted_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < MEDIA_V3_CONTROL_REQUEST_INTERVAL {
                return Ok(MediaV3ControlDecision::Pace(
                    MEDIA_V3_CONTROL_REQUEST_INTERVAL - elapsed,
                ));
            }
        }
        self.last_accepted_at = Some(now);
        Ok(MediaV3ControlDecision::Accept)
    }
}

pub(super) async fn forward_media_v3_control_requests<R>(
    mut reader: R,
    sender: tokio::sync::watch::Sender<Option<MediaControlRequestV3>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut gate = MediaV3ControlGate::default();
    while let Some(request) = read_media_control_request_v3(&mut reader).await? {
        match gate.accept(request, Instant::now())? {
            MediaV3ControlDecision::Accept => {}
            MediaV3ControlDecision::Pace(retry_after) => {
                // Stop reading during the rejection interval so QUIC flow
                // control, rather than this task, absorbs an abusive burst.
                // This also bounds rejection logging to the configured rate.
                debug!(
                    request_id = request.request_id,
                    retry_after_ms = retry_after.as_millis(),
                    "paced rate-limited v3 keyframe request"
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }
        }
        sender.send_replace(Some(request));
        if sender.is_closed() {
            return Ok(());
        }
    }
    Ok(())
}

struct ResetOnDropSendStreamV3(Option<SendStream>);

impl ResetOnDropSendStreamV3 {
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

impl Drop for ResetOnDropSendStreamV3 {
    fn drop(&mut self) {
        if let Some(stream) = self.0.as_mut() {
            let _ = stream.reset(MEDIA_V3_RESET_CODE.into());
        }
    }
}

pub(super) async fn serve_media_v3(
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
        .context("timed out accepting media v3 handshake stream")?
        .context("accepting media v3 handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media v3 hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media v3 peer"));
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

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    send.finish()
        .context("finishing media v3 handshake response")?;
    drop(send);
    info!(%remote, session_id = lease.session_id, "media v3 client accepted");

    let session_result = run_media_v3_session(
        &connection,
        &config,
        &mut current_gop_receiver,
        recv,
        remote,
        encoder_control,
        Arc::clone(&lease.media_v3_telemetry),
    )
    .await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media v3 client released");
    session_result
}

async fn run_media_v3_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    control_recv: iroh::endpoint::RecvStream,
    remote: EndpointId,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let delivery_timeout_ms = media_v3_delivery_timeout_ms(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut group_cursor = MediaV3GroupCursor::default();
    let mut scheduler = MediaV3Scheduler::default();
    let mut send_tasks = tokio::task::JoinSet::new();
    let (control_sender, mut control_requests) = tokio::sync::watch::channel(None);
    let mut control_task = tokio::spawn(forward_media_v3_control_requests(
        control_recv,
        control_sender,
    ));
    let mut control_task_finished = false;
    let mut control_receiver_open = true;
    let mut forced_idr = ForcedIdrCoordinator::new(encoder_control, Arc::clone(&telemetry));

    let result = loop {
        telemetry.record_selected_path(connection);
        tokio::select! {
            biased;
            control_result = &mut control_task, if !control_task_finished => {
                control_task_finished = true;
                match control_result {
                    Ok(Ok(())) => {
                        // Keep polling the watch receiver after clean EOF so a
                        // final request published immediately before the
                        // sender closed cannot be lost.
                        debug!(%remote, "media v3 control stream closed");
                    }
                    Ok(Err(error)) => {
                        break Err(error).context("reading media v3 control stream");
                    }
                    Err(error) => {
                        break Err(error).context("media v3 control task failed");
                    }
                }
            }
            changed = control_requests.changed(), if control_receiver_open => {
                if changed.is_err() {
                    control_receiver_open = false;
                    continue;
                }
                let request = *control_requests.borrow_and_update();
                let Some(request) = request else {
                    continue;
                };
                let through_sequence = current_gop_receiver
                    .borrow()
                    .as_ref()
                    .and_then(|gop| gop.frames.last())
                    .map(|frame| frame.sequence);
                let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
                    &mut scheduler,
                    &mut group_cursor,
                    &mut replay_cursor,
                    through_sequence,
                    request.reason,
                );
                if !cancel_sequences.is_empty() {
                    telemetry.scheduler_cancellations.fetch_add(
                        u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    send_tasks.abort_all();
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
                    coalesced = !transitioned,
                    ?cancel_sequences,
                    ?forced_idr_disposition,
                    "accepted media v3 keyframe request"
                );
            }
            acknowledgement = forced_idr.acknowledgements.join_next(),
                if forced_idr.pending_revision.is_some() =>
            {
                forced_idr.complete(acknowledgement, remote, "grouped-v3");
            }
            closed = connection.closed() => {
                debug!(%remote, ?closed, "media v3 connection closed");
                break Ok(());
            }
            task = send_tasks.join_next(), if !send_tasks.is_empty() => {
                match task.expect("guarded by non-empty send task set") {
                    Ok((sequence, Ok(()))) => scheduler.complete(sequence),
                    Ok((sequence, Err(error))) => {
                        let through_sequence = current_gop_receiver
                            .borrow()
                            .as_ref()
                            .and_then(|gop| gop.frames.last())
                            .map(|frame| frame.sequence);
                        if let Some(cancel_sequences) = apply_media_v3_send_failure(
                            &mut scheduler,
                            &mut group_cursor,
                            &mut replay_cursor,
                            sequence,
                            through_sequence,
                        ) {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            warn!(
                                sequence,
                                %error,
                                "media v3 object send failed; waiting for keyframe"
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                        } else {
                            debug!(
                                sequence,
                                %error,
                                "ignored stale media v3 object failure from a superseded group"
                            );
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        warn!(%error, "media v3 object task failed; waiting for keyframe");
                        let cancel_sequences = scheduler.fail_all();
                        telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                        telemetry.scheduler_cancellations.fetch_add(
                            u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                            Ordering::Relaxed,
                        );
                        group_cursor.request_keyframe();
                        let through_sequence = current_gop_receiver
                            .borrow()
                            .as_ref()
                            .and_then(|gop| gop.frames.last())
                            .map(|frame| frame.sequence);
                        replay_cursor.enter_resync_through(through_sequence);
                        if !cancel_sequences.is_empty() {
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
                        MediaReplayDecision::SkipUntilKeyframe => {
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            break;
                        }
                        MediaReplayDecision::DiscardStaleSuffix { .. } => {
                            group_cursor.request_keyframe();
                            let cancel_sequences = scheduler.fail_all();
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let position = match group_cursor.classify(&frame) {
                        MediaV3GroupDecision::Send(position) => position,
                        MediaV3GroupDecision::SkipUntilKeyframe => {
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            break;
                        }
                        MediaV3GroupDecision::EnterResync => {
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            let cancel_sequences = scheduler.fail_all();
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let independently_decodable = position.object_id == 0;
                    let (scheduler_discontinuity, cancel_sequences) =
                        match scheduler.schedule(frame.sequence, independently_decodable) {
                            MediaV3ScheduleDecision::Send {
                                discontinuity,
                                cancel_sequences,
                            } => (discontinuity, cancel_sequences),
                            MediaV3ScheduleDecision::SkipUntilKeyframe => continue,
                            MediaV3ScheduleDecision::EnterResync { cancel_sequences } => {
                                group_cursor.request_keyframe();
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                telemetry.scheduler_cancellations.fetch_add(
                                    u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                    Ordering::Relaxed,
                                );
                                if !cancel_sequences.is_empty() {
                                    send_tasks.abort_all();
                                }
                                break;
                            }
                        };
                    if !cancel_sequences.is_empty() {
                        telemetry.scheduler_cancellations.fetch_add(
                            u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                            Ordering::Relaxed,
                        );
                        debug!(
                            sequence = frame.sequence,
                            group_id = position.group_id,
                            ?cancel_sequences,
                            "configured keyframe superseding media v3 objects"
                        );
                        send_tasks.abort_all();
                    }

                    let media_object = media_v3_object_for_encoded(
                        config,
                        &frame,
                        position,
                        replay_discontinuity
                            || position.discontinuity
                            || scheduler_discontinuity,
                        delivery_timeout_ms,
                    )?;
                    let sequence = frame.sequence;
                    let stream = match tokio::time::timeout(
                        Duration::from_millis(u64::from(delivery_timeout_ms)),
                        connection.open_uni(),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            warn!(sequence, %error, "opening media v3 object stream failed");
                            if let Some(cancel_sequences) = apply_media_v3_send_failure(
                                &mut scheduler,
                                &mut group_cursor,
                                &mut replay_cursor,
                                sequence,
                                Some(replay_through_sequence),
                            ) && !cancel_sequences.is_empty()
                            {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                        Err(_) => {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            warn!(sequence, "opening media v3 object stream timed out");
                            if let Some(cancel_sequences) = apply_media_v3_send_failure(
                                &mut scheduler,
                                &mut group_cursor,
                                &mut replay_cursor,
                                sequence,
                                Some(replay_through_sequence),
                            ) && !cancel_sequences.is_empty()
                            {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let stream = ResetOnDropSendStreamV3::new(stream);
                    let transport_priority =
                        media_v3_transport_priority(media_object.header.publisher_priority)?;
                    if let Err(error) = stream.stream().set_priority(transport_priority) {
                        telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                        warn!(sequence, %error, "setting media v3 object priority failed");
                        if let Some(cancel_sequences) = apply_media_v3_send_failure(
                            &mut scheduler,
                            &mut group_cursor,
                            &mut replay_cursor,
                            sequence,
                            Some(replay_through_sequence),
                        ) && !cancel_sequences.is_empty()
                        {
                            send_tasks.abort_all();
                        }
                        break;
                    }
                    send_tasks.spawn(async move {
                        (
                            sequence,
                            send_media_v3_object(stream, media_object).await,
                        )
                    });
                    replay_cursor.commit_sent(&frame);
                }
            }
        }
    };

    forced_idr.abort_and_drain(remote, "grouped-v3").await;
    send_tasks.abort_all();
    while send_tasks.join_next().await.is_some() {}
    if !control_task_finished {
        control_task.abort();
        let _ = control_task.await;
    }
    result
}

fn media_v3_delivery_timeout_ms(framerate: u32) -> u32 {
    debug_assert!(framerate > 0);
    let timeout_ms = 1_000_u64
        .saturating_mul(MEDIA_V3_DELIVERY_FRAME_PERIODS)
        .div_ceil(u64::from(framerate.max(1)));
    u32::try_from(timeout_ms).unwrap_or(u32::MAX).clamp(
        MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
        MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    )
}

fn media_v3_object_for_encoded(
    _config: &HostConfig,
    frame: &EncodedFrame,
    position: MediaV3ObjectPosition,
    discontinuity: bool,
    delivery_timeout_ms: u32,
) -> Result<MediaObjectV3> {
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
    let publisher_priority = if position.object_id == 0 {
        MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY
    } else {
        MEDIA_V3_DELTA_PUBLISHER_PRIORITY
    };
    let header = MediaObjectHeaderV3::h264(
        frame.width,
        frame.height,
        frame.data.len(),
        publisher_priority,
        flags,
        position.object_id,
        position.group_id,
        frame.sequence,
        frame.capture_timestamp_micros,
        frame.presentation_timestamp_micros,
        delivery_timeout_ms,
    )?;
    MediaObjectV3::new(header, frame.data.clone()).map_err(Into::into)
}

async fn send_media_v3_object(
    mut stream: ResetOnDropSendStreamV3,
    object: MediaObjectV3,
) -> Result<()> {
    let delivery_timeout = Duration::from_millis(u64::from(object.header.delivery_timeout_ms));
    tokio::time::timeout(delivery_timeout, async {
        write_media_object_v3(stream.stream_mut(), &object)
            .await
            .context("writing media v3 object")?;
        stream
            .stream_mut()
            .finish()
            .context("finishing media v3 object stream")?;
        match stream
            .stream()
            .stopped()
            .await
            .context("waiting for media v3 object acknowledgement")?
        {
            None => {
                stream.disarm();
                Ok(())
            }
            Some(code) => bail!("peer stopped media v3 object stream with code {code}"),
        }
    })
    .await
    .context("media v3 object exceeded its delivery timeout")?
}

fn media_v3_transport_priority(publisher_priority: u8) -> Result<i32> {
    // V3 follows MoQ's lower-is-higher publisher priority, while Iroh/QUIC
    // uses larger integers for more important streams. Keep the inversion
    // explicit rather than leaking either convention across the boundary.
    match publisher_priority {
        MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY => Ok(MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY),
        MEDIA_V3_DELTA_PUBLISHER_PRIORITY => Ok(MEDIA_V3_DELTA_TRANSPORT_PRIORITY),
        _ => bail!("unsupported media v3 publisher priority {publisher_priority}"),
    }
}

pub(super) fn new_current_gop_frames(
    current_gop: EncodedGop,
    last_sequence: Option<u64>,
) -> impl Iterator<Item = EncodedFrame> {
    current_gop
        .frames
        .into_iter()
        .skip_while(move |frame| last_sequence.is_some_and(|last| frame.sequence <= last))
}

#[cfg(test)]
mod tests {
    use super::super::media_v3_encoded_frame;
    use super::*;

    #[test]
    fn media_v3_groups_begin_at_configured_idr_and_assign_contiguous_objects() {
        let mut cursor = MediaV3GroupCursor::default();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(10, true, true, 4)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 10,
                object_id: 0,
                discontinuity: false,
            })
        );
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(11, false, false, 3)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 10,
                object_id: 1,
                discontinuity: false,
            })
        );

        cursor.request_keyframe();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(12, false, false, 3)),
            MediaV3GroupDecision::SkipUntilKeyframe
        );
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(20, true, true, 4)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 20,
                object_id: 0,
                discontinuity: true,
            })
        );
    }

    #[test]
    fn media_v3_group_rejects_noncontiguous_or_unconfigured_frames() {
        let mut cursor = MediaV3GroupCursor::default();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(10, true, false, 1)),
            MediaV3GroupDecision::SkipUntilKeyframe
        );
        assert!(matches!(
            cursor.classify(&media_v3_encoded_frame(20, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(22, false, false, 1)),
            MediaV3GroupDecision::EnterResync
        );
        assert!(cursor.waiting_for_keyframe);
    }

    #[test]
    fn media_v3_group_accepts_exact_limits_and_rejects_overflow() {
        let mut object_cursor = MediaV3GroupCursor::default();
        assert!(matches!(
            object_cursor.classify(&media_v3_encoded_frame(0, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        for sequence in 1..=u64::from(MAX_MEDIA_OBJECT_ID_V3) {
            assert!(matches!(
                object_cursor.classify(&media_v3_encoded_frame(sequence, false, false, 1)),
                MediaV3GroupDecision::Send(MediaV3ObjectPosition { object_id, .. })
                    if u64::from(object_id) == sequence
            ));
        }
        assert_eq!(
            object_cursor.classify(&media_v3_encoded_frame(
                u64::from(MAX_MEDIA_OBJECT_ID_V3) + 1,
                false,
                false,
                1,
            )),
            MediaV3GroupDecision::EnterResync
        );

        let mut byte_cursor = MediaV3GroupCursor::default();
        assert!(matches!(
            byte_cursor.classify(&media_v3_encoded_frame(10, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        byte_cursor.payload_bytes = MAX_MEDIA_GROUP_BYTES_V3 - 1;
        assert!(matches!(
            byte_cursor.classify(&media_v3_encoded_frame(11, false, false, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(byte_cursor.payload_bytes, MAX_MEDIA_GROUP_BYTES_V3);
        assert_eq!(
            byte_cursor.classify(&media_v3_encoded_frame(12, false, false, 1)),
            MediaV3GroupDecision::EnterResync
        );
    }

    #[test]
    fn media_v3_keyframe_requests_cancel_once_and_recover_discontinuously() {
        let mut scheduler = MediaV3Scheduler::default();
        assert!(matches!(
            scheduler.schedule(10, true),
            MediaV3ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(11, false),
            MediaV3ScheduleDecision::Send { .. }
        ));
        assert_eq!(scheduler.request_keyframe(), vec![10, 11]);
        assert!(scheduler.request_keyframe().is_empty());
        assert_eq!(
            scheduler.schedule(12, false),
            MediaV3ScheduleDecision::SkipUntilKeyframe
        );
        assert_eq!(
            scheduler.schedule(20, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![],
            }
        );
    }

    #[test]
    fn media_v3_join_request_preserves_initial_current_gop_replay() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();

        let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
            &mut scheduler,
            &mut group_cursor,
            &mut replay_cursor,
            Some(42),
            KeyframeRequestReasonV3::Join,
        );

        assert!(!transitioned);
        assert!(cancel_sequences.is_empty());
        assert_eq!(replay_cursor.last_sequence, None);
        assert!(replay_cursor.waiting_for_keyframe);
        assert!(!replay_cursor.discontinuity_pending);
        assert!(group_cursor.waiting_for_keyframe);
        assert!(!group_cursor.discontinuity_pending);
        assert!(!scheduler.discontinuity_pending);

        assert!(matches!(
            group_cursor.classify(&media_v3_encoded_frame(10, true, true, 1)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                discontinuity: false,
                ..
            })
        ));
        assert!(matches!(
            scheduler.schedule(10, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v3_late_join_preserves_active_static_current_gop() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();
        let keyframe = media_v3_encoded_frame(10, true, true, 1);
        let delta = media_v3_encoded_frame(11, false, false, 1);

        assert!(matches!(
            group_cursor.classify(&keyframe),
            MediaV3GroupDecision::Send(_)
        ));
        assert!(matches!(
            scheduler.schedule(keyframe.sequence, true),
            MediaV3ScheduleDecision::Send { .. }
        ));
        replay_cursor.commit_sent(&keyframe);
        assert!(matches!(
            group_cursor.classify(&delta),
            MediaV3GroupDecision::Send(_)
        ));
        assert!(matches!(
            scheduler.schedule(delta.sequence, false),
            MediaV3ScheduleDecision::Send { .. }
        ));
        replay_cursor.commit_sent(&delta);

        let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
            &mut scheduler,
            &mut group_cursor,
            &mut replay_cursor,
            Some(delta.sequence),
            KeyframeRequestReasonV3::Join,
        );

        assert!(!transitioned);
        assert!(cancel_sequences.is_empty());
        assert_eq!(scheduler.in_flight, vec![10, 11]);
        assert!(!scheduler.waiting_for_keyframe);
        assert!(!scheduler.discontinuity_pending);
        assert_eq!(group_cursor.group_id, Some(10));
        assert_eq!(group_cursor.last_sequence, Some(11));
        assert!(!group_cursor.waiting_for_keyframe);
        assert_eq!(replay_cursor.last_sequence, Some(11));
        assert!(!replay_cursor.waiting_for_keyframe);
    }

    #[test]
    fn media_v3_stale_old_group_failure_preserves_replacement_group() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();
        let old_keyframe = media_v3_encoded_frame(60, true, true, 1);
        let old_delta = media_v3_encoded_frame(61, false, false, 1);
        let replacement = media_v3_encoded_frame(70, true, true, 1);

        for frame in [&old_keyframe, &old_delta] {
            assert!(matches!(
                group_cursor.classify(frame),
                MediaV3GroupDecision::Send(_)
            ));
            assert!(matches!(
                scheduler.schedule(frame.sequence, frame.keyframe && frame.codec_config),
                MediaV3ScheduleDecision::Send { .. }
            ));
            replay_cursor.commit_sent(frame);
        }
        assert!(matches!(
            group_cursor.classify(&replacement),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(
            scheduler.schedule(replacement.sequence, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![60, 61],
            }
        );
        replay_cursor.commit_sent(&replacement);

        assert_eq!(
            apply_media_v3_send_failure(
                &mut scheduler,
                &mut group_cursor,
                &mut replay_cursor,
                old_keyframe.sequence,
                Some(replacement.sequence),
            ),
            None
        );
        assert_eq!(scheduler.in_flight, vec![70]);
        assert!(!scheduler.waiting_for_keyframe);
        assert_eq!(group_cursor.group_id, Some(70));
        assert!(!group_cursor.waiting_for_keyframe);
        assert_eq!(replay_cursor.last_sequence, Some(70));
        assert!(!replay_cursor.waiting_for_keyframe);

        let next_delta = media_v3_encoded_frame(71, false, false, 1);
        assert!(matches!(
            group_cursor.classify(&next_delta),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 70,
                object_id: 1,
                ..
            })
        ));
        assert!(matches!(
            scheduler.schedule(next_delta.sequence, false),
            MediaV3ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v3_control_gate_is_monotonic_and_accepts_at_most_ten_per_second() {
        let started = Instant::now();
        let request = |request_id| {
            MediaControlRequestV3::request_keyframe(
                request_id,
                None,
                sigil_protocol::KeyframeRequestReasonV3::TransportGap,
            )
        };
        let mut gate = MediaV3ControlGate::default();
        assert_eq!(
            gate.accept(request(1), started).unwrap(),
            MediaV3ControlDecision::Accept
        );
        assert_eq!(
            gate.accept(
                request(2),
                started + MEDIA_V3_CONTROL_REQUEST_INTERVAL - Duration::from_nanos(1),
            )
            .unwrap(),
            MediaV3ControlDecision::Pace(Duration::from_nanos(1))
        );
        assert_eq!(
            gate.accept(request(3), started + MEDIA_V3_CONTROL_REQUEST_INTERVAL)
                .unwrap(),
            MediaV3ControlDecision::Accept
        );
        assert!(
            gate.accept(request(3), started + Duration::from_secs(1))
                .is_err()
        );
    }

    #[tokio::test]
    async fn media_v3_rejected_control_requests_apply_read_side_pacing() {
        use tokio::io::AsyncWriteExt as _;

        let requests = [1, 2, 3].map(|request_id| {
            MediaControlRequestV3::request_keyframe(
                request_id,
                None,
                KeyframeRequestReasonV3::TransportGap,
            )
        });
        let (mut writer, reader) = tokio::io::duplex(128);
        for request in &requests {
            sigil_protocol::write_media_control_request_v3(&mut writer, request)
                .await
                .unwrap();
        }
        writer.shutdown().await.unwrap();
        let (sender, mut receiver) = tokio::sync::watch::channel(None);

        let started = Instant::now();
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();

        assert!(started.elapsed() >= MEDIA_V3_CONTROL_REQUEST_INTERVAL);
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(requests[2]));
    }

    #[tokio::test]
    async fn media_v3_control_eof_is_clean_and_malformed_input_is_terminal() {
        use tokio::io::AsyncWriteExt as _;

        let (writer, reader) = tokio::io::duplex(64);
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        drop(writer);
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();
        assert!(receiver.changed().await.is_err());

        let final_request = MediaControlRequestV3::request_keyframe(
            1,
            Some(9),
            sigil_protocol::KeyframeRequestReasonV3::DecoderReset,
        );
        let (mut writer, reader) = tokio::io::duplex(64);
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        sigil_protocol::write_media_control_request_v3(&mut writer, &final_request)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(final_request));

        let (mut writer, reader) = tokio::io::duplex(64);
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        writer
            .write_all(&[0; sigil_protocol::MEDIA_CONTROL_REQUEST_V3_LEN])
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        assert!(
            forward_media_v3_control_requests(reader, sender)
                .await
                .is_err()
        );
    }

    #[test]
    fn media_v3_deadline_ceil_clamps_four_frame_periods() {
        assert_eq!(media_v3_delivery_timeout_ms(60), 67);
        assert_eq!(media_v3_delivery_timeout_ms(240), 17);
        assert_eq!(
            media_v3_delivery_timeout_ms(1_000),
            MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS
        );
        assert_eq!(
            media_v3_delivery_timeout_ms(1),
            MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS
        );
    }

    #[test]
    fn media_v3_publisher_priority_maps_to_inverse_transport_priority() {
        let keyframe_transport =
            media_v3_transport_priority(MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY).unwrap();
        let delta_transport =
            media_v3_transport_priority(MEDIA_V3_DELTA_PUBLISHER_PRIORITY).unwrap();
        assert_eq!(keyframe_transport, MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY);
        assert_eq!(delta_transport, MEDIA_V3_DELTA_TRANSPORT_PRIORITY);
        assert!(keyframe_transport > delta_transport);
        assert!(media_v3_transport_priority(1).is_err());
    }

    #[test]
    fn current_gop_replay_is_complete_and_skips_already_sent_frames() {
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: std::time::Instant::now(),
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
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
}
