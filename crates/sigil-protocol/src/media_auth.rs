use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use sha2::{Digest as _, Sha256};

use crate::{MAX_MEDIA_PAYLOAD_LEN, MEDIA_HEADER_LEN, ProtocolError, Result};

const CERTIFICATE_MAGIC: [u8; 4] = *b"SGMC";
const CERTIFICATE_VERSION: u16 = 1;
const CERTIFICATE_CLAIMS_LEN: usize = 96;
const SIGNATURE_LEN: usize = 64;
const CERTIFICATE_LEN: usize = CERTIFICATE_CLAIMS_LEN + SIGNATURE_LEN;
const CERTIFICATE_DOMAIN: &[u8] = b"goq.sh/sigil-media-certificate/v1\0";
const OBJECT_MAGIC: [u8; 4] = *b"SGMA";
const OBJECT_VERSION: u16 = 1;
const OBJECT_UNSIGNED_HEADER_LEN: usize = 70;
const OBJECT_DOMAIN: &[u8] = b"goq.sh/sigil-media-object/v1\0";
const MAX_GENERATION_CERTIFICATE_TTL_SECS: u64 = 24 * 60 * 60;

pub const MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX: &str = "goq-media-cert-v1.";
pub const MAX_MEDIA_GENERATION_CERTIFICATE_TOKEN_LEN: usize = 256;
pub const AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN: usize = OBJECT_UNSIGNED_HEADER_LEN + SIGNATURE_LEN;
const MAX_AUTHENTICATED_PAYLOAD_LEN: usize = MEDIA_HEADER_LEN + MAX_MEDIA_PAYLOAD_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaAuthMode {
    Ed25519 = 1,
}

impl MediaAuthMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519-v1",
        }
    }
}

impl TryFrom<u8> for MediaAuthMode {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Ed25519),
            _ => invalid(
                "media authentication mode",
                "unsupported authentication mode",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaTrack {
    VideoH264 = 1,
    AudioOpus = 2,
}

impl MediaTrack {
    pub const fn name(self) -> &'static str {
        match self {
            Self::VideoH264 => crate::MOQ_VIDEO_H264_TRACK,
            Self::AudioOpus => "audio/opus",
        }
    }
}

impl TryFrom<u8> for MediaTrack {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::VideoH264),
            2 => Ok(Self::AudioOpus),
            _ => invalid("authenticated media object", "unsupported media track"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaGenerationCertificateClaims {
    pub host_node_id: [u8; 32],
    pub generation_id: u64,
    pub generation_signing_key: [u8; 32],
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub authentication_mode: MediaAuthMode,
}

impl MediaGenerationCertificateClaims {
    pub fn new(
        host_node_id: [u8; 32],
        generation_id: u64,
        generation_signing_key: [u8; 32],
        issued_at_unix: u64,
        expires_at_unix: u64,
    ) -> Result<Self> {
        let claims = Self {
            host_node_id,
            generation_id,
            generation_signing_key,
            issued_at_unix,
            expires_at_unix,
            authentication_mode: MediaAuthMode::Ed25519,
        };
        claims.validate()?;
        Ok(claims)
    }

    pub fn validate(&self) -> Result<()> {
        if self.authentication_mode != MediaAuthMode::Ed25519 {
            return invalid(
                "media generation certificate",
                "unsupported authentication mode",
            );
        }
        if self.generation_id == 0 {
            return invalid(
                "media generation certificate",
                "generation id must be non-zero",
            );
        }
        if self.issued_at_unix >= self.expires_at_unix
            || self.expires_at_unix - self.issued_at_unix > MAX_GENERATION_CERTIFICATE_TTL_SECS
        {
            return invalid(
                "media generation certificate",
                "certificate lifetime must be within 1..=86400 seconds",
            );
        }
        VerifyingKey::from_bytes(&self.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "media generation certificate",
                reason: "host node id is not an Ed25519 public key",
            }
        })?;
        VerifyingKey::from_bytes(&self.generation_signing_key).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "media generation certificate",
                reason: "generation key is not an Ed25519 public key",
            }
        })?;
        Ok(())
    }

    fn encode(&self) -> [u8; CERTIFICATE_CLAIMS_LEN] {
        let mut bytes = [0_u8; CERTIFICATE_CLAIMS_LEN];
        bytes[0..4].copy_from_slice(&CERTIFICATE_MAGIC);
        bytes[4..6].copy_from_slice(&CERTIFICATE_VERSION.to_be_bytes());
        bytes[6] = self.authentication_mode as u8;
        bytes[8..40].copy_from_slice(&self.host_node_id);
        bytes[40..48].copy_from_slice(&self.generation_id.to_be_bytes());
        bytes[48..80].copy_from_slice(&self.generation_signing_key);
        bytes[80..88].copy_from_slice(&self.issued_at_unix.to_be_bytes());
        bytes[88..96].copy_from_slice(&self.expires_at_unix.to_be_bytes());
        bytes
    }

    fn signing_message(&self) -> Vec<u8> {
        let mut message = Vec::with_capacity(CERTIFICATE_DOMAIN.len() + CERTIFICATE_CLAIMS_LEN);
        message.extend_from_slice(CERTIFICATE_DOMAIN);
        message.extend_from_slice(&self.encode());
        message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedMediaGenerationCertificate {
    pub claims: MediaGenerationCertificateClaims,
    signature: [u8; SIGNATURE_LEN],
}

impl SignedMediaGenerationCertificate {
    pub fn issue(claims: MediaGenerationCertificateClaims, host_secret: &[u8; 32]) -> Result<Self> {
        claims.validate()?;
        let signing_key = SigningKey::from_bytes(host_secret);
        if signing_key.verifying_key().to_bytes() != claims.host_node_id {
            return invalid(
                "media generation certificate",
                "host signing key does not match the certificate host",
            );
        }
        let signature = signing_key.sign(&claims.signing_message()).to_bytes();
        Ok(Self { claims, signature })
    }

    pub fn verify(&self) -> Result<()> {
        self.claims.validate()?;
        let key = VerifyingKey::from_bytes(&self.claims.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "media generation certificate",
                reason: "invalid host verification key",
            }
        })?;
        key.verify_strict(
            &self.claims.signing_message(),
            &Signature::from_bytes(&self.signature),
        )
        .map_err(|_| ProtocolError::InvalidMessage {
            message_type: "media generation certificate",
            reason: "host signature verification failed",
        })
    }

    pub fn verify_binding(
        &self,
        expected_host: [u8; 32],
        expected_generation: u64,
        now_unix: u64,
    ) -> Result<()> {
        self.verify()?;
        if self.claims.host_node_id != expected_host {
            return invalid(
                "media generation certificate",
                "certificate is bound to another host",
            );
        }
        if self.claims.generation_id != expected_generation {
            return invalid(
                "media generation certificate",
                "certificate is bound to another generation",
            );
        }
        if now_unix < self.claims.issued_at_unix || now_unix > self.claims.expires_at_unix {
            return invalid(
                "media generation certificate",
                "certificate is not currently valid",
            );
        }
        Ok(())
    }

    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(CERTIFICATE_LEN);
        bytes.extend_from_slice(&self.claims.encode());
        bytes.extend_from_slice(&self.signature);
        format!(
            "{MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )
    }

    pub fn decode(token: &str) -> Result<Self> {
        if token.len() > MAX_MEDIA_GENERATION_CERTIFICATE_TOKEN_LEN
            || token.contains('=')
            || token.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return invalid(
                "media generation certificate",
                "certificate token is oversized or non-canonical",
            );
        }
        let encoded = token
            .strip_prefix(MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX)
            .ok_or(ProtocolError::InvalidMessage {
                message_type: "media generation certificate",
                reason: "certificate token prefix is invalid",
            })?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ProtocolError::InvalidMessage {
                message_type: "media generation certificate",
                reason: "certificate token is not canonical base64url",
            })?;
        if bytes.len() != CERTIFICATE_LEN {
            return Err(ProtocolError::InvalidMessageLength {
                actual: bytes.len(),
                maximum: CERTIFICATE_LEN,
            });
        }
        if bytes[0..4] != CERTIFICATE_MAGIC {
            return invalid(
                "media generation certificate",
                "certificate magic is invalid",
            );
        }
        let version = u16::from_be_bytes(bytes[4..6].try_into().expect("fixed certificate"));
        if version != CERTIFICATE_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: CERTIFICATE_VERSION,
                actual: version,
            });
        }
        if bytes[7] != 0 {
            return invalid(
                "media generation certificate",
                "reserved fields must be zero",
            );
        }
        let claims = MediaGenerationCertificateClaims {
            authentication_mode: MediaAuthMode::try_from(bytes[6])?,
            host_node_id: bytes[8..40].try_into().expect("fixed certificate"),
            generation_id: u64::from_be_bytes(bytes[40..48].try_into().expect("fixed certificate")),
            generation_signing_key: bytes[48..80].try_into().expect("fixed certificate"),
            issued_at_unix: u64::from_be_bytes(
                bytes[80..88].try_into().expect("fixed certificate"),
            ),
            expires_at_unix: u64::from_be_bytes(
                bytes[88..96].try_into().expect("fixed certificate"),
            ),
        };
        let certificate = Self {
            claims,
            signature: bytes[CERTIFICATE_CLAIMS_LEN..]
                .try_into()
                .expect("fixed certificate signature"),
        };
        certificate.verify()?;
        if certificate.encode() != token {
            return invalid(
                "media generation certificate",
                "certificate encoding is not canonical",
            );
        }
        Ok(certificate)
    }
}

pub struct MediaGenerationSigningKey(SigningKey);

impl std::fmt::Debug for MediaGenerationSigningKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MediaGenerationSigningKey([REDACTED])")
    }
}

impl MediaGenerationSigningKey {
    pub fn from_bytes(secret: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(secret))
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    pub fn certify(
        &self,
        host_node_id: [u8; 32],
        host_secret: &[u8; 32],
        generation_id: u64,
        issued_at_unix: u64,
        expires_at_unix: u64,
    ) -> Result<SignedMediaGenerationCertificate> {
        SignedMediaGenerationCertificate::issue(
            MediaGenerationCertificateClaims::new(
                host_node_id,
                generation_id,
                self.public_key(),
                issued_at_unix,
                expires_at_unix,
            )?,
            host_secret,
        )
    }

    pub fn authenticate(
        &self,
        coordinates: MediaObjectCoordinates,
        payload: &[u8],
    ) -> Result<AuthenticatedMediaObject> {
        coordinates.validate(payload.len())?;
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let unsigned = encode_unsigned_object_header(coordinates, payload.len(), digest)?;
        let mut message = Vec::with_capacity(OBJECT_DOMAIN.len() + unsigned.len());
        message.extend_from_slice(OBJECT_DOMAIN);
        message.extend_from_slice(&unsigned);
        let signature = self.0.sign(&message).to_bytes();
        let mut bytes = Vec::with_capacity(AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN + payload.len());
        bytes.extend_from_slice(&unsigned);
        bytes.extend_from_slice(&signature);
        bytes.extend_from_slice(payload);
        Ok(AuthenticatedMediaObject { bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaObjectCoordinates {
    pub generation_id: u64,
    pub track: MediaTrack,
    pub group_id: u64,
    pub object_id: u32,
    pub flags: u16,
}

impl MediaObjectCoordinates {
    fn validate(&self, payload_len: usize) -> Result<()> {
        if self.generation_id == 0 {
            return invalid(
                "authenticated media object",
                "generation id must be non-zero",
            );
        }
        if self.flags & !0x0007 != 0 {
            return invalid("authenticated media object", "unknown media flags are set");
        }
        if payload_len == 0 || payload_len > MAX_AUTHENTICATED_PAYLOAD_LEN {
            return Err(ProtocolError::InvalidMediaPayloadLength {
                actual: payload_len,
                maximum: MAX_AUTHENTICATED_PAYLOAD_LEN,
            });
        }
        u32::try_from(payload_len).map_err(|_| ProtocolError::InvalidMediaPayloadLength {
            actual: payload_len,
            maximum: MAX_AUTHENTICATED_PAYLOAD_LEN,
        })?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedMediaObject {
    bytes: Vec<u8>,
}

impl AuthenticatedMediaObject {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn coordinates(object: &[u8]) -> Result<MediaObjectCoordinates> {
        if object.len() < AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN {
            return invalid(
                "authenticated media object",
                "object ended before its authentication header",
            );
        }
        let unsigned = &object[..OBJECT_UNSIGNED_HEADER_LEN];
        if unsigned[0..4] != OBJECT_MAGIC {
            return invalid(
                "authenticated media object",
                "authentication magic is invalid",
            );
        }
        let version = u16::from_be_bytes(unsigned[4..6].try_into().expect("fixed auth header"));
        if version != OBJECT_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: OBJECT_VERSION,
                actual: version,
            });
        }
        let header_len = u16::from_be_bytes(unsigned[6..8].try_into().expect("fixed auth header"));
        if usize::from(header_len) != AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN {
            return invalid(
                "authenticated media object",
                "authentication header length is invalid",
            );
        }
        if MediaAuthMode::try_from(unsigned[8])? != MediaAuthMode::Ed25519
            || unsigned[12..14].iter().any(|byte| *byte != 0)
        {
            return invalid(
                "authenticated media object",
                "authentication mode or reserved fields are invalid",
            );
        }
        let coordinates = MediaObjectCoordinates {
            generation_id: u64::from_be_bytes(
                unsigned[14..22].try_into().expect("fixed auth header"),
            ),
            track: MediaTrack::try_from(unsigned[9])?,
            group_id: u64::from_be_bytes(unsigned[22..30].try_into().expect("fixed auth header")),
            object_id: u32::from_be_bytes(unsigned[30..34].try_into().expect("fixed auth header")),
            flags: u16::from_be_bytes(unsigned[10..12].try_into().expect("fixed auth header")),
        };
        let payload_len =
            u32::from_be_bytes(unsigned[34..38].try_into().expect("fixed auth header")) as usize;
        coordinates.validate(payload_len)?;
        Ok(coordinates)
    }

    pub fn verify<'a>(
        object: &'a [u8],
        certificate: &SignedMediaGenerationCertificate,
        expected: MediaObjectCoordinates,
    ) -> Result<&'a [u8]> {
        certificate.verify()?;
        let actual = Self::coordinates(object)?;
        let unsigned = &object[..OBJECT_UNSIGNED_HEADER_LEN];
        let payload_len =
            u32::from_be_bytes(unsigned[34..38].try_into().expect("fixed auth header")) as usize;
        if actual != expected || actual.generation_id != certificate.claims.generation_id {
            return invalid(
                "authenticated media object",
                "object coordinates do not match delivery coordinates",
            );
        }
        let payload = &object[AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN..];
        if payload.len() != payload_len {
            return invalid(
                "authenticated media object",
                "payload length does not match authentication header",
            );
        }
        let expected_digest: [u8; 32] = unsigned[38..70].try_into().expect("fixed auth digest");
        let actual_digest: [u8; 32] = Sha256::digest(payload).into();
        if actual_digest != expected_digest {
            return invalid(
                "authenticated media object",
                "payload digest verification failed",
            );
        }
        let key =
            VerifyingKey::from_bytes(&certificate.claims.generation_signing_key).map_err(|_| {
                ProtocolError::InvalidMessage {
                    message_type: "authenticated media object",
                    reason: "certificate generation key is invalid",
                }
            })?;
        let mut message = Vec::with_capacity(OBJECT_DOMAIN.len() + unsigned.len());
        message.extend_from_slice(OBJECT_DOMAIN);
        message.extend_from_slice(unsigned);
        let signature = Signature::from_bytes(
            object[OBJECT_UNSIGNED_HEADER_LEN..AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN]
                .try_into()
                .expect("fixed auth signature"),
        );
        key.verify_strict(&message, &signature)
            .map_err(|_| ProtocolError::InvalidMessage {
                message_type: "authenticated media object",
                reason: "object signature verification failed",
            })?;
        Ok(payload)
    }
}

fn encode_unsigned_object_header(
    coordinates: MediaObjectCoordinates,
    payload_len: usize,
    digest: [u8; 32],
) -> Result<[u8; OBJECT_UNSIGNED_HEADER_LEN]> {
    coordinates.validate(payload_len)?;
    let mut bytes = [0_u8; OBJECT_UNSIGNED_HEADER_LEN];
    bytes[0..4].copy_from_slice(&OBJECT_MAGIC);
    bytes[4..6].copy_from_slice(&OBJECT_VERSION.to_be_bytes());
    bytes[6..8].copy_from_slice(&(AUTHENTICATED_MEDIA_OBJECT_HEADER_LEN as u16).to_be_bytes());
    bytes[8] = MediaAuthMode::Ed25519 as u8;
    bytes[9] = coordinates.track as u8;
    bytes[10..12].copy_from_slice(&coordinates.flags.to_be_bytes());
    bytes[14..22].copy_from_slice(&coordinates.generation_id.to_be_bytes());
    bytes[22..30].copy_from_slice(&coordinates.group_id.to_be_bytes());
    bytes[30..34].copy_from_slice(&coordinates.object_id.to_be_bytes());
    bytes[34..38].copy_from_slice(&(payload_len as u32).to_be_bytes());
    bytes[38..70].copy_from_slice(&digest);
    Ok(bytes)
}

fn invalid<T>(message_type: &'static str, reason: &'static str) -> Result<T> {
    Err(ProtocolError::InvalidMessage {
        message_type,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (MediaGenerationSigningKey, SignedMediaGenerationCertificate) {
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
        (generation, certificate)
    }

    #[test]
    fn generation_certificate_is_canonical_and_host_bound() {
        let (_, certificate) = fixture();
        let token = certificate.encode();
        assert!(token.starts_with(MEDIA_GENERATION_CERTIFICATE_TOKEN_PREFIX));
        assert_eq!(
            SignedMediaGenerationCertificate::decode(&token).unwrap(),
            certificate
        );
        certificate
            .verify_binding(certificate.claims.host_node_id, 42, 1_700_000_300)
            .unwrap();
        assert!(
            certificate
                .verify_binding([1; 32], 42, 1_700_000_300)
                .is_err()
        );
        assert!(
            certificate
                .verify_binding(certificate.claims.host_node_id, 43, 1_700_000_300)
                .is_err()
        );
        assert!(
            certificate
                .verify_binding(certificate.claims.host_node_id, 42, 1_700_001_000)
                .is_err()
        );
    }

    #[test]
    fn authenticated_object_header_has_stable_golden_prefix_and_round_trips() {
        let (generation, certificate) = fixture();
        let coordinates = MediaObjectCoordinates {
            generation_id: 42,
            track: MediaTrack::VideoH264,
            group_id: 3,
            object_id: 4,
            flags: 5,
        };
        let object = generation.authenticate(coordinates, b"frame").unwrap();
        assert_eq!(
            &object.as_bytes()[..38],
            &[
                0x53, 0x47, 0x4d, 0x41, 0, 1, 0, 134, 1, 1, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0,
                0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5,
            ]
        );
        assert_eq!(
            AuthenticatedMediaObject::verify(object.as_bytes(), &certificate, coordinates).unwrap(),
            b"frame"
        );
    }

    #[test]
    fn tampering_and_wrong_coordinates_fail_closed() {
        let (generation, certificate) = fixture();
        let coordinates = MediaObjectCoordinates {
            generation_id: 42,
            track: MediaTrack::VideoH264,
            group_id: 3,
            object_id: 4,
            flags: 0,
        };
        let object = generation
            .authenticate(coordinates, b"frame")
            .unwrap()
            .into_bytes();
        let mut payload_tampered = object.clone();
        *payload_tampered.last_mut().unwrap() ^= 1;
        assert!(
            AuthenticatedMediaObject::verify(&payload_tampered, &certificate, coordinates).is_err()
        );
        let mut signature_tampered = object.clone();
        signature_tampered[OBJECT_UNSIGNED_HEADER_LEN] ^= 1;
        assert!(
            AuthenticatedMediaObject::verify(&signature_tampered, &certificate, coordinates)
                .is_err()
        );
        let wrong = MediaObjectCoordinates {
            object_id: 5,
            ..coordinates
        };
        assert!(AuthenticatedMediaObject::verify(&object, &certificate, wrong).is_err());
    }

    #[test]
    fn malformed_lengths_versions_modes_and_reserved_fields_fail_closed() {
        let (generation, certificate) = fixture();
        let coordinates = MediaObjectCoordinates {
            generation_id: 42,
            track: MediaTrack::VideoH264,
            group_id: 0,
            object_id: 0,
            flags: 0,
        };
        let valid = generation
            .authenticate(coordinates, b"frame")
            .unwrap()
            .into_bytes();
        for offset in [0, 5, 7, 8, 9, 12] {
            let mut malformed = valid.clone();
            malformed[offset] ^= 0xff;
            assert!(
                AuthenticatedMediaObject::verify(&malformed, &certificate, coordinates).is_err(),
                "accepted malformed offset {offset}"
            );
        }
        assert!(
            AuthenticatedMediaObject::verify(&valid[..100], &certificate, coordinates).is_err()
        );
    }
}
