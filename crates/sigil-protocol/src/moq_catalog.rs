use serde::{Deserialize, Serialize};

use crate::{
    MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX, MOQ_VIDEO_H264_TRACK, MediaAuthMode, ProtocolError,
    Result, SignedMediaGenerationCertificate,
};

pub const MOQ_CATALOG_EXTENSION_VERSION_V1: u16 = 1;
pub const MOQ_CATALOG_EXTENSION_VERSION_V2: u16 = 2;
/// Maximum immutable catalog snapshot accepted from a peer before JSON decoding.
pub const MAX_MOQ_CATALOG_BYTES: usize = 4 * 1024;
pub const MOQ_MEDIA_OBJECT_FORMAT_V1: &str = "sigil/media-frame/1";
pub const MOQ_AUTHENTICATED_MEDIA_OBJECT_FORMAT_V1: &str = "sigil/authenticated-media-object/1";
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqTrackAuthenticationV2 {
    pub mode: String,
    pub object_format: String,
    pub generation_certificate: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqVideoCatalogV2 {
    pub track: MoqTrackDescriptorV1,
    pub codec: String,
    pub payload_format: String,
    pub group_format: String,
    pub authentication: MoqTrackAuthenticationV2,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoqCatalogExtensionV2 {
    pub version: u16,
    pub generation_id: u64,
    pub video: MoqVideoCatalogV2,
}

impl MoqCatalogExtensionV2 {
    pub fn video_h264(
        generation_id: u64,
        certificate: &SignedMediaGenerationCertificate,
    ) -> Result<Self> {
        let extension = Self {
            version: MOQ_CATALOG_EXTENSION_VERSION_V2,
            generation_id,
            video: MoqVideoCatalogV2 {
                track: MoqTrackDescriptorV1 {
                    name: MOQ_VIDEO_H264_TRACK.to_owned(),
                    priority: MOQ_VIDEO_TRACK_PRIORITY,
                },
                codec: "h264".to_owned(),
                payload_format: MOQ_MEDIA_OBJECT_FORMAT_V1.to_owned(),
                group_format: MOQ_GOP_GROUP_FORMAT_V1.to_owned(),
                authentication: MoqTrackAuthenticationV2 {
                    mode: MediaAuthMode::Ed25519.label().to_owned(),
                    object_format: MOQ_AUTHENTICATED_MEDIA_OBJECT_FORMAT_V1.to_owned(),
                    generation_certificate: certificate.encode(),
                },
            },
        };
        extension.validate()?;
        Ok(extension)
    }

    pub fn validate(&self) -> Result<SignedMediaGenerationCertificate> {
        if self.version != MOQ_CATALOG_EXTENSION_VERSION_V2 || self.generation_id == 0 {
            return invalid_v2("unsupported extension version or zero generation");
        }
        if self.video.track.name != MOQ_VIDEO_H264_TRACK
            || self.video.track.priority != MOQ_VIDEO_TRACK_PRIORITY
            || self.video.codec != "h264"
            || self.video.payload_format != MOQ_MEDIA_OBJECT_FORMAT_V1
            || self.video.group_format != MOQ_GOP_GROUP_FORMAT_V1
        {
            return invalid_v2("video track contract does not match authenticated H.264");
        }
        if self.video.authentication.mode != MediaAuthMode::Ed25519.label()
            || self.video.authentication.object_format != MOQ_AUTHENTICATED_MEDIA_OBJECT_FORMAT_V1
            || !self
                .video
                .authentication
                .generation_certificate
                .starts_with(MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX)
        {
            return invalid_v2("unknown or malformed media authentication contract");
        }
        let certificate = SignedMediaGenerationCertificate::decode(
            &self.video.authentication.generation_certificate,
        )?;
        if certificate.claims.generation_id != self.generation_id
            || certificate.claims.authentication_mode != MediaAuthMode::Ed25519
        {
            return invalid_v2("generation certificate does not match the catalog generation");
        }
        Ok(certificate)
    }
}

fn invalid_v2<T>(reason: &'static str) -> Result<T> {
    Err(ProtocolError::InvalidMessage {
        message_type: "Goq MoQ catalog extension v2",
        reason,
    })
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoqCatalogDocumentV2 {
    #[serde(flatten)]
    pub media: hang::Catalog,
    pub goq: MoqCatalogExtensionV2,
}

impl GoqCatalogDocumentV2 {
    pub fn video_h264(
        generation_id: u64,
        certificate: &SignedMediaGenerationCertificate,
    ) -> Result<Self> {
        Ok(Self {
            media: hang::Catalog::default(),
            goq: MoqCatalogExtensionV2::video_h264(generation_id, certificate)?,
        })
    }

    pub fn validate(&self) -> Result<SignedMediaGenerationCertificate> {
        if self.media != hang::Catalog::default() {
            return Err(ProtocolError::InvalidMessage {
                message_type: "Goq MoQ catalog document v2",
                reason: "catalog must not advertise a standard Hang rendition for enveloped media",
            });
        }
        self.goq.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaGenerationSigningKey;
    use ed25519_dalek::SigningKey;

    fn v2_document() -> GoqCatalogDocumentV2 {
        let host = SigningKey::from_bytes(&[7; 32]);
        let generation = MediaGenerationSigningKey::from_bytes(&[9; 32]);
        let certificate = generation
            .certify(
                host.verifying_key().to_bytes(),
                &[7; 32],
                42,
                1_700_000_000,
                1_700_000_600,
            )
            .unwrap();
        GoqCatalogDocumentV2::video_h264(42, &certificate).unwrap()
    }

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

    #[test]
    fn authenticated_v2_catalog_is_strict_and_preserves_v1_decoding() {
        let document = v2_document();
        let certificate = document.validate().unwrap();
        assert_eq!(certificate.claims.generation_id, 42);
        let json = serde_json::to_string(&document).unwrap();
        assert!(json.contains(r#""version":2"#));
        assert!(json.contains(r#""mode":"ed25519-v1""#));
        assert!(json.contains(MOQ_AUTHENTICATED_MEDIA_OBJECT_FORMAT_V1));
        assert_eq!(
            serde_json::from_str::<GoqCatalogDocumentV2>(&json).unwrap(),
            document
        );

        let legacy = serde_json::to_string(&GoqCatalogDocument::video_h264()).unwrap();
        serde_json::from_str::<GoqCatalogDocument>(&legacy)
            .unwrap()
            .validate()
            .unwrap();
        assert!(serde_json::from_str::<GoqCatalogDocumentV2>(&legacy).is_err());
    }

    #[test]
    fn authenticated_v2_catalog_rejects_unknown_mode_generation_and_fields() {
        let mut wrong_mode = v2_document();
        wrong_mode.goq.video.authentication.mode = "shared-mac-v1".into();
        assert!(wrong_mode.validate().is_err());

        let mut wrong_generation = v2_document();
        wrong_generation.goq.generation_id = 43;
        assert!(wrong_generation.validate().is_err());

        let mut value = serde_json::to_value(v2_document()).unwrap();
        value["goq"]["video"]["authentication"]["key"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<GoqCatalogDocumentV2>(value).is_err());
    }
}
