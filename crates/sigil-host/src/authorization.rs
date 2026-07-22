use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sigil_protocol::{
    INVITATION_CLOCK_SKEW_SECS, InvitationClaims, InvitationGrants, MAX_INVITATION_TTL_SECS,
    SignedInvitation,
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

const STATE_VERSION: u16 = 1;
const STATE_FILE_NAME: &str = "authorization-v1.json";
const LOCK_FILE_NAME: &str = "authorization-v1.lock";
const MAX_STATE_FILE_LEN: u64 = 64 * 1024;
const MAX_CONSUMED_INVITATIONS: usize = 64;

#[derive(Clone, Debug)]
pub struct AuthorizationStore {
    state_directory: PathBuf,
    host: EndpointId,
}

#[derive(Clone, Debug)]
pub enum AuthorizationPolicy {
    Required(AuthorizationStore),
    /// Explicit direct `sigil serve --identity ... --source test-pattern`
    /// proof mode. Configured hosts never select this branch.
    TestPatternProof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizationSnapshot {
    pub epoch: u64,
    pub peer: Option<EndpointId>,
    pub grants: Option<InvitationGrants>,
    pub enrolled_at_unix: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizationInspection {
    pub snapshot: AuthorizationSnapshot,
    pub storage_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevocationOutcome {
    pub had_enrollment: bool,
    pub previous_epoch: u64,
    pub current_epoch: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationState {
    version: u16,
    host_node_id: String,
    enrollment_epoch: u64,
    enrollment: Option<Enrollment>,
    consumed_invitations: Vec<ConsumedInvitation>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    peer_node_id: String,
    grants: InvitationGrants,
    enrolled_at_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsumedInvitation {
    nonce: [u8; 32],
    expires_at_unix: u64,
}

impl AuthorizationState {
    fn empty(host: EndpointId) -> Self {
        Self {
            version: STATE_VERSION,
            host_node_id: host.to_string(),
            enrollment_epoch: 1,
            enrollment: None,
            consumed_invitations: Vec::new(),
        }
    }

    fn validate(&self, expected_host: EndpointId) -> Result<()> {
        ensure!(
            self.version == STATE_VERSION,
            "unsupported authorization state version"
        );
        ensure!(
            self.host_node_id.parse::<EndpointId>().ok() == Some(expected_host),
            "authorization state belongs to a different Sigil host"
        );
        ensure!(
            self.enrollment_epoch != 0,
            "authorization epoch must be non-zero"
        );
        ensure!(
            self.consumed_invitations.len() <= MAX_CONSUMED_INVITATIONS,
            "authorization replay ledger exceeds its fixed bound"
        );
        if let Some(enrollment) = &self.enrollment {
            enrollment
                .peer_node_id
                .parse::<EndpointId>()
                .context("authorization enrollment contains an invalid peer node ID")?;
            enrollment.grants.validate()?;
            ensure!(
                enrollment.grants.contains(InvitationGrants::VIEW),
                "authorization enrollment must include view permission"
            );
        }
        for (index, consumed) in self.consumed_invitations.iter().enumerate() {
            ensure!(
                consumed.nonce != [0; 32],
                "consumed invitation nonce is zero"
            );
            ensure!(
                !self.consumed_invitations[..index]
                    .iter()
                    .any(|earlier| earlier.nonce == consumed.nonce),
                "authorization replay ledger contains a duplicate nonce"
            );
        }
        Ok(())
    }

    fn prune_expired(&mut self, now: u64) -> bool {
        let before = self.consumed_invitations.len();
        self.consumed_invitations.retain(|consumed| {
            consumed
                .expires_at_unix
                .saturating_add(INVITATION_CLOCK_SKEW_SECS)
                >= now
        });
        before != self.consumed_invitations.len()
    }
}

impl AuthorizationStore {
    pub fn open(state_directory: impl Into<PathBuf>, host: EndpointId) -> Result<Self> {
        let store = Self {
            state_directory: state_directory.into(),
            host,
        };
        store.ensure_state_directory()?;
        let lock = store.open_lock()?;
        store.lock_exclusive(&lock)?;
        let state = store.load_state()?;
        state.validate(host)?;
        Ok(store)
    }

    pub fn issue_claims(
        &self,
        peer: EndpointId,
        grants: InvitationGrants,
        ttl_seconds: u64,
        now: u64,
        nonce: [u8; 32],
    ) -> Result<InvitationClaims> {
        grants.validate()?;
        ensure!(
            grants.contains(InvitationGrants::VIEW),
            "invitations must grant view permission"
        );
        ensure!(
            (1..=MAX_INVITATION_TTL_SECS).contains(&ttl_seconds),
            "invitation TTL must be between 1 and {MAX_INVITATION_TTL_SECS} seconds"
        );
        let lock = self.open_lock()?;
        self.lock_exclusive(&lock)?;
        let state = self.load_state()?;
        state.validate(self.host)?;
        ensure!(
            state.enrollment.is_none(),
            "a Portal peer is already enrolled; revoke it before issuing another invitation"
        );
        InvitationClaims::new(
            *self.host.as_bytes(),
            *peer.as_bytes(),
            now,
            now.checked_add(ttl_seconds)
                .context("invitation expiry overflow")?,
            state.enrollment_epoch,
            nonce,
            grants,
        )
        .map_err(Into::into)
    }

    pub fn authorize_or_redeem(
        &self,
        remote: EndpointId,
        invitation_token: Option<&str>,
        now: u64,
    ) -> Result<InvitationGrants> {
        let lock = self.open_lock()?;
        self.lock_exclusive(&lock)?;
        let mut state = self.load_state()?;
        state.validate(self.host)?;
        let mut changed = state.prune_expired(now);

        let result = match invitation_token {
            None => {
                let enrollment = state
                    .enrollment
                    .as_ref()
                    .context("Portal peer is not enrolled")?;
                ensure!(
                    enrollment.peer_node_id.parse::<EndpointId>()? == remote,
                    "Portal peer is not enrolled"
                );
                enrollment.grants
            }
            Some(token) => {
                ensure!(
                    state.enrollment.is_none(),
                    "an invitation cannot be used after enrollment"
                );
                let invitation = SignedInvitation::decode(token)
                    .context("decoding and verifying enrollment invitation")?;
                let claims = invitation.claims;
                ensure!(
                    claims.host_node_id == *self.host.as_bytes(),
                    "invitation belongs to a different Sigil host"
                );
                ensure!(
                    claims.intended_peer_id == *remote.as_bytes(),
                    "invitation is bound to a different Portal peer"
                );
                ensure!(
                    claims.enrollment_epoch == state.enrollment_epoch,
                    "invitation enrollment epoch is stale"
                );
                ensure!(
                    claims.issued_at_unix <= now.saturating_add(INVITATION_CLOCK_SKEW_SECS),
                    "invitation was issued too far in the future"
                );
                ensure!(now <= claims.expires_at_unix, "invitation has expired");
                ensure!(
                    claims.grants.contains(InvitationGrants::VIEW),
                    "invitation does not grant view permission"
                );
                ensure!(
                    !state
                        .consumed_invitations
                        .iter()
                        .any(|consumed| consumed.nonce == claims.nonce),
                    "invitation has already been consumed"
                );
                ensure!(
                    state.consumed_invitations.len() < MAX_CONSUMED_INVITATIONS,
                    "authorization replay ledger is full"
                );
                state.enrollment = Some(Enrollment {
                    peer_node_id: remote.to_string(),
                    grants: claims.grants,
                    enrolled_at_unix: now,
                });
                state.consumed_invitations.push(ConsumedInvitation {
                    nonce: claims.nonce,
                    expires_at_unix: claims.expires_at_unix,
                });
                changed = true;
                claims.grants
            }
        };

        if changed {
            self.write_state(&state)?;
        }
        Ok(result)
    }

    pub fn snapshot(&self) -> Result<AuthorizationSnapshot> {
        let lock = self.open_lock()?;
        self.lock_exclusive(&lock)?;
        let state = self.load_state()?;
        state.validate(self.host)?;
        let (peer, grants, enrolled_at_unix) = match state.enrollment {
            Some(enrollment) => (
                Some(enrollment.peer_node_id.parse::<EndpointId>()?),
                Some(enrollment.grants),
                Some(enrollment.enrolled_at_unix),
            ),
            None => (None, None, None),
        };
        Ok(AuthorizationSnapshot {
            epoch: state.enrollment_epoch,
            peer,
            grants,
            enrolled_at_unix,
        })
    }

    /// Inspect durable enrollment without creating the state directory or lock.
    /// The authorization writer uses atomic replacement, so an unlocked reader
    /// observes either the previous complete document or the next one.
    pub fn inspect_existing(
        state_directory: impl Into<PathBuf>,
        host: EndpointId,
    ) -> Result<AuthorizationInspection> {
        let store = Self {
            state_directory: state_directory.into(),
            host,
        };
        let directory_metadata = match fs::symlink_metadata(&store.state_directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthorizationInspection {
                    snapshot: AuthorizationSnapshot {
                        epoch: 1,
                        peer: None,
                        grants: None,
                        enrolled_at_unix: None,
                    },
                    storage_present: false,
                });
            }
            Err(error) => return Err(error).context("inspecting authorization state directory"),
        };
        ensure!(
            !directory_metadata.file_type().is_symlink(),
            "authorization state directory is a symlink"
        );
        ensure!(
            directory_metadata.is_dir(),
            "authorization state path is not a directory"
        );
        #[cfg(unix)]
        {
            ensure!(
                directory_metadata.mode() & 0o077 == 0,
                "authorization state directory is accessible by group or others"
            );
            ensure!(
                directory_metadata.uid() == unsafe { libc::geteuid() },
                "authorization state directory has the wrong owner"
            );
        }

        let path = store.state_path();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthorizationInspection {
                    snapshot: AuthorizationSnapshot {
                        epoch: 1,
                        peer: None,
                        grants: None,
                        enrolled_at_unix: None,
                    },
                    storage_present: false,
                });
            }
            Err(error) => return Err(error).context("opening authorization state"),
        };
        store.validate_secure_file(&file, &path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.len() <= MAX_STATE_FILE_LEN,
            "authorization state exceeds its fixed size bound"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_STATE_FILE_LEN.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_FILE_LEN,
            "authorization state exceeds its fixed size bound"
        );
        ensure!(!bytes.is_empty(), "authorization state is empty");
        let state: AuthorizationState =
            serde_json::from_slice(&bytes).context("parsing authorization state")?;
        state.validate(host)?;
        let snapshot = match state.enrollment {
            Some(enrollment) => AuthorizationSnapshot {
                epoch: state.enrollment_epoch,
                peer: Some(enrollment.peer_node_id.parse::<EndpointId>()?),
                grants: Some(enrollment.grants),
                enrolled_at_unix: Some(enrollment.enrolled_at_unix),
            },
            None => AuthorizationSnapshot {
                epoch: state.enrollment_epoch,
                peer: None,
                grants: None,
                enrolled_at_unix: None,
            },
        };
        Ok(AuthorizationInspection {
            snapshot,
            storage_present: true,
        })
    }

    pub fn revoke_with_outcome(&self, now: u64) -> Result<RevocationOutcome> {
        let lock = self.open_lock()?;
        self.lock_exclusive(&lock)?;
        let mut state = self.load_state()?;
        state.validate(self.host)?;
        let previous_epoch = state.enrollment_epoch;
        let had_enrollment = state.enrollment.take().is_some();
        state.enrollment_epoch = state
            .enrollment_epoch
            .checked_add(1)
            .context("authorization epoch exhausted")?;
        state.prune_expired(now);
        self.write_state(&state)?;
        Ok(RevocationOutcome {
            had_enrollment,
            previous_epoch,
            current_epoch: state.enrollment_epoch,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.state_directory.join(STATE_FILE_NAME)
    }

    fn lock_path(&self) -> PathBuf {
        self.state_directory.join(LOCK_FILE_NAME)
    }

    fn ensure_state_directory(&self) -> Result<()> {
        if self.state_directory.exists() {
            let metadata = fs::symlink_metadata(&self.state_directory)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "authorization state directory is a symlink"
            );
            ensure!(
                metadata.is_dir(),
                "authorization state path is not a directory"
            );
            #[cfg(unix)]
            {
                ensure!(
                    metadata.mode() & 0o077 == 0,
                    "authorization state directory is accessible by group or others"
                );
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "authorization state directory has the wrong owner"
                );
            }
        } else {
            #[cfg(unix)]
            {
                let mut builder = fs::DirBuilder::new();
                builder
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.state_directory)?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(&self.state_directory)?;
        }
        Ok(())
    }

    fn open_lock(&self) -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options
            .open(self.lock_path())
            .context("opening authorization lock")?;
        self.validate_secure_file(&file, &self.lock_path())?;
        Ok(file)
    }

    fn lock_exclusive(&self, file: &File) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            ensure!(
                status == 0,
                "locking authorization state failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn load_state(&self) -> Result<AuthorizationState> {
        let path = self.state_path();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthorizationState::empty(self.host));
            }
            Err(error) => return Err(error).context("opening authorization state"),
        };
        self.validate_secure_file(&file, &path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.len() <= MAX_STATE_FILE_LEN,
            "authorization state exceeds its fixed size bound"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_STATE_FILE_LEN.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_FILE_LEN,
            "authorization state exceeds its fixed size bound"
        );
        ensure!(!bytes.is_empty(), "authorization state is empty");
        serde_json::from_slice(&bytes).context("parsing authorization state")
    }

    fn write_state(&self, state: &AuthorizationState) -> Result<()> {
        state.validate(self.host)?;
        let bytes = serde_json::to_vec_pretty(state)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_FILE_LEN,
            "authorization state exceeds its fixed size bound"
        );
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).context("generating authorization temporary-file name")?;
        let temporary = self.state_directory.join(format!(
            ".{STATE_FILE_NAME}.{:016x}.tmp",
            u64::from_be_bytes(random)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options
            .open(&temporary)
            .context("creating authorization state temporary file")?;
        let write_result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, self.state_path())?;
            File::open(&self.state_directory)?.sync_all()?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    fn validate_secure_file(&self, file: &File, path: &Path) -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "{} is not a regular file",
            path.display()
        );
        #[cfg(unix)]
        {
            ensure!(
                metadata.mode() & 0o077 == 0,
                "{} is accessible by group or others",
                path.display()
            );
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() },
                "{} has the wrong owner",
                path.display()
            );
        }
        Ok(())
    }
}

impl AuthorizationPolicy {
    pub fn authorize_or_redeem(
        &self,
        remote: EndpointId,
        invitation_token: Option<&str>,
        now: u64,
    ) -> Result<InvitationGrants> {
        match self {
            Self::Required(store) => store.authorize_or_redeem(remote, invitation_token, now),
            Self::TestPatternProof => {
                ensure!(
                    invitation_token.is_none(),
                    "proof mode does not accept invitations"
                );
                Ok(InvitationGrants::ALL)
            }
        }
    }
}

pub fn unix_timestamp_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

pub fn grant_names(grants: InvitationGrants) -> &'static str {
    match (
        grants.contains(InvitationGrants::POINTER_KEYBOARD),
        grants.contains(InvitationGrants::GAMEPAD),
    ) {
        (false, false) => "view",
        (true, false) => "view,pointer-keyboard",
        (false, true) => "view,gamepad",
        (true, true) => "view,pointer-keyboard,gamepad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use sigil_protocol::SignedInvitation;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn store() -> (tempfile::TempDir, SecretKey, SecretKey, AuthorizationStore) {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let host = SecretKey::from_bytes(&[7; 32]);
        let peer = SecretKey::from_bytes(&[9; 32]);
        let store = AuthorizationStore::open(directory.path(), host.public()).unwrap();
        (directory, host, peer, store)
    }

    fn token(
        store: &AuthorizationStore,
        host: &SecretKey,
        peer: EndpointId,
        now: u64,
        nonce: [u8; 32],
        grants: InvitationGrants,
    ) -> String {
        let claims = store.issue_claims(peer, grants, 600, now, nonce).unwrap();
        SignedInvitation::issue(claims, &host.to_bytes())
            .unwrap()
            .encode()
    }

    #[test]
    fn invitation_enrolls_once_and_reconnects_ticket_free() {
        let (_directory, host, peer, store) = store();
        let grants = InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD);
        let token = token(&store, &host, peer.public(), 1_000, [1; 32], grants);
        assert_eq!(
            store
                .authorize_or_redeem(peer.public(), Some(&token), 1_001)
                .unwrap(),
            grants
        );
        assert_eq!(
            store
                .authorize_or_redeem(peer.public(), None, 1_002)
                .unwrap(),
            grants
        );
        assert!(
            store
                .authorize_or_redeem(peer.public(), Some(&token), 1_002)
                .unwrap_err()
                .to_string()
                .contains("after enrollment")
        );

        let reopened =
            AuthorizationStore::open(store.state_directory.clone(), host.public()).unwrap();
        assert_eq!(
            reopened
                .authorize_or_redeem(peer.public(), None, 1_003)
                .unwrap(),
            grants
        );
    }

    #[test]
    fn read_only_inspection_never_creates_state_and_retains_enrollment_time() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let missing = root.path().join("missing-state");
        let host = SecretKey::from_bytes(&[7; 32]);
        let inspection = AuthorizationStore::inspect_existing(&missing, host.public()).unwrap();
        assert!(!inspection.storage_present);
        assert_eq!(inspection.snapshot.epoch, 1);
        assert!(inspection.snapshot.peer.is_none());
        assert!(!missing.exists());

        let (directory, host, peer, store) = store();
        let token = token(
            &store,
            &host,
            peer.public(),
            1_000,
            [11; 32],
            InvitationGrants::ALL,
        );
        store
            .authorize_or_redeem(peer.public(), Some(&token), 1_001)
            .unwrap();
        let inspection =
            AuthorizationStore::inspect_existing(directory.path(), host.public()).unwrap();
        assert!(inspection.storage_present);
        assert_eq!(inspection.snapshot.peer, Some(peer.public()));
        assert_eq!(inspection.snapshot.enrolled_at_unix, Some(1_001));
    }

    #[test]
    fn peer_host_expiry_and_future_time_fail_closed() {
        let (_directory, host, peer, store) = store();
        let other_peer = SecretKey::from_bytes(&[10; 32]);
        let token = token(
            &store,
            &host,
            peer.public(),
            1_000,
            [2; 32],
            InvitationGrants::VIEW,
        );
        assert!(
            store
                .authorize_or_redeem(other_peer.public(), Some(&token), 1_001)
                .is_err()
        );
        assert!(
            store
                .authorize_or_redeem(peer.public(), Some(&token), 1_601)
                .is_err()
        );
        assert!(
            store
                .authorize_or_redeem(peer.public(), Some(&token), 900)
                .is_err()
        );

        let other_host = SecretKey::from_bytes(&[11; 32]);
        let claims = InvitationClaims::new(
            *other_host.public().as_bytes(),
            *peer.public().as_bytes(),
            1_000,
            1_600,
            1,
            [3; 32],
            InvitationGrants::VIEW,
        )
        .unwrap();
        let cross_host = SignedInvitation::issue(claims, &other_host.to_bytes())
            .unwrap()
            .encode();
        assert!(
            store
                .authorize_or_redeem(peer.public(), Some(&cross_host), 1_001)
                .is_err()
        );
    }

    #[test]
    fn revoke_invalidates_outstanding_epoch_and_clears_peer() {
        let (_directory, host, peer, store) = store();
        let token = token(
            &store,
            &host,
            peer.public(),
            1_000,
            [4; 32],
            InvitationGrants::VIEW,
        );
        assert!(!store.revoke_with_outcome(1_001).unwrap().had_enrollment);
        assert!(
            store
                .authorize_or_redeem(peer.public(), Some(&token), 1_002)
                .unwrap_err()
                .to_string()
                .contains("epoch")
        );
        assert_eq!(store.snapshot().unwrap().epoch, 2);
        assert!(store.snapshot().unwrap().peer.is_none());
    }

    #[test]
    fn copied_state_is_bound_to_host_and_permissions_are_strict() {
        let (_directory, host, peer, store) = store();
        let token = token(
            &store,
            &host,
            peer.public(),
            1_000,
            [5; 32],
            InvitationGrants::VIEW.union(InvitationGrants::POINTER_KEYBOARD),
        );
        store
            .authorize_or_redeem(peer.public(), Some(&token), 1_001)
            .unwrap();
        let other_host = SecretKey::from_bytes(&[12; 32]);
        assert!(
            AuthorizationStore::open(store.state_directory.clone(), other_host.public()).is_err()
        );
        let snapshot = store.snapshot().unwrap();
        assert!(
            snapshot
                .grants
                .unwrap()
                .contains(InvitationGrants::POINTER_KEYBOARD)
        );
        assert!(!snapshot.grants.unwrap().contains(InvitationGrants::GAMEPAD));
    }

    #[test]
    fn consumed_nonce_replay_survives_revoke_and_store_reopen() {
        let (_directory, host, peer, store) = store();
        let nonce = [6; 32];
        let first = token(
            &store,
            &host,
            peer.public(),
            1_000,
            nonce,
            InvitationGrants::VIEW,
        );
        store
            .authorize_or_redeem(peer.public(), Some(&first), 1_001)
            .unwrap();
        assert!(store.revoke_with_outcome(1_002).unwrap().had_enrollment);

        let reopened =
            AuthorizationStore::open(store.state_directory.clone(), host.public()).unwrap();
        let replay = token(
            &reopened,
            &host,
            peer.public(),
            1_003,
            nonce,
            InvitationGrants::VIEW,
        );
        let state_before = fs::read(reopened.state_path()).unwrap();
        let error = reopened
            .authorize_or_redeem(peer.public(), Some(&replay), 1_004)
            .unwrap_err();

        assert!(error.to_string().contains("already been consumed"));
        assert_eq!(fs::read(reopened.state_path()).unwrap(), state_before);
        let snapshot = reopened.snapshot().unwrap();
        assert_eq!(snapshot.epoch, 2);
        assert!(snapshot.peer.is_none());
    }

    #[test]
    fn invitation_clock_boundaries_are_inclusive_and_fail_closed_outside_skew() {
        let (_directory, host, peer, expiry_store) = store();
        let expires_at = token(
            &expiry_store,
            &host,
            peer.public(),
            1_000,
            [7; 32],
            InvitationGrants::VIEW,
        );
        assert!(
            expiry_store
                .authorize_or_redeem(peer.public(), Some(&expires_at), 1_600)
                .is_ok()
        );

        let (_directory, host, peer, skew_store) = store();
        let exact_skew = token(
            &skew_store,
            &host,
            peer.public(),
            1_060,
            [8; 32],
            InvitationGrants::VIEW,
        );
        assert!(
            skew_store
                .authorize_or_redeem(peer.public(), Some(&exact_skew), 1_000)
                .is_ok()
        );

        let (_directory, host, peer, outside_store) = store();
        let outside_skew = token(
            &outside_store,
            &host,
            peer.public(),
            1_061,
            [9; 32],
            InvitationGrants::VIEW,
        );
        let error = outside_store
            .authorize_or_redeem(peer.public(), Some(&outside_skew), 1_000)
            .unwrap_err();
        assert!(error.to_string().contains("too far in the future"));
    }
}
