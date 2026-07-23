use anyhow::{Context, Result, ensure};
use moq_net::BroadcastProducer;
use sigil_protocol::{GoqCatalogDocument, MAX_MOQ_CATALOG_BYTES};

pub(crate) struct GoqCatalogProducer {
    producer: moq_json::Producer<GoqCatalogDocument>,
}

impl GoqCatalogProducer {
    pub(crate) fn finish(mut self) -> Result<()> {
        self.producer
            .finish()
            .context("finishing Goq catalog track")
    }
}

pub(crate) fn publish_goq_catalog(broadcast: &mut BroadcastProducer) -> Result<GoqCatalogProducer> {
    let track = broadcast
        .create_track(hang::Catalog::default_track())
        .context("creating catalog.json track")?;
    let mut producer = moq_json::Producer::new(track, moq_json::Config::default());
    let document = GoqCatalogDocument::video_h264();
    document
        .validate()
        .context("validating Goq catalog document")?;
    let snapshot = serde_json::to_vec(&document).context("serializing Goq catalog snapshot")?;
    ensure!(
        snapshot.len() <= MAX_MOQ_CATALOG_BYTES,
        "Goq catalog snapshot exceeds {MAX_MOQ_CATALOG_BYTES} bytes"
    );
    producer
        .update(&document)
        .context("publishing immutable Goq catalog snapshot")?;
    Ok(GoqCatalogProducer { producer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_net::{Broadcast, Track};
    use std::time::Duration;

    #[tokio::test]
    async fn catalog_snapshot_is_late_subscribable_and_hang_compatible() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track {
                name: sigil_protocol::MOQ_VIDEO_H264_TRACK.to_owned(),
                priority: sigil_protocol::MOQ_VIDEO_TRACK_PRIORITY,
            })
            .unwrap();
        let catalog = publish_goq_catalog(&mut broadcast).unwrap();

        let base_track = broadcast
            .consume()
            .subscribe_track(&hang::Catalog::default_track())
            .unwrap();
        let mut base_consumer = moq_json::Consumer::<hang::Catalog>::new(base_track);
        let base = tokio::time::timeout(Duration::from_millis(100), base_consumer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(base, hang::Catalog::default());

        let document_track = broadcast
            .consume()
            .subscribe_track(&hang::Catalog::default_track())
            .unwrap();
        let mut document_consumer = moq_json::Consumer::<GoqCatalogDocument>::new(document_track);
        let document = tokio::time::timeout(Duration::from_millis(100), document_consumer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        document.validate().unwrap();
        assert_eq!(document, GoqCatalogDocument::video_h264());
        catalog.finish().unwrap();
    }
}
