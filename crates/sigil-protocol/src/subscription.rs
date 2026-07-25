use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use crate::{ProtocolError, Result};

const SUBSCRIPTION_MAGIC: [u8; 4] = *b"SGSC";
const SUBSCRIPTION_VERSION: u16 = 1;
const CLAIMS_LEN: usize = 136;
const SIGNATURE_LEN: usize = 64;
const CAPABILITY_LEN: usize = CLAIMS_LEN + SIGNATURE_LEN;
const SIGNATURE_DOMAIN: &[u8] = b"goq.sh/sigil-subscription/v1\0";

pub const SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX: &str = "goq-subscription-v1.";
pub const MAX_SUBSCRIPTION_CAPABILITY_TOKEN_LEN: usize = 320;
pub const MAX_SUBSCRIPTION_TTL_SECS: u64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionTracks(u8);

impl SubscriptionTracks {
    pub const VIDEO_H264: Self = Self(1 << 0);
    pub const AUDIO_OPUS: Self = Self(1 << 1);
    pub const ALL: Self = Self(Self::VIDEO_H264.0 | Self::AUDIO_OPUS.0);
    const KNOWN: u8 = Self::ALL.0;

    pub fn new(bits: u8) -> Result<Self> {
        if bits == 0 || bits & !Self::KNOWN != 0 {
            return invalid(
                "subscription tracks",
                "tracks must contain only known non-zero bits",
            );
        }
        Ok(Self(bits))
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionClaims {
    pub host_node_id: [u8; 32],
    pub media_generation_id: u64,
    pub subscriber_endpoint_id: [u8; 32],
    pub tracks: SubscriptionTracks,
    pub authorization_revision: u64,
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub nonce: [u8; 32],
    pub relay_hops: u8,
}

impl SubscriptionClaims {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_node_id: [u8; 32],
        media_generation_id: u64,
        subscriber_endpoint_id: [u8; 32],
        tracks: SubscriptionTracks,
        authorization_revision: u64,
        issued_at_unix: u64,
        expires_at_unix: u64,
        nonce: [u8; 32],
        relay_hops: u8,
    ) -> Result<Self> {
        let claims = Self {
            host_node_id,
            media_generation_id,
            subscriber_endpoint_id,
            tracks,
            authorization_revision,
            issued_at_unix,
            expires_at_unix,
            nonce,
            relay_hops,
        };
        claims.validate()?;
        Ok(claims)
    }

    pub fn validate(&self) -> Result<()> {
        SubscriptionTracks::new(self.tracks.bits())?;
        if self.media_generation_id == 0 || self.authorization_revision == 0 {
            return invalid(
                "subscription capability",
                "media generation and authorization revision must be non-zero",
            );
        }
        if self.relay_hops != 1 {
            return invalid(
                "subscription capability",
                "exactly one authorized relay hop is required",
            );
        }
        if self.issued_at_unix >= self.expires_at_unix
            || self.expires_at_unix - self.issued_at_unix > MAX_SUBSCRIPTION_TTL_SECS
        {
            return invalid(
                "subscription capability",
                "capability lifetime must be within 1..=900 seconds",
            );
        }
        if self.nonce == [0; 32] {
            return invalid("subscription capability", "nonce must be non-zero");
        }
        VerifyingKey::from_bytes(&self.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "subscription capability",
                reason: "host node id is not an Ed25519 public key",
            }
        })?;
        VerifyingKey::from_bytes(&self.subscriber_endpoint_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "subscription capability",
                reason: "subscriber endpoint id is not an Ed25519 public key",
            }
        })?;
        Ok(())
    }

    fn encode(&self) -> [u8; CLAIMS_LEN] {
        let mut bytes = [0_u8; CLAIMS_LEN];
        bytes[0..4].copy_from_slice(&SUBSCRIPTION_MAGIC);
        bytes[4..6].copy_from_slice(&SUBSCRIPTION_VERSION.to_be_bytes());
        bytes[6] = self.relay_hops;
        bytes[7] = self.tracks.bits();
        bytes[8..40].copy_from_slice(&self.host_node_id);
        bytes[40..48].copy_from_slice(&self.media_generation_id.to_be_bytes());
        bytes[48..80].copy_from_slice(&self.subscriber_endpoint_id);
        bytes[80..88].copy_from_slice(&self.authorization_revision.to_be_bytes());
        bytes[88..96].copy_from_slice(&self.issued_at_unix.to_be_bytes());
        bytes[96..104].copy_from_slice(&self.expires_at_unix.to_be_bytes());
        bytes[104..136].copy_from_slice(&self.nonce);
        bytes
    }

    fn signing_message(&self) -> Vec<u8> {
        let mut message = Vec::with_capacity(SIGNATURE_DOMAIN.len() + CLAIMS_LEN);
        message.extend_from_slice(SIGNATURE_DOMAIN);
        message.extend_from_slice(&self.encode());
        message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedSubscriptionCapability {
    pub claims: SubscriptionClaims,
    signature: [u8; SIGNATURE_LEN],
}

impl SignedSubscriptionCapability {
    pub fn issue(claims: SubscriptionClaims, host_secret: &[u8; 32]) -> Result<Self> {
        claims.validate()?;
        let signing_key = SigningKey::from_bytes(host_secret);
        if signing_key.verifying_key().to_bytes() != claims.host_node_id {
            return invalid(
                "subscription capability",
                "host signing key does not match the capability host",
            );
        }
        let signature = signing_key.sign(&claims.signing_message()).to_bytes();
        Ok(Self { claims, signature })
    }

    pub fn verify(&self) -> Result<()> {
        self.claims.validate()?;
        let key = VerifyingKey::from_bytes(&self.claims.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "subscription capability",
                reason: "invalid host verification key",
            }
        })?;
        key.verify_strict(
            &self.claims.signing_message(),
            &Signature::from_bytes(&self.signature),
        )
        .map_err(|_| ProtocolError::InvalidMessage {
            message_type: "subscription capability",
            reason: "host signature verification failed",
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn verify_binding(
        &self,
        expected_host: [u8; 32],
        expected_generation: u64,
        expected_subscriber: [u8; 32],
        required_tracks: SubscriptionTracks,
        expected_authorization_revision: u64,
        now_unix: u64,
    ) -> Result<()> {
        self.verify()?;
        if self.claims.host_node_id != expected_host {
            return invalid(
                "subscription capability",
                "capability is bound to another host",
            );
        }
        if self.claims.media_generation_id != expected_generation {
            return invalid(
                "subscription capability",
                "capability is bound to another media generation",
            );
        }
        if self.claims.subscriber_endpoint_id != expected_subscriber {
            return invalid(
                "subscription capability",
                "capability is bound to another subscriber",
            );
        }
        if !self.claims.tracks.contains(required_tracks) {
            return invalid(
                "subscription capability",
                "capability does not authorize the required tracks",
            );
        }
        if self.claims.authorization_revision != expected_authorization_revision {
            return invalid(
                "subscription capability",
                "capability authorization revision is stale",
            );
        }
        if now_unix < self.claims.issued_at_unix || now_unix > self.claims.expires_at_unix {
            return invalid(
                "subscription capability",
                "capability is not currently valid",
            );
        }
        Ok(())
    }

    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(CAPABILITY_LEN);
        bytes.extend_from_slice(&self.claims.encode());
        bytes.extend_from_slice(&self.signature);
        format!(
            "{SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )
    }

    pub fn decode(token: &str) -> Result<Self> {
        if token.len() > MAX_SUBSCRIPTION_CAPABILITY_TOKEN_LEN
            || token.contains('=')
            || token.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return invalid(
                "subscription capability",
                "capability token is oversized or non-canonical",
            );
        }
        let encoded = token
            .strip_prefix(SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX)
            .ok_or(ProtocolError::InvalidMessage {
                message_type: "subscription capability",
                reason: "capability token prefix is invalid",
            })?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ProtocolError::InvalidMessage {
                message_type: "subscription capability",
                reason: "capability token is not canonical base64url",
            })?;
        if bytes.len() != CAPABILITY_LEN {
            return Err(ProtocolError::InvalidMessageLength {
                actual: bytes.len(),
                maximum: CAPABILITY_LEN,
            });
        }
        if bytes[0..4] != SUBSCRIPTION_MAGIC {
            return invalid("subscription capability", "capability magic is invalid");
        }
        let version = u16::from_be_bytes(bytes[4..6].try_into().expect("fixed capability"));
        if version != SUBSCRIPTION_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: SUBSCRIPTION_VERSION,
                actual: version,
            });
        }
        let claims = SubscriptionClaims::new(
            bytes[8..40].try_into().expect("fixed capability"),
            u64::from_be_bytes(bytes[40..48].try_into().expect("fixed capability")),
            bytes[48..80].try_into().expect("fixed capability"),
            SubscriptionTracks::new(bytes[7])?,
            u64::from_be_bytes(bytes[80..88].try_into().expect("fixed capability")),
            u64::from_be_bytes(bytes[88..96].try_into().expect("fixed capability")),
            u64::from_be_bytes(bytes[96..104].try_into().expect("fixed capability")),
            bytes[104..136].try_into().expect("fixed capability"),
            bytes[6],
        )?;
        let capability = Self {
            claims,
            signature: bytes[CLAIMS_LEN..]
                .try_into()
                .expect("fixed capability signature"),
        };
        capability.verify()?;
        if capability.encode() != token {
            return invalid(
                "subscription capability",
                "capability encoding is not canonical",
            );
        }
        Ok(capability)
    }
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

    fn capability() -> SignedSubscriptionCapability {
        let host = SigningKey::from_bytes(&[7; 32]);
        let subscriber = SigningKey::from_bytes(&[9; 32]);
        SignedSubscriptionCapability::issue(
            SubscriptionClaims::new(
                host.verifying_key().to_bytes(),
                42,
                subscriber.verifying_key().to_bytes(),
                SubscriptionTracks::VIDEO_H264,
                3,
                1_700_000_000,
                1_700_000_600,
                [11; 32],
                1,
            )
            .unwrap(),
            &[7; 32],
        )
        .unwrap()
    }

    #[test]
    fn endpoint_bound_capability_is_canonical_and_round_trips() {
        let capability = capability();
        let token = capability.encode();
        assert!(token.starts_with(SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX));
        assert_eq!(
            SignedSubscriptionCapability::decode(&token).unwrap(),
            capability
        );
        capability
            .verify_binding(
                capability.claims.host_node_id,
                42,
                capability.claims.subscriber_endpoint_id,
                SubscriptionTracks::VIDEO_H264,
                3,
                1_700_000_300,
            )
            .unwrap();
    }

    #[test]
    fn wrong_host_subscriber_generation_revision_track_and_time_fail_closed() {
        let capability = capability();
        let valid = (
            capability.claims.host_node_id,
            42,
            capability.claims.subscriber_endpoint_id,
            SubscriptionTracks::VIDEO_H264,
            3,
            1_700_000_300,
        );
        assert!(
            capability
                .verify_binding([1; 32], valid.1, valid.2, valid.3, valid.4, valid.5)
                .is_err()
        );
        assert!(
            capability
                .verify_binding(valid.0, 43, valid.2, valid.3, valid.4, valid.5)
                .is_err()
        );
        assert!(
            capability
                .verify_binding(valid.0, valid.1, [2; 32], valid.3, valid.4, valid.5)
                .is_err()
        );
        assert!(
            capability
                .verify_binding(
                    valid.0,
                    valid.1,
                    valid.2,
                    SubscriptionTracks::AUDIO_OPUS,
                    valid.4,
                    valid.5
                )
                .is_err()
        );
        assert!(
            capability
                .verify_binding(valid.0, valid.1, valid.2, valid.3, 4, valid.5)
                .is_err()
        );
        assert!(
            capability
                .verify_binding(valid.0, valid.1, valid.2, valid.3, valid.4, 1_700_001_000)
                .is_err()
        );
    }

    #[test]
    fn tampering_unknown_tracks_hops_and_noncanonical_tokens_fail_closed() {
        let token = capability().encode();
        let encoded = token
            .strip_prefix(SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX)
            .unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        for (offset, value) in [(0, 0), (5, 2), (6, 2), (7, 0x80), (CLAIMS_LEN, 0)] {
            let mut malformed = bytes.clone();
            malformed[offset] = value;
            let token = format!(
                "{SUBSCRIPTION_CAPABILITY_TOKEN_PREFIX}{}",
                URL_SAFE_NO_PAD.encode(malformed)
            );
            assert!(SignedSubscriptionCapability::decode(&token).is_err());
        }
        assert!(SignedSubscriptionCapability::decode(&(token + "=")).is_err());
    }
}
