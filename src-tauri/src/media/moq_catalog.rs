use std::time::Duration;

use moq_net::{BroadcastConsumer, Error as MoqError, Track, TrackConsumer};
use serde::{Deserialize, Serialize};
use sigil_protocol::{MAX_MOQ_CATALOG_BYTES, MoqCatalogExtensionV1};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoqCatalogDocument {
    #[serde(flatten)]
    media: hang::Catalog,
    goq: MoqCatalogExtensionV1,
}

impl GoqCatalogDocument {
    #[cfg(test)]
    fn video_h264() -> Self {
        Self {
            media: hang::Catalog::default(),
            goq: MoqCatalogExtensionV1::video_h264(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.media != hang::Catalog::default() {
            return Err(
                "Goq catalog falsely advertises a standard Hang rendition for enveloped media"
                    .to_string(),
            );
        }
        self.goq
            .validate()
            .map_err(|error| format!("Invalid Goq catalog extension: {error}"))
    }
}

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

fn subscribe_static_video_track(
    broadcast: &BroadcastConsumer,
) -> Result<MoqCatalogSelection, String> {
    let track = broadcast
        .subscribe_track(&Track::new(sigil_protocol::MOQ_VIDEO_H264_TRACK))
        .map_err(|error| {
            format!("Failed to subscribe to static MoQ H.264 compatibility track: {error}")
        })?;
    Ok(MoqCatalogSelection {
        track,
        mode: MoqCatalogMode::AbsentStaticTrackCompat,
    })
}

pub(crate) async fn subscribe_goq_video_track(
    broadcast: &BroadcastConsumer,
    timeout: Duration,
) -> Result<MoqCatalogSelection, String> {
    let catalog_track = match broadcast.subscribe_track(&hang::Catalog::default_track()) {
        Ok(track) => track,
        Err(MoqError::NotFound) => {
            return subscribe_static_video_track(broadcast);
        }
        Err(error) => {
            return Err(format!(
                "Failed to subscribe to catalog.json track: {error}"
            ));
        }
    };

    let mut catalog_track = catalog_track;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut group = match tokio::time::timeout_at(deadline, catalog_track.next_group()).await {
        Err(_) => return Err("Timed out waiting for Goq catalog snapshot".to_string()),
        Ok(Err(error)) if is_track_not_found(&error) => {
            return subscribe_static_video_track(broadcast);
        }
        Ok(Err(error)) => return Err(format!("Failed to read Goq catalog snapshot: {error}")),
        Ok(Ok(None)) => return Err("Goq catalog track ended before its snapshot".to_string()),
        Ok(Ok(Some(group))) => group,
    };
    if group.sequence != 0 {
        return Err("Goq catalog snapshot must be immutable group 0".to_string());
    }
    let frame_count = tokio::time::timeout_at(deadline, group.finished())
        .await
        .map_err(|_| "Timed out waiting for Goq catalog group completion".to_string())?
        .map_err(|error| format!("Failed to finish Goq catalog group: {error}"))?;
    if frame_count != 1 {
        return Err("Goq catalog snapshot group must contain exactly one frame".to_string());
    }
    let mut frame = tokio::time::timeout_at(deadline, group.get_frame(0))
        .await
        .map_err(|_| "Timed out waiting for Goq catalog frame".to_string())?
        .map_err(|error| format!("Failed to open Goq catalog frame: {error}"))?
        .ok_or_else(|| "Goq catalog snapshot group has no frame 0".to_string())?;
    if frame.size > MAX_MOQ_CATALOG_BYTES as u64 {
        return Err(format!(
            "Goq catalog snapshot exceeds {MAX_MOQ_CATALOG_BYTES} bytes"
        ));
    }
    let snapshot = tokio::time::timeout_at(deadline, frame.read_all())
        .await
        .map_err(|_| "Timed out reading Goq catalog frame".to_string())?
        .map_err(|error| format!("Failed to read Goq catalog frame: {error}"))?;
    let document: GoqCatalogDocument = serde_json::from_slice(&snapshot)
        .map_err(|error| format!("Failed to decode Goq catalog snapshot: {error}"))?;
    document.validate()?;
    let track = broadcast
        .subscribe_track(&Track::new(document.goq.video.track.name))
        .map_err(|error| {
            format!("Failed to subscribe to catalog-selected Goq video track: {error}")
        })?;
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
        assert!(
            selection
                .err()
                .is_some_and(|error| error.contains("remote error: code=14"))
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
        assert!(
            result
                .err()
                .is_some_and(|error| error.contains("Invalid Goq catalog extension"))
        );

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
                .is_some_and(|error| error.contains("Timed out"))
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
                .is_some_and(|error| error.contains("exceeds 4096 bytes"))
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
                .is_some_and(|error| error.contains("exactly one frame"))
        );
    }
}
