use anyhow::{Context, Result, ensure};
use moq_net::BroadcastProducer;
use sigil_protocol::{
    GoqCatalogDocument, GoqCatalogDocumentV2, MAX_MOQ_CATALOG_BYTES,
    SignedMediaGenerationCertificate,
};

pub(crate) enum GoqCatalogProducer {
    V1(moq_json::Producer<GoqCatalogDocument>),
    V2(moq_json::Producer<GoqCatalogDocumentV2>),
}

impl GoqCatalogProducer {
    pub(crate) fn finish(self) -> Result<()> {
        match self {
            Self::V1(mut producer) => producer.finish(),
            Self::V2(mut producer) => producer.finish(),
        }
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
    Ok(GoqCatalogProducer::V1(producer))
}

pub(crate) fn publish_goq_catalog_v2(
    broadcast: &mut BroadcastProducer,
    generation_id: u64,
    certificate: &SignedMediaGenerationCertificate,
) -> Result<GoqCatalogProducer> {
    let track = broadcast
        .create_track(hang::Catalog::default_track())
        .context("creating authenticated catalog.json track")?;
    let mut producer = moq_json::Producer::new(track, moq_json::Config::default());
    let document = GoqCatalogDocumentV2::video_h264(generation_id, certificate)
        .context("constructing authenticated Goq catalog")?;
    document
        .validate()
        .context("validating authenticated Goq catalog document")?;
    let snapshot =
        serde_json::to_vec(&document).context("serializing authenticated Goq catalog snapshot")?;
    ensure!(
        snapshot.len() <= MAX_MOQ_CATALOG_BYTES,
        "authenticated Goq catalog snapshot exceeds {MAX_MOQ_CATALOG_BYTES} bytes"
    );
    producer
        .update(&document)
        .context("publishing immutable authenticated Goq catalog snapshot")?;
    Ok(GoqCatalogProducer::V2(producer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_net::{Broadcast, Track};
    use std::time::Duration;

    fn certificate() -> SignedMediaGenerationCertificate {
        let host = iroh::SecretKey::from_bytes(&[7; 32]);
        let generation = sigil_protocol::MediaGenerationSigningKey::from_bytes(&[9; 32]);
        generation
            .certify(
                *host.public().as_bytes(),
                &host.to_bytes(),
                42,
                1_700_000_000,
                1_700_000_600,
            )
            .unwrap()
    }

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

    #[tokio::test]
    async fn authenticated_catalog_snapshot_carries_the_certified_generation() {
        let mut broadcast = Broadcast::new().produce();
        let _video = broadcast
            .create_track(Track {
                name: sigil_protocol::MOQ_VIDEO_H264_TRACK.to_owned(),
                priority: sigil_protocol::MOQ_VIDEO_TRACK_PRIORITY,
            })
            .unwrap();
        let certificate = certificate();
        let catalog = publish_goq_catalog_v2(&mut broadcast, 42, &certificate).unwrap();
        let document_track = broadcast
            .consume()
            .subscribe_track(&hang::Catalog::default_track())
            .unwrap();
        let mut consumer = moq_json::Consumer::<GoqCatalogDocumentV2>::new(document_track);
        let document = tokio::time::timeout(Duration::from_millis(100), consumer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(document.validate().unwrap(), certificate);
        catalog.finish().unwrap();
    }
}
