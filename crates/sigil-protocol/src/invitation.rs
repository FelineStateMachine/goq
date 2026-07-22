use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{PROTOCOL_VERSION, ProtocolError, Result};

const INVITATION_MAGIC: [u8; 4] = *b"SGIV";
const CLAIMS_LEN: usize = 127;
const SIGNATURE_LEN: usize = 64;
const INVITATION_LEN: usize = CLAIMS_LEN + SIGNATURE_LEN;
const SIGNATURE_DOMAIN: &[u8] = b"goq.sh/sigil-invitation/v1\0";

pub const INVITATION_TOKEN_PREFIX: &str = "goq-invite-v1.";
pub const MAX_INVITATION_TOKEN_LEN: usize = 384;
pub const MAX_INVITATION_TTL_SECS: u64 = 15 * 60;
pub const INVITATION_CLOCK_SKEW_SECS: u64 = 60;

/// Product authorization scopes. These are deliberately separate from wire
/// capabilities, which also contain operational protocol extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvitationGrants(u8);

impl InvitationGrants {
    pub const VIEW: Self = Self(1 << 0);
    pub const POINTER_KEYBOARD: Self = Self(1 << 1);
    pub const GAMEPAD: Self = Self(1 << 2);
    pub const ALL: Self = Self(Self::VIEW.0 | Self::POINTER_KEYBOARD.0 | Self::GAMEPAD.0);
    const KNOWN: u8 = Self::ALL.0;

    pub fn new(bits: u8) -> Result<Self> {
        if bits == 0 || bits & !Self::KNOWN != 0 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation grants",
                reason: "grants must contain only known non-zero permission bits",
            });
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

    pub fn validate(self) -> Result<()> {
        Self::new(self.0).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitationClaims {
    pub host_node_id: [u8; 32],
    pub intended_peer_id: [u8; 32],
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub enrollment_epoch: u64,
    pub nonce: [u8; 32],
    pub grants: InvitationGrants,
}

impl InvitationClaims {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_node_id: [u8; 32],
        intended_peer_id: [u8; 32],
        issued_at_unix: u64,
        expires_at_unix: u64,
        enrollment_epoch: u64,
        nonce: [u8; 32],
        grants: InvitationGrants,
    ) -> Result<Self> {
        let claims = Self {
            host_node_id,
            intended_peer_id,
            issued_at_unix,
            expires_at_unix,
            enrollment_epoch,
            nonce,
            grants,
        };
        claims.validate()?;
        Ok(claims)
    }

    pub fn validate(&self) -> Result<()> {
        self.grants.validate()?;
        if self.issued_at_unix >= self.expires_at_unix
            || self.expires_at_unix - self.issued_at_unix > MAX_INVITATION_TTL_SECS
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "invitation lifetime must be 1..=900 seconds",
            });
        }
        if self.enrollment_epoch == 0 {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "enrollment epoch must be non-zero",
            });
        }
        if self.nonce == [0; 32] {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "invitation nonce must be non-zero",
            });
        }
        VerifyingKey::from_bytes(&self.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "host node ID is not a valid Ed25519 public key",
            }
        })?;
        VerifyingKey::from_bytes(&self.intended_peer_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "intended peer ID is not a valid Ed25519 public key",
            }
        })?;
        Ok(())
    }

    fn encode(&self) -> [u8; CLAIMS_LEN] {
        let mut bytes = [0_u8; CLAIMS_LEN];
        bytes[0..4].copy_from_slice(&INVITATION_MAGIC);
        bytes[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[6..38].copy_from_slice(&self.host_node_id);
        bytes[38..70].copy_from_slice(&self.intended_peer_id);
        bytes[70..78].copy_from_slice(&self.issued_at_unix.to_be_bytes());
        bytes[78..86].copy_from_slice(&self.expires_at_unix.to_be_bytes());
        bytes[86..94].copy_from_slice(&self.enrollment_epoch.to_be_bytes());
        bytes[94..126].copy_from_slice(&self.nonce);
        bytes[126] = self.grants.bits();
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
pub struct SignedInvitation {
    pub claims: InvitationClaims,
    signature: [u8; SIGNATURE_LEN],
}

impl SignedInvitation {
    pub fn issue(claims: InvitationClaims, host_secret: &[u8; 32]) -> Result<Self> {
        claims.validate()?;
        let signing_key = SigningKey::from_bytes(host_secret);
        if signing_key.verifying_key().to_bytes() != claims.host_node_id {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation claims",
                reason: "signing key does not match the claimed host",
            });
        }
        let signature = signing_key.sign(&claims.signing_message()).to_bytes();
        Ok(Self { claims, signature })
    }

    pub fn verify(&self) -> Result<()> {
        self.claims.validate()?;
        let key = VerifyingKey::from_bytes(&self.claims.host_node_id).map_err(|_| {
            ProtocolError::InvalidMessage {
                message_type: "invitation",
                reason: "invalid host verification key",
            }
        })?;
        let signature = Signature::from_bytes(&self.signature);
        key.verify_strict(&self.claims.signing_message(), &signature)
            .map_err(|_| ProtocolError::InvalidMessage {
                message_type: "invitation",
                reason: "signature verification failed",
            })
    }

    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(INVITATION_LEN);
        bytes.extend_from_slice(&self.claims.encode());
        bytes.extend_from_slice(&self.signature);
        format!("{INVITATION_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(token: &str) -> Result<Self> {
        if token.len() > MAX_INVITATION_TOKEN_LEN
            || token.contains('=')
            || token.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation",
                reason: "token is oversized or non-canonical",
            });
        }
        let encoded =
            token
                .strip_prefix(INVITATION_TOKEN_PREFIX)
                .ok_or(ProtocolError::InvalidMessage {
                    message_type: "invitation",
                    reason: "token prefix is invalid",
                })?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ProtocolError::InvalidMessage {
                message_type: "invitation",
                reason: "token is not canonical base64url",
            })?;
        if bytes.len() != INVITATION_LEN {
            return Err(ProtocolError::InvalidMessageLength {
                actual: bytes.len(),
                maximum: INVITATION_LEN,
            });
        }
        let magic: [u8; 4] = bytes[0..4].try_into().expect("fixed ticket length");
        if magic != INVITATION_MAGIC {
            return Err(ProtocolError::InvalidInvitationMagic(magic));
        }
        let version = u16::from_be_bytes(bytes[4..6].try_into().expect("fixed ticket length"));
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                expected: PROTOCOL_VERSION,
                actual: version,
            });
        }
        let claims = InvitationClaims::new(
            bytes[6..38].try_into().expect("fixed ticket length"),
            bytes[38..70].try_into().expect("fixed ticket length"),
            u64::from_be_bytes(bytes[70..78].try_into().expect("fixed ticket length")),
            u64::from_be_bytes(bytes[78..86].try_into().expect("fixed ticket length")),
            u64::from_be_bytes(bytes[86..94].try_into().expect("fixed ticket length")),
            bytes[94..126].try_into().expect("fixed ticket length"),
            InvitationGrants::new(bytes[126])?,
        )?;
        let invitation = Self {
            claims,
            signature: bytes[CLAIMS_LEN..]
                .try_into()
                .expect("fixed signature length"),
        };
        invitation.verify()?;
        if invitation.encode() != token {
            return Err(ProtocolError::InvalidMessage {
                message_type: "invitation",
                reason: "token encoding is not canonical",
            });
        }
        Ok(invitation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation() -> SignedInvitation {
        let host = SigningKey::from_bytes(&[7; 32]);
        let peer = SigningKey::from_bytes(&[9; 32]);
        let claims = InvitationClaims::new(
            host.verifying_key().to_bytes(),
            peer.verifying_key().to_bytes(),
            1_700_000_000,
            1_700_000_600,
            3,
            [11; 32],
            InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
        )
        .unwrap();
        SignedInvitation::issue(claims, &[7; 32]).unwrap()
    }

    #[test]
    fn signed_invitation_is_canonical_and_round_trips() {
        let invitation = invitation();
        let token = invitation.encode();
        assert_eq!(
            token,
            "goq-invite-v1.U0dJVgAB6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iz9FyQ4WqDHW2T7eM1gL6HZkf3r92sTxY7XAurINen2GAAAAABlU_EAAAAAAGVT81gAAAAAAAAAAwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLBUuO9fzI2gGLT6L3Lz5XPM500MPzW5eaB2h4FnOWZHM9ENlCqMERXCvwxZTG3hFR4WumyJeqoAlqH_gCeJOsbQs"
        );
        assert!(token.starts_with(INVITATION_TOKEN_PREFIX));
        assert!(token.len() <= MAX_INVITATION_TOKEN_LEN);
        assert_eq!(SignedInvitation::decode(&token).unwrap(), invitation);
    }

    #[test]
    fn tampering_and_noncanonical_tokens_fail_closed() {
        let token = invitation().encode();
        let mut bytes = URL_SAFE_NO_PAD
            .decode(token.strip_prefix(INVITATION_TOKEN_PREFIX).unwrap())
            .unwrap();
        bytes[126] ^= InvitationGrants::POINTER_KEYBOARD.bits();
        let tampered = format!("{INVITATION_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
        assert!(SignedInvitation::decode(&tampered).is_err());
        assert!(SignedInvitation::decode(&(token + "=")).is_err());
    }

    #[test]
    fn malformed_ticket_shapes_fail_closed() {
        let token = invitation().encode();
        let encoded = token.strip_prefix(INVITATION_TOKEN_PREFIX).unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();

        let encode =
            |bytes: &[u8]| format!("{INVITATION_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
        let mut bad_magic = bytes.clone();
        bad_magic[0] ^= 0xff;
        let mut bad_version = bytes.clone();
        bad_version[5] ^= 0xff;
        let mut truncated = bytes.clone();
        truncated.pop();
        let mut oversized = bytes.clone();
        oversized.push(0);

        let malformed = [
            (
                "wrong prefix",
                token.replacen(INVITATION_TOKEN_PREFIX, "wrong.", 1),
            ),
            ("leading whitespace", format!(" {token}")),
            ("trailing whitespace", format!("{token}\n")),
            ("invalid base64url", format!("{INVITATION_TOKEN_PREFIX}!")),
            ("wrong decoded size", encode(&truncated)),
            ("oversized decoded body", encode(&oversized)),
            ("wrong magic", encode(&bad_magic)),
            ("wrong version", encode(&bad_version)),
        ];

        for (case, malformed) in malformed {
            assert!(
                SignedInvitation::decode(&malformed).is_err(),
                "malformed invitation unexpectedly accepted: {case}"
            );
        }
    }

    #[test]
    fn every_nonzero_known_grant_combination_round_trips_exactly() {
        let host = SigningKey::from_bytes(&[7; 32]);
        let peer = SigningKey::from_bytes(&[9; 32]);

        for bits in 1..=InvitationGrants::ALL.bits() {
            let grants = InvitationGrants::new(bits).unwrap();
            let claims = InvitationClaims::new(
                host.verifying_key().to_bytes(),
                peer.verifying_key().to_bytes(),
                1_700_000_000,
                1_700_000_600,
                3,
                [bits; 32],
                grants,
            )
            .unwrap();
            let token = SignedInvitation::issue(claims, &[7; 32]).unwrap().encode();

            assert_eq!(
                SignedInvitation::decode(&token).unwrap().claims.grants,
                grants
            );
        }
    }

    #[test]
    fn claims_reject_unknown_grants_lifetime_epoch_and_nonce() {
        assert!(InvitationGrants::new(0).is_err());
        assert!(InvitationGrants::new(0x80).is_err());
        let host = SigningKey::from_bytes(&[7; 32]);
        let peer = SigningKey::from_bytes(&[9; 32]);
        let make = |issued, expires, epoch, nonce| {
            InvitationClaims::new(
                host.verifying_key().to_bytes(),
                peer.verifying_key().to_bytes(),
                issued,
                expires,
                epoch,
                nonce,
                InvitationGrants::VIEW,
            )
        };
        assert!(make(10, 10, 1, [1; 32]).is_err());
        assert!(make(10, 911, 1, [1; 32]).is_err());
        assert!(make(10, 20, 0, [1; 32]).is_err());
        assert!(make(10, 20, 1, [0; 32]).is_err());
    }

    #[test]
    fn signing_key_must_match_claimed_host() {
        let invitation = invitation();
        assert!(SignedInvitation::issue(invitation.claims, &[8; 32]).is_err());
    }
}
