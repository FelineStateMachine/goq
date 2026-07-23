use serde::{Deserialize, Serialize};

use crate::{MOQ_VIDEO_H264_TRACK, ProtocolError, Result};

pub const MOQ_CATALOG_EXTENSION_VERSION_V1: u16 = 1;
/// Maximum immutable catalog snapshot accepted from a peer before JSON decoding.
pub const MAX_MOQ_CATALOG_BYTES: usize = 4 * 1024;
pub const MOQ_MEDIA_OBJECT_FORMAT_V1: &str = "sigil/media-frame/1";
pub const MOQ_GOP_GROUP_FORMAT_V1: &str = "sigil/moq-gop/1";
pub const MOQ_VIDEO_TRACK_PRIORITY: u8 = u8::MAX;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqTrackDescriptorV1 {
    pub name: String,
    pub priority: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqVideoCatalogV1 {
    pub track: MoqTrackDescriptorV1,
    pub codec: String,
    pub object_format: String,
    pub group_format: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqCatalogExtensionV1 {
    pub version: u16,
    pub video: MoqVideoCatalogV1,
}

impl MoqCatalogExtensionV1 {
    pub fn video_h264() -> Self {
        Self {
            version: MOQ_CATALOG_EXTENSION_VERSION_V1,
            video: MoqVideoCatalogV1 {
                track: MoqTrackDescriptorV1 {
                    name: MOQ_VIDEO_H264_TRACK.to_owned(),
                    priority: MOQ_VIDEO_TRACK_PRIORITY,
                },
                codec: "h264".to_owned(),
                object_format: MOQ_MEDIA_OBJECT_FORMAT_V1.to_owned(),
                group_format: MOQ_GOP_GROUP_FORMAT_V1.to_owned(),
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != MOQ_CATALOG_EXTENSION_VERSION_V1 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unsupported extension version",
            });
        }
        if self.video.track.name != MOQ_VIDEO_H264_TRACK {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unexpected video track name",
            });
        }
        if self.video.track.priority != MOQ_VIDEO_TRACK_PRIORITY {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unexpected video track priority",
            });
        }
        if self.video.codec != "h264" {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unsupported video codec",
            });
        }
        if self.video.object_format != MOQ_MEDIA_OBJECT_FORMAT_V1 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unsupported media object format",
            });
        }
        if self.video.group_format != MOQ_GOP_GROUP_FORMAT_V1 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog extension",
                reason: "unsupported GOP group format",
            });
        }
        Ok(())
    }
}

/// The immutable catalog.json document both peers must agree on byte-for-byte:
/// a default (empty) Hang catalog envelope carrying the Goq extension. This is
/// the single definition — the host producer, Portal subscriber, and probe all
/// consume it from here so the wire shape cannot drift between peers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoqCatalogDocument {
    #[serde(flatten)]
    pub media: hang::Catalog,
    pub goq: MoqCatalogExtensionV1,
}

impl GoqCatalogDocument {
    pub fn video_h264() -> Self {
        Self {
            media: hang::Catalog::default(),
            goq: MoqCatalogExtensionV1::video_h264(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.media != hang::Catalog::default() {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog document",
                reason: "catalog must not advertise a standard Hang rendition for enveloped media",
            });
        }
        self.goq.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goq_catalog_extension_has_a_stable_golden_document() {
        let extension = MoqCatalogExtensionV1::video_h264();
        extension.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&extension).unwrap(),
            r#"{"version":1,"video":{"track":{"name":"video/h264","priority":255},"codec":"h264","objectFormat":"sigil/media-frame/1","groupFormat":"sigil/moq-gop/1"}}"#
        );
    }

    #[test]
    fn goq_catalog_document_has_a_stable_golden_envelope() {
        let document = GoqCatalogDocument::video_h264();
        document.validate().unwrap();
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(
            json["goq"],
            serde_json::to_value(MoqCatalogExtensionV1::video_h264()).unwrap()
        );
        let round_trip: GoqCatalogDocument = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, document);
    }

    #[test]
    fn goq_catalog_validation_rejects_every_mismatched_contract_field() {
        let mut cases = Vec::new();

        let mut value = MoqCatalogExtensionV1::video_h264();
        value.version = 2;
        cases.push(value);
        let mut value = MoqCatalogExtensionV1::video_h264();
        value.video.track.name = "video/other".into();
        cases.push(value);
        let mut value = MoqCatalogExtensionV1::video_h264();
        value.video.track.priority = 0;
        cases.push(value);
        let mut value = MoqCatalogExtensionV1::video_h264();
        value.video.codec = "av1".into();
        cases.push(value);
        let mut value = MoqCatalogExtensionV1::video_h264();
        value.video.object_format = "hang/legacy".into();
        cases.push(value);
        let mut value = MoqCatalogExtensionV1::video_h264();
        value.video.group_format = "hang/gop".into();
        cases.push(value);

        for invalid in cases {
            assert!(invalid.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn goq_catalog_extension_rejects_unknown_fields() {
        let json = r#"{"version":1,"video":{"track":{"name":"video/h264","priority":255,"extra":true},"codec":"h264","objectFormat":"sigil/media-frame/1","groupFormat":"sigil/moq-gop/1"}}"#;
        assert!(serde_json::from_str::<MoqCatalogExtensionV1>(json).is_err());
    }
}
