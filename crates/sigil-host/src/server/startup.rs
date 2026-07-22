use super::*;

const GAMESCOPE_STARTUP_SAMPLE_WINDOW: Duration = Duration::from_millis(500);
const GAMESCOPE_STARTUP_MIN_OBSERVATION_SPAN: Duration = Duration::from_millis(100);
const GAMESCOPE_STARTUP_TARGET_MIN_FRAMES: u64 = 8;
const GAMESCOPE_STARTUP_TARGET_MIN_FPS: f64 = 45.0;

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
        encoder_control: _,
    } = source;
    // Closing both bounded outputs gives the capture task a chance to kill and
    // wait for its GStreamer child itself. Abort is only the bounded fallback.
    drop(frames);
    drop(current_gop);
    SourceTaskGuard::new(task)
        .wait_or_abort(grace_timeout)
        .await;
}

pub(super) async fn select_gamescope_startup_source(
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

#[cfg(test)]
mod tests {
    use super::super::DropNotify;
    use super::*;

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
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 0,
            presentation_timestamp_micros: 0,
            observed_at: now,
            keyframe: true,
            codec_config: true,
            discontinuity: false,
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
            encoder_control: None,
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
            encoder_control: None,
        };

        started_rx.await.unwrap();
        reap_encoded_source_with_timeout(source, Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_millis(100), reaped_rx)
            .await
            .expect("stalled source task was not aborted and reaped")
            .unwrap();
    }
}
