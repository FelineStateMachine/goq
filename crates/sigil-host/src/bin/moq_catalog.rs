use std::time::Duration;

use anyhow::{Context, Result};
use moq_net::{BroadcastConsumer, Error as MoqError, Track, TrackConsumer};
use sigil_protocol::{GoqCatalogDocument, MAX_MOQ_CATALOG_BYTES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MoqCatalogMode {
    GoqV1,
    AbsentStaticTrackCompat,
}

impl MoqCatalogMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::GoqV1 => "goq-v1",
            Self::AbsentStaticTrackCompat => "absent-static-track-compat",
        }
    }
}

pub(crate) struct MoqCatalogSelection {
    pub(crate) track: TrackConsumer,
    pub(crate) mode: MoqCatalogMode,
}

fn is_track_not_found(error: &MoqError) -> bool {
    matches!(error, MoqError::NotFound)
        || matches!(error, MoqError::Remote(code) if *code == MoqError::NotFound.to_code())
}

fn subscribe_static_video_track(broadcast: &BroadcastConsumer) -> Result<MoqCatalogSelection> {
    let track = broadcast
        .subscribe_track(&Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
        .context("subscribing to static MoQ H.264 compatibility track")?;
    Ok(MoqCatalogSelection {
        track,
        mode: MoqCatalogMode::AbsentStaticTrackCompat,
    })
}

pub(crate) async fn subscribe_goq_video_track(
    broadcast: &BroadcastConsumer,
    timeout: Duration,
) -> Result<MoqCatalogSelection> {
    let catalog_track = match broadcast.subscribe_track(&hang::Catalog::default_track()) {
        Ok(track) => track,
        Err(MoqError::NotFound) => {
            return subscribe_static_video_track(broadcast);
        }
        Err(error) => return Err(error).context("subscribing to catalog.json track"),
    };

    let mut catalog_track = catalog_track;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut group = match tokio::time::timeout_at(deadline, catalog_track.next_group()).await {
        Err(_) => anyhow::bail!("timed out waiting for Goq catalog snapshot"),
        Ok(Err(error)) if is_track_not_found(&error) => {
            return subscribe_static_video_track(broadcast);
        }
        Ok(Err(error)) => return Err(error).context("reading Goq catalog snapshot"),
        Ok(Ok(None)) => anyhow::bail!("Goq catalog track ended before its snapshot"),
        Ok(Ok(Some(group))) => group,
    };
    anyhow::ensure!(
        group.sequence == 0,
        "Goq catalog snapshot must be immutable group 0"
    );
    let frame_count = tokio::time::timeout_at(deadline, group.finished())
        .await
        .context("timed out waiting for Goq catalog group completion")?
        .context("finishing Goq catalog group")?;
    anyhow::ensure!(
        frame_count == 1,
        "Goq catalog snapshot group must contain exactly one frame"
    );
    let mut frame = tokio::time::timeout_at(deadline, group.get_frame(0))
        .await
        .context("timed out waiting for Goq catalog frame")?
        .context("opening Goq catalog frame")?
        .context("Goq catalog snapshot group has no frame 0")?;
    anyhow::ensure!(
        frame.size <= MAX_MOQ_CATALOG_BYTES as u64,
        "Goq catalog snapshot exceeds {MAX_MOQ_CATALOG_BYTES} bytes"
    );
    let snapshot = tokio::time::timeout_at(deadline, frame.read_all())
        .await
        .context("timed out reading Goq catalog frame")?
        .context("reading Goq catalog frame")?;
    let document: GoqCatalogDocument =
        serde_json::from_slice(&snapshot).context("decoding Goq catalog snapshot")?;
    document
        .validate()
        .context("validating Goq catalog document")?;
    let track = broadcast
        .subscribe_track(&Track::new(document.goq.video.track.name))
        .context("subscribing to catalog-selected Goq video track")?;
    Ok(MoqCatalogSelection {
        track,
        mode: MoqCatalogMode::GoqV1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_net::Broadcast;

    fn publish_catalog(
        broadcast: &mut moq_net::BroadcastProducer,
        document: &GoqCatalogDocument,
    ) -> moq_net::TrackProducer {
        let mut track = broadcast
            .create_track(hang::Catalog::default_track())
            .unwrap();
        track
            .write_frame(serde_json::to_vec(document).unwrap())
            .unwrap();
        track
    }

    #[tokio::test]
    async fn catalog_selects_the_exact_goq_video_track() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let _catalog = publish_catalog(&mut broadcast, &GoqCatalogDocument::video_h264());
        let selection = subscribe_goq_video_track(&broadcast.consume(), Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(selection.mode, MoqCatalogMode::GoqV1);
    }

    #[tokio::test]
    async fn only_an_absent_catalog_uses_the_legacy_static_track() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let selection = subscribe_goq_video_track(&broadcast.consume(), Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(selection.mode, MoqCatalogMode::AbsentStaticTrackCompat);
    }

    #[tokio::test]
    async fn asynchronous_remote_not_found_uses_static_track_compatibility() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let mut dynamic = broadcast.dynamic();
        let consumer = broadcast.consume();

        let select = subscribe_goq_video_track(&consumer, Duration::from_millis(100));
        let reject = async {
            let mut requested = dynamic.requested_track().await.unwrap();
            assert_eq!(requested.name, hang::Catalog::default_track().name);
            requested
                .abort(MoqError::Remote(MoqError::NotFound.to_code()))
                .unwrap();
        };
        let (selection, ()) = tokio::join!(select, reject);
        assert_eq!(
            selection.unwrap().mode,
            MoqCatalogMode::AbsentStaticTrackCompat
        );
    }

    #[tokio::test]
    async fn asynchronous_non_not_found_error_fails_closed() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let mut dynamic = broadcast.dynamic();
        let consumer = broadcast.consume();

        let select = subscribe_goq_video_track(&consumer, Duration::from_millis(100));
        let reject = async {
            let mut requested = dynamic.requested_track().await.unwrap();
            requested
                .abort(MoqError::Remote(MoqError::WrongSize.to_code()))
                .unwrap();
        };
        let (selection, ()) = tokio::join!(select, reject);
        let error = selection.err().expect("non-NotFound must fail closed");
        assert!(
            format!("{error:#}").contains("remote error: code=14"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn present_invalid_or_stalled_catalogs_fail_closed() {
        let mut invalid_broadcast = Broadcast::new().produce();
        let _video = invalid_broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let mut invalid = GoqCatalogDocument::video_h264();
        invalid.goq.video.object_format = "hang/legacy".into();
        let _catalog = publish_catalog(&mut invalid_broadcast, &invalid);
        let result =
            subscribe_goq_video_track(&invalid_broadcast.consume(), Duration::from_millis(100))
                .await;
        assert!(result.err().is_some_and(|error| {
            format!("{error:#}").contains("validating Goq catalog document")
        }));

        let mut stalled_broadcast = Broadcast::new().produce();
        let _catalog = stalled_broadcast
            .create_track(hang::Catalog::default_track())
            .unwrap();
        let result =
            subscribe_goq_video_track(&stalled_broadcast.consume(), Duration::from_millis(10))
                .await;
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("timed out"))
        );

        let mut oversized_broadcast = Broadcast::new().produce();
        let _video = oversized_broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let mut catalog = oversized_broadcast
            .create_track(hang::Catalog::default_track())
            .unwrap();
        catalog
            .write_frame(vec![b' '; MAX_MOQ_CATALOG_BYTES + 1])
            .unwrap();
        let result =
            subscribe_goq_video_track(&oversized_broadcast.consume(), Duration::from_millis(100))
                .await;
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("exceeds 4096 bytes"))
        );

        let mut multi_frame_broadcast = Broadcast::new().produce();
        let _video = multi_frame_broadcast
            .create_track(Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
            .unwrap();
        let mut catalog = multi_frame_broadcast
            .create_track(hang::Catalog::default_track())
            .unwrap();
        let mut group = catalog.append_group().unwrap();
        group
            .write_frame(serde_json::to_vec(&GoqCatalogDocument::video_h264()).unwrap())
            .unwrap();
        group.write_frame(b"{}".as_slice()).unwrap();
        group.finish().unwrap();
        let result =
            subscribe_goq_video_track(&multi_frame_broadcast.consume(), Duration::from_millis(100))
                .await;
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("exactly one frame"))
        );
    }
}
