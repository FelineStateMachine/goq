use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::watch;

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use crate::clock::SessionClock;
#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use crate::config::{HostConfig, VaapiRateControl};
#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use sigil_protocol::PointerSurfaceDimensions;

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use super::{
    AccessUnitPublisher, CaptureVideoMode, EncodedFrame, EncodedGop, EncodedSource,
    GamescopePreflight, MAX_ENCODE_BUFFER, gamescope_config, has_h264_codec_config,
    interactive_gop_frames, is_h264_keyframe,
};

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use gstreamer::prelude::ObjectExt;
#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use tracing::{debug, warn};

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
const RESOLUTION_APPLY_TIMEOUT: Duration = Duration::from_secs(2);

const MAX_ENCODER_BITRATE_KBPS: u32 = 100_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RevisedBitrate {
    revision: u64,
    kbps: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RevisedResolution {
    revision: u64,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EncoderControlDesired {
    revision: u64,
    bitrate: Option<RevisedBitrate>,
    resolution: Option<RevisedResolution>,
    force_keyframe_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderControlStatus {
    pub latest_revision: u64,
    pub applied_bitrate_revision: Option<u64>,
    pub applied_bitrate_kbps: Option<u32>,
    pub requested_resolution_revision: Option<u64>,
    pub applied_resolution_revision: Option<u64>,
    pub applied_width: Option<u16>,
    pub applied_height: Option<u16>,
    pub requested_force_keyframe_revision: Option<u64>,
    pub acknowledged_force_keyframe_revision: Option<u64>,
}

/// Bounded latest-state control for an in-process encoder.
///
/// Both directions are Tokio watch channels: callers can update control state
/// arbitrarily often without building a queue, while the encoder applies only
/// the latest revision. Revisions are process-monotonic and never wrap.
#[derive(Clone, Debug)]
pub struct EncoderControl {
    initial_width: u16,
    initial_height: u16,
    next_revision: Arc<AtomicU64>,
    desired: watch::Sender<EncoderControlDesired>,
    status: watch::Receiver<EncoderControlStatus>,
}

#[cfg_attr(
    not(any(test, all(target_os = "linux", feature = "in-process-gstreamer"))),
    expect(dead_code, reason = "constructed only by the opt-in encoder backend")
)]
impl EncoderControl {
    fn new(
        initial_width: u16,
        initial_height: u16,
    ) -> (
        Self,
        watch::Receiver<EncoderControlDesired>,
        watch::Sender<EncoderControlStatus>,
    ) {
        let (desired, desired_rx) = watch::channel(EncoderControlDesired::default());
        let (status_tx, status) = watch::channel(EncoderControlStatus::default());
        (
            Self {
                initial_width,
                initial_height,
                next_revision: Arc::new(AtomicU64::new(0)),
                desired,
                status,
            },
            desired_rx,
            status_tx,
        )
    }

    pub(crate) fn initial_dimensions(&self) -> (u16, u16) {
        (self.initial_width, self.initial_height)
    }

    fn next_revision(&self) -> Result<u64> {
        self.next_revision
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
                revision.checked_add(1)
            })
            .map(|revision| revision + 1)
            .map_err(|_| anyhow::anyhow!("encoder control revision exhausted"))
    }

    pub fn request_bitrate_kbps(&self, kbps: u32) -> Result<u64> {
        ensure!(
            (1..=MAX_ENCODER_BITRATE_KBPS).contains(&kbps),
            "encoder bitrate must be between 1 and {MAX_ENCODER_BITRATE_KBPS} kbps"
        );
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.bitrate = Some(RevisedBitrate { revision, kbps });
        });
        Ok(revision)
    }

    pub fn request_force_keyframe(&self) -> Result<u64> {
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.force_keyframe_revision = Some(revision);
        });
        Ok(revision)
    }

    pub fn request_resolution(&self, width: u16, height: u16) -> Result<u64> {
        ensure!(
            width >= 64 && height >= 64,
            "encoder dimensions must be at least 64x64"
        );
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "H.264 encoder dimensions must be even"
        );
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.resolution = Some(RevisedResolution {
                revision,
                width,
                height,
            });
        });
        Ok(revision)
    }

    pub fn status(&self) -> EncoderControlStatus {
        *self.status.borrow()
    }

    /// Wait until the encoder confirms the exact bitrate revision and exposes
    /// its property readback. A newer applied revision means this request was
    /// coalesced before application and therefore fails rather than pretending
    /// the requested revision committed.
    pub async fn wait_for_bitrate_applied(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.applied_bitrate_revision {
                    Some(applied) if applied == revision => return Ok(snapshot),
                    Some(applied) if applied > revision => bail!(
                        "encoder bitrate revision {revision} was superseded by revision {applied}"
                    ),
                    _ => {}
                }
                status
                    .changed()
                    .await
                    .context("encoder control status closed before bitrate was applied")?;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .with_context(|| format!("timed out waiting for encoder bitrate revision {revision}"))?
    }

    /// Wait for a forced-keyframe request to be committed by observing a
    /// keyframe that also carries codec configuration.
    pub async fn wait_for_force_keyframe_acknowledged(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.acknowledged_force_keyframe_revision {
                    Some(acknowledged) if acknowledged == revision => return Ok(snapshot),
                    Some(acknowledged) if acknowledged > revision => bail!(
                        "force-keyframe revision {revision} was superseded by revision {acknowledged}"
                    ),
                    _ => {}
                }
                if snapshot
                    .requested_force_keyframe_revision
                    .is_some_and(|requested| requested > revision)
                {
                    bail!("force-keyframe revision {revision} was coalesced before application");
                }
                status.changed().await.context(
                    "encoder control status closed before forced keyframe was acknowledged",
                )?;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .with_context(|| format!("timed out waiting for force-keyframe revision {revision}"))?
    }

    /// Wait for recovery to reach any configured IDR at or after `revision`.
    ///
    /// Recovery requests are deliberately coalesced with later encoder
    /// controls. Unlike transactional adaptive updates, a newer configured IDR
    /// still satisfies the decoder barrier established by an older request.
    pub async fn wait_for_recovery_keyframe_acknowledged(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                if snapshot
                    .acknowledged_force_keyframe_revision
                    .is_some_and(|acknowledged| acknowledged >= revision)
                {
                    return Ok(snapshot);
                }
                status.changed().await.context(
                    "encoder control status closed before recovery keyframe was acknowledged",
                )?;
            }
        };
        tokio::time::timeout(timeout, wait).await.with_context(|| {
            format!("timed out waiting for recovery keyframe revision {revision}")
        })?
    }

    /// Wait until the exact target dimensions emerge from the encoder on an
    /// independently decodable access unit. A property write or caps event is
    /// not an application acknowledgement.
    pub async fn wait_for_resolution_applied(
        &self,
        revision: u64,
        width: u16,
        height: u16,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.applied_resolution_revision {
                    Some(applied) if applied == revision => {
                        ensure!(
                            snapshot.applied_width == Some(width)
                                && snapshot.applied_height == Some(height),
                            "encoder resolution revision {revision} acknowledged {:?}x{:?}, expected {width}x{height}",
                            snapshot.applied_width,
                            snapshot.applied_height
                        );
                        return Ok(snapshot);
                    }
                    Some(applied) if applied > revision => bail!(
                        "encoder resolution revision {revision} was superseded by revision {applied}"
                    ),
                    _ => {}
                }
                if snapshot
                    .requested_resolution_revision
                    .is_some_and(|requested| requested > revision)
                {
                    bail!(
                        "encoder resolution revision {revision} was coalesced before application"
                    );
                }
                status
                    .changed()
                    .await
                    .context("encoder control status closed before resolution was applied")?;
            }
        };
        tokio::time::timeout(timeout, wait).await.with_context(|| {
            format!("timed out waiting for encoder resolution revision {revision}")
        })?
    }
}

#[cfg(test)]
pub(crate) struct EncoderControlTestHarness {
    pub control: EncoderControl,
    pub status: watch::Sender<EncoderControlStatus>,
    _desired: watch::Receiver<EncoderControlDesired>,
}

#[cfg(test)]
impl EncoderControlTestHarness {
    pub(crate) fn new() -> Self {
        Self::with_dimensions(1_280, 800)
    }

    pub(crate) fn with_dimensions(width: u16, height: u16) -> Self {
        let (control, desired, status) = EncoderControl::new(width, height);
        Self {
            control,
            status,
            _desired: desired,
        }
    }

    pub(crate) fn requested_force_keyframe_revision(&self) -> Option<u64> {
        self._desired.borrow().force_keyframe_revision
    }

    pub(crate) fn requested_bitrate(&self) -> Option<(u64, u32)> {
        self._desired
            .borrow()
            .bitrate
            .map(|bitrate| (bitrate.revision, bitrate.kbps))
    }

    pub(crate) fn requested_resolution(&self) -> Option<(u64, u16, u16)> {
        self._desired
            .borrow()
            .resolution
            .map(|resolution| (resolution.revision, resolution.width, resolution.height))
    }
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
pub(super) fn spawn_gamescope_pipewire_in_process_with_target(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<EncodedSource> {
    let description = gamescope_in_process_pipeline_description(
        &config,
        &preflight.target_object,
        preflight.video_mode,
    )?;
    let max_gop_frames = interactive_gop_frames(preflight.video_mode.framerate) as usize;
    let expected_device_path = Some(
        gamescope_config(&config)?
            .vaapi_render_node
            .to_string_lossy()
            .into_owned(),
    );
    spawn_in_process_pipeline(
        description,
        session_clock,
        max_gop_frames,
        Some(preflight.pointer_surface_dimensions),
        expected_device_path,
        preflight.video_mode.width,
        preflight.video_mode.height,
    )
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn spawn_in_process_pipeline(
    description: String,
    session_clock: SessionClock,
    max_gop_frames: usize,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    expected_device_path: Option<String>,
    initial_width: u16,
    initial_height: u16,
) -> Result<EncodedSource> {
    let (sender, receiver) = watch::channel(None);
    let (current_gop_sender, current_gop) = watch::channel(None);
    let (control, desired, status) = EncoderControl::new(initial_width, initial_height);
    let task = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            run_in_process_pipeline(
                &description,
                session_clock,
                sender,
                current_gop_sender,
                max_gop_frames,
                desired,
                status,
                expected_device_path.as_deref(),
                initial_width,
                initial_height,
            )
        })
        .await
        .context("in-process GStreamer worker panicked")?
    });
    Ok(EncodedSource {
        frames: receiver,
        current_gop,
        task,
        pointer_surface_dimensions,
        encoder_control: Some(control),
    })
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn gamescope_in_process_pipeline_description(
    config: &HostConfig,
    target_object: &str,
    video_mode: CaptureVideoMode,
) -> Result<String> {
    let pipewire = gamescope_config(config)?;
    validate_pipewire_target_object(target_object)?;
    ensure!(
        pipewire.rate_control == VaapiRateControl::Cbr,
        "in-process GStreamer requires CBR so bitrate can be changed while playing"
    );
    let bitrate = pipewire
        .bitrate_kbps
        .context("CBR bitrate is missing after validation")?;
    let raw_caps = format!(
        "video/x-raw,format=NV12,width={},height={},framerate={}/1",
        video_mode.width, video_mode.height, video_mode.framerate
    );
    Ok(format!(
        "pipewiresrc do-timestamp=true min-buffers=1 max-buffers=4 use-bufferpool=false target-object={target_object} \
         ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
         ! videorate drop-only=true max-rate={} \
         ! videoconvert ! videoscale \
         ! capsfilter name=sigil_scale_caps caps={raw_caps} caps-change-mode=delayed \
         ! {} name=sigil_encoder rate-control=cbr bitrate={bitrate} target-usage=7 key-int-max={} b-frames=0 ref-frames=1 aud=true \
         ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sigil_sink max-buffers=1 drop=false sync=false",
        video_mode.framerate,
        pipewire.vaapi_encoder,
        interactive_gop_frames(video_mode.framerate),
    ))
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_pipewire_target_object(target_object: &str) -> Result<()> {
    ensure!(
        !target_object.is_empty()
            && target_object.len() <= 32
            && target_object.bytes().all(|byte| byte.is_ascii_digit()),
        "resolved PipeWire object.serial is invalid"
    );
    Ok(())
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingResolutionTransition {
    target: RevisedResolution,
    deadline: Instant,
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
fn classify_resolution_sample(
    current: (u16, u16),
    pending: Option<PendingResolutionTransition>,
    sample: (u16, u16),
    configured_idr: bool,
) -> Result<Option<bool>> {
    if let Some(pending) = pending {
        if sample != (pending.target.width, pending.target.height) || !configured_idr {
            return Ok(None);
        }
        return Ok(Some(true));
    }
    ensure!(
        sample == current,
        "encoder changed resolution from {}x{} to {}x{} without a pending control revision",
        current.0,
        current.1,
        sample.0,
        sample.1
    );
    Ok(Some(false))
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn run_in_process_pipeline(
    description: &str,
    session_clock: SessionClock,
    sender: watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: watch::Sender<Option<EncodedGop>>,
    max_gop_frames: usize,
    mut desired: watch::Receiver<EncoderControlDesired>,
    status: watch::Sender<EncoderControlStatus>,
    expected_device_path: Option<&str>,
    initial_width: u16,
    initial_height: u16,
) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    gst::init().context("initializing in-process GStreamer")?;
    let pipeline = gst::parse::launch(description)
        .context("parsing in-process GStreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| {
            anyhow::anyhow!("in-process GStreamer description did not create a pipeline")
        })?;
    let encoder = pipeline
        .by_name("sigil_encoder")
        .context("in-process GStreamer pipeline has no named encoder")?;
    let scale_caps = pipeline
        .by_name("sigil_scale_caps")
        .context("in-process GStreamer pipeline has no named scale capsfilter")?;
    let sink = pipeline
        .by_name("sigil_sink")
        .context("in-process GStreamer pipeline has no named appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("named in-process sink is not an appsink"))?;
    sink.set_max_buffers(1);
    // Encoded dependent AUs must not be dropped here: the raw queue before the
    // encoder is the only latest-frame boundary. This one-buffer sink applies
    // bounded backpressure until the publishing loop consumes the AU.
    sink.set_drop(false);
    sink.set_sync(false);
    validate_mutable_playing_bitrate(&encoder)?;
    validate_resolution_capsfilter(&scale_caps)?;

    pipeline
        .set_state(gst::State::Playing)
        .context("starting in-process GStreamer pipeline")?;
    let result = (|| {
        let (state_change, state, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        state_change.context("in-process GStreamer pipeline failed while waiting for PLAYING")?;
        ensure!(
            state == gst::State::Playing,
            "in-process GStreamer pipeline did not reach PLAYING within 5 seconds (state {state:?}, pending {pending:?})"
        );
        if let Some(expected_device_path) = expected_device_path {
            validate_active_encoder_device_path(&encoder, expected_device_path)?;
        }
        let bus = pipeline
            .bus()
            .context("in-process GStreamer pipeline has no bus")?;
        let mut publisher = AccessUnitPublisher::new(
            session_clock,
            &sender,
            &current_gop_sender,
            max_gop_frames,
            initial_width,
            initial_height,
        );
        let mut applied_desired_revision = 0_u64;
        let mut pending_force_keyframe_revision = None;
        let mut pending_resolution = None;
        status.send_modify(|status| {
            status.applied_width = Some(initial_width);
            status.applied_height = Some(initial_height);
        });

        loop {
            if let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) {
                match message.view() {
                    gst::MessageView::Error(error) => {
                        bail!(
                            "in-process GStreamer pipeline error from {:?}: {} ({:?})",
                            error.src().map(|source| source.path_string()),
                            error.error(),
                            error.debug()
                        );
                    }
                    gst::MessageView::Eos(_) => {
                        bail!("in-process GStreamer pipeline ended unexpectedly");
                    }
                    _ => {}
                }
            }

            let desired_state = *desired.borrow_and_update();
            if desired_state.revision > applied_desired_revision {
                apply_encoder_control(
                    &encoder,
                    &scale_caps,
                    &sink,
                    desired_state,
                    &status,
                    &current_gop_sender,
                    &mut pending_force_keyframe_revision,
                    &mut pending_resolution,
                )?;
                applied_desired_revision = desired_state.revision;
            }

            if pending_resolution.is_some_and(|pending: PendingResolutionTransition| {
                Instant::now() >= pending.deadline
            }) {
                let pending = pending_resolution.expect("checked as present");
                bail!(
                    "encoder resolution revision {} did not produce a configured {}x{} IDR within {:?}",
                    pending.target.revision,
                    pending.target.width,
                    pending.target.height,
                    RESOLUTION_APPLY_TIMEOUT
                );
            }

            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                let (width, height) = encoded_sample_dimensions(&sample)?;
                let buffer = sample
                    .buffer()
                    .context("in-process appsink sample has no buffer")?;
                let map = buffer
                    .map_readable()
                    .context("mapping in-process encoded access unit")?;
                ensure!(
                    map.as_slice().len() <= MAX_ENCODE_BUFFER,
                    "in-process encoded access unit exceeds {MAX_ENCODE_BUFFER} bytes"
                );
                let access_unit = map.as_slice();
                let configured_idr =
                    is_h264_keyframe(access_unit) && has_h264_codec_config(access_unit);
                let Some(discontinuity) = classify_resolution_sample(
                    (publisher.width, publisher.height),
                    pending_resolution,
                    (width, height),
                    configured_idr,
                )?
                else {
                    continue;
                };
                let configured_idr = publisher.publish_with_metadata(
                    access_unit.to_vec(),
                    width,
                    height,
                    discontinuity,
                )?;
                if discontinuity {
                    let applied = pending_resolution
                        .take()
                        .expect("discontinuity requires a pending resolution");
                    status.send_modify(|status| {
                        status.applied_resolution_revision = Some(applied.target.revision);
                        status.applied_width = Some(width);
                        status.applied_height = Some(height);
                    });
                }
                acknowledge_force_keyframe_if_configured_idr(
                    configured_idr,
                    &mut pending_force_keyframe_revision,
                    &status,
                );
            } else if sink.is_eos() {
                bail!("in-process GStreamer appsink reached EOS unexpectedly");
            }

            if publisher.receivers_closed() {
                debug!("in-process encoded source has no receivers");
                return Ok(());
            }
        }
    })();
    if let Err(error) = pipeline.set_state(gst::State::Null) {
        warn!(%error, "failed to stop in-process GStreamer pipeline cleanly");
    }
    result
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_active_encoder_device_path(
    encoder: &gstreamer::Element,
    expected_device_path: &str,
) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    let property = encoder
        .find_property("device-path")
        .context("configured in-process VA encoder has no device-path property")?;
    ensure!(
        property.flags().contains(gst::glib::ParamFlags::READABLE),
        "configured in-process VA encoder device-path property is not readable"
    );
    let observed_device_path = encoder.property::<Option<String>>("device-path");
    ensure!(
        observed_device_path.as_deref() == Some(expected_device_path),
        "configured in-process VA encoder uses device-path {observed_device_path:?}, expected {expected_device_path:?}"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_mutable_playing_bitrate(encoder: &gstreamer::Element) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    let property = encoder
        .find_property("bitrate")
        .context("configured in-process encoder has no bitrate property")?;
    ensure!(
        property.flags().contains(gst::glib::ParamFlags::WRITABLE),
        "configured in-process encoder bitrate property is not writable"
    );
    ensure!(
        property.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
        "configured in-process encoder bitrate property is not mutable while playing"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_resolution_capsfilter(capsfilter: &gstreamer::Element) -> Result<()> {
    use gstreamer::prelude::*;

    let property = capsfilter
        .find_property("caps")
        .context("configured scale capsfilter has no caps property")?;
    ensure!(
        property
            .flags()
            .contains(gstreamer::glib::ParamFlags::WRITABLE),
        "configured scale capsfilter caps property is not writable"
    );
    let caps = capsfilter.property::<gstreamer::Caps>("caps");
    let _ = dimensions_from_caps(&caps)?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn dimensions_from_caps(caps: &gstreamer::CapsRef) -> Result<(u16, u16)> {
    let structure = caps
        .structure(0)
        .context("configured video caps have no structure")?;
    let width = structure
        .get::<i32>("width")
        .context("configured video caps have no integer width")?;
    let height = structure
        .get::<i32>("height")
        .context("configured video caps have no integer height")?;
    Ok((
        u16::try_from(width).context("configured video caps width is outside u16")?,
        u16::try_from(height).context("configured video caps height is outside u16")?,
    ))
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn encoded_sample_dimensions(sample: &gstreamer::Sample) -> Result<(u16, u16)> {
    let caps = sample
        .caps()
        .context("in-process encoded sample has no negotiated caps")?;
    dimensions_from_caps(caps)
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn apply_resolution_caps(
    capsfilter: &gstreamer::Element,
    resolution: RevisedResolution,
) -> Result<()> {
    use gstreamer::prelude::*;

    let mut caps = capsfilter.property::<gstreamer::Caps>("caps");
    let caps_mut = caps.make_mut();
    let structure = caps_mut
        .structure_mut(0)
        .context("configured scale capsfilter has no caps structure")?;
    structure.set("width", i32::from(resolution.width));
    structure.set("height", i32::from(resolution.height));
    capsfilter.set_property("caps", caps);
    let readback = capsfilter.property::<gstreamer::Caps>("caps");
    ensure!(
        dimensions_from_caps(&readback)? == (resolution.width, resolution.height),
        "scale capsfilter resolution readback does not match requested {}x{}",
        resolution.width,
        resolution.height
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn push_configured_keyframe_request(sink: &gstreamer_app::AppSink) -> Result<()> {
    use gstreamer::prelude::*;

    let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    let sink_pad = sink
        .static_pad("sink")
        .context("configured in-process appsink has no sink pad")?;
    ensure!(
        sink_pad.push_event(event),
        "in-process encoder rejected upstream ForceKeyUnit"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn apply_encoder_control(
    encoder: &gstreamer::Element,
    scale_caps: &gstreamer::Element,
    sink: &gstreamer_app::AppSink,
    desired: EncoderControlDesired,
    status: &watch::Sender<EncoderControlStatus>,
    current_gop_sender: &watch::Sender<Option<EncodedGop>>,
    pending_force_keyframe_revision: &mut Option<u64>,
    pending_resolution: &mut Option<PendingResolutionTransition>,
) -> Result<()> {
    let current = *status.borrow();
    if let Some(bitrate) = desired.bitrate
        && current
            .applied_bitrate_revision
            .is_none_or(|revision| bitrate.revision > revision)
    {
        encoder.set_property("bitrate", bitrate.kbps);
        let readback = encoder.property::<u32>("bitrate");
        ensure!(
            readback == bitrate.kbps,
            "encoder bitrate readback mismatch: requested {} kbps, observed {readback} kbps",
            bitrate.kbps
        );
        status.send_modify(|status| {
            status.applied_bitrate_revision = Some(bitrate.revision);
            status.applied_bitrate_kbps = Some(readback);
        });
    }

    let mut keyframe_requested = false;
    if let Some(resolution) = desired.resolution
        && current
            .requested_resolution_revision
            .is_none_or(|requested| resolution.revision > requested)
    {
        apply_resolution_caps(scale_caps, resolution)?;
        current_gop_sender.send_replace(None);
        push_configured_keyframe_request(sink)?;
        keyframe_requested = true;
        *pending_resolution = Some(PendingResolutionTransition {
            target: resolution,
            deadline: Instant::now() + RESOLUTION_APPLY_TIMEOUT,
        });
        status.send_modify(|status| {
            status.requested_resolution_revision = Some(resolution.revision);
        });
    }

    if let Some(revision) = desired.force_keyframe_revision
        && current
            .requested_force_keyframe_revision
            .is_none_or(|requested| revision > requested)
    {
        if !keyframe_requested {
            push_configured_keyframe_request(sink)?;
        }
        *pending_force_keyframe_revision = Some(revision);
        status.send_modify(|status| {
            status.requested_force_keyframe_revision = Some(revision);
        });
    }

    status.send_modify(|status| status.latest_revision = desired.revision);
    Ok(())
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
fn acknowledge_force_keyframe_if_configured_idr(
    configured_idr: bool,
    pending_revision: &mut Option<u64>,
    status: &watch::Sender<EncoderControlStatus>,
) {
    if configured_idr && let Some(revision) = pending_revision.take() {
        status.send_modify(|status| {
            status.acknowledged_force_keyframe_revision = Some(revision);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_control_retains_initial_session_dimensions() {
        let (native, _desired, _status) = EncoderControl::new(2_560, 1_600);
        let (other, _desired, _status) = EncoderControl::new(1_366, 768);

        assert_eq!(native.initial_dimensions(), (2_560, 1_600));
        assert_eq!(other.initial_dimensions(), (1_366, 768));
        native.request_resolution(1_280, 800).unwrap();
        assert_eq!(native.initial_dimensions(), (2_560, 1_600));
        assert_eq!(other.initial_dimensions(), (1_366, 768));
    }

    #[tokio::test]
    async fn encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr() {
        let (control, mut desired, status) = EncoderControl::new(1_280, 800);

        let superseded_bitrate_revision = control.request_bitrate_kbps(4_000).unwrap();
        let bitrate_revision = control.request_bitrate_kbps(8_000).unwrap();
        let superseded_resolution_revision = control.request_resolution(960, 600).unwrap();
        let resolution_revision = control.request_resolution(640, 400).unwrap();
        let force_keyframe_revision = control.request_force_keyframe().unwrap();

        assert!(superseded_bitrate_revision < bitrate_revision);
        assert!(bitrate_revision < superseded_resolution_revision);
        assert!(superseded_resolution_revision < resolution_revision);
        assert!(resolution_revision < force_keyframe_revision);
        let latest = *desired.borrow_and_update();
        assert_eq!(latest.revision, force_keyframe_revision);
        assert_eq!(
            latest.bitrate,
            Some(RevisedBitrate {
                revision: bitrate_revision,
                kbps: 8_000,
            })
        );
        assert_eq!(
            latest.resolution,
            Some(RevisedResolution {
                revision: resolution_revision,
                width: 640,
                height: 400,
            })
        );
        assert_eq!(
            latest.force_keyframe_revision,
            Some(force_keyframe_revision)
        );

        status.send_modify(|status| {
            status.latest_revision = latest.revision;
            status.applied_bitrate_revision = Some(bitrate_revision);
            status.applied_bitrate_kbps = Some(8_000);
            status.requested_resolution_revision = Some(resolution_revision);
            status.applied_resolution_revision = Some(resolution_revision);
            status.applied_width = Some(640);
            status.applied_height = Some(400);
            status.requested_force_keyframe_revision = Some(force_keyframe_revision);
        });
        let mut pending = Some(force_keyframe_revision);
        acknowledge_force_keyframe_if_configured_idr(false, &mut pending, &status);
        assert_eq!(pending, Some(force_keyframe_revision));
        assert_eq!(control.status().acknowledged_force_keyframe_revision, None);

        acknowledge_force_keyframe_if_configured_idr(true, &mut pending, &status);
        assert_eq!(pending, None);
        let applied = control
            .wait_for_bitrate_applied(bitrate_revision, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(applied.applied_bitrate_kbps, Some(8_000));
        let applied_resolution = control
            .wait_for_resolution_applied(resolution_revision, 640, 400, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(applied_resolution.applied_width, Some(640));
        assert_eq!(applied_resolution.applied_height, Some(400));
        assert!(
            control
                .wait_for_resolution_applied(
                    superseded_resolution_revision,
                    960,
                    600,
                    Duration::from_millis(10),
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
        let acknowledged = control
            .wait_for_force_keyframe_acknowledged(
                force_keyframe_revision,
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(
            acknowledged.acknowledged_force_keyframe_revision,
            Some(force_keyframe_revision)
        );
        assert!(control.request_bitrate_kbps(0).is_err());
        assert!(control.request_resolution(0, 400).is_err());
        assert!(control.request_resolution(641, 400).is_err());
        assert!(
            control
                .request_bitrate_kbps(MAX_ENCODER_BITRATE_KBPS + 1)
                .is_err()
        );

        let (closed_control, _desired, closed_status) = EncoderControl::new(1_280, 800);
        let closed_revision = closed_control.request_force_keyframe().unwrap();
        drop(closed_status);
        assert!(
            closed_control
                .wait_for_force_keyframe_acknowledged(closed_revision, Duration::from_millis(10))
                .await
                .unwrap_err()
                .to_string()
                .contains("status closed")
        );
    }

    #[tokio::test]
    async fn recovery_waiter_accepts_a_newer_configured_idr_revision() {
        let (control, _desired, status) = EncoderControl::new(1_280, 800);
        let recovery_revision = control.request_force_keyframe().unwrap();
        let newer_revision = control.request_force_keyframe().unwrap();
        assert!(newer_revision > recovery_revision);
        status.send_modify(|status| {
            status.requested_force_keyframe_revision = Some(newer_revision);
            status.acknowledged_force_keyframe_revision = Some(newer_revision);
        });

        let acknowledged = control
            .wait_for_recovery_keyframe_acknowledged(recovery_revision, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(
            acknowledged.acknowledged_force_keyframe_revision,
            Some(newer_revision)
        );
        assert!(
            control
                .wait_for_force_keyframe_acknowledged(recovery_revision, Duration::from_millis(10),)
                .await
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
    }

    #[test]
    fn resolution_transition_suppresses_until_exact_configured_target_idr() {
        let transition = PendingResolutionTransition {
            target: RevisedResolution {
                revision: 7,
                width: 960,
                height: 600,
            },
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(transition.deadline > Instant::now());
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (1280, 800), true).unwrap(),
            None
        );
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (960, 600), false).unwrap(),
            None
        );
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (960, 600), true).unwrap(),
            Some(true)
        );
        assert_eq!(
            classify_resolution_sample((960, 600), None, (960, 600), false).unwrap(),
            Some(false)
        );
        assert!(classify_resolution_sample((960, 600), None, (1280, 800), true).is_err());
    }

    #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
    #[test]
    fn in_process_pipeline_drops_only_before_encoding() {
        use super::super::tests::{configured_video_mode, gamescope_config};
        use crate::config::GamescopeEncoderBackend;

        let mut config = gamescope_config();
        config.gamescope_pipewire.as_mut().unwrap().encoder_backend =
            GamescopeEncoderBackend::InProcessGstreamer;
        let description =
            gamescope_in_process_pipeline_description(&config, "1234", configured_video_mode())
                .unwrap();

        assert!(description.contains(
            "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream"
        ));
        assert!(description.contains(
            "capsfilter name=sigil_scale_caps caps=video/x-raw,format=NV12,width=1280,height=800,framerate=60/1 caps-change-mode=delayed"
        ));
        assert!(
            description.contains("appsink name=sigil_sink max-buffers=1 drop=false sync=false")
        );
        assert!(!description.contains("appsink name=sigil_sink max-buffers=1 drop=true"));
    }

    #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires GStreamer x264 and app plugins"]
    async fn in_process_gstreamer_x264_smoke() {
        let description = "videotestsrc is-live=true pattern=ball \
            ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
            ! videoconvert ! videoscale \
            ! capsfilter name=sigil_scale_caps caps=video/x-raw,width=320,height=180,framerate=30/1 caps-change-mode=delayed \
            ! x264enc name=sigil_encoder bitrate=2000 tune=zerolatency speed-preset=ultrafast key-int-max=15 bframes=0 byte-stream=true aud=true \
            ! h264parse config-interval=-1 \
            ! video/x-h264,stream-format=byte-stream,alignment=au \
            ! appsink name=sigil_sink max-buffers=1 drop=false sync=false"
            .to_owned();
        let mut source =
            spawn_in_process_pipeline(description, SessionClock::start(), 15, None, None, 320, 180)
                .unwrap();
        let control = source
            .encoder_control
            .clone()
            .expect("x264 source has no encoder control");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                source.frames.changed().await.unwrap();
                if source
                    .frames
                    .borrow()
                    .as_ref()
                    .is_some_and(|frame| frame.keyframe && frame.codec_config)
                {
                    break;
                }
            }
        })
        .await
        .expect("x264 did not publish an initial configured IDR");

        let bitrate_revision = control.request_bitrate_kbps(1_500).unwrap();
        let force_keyframe_revision = control.request_force_keyframe().unwrap();
        let bitrate_status = control
            .wait_for_bitrate_applied(bitrate_revision, Duration::from_secs(10))
            .await
            .expect("x264 did not apply the bitrate revision");
        assert_eq!(bitrate_status.applied_bitrate_kbps, Some(1_500));
        control
            .wait_for_force_keyframe_acknowledged(force_keyframe_revision, Duration::from_secs(10))
            .await
            .expect("x264 did not acknowledge a configured forced IDR");
        let before_resolution_sequence = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .map(|frame| frame.sequence)
            .expect("x264 has no configured GOP before resolution transition");

        let reduced_revision = control.request_resolution(256, 144).unwrap();
        control
            .wait_for_resolution_applied(reduced_revision, 256, 144, Duration::from_secs(10))
            .await
            .expect("x264 did not apply the reduced resolution");
        let reduced = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .cloned()
            .expect("x264 has no reduced configured GOP");
        assert_eq!((reduced.width, reduced.height), (256, 144));
        assert!(reduced.keyframe && reduced.codec_config && reduced.discontinuity);
        assert!(reduced.sequence > before_resolution_sequence);

        let restored_revision = control.request_resolution(320, 180).unwrap();
        control
            .wait_for_resolution_applied(restored_revision, 320, 180, Duration::from_secs(10))
            .await
            .expect("x264 did not restore the native resolution");
        let restored = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .cloned()
            .expect("x264 has no restored configured GOP");
        assert_eq!((restored.width, restored.height), (320, 180));
        assert!(restored.keyframe && restored.codec_config && restored.discontinuity);
        assert!(restored.sequence > reduced.sequence);

        {
            let current_gop = source.current_gop.borrow();
            let gop = current_gop
                .as_ref()
                .expect("x264 did not retain a current GOP");
            assert!(gop.frames[0].keyframe && gop.frames[0].codec_config);
            assert!(gop.frames.len() <= 15);
        }
        let EncodedSource {
            frames,
            current_gop,
            task,
            ..
        } = source;
        drop(frames);
        drop(current_gop);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("x264 pipeline did not stop after its receivers closed")
            .expect("x264 pipeline task panicked")
            .expect("x264 pipeline returned an error");
    }
}
