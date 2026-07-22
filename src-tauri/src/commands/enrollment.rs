use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::PublicKey;
use serde::{Deserialize, Serialize};
use sigil_protocol::{InvitationGrants, MAX_INVITATION_TOKEN_LEN, SignedInvitation};
use tauri::{AppHandle, Emitter as _, Manager as _, State};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const PROFILE_VERSION: u16 = 1;
const PROFILE_FILE: &str = "enrollment-v1.json";
const MAX_PROFILE_BYTES: u64 = 2_048;
const MAX_IMPORT_BYTES: u64 = (MAX_INVITATION_TOKEN_LEN + 1) as u64;

#[derive(Debug, Default)]
pub struct EnrollmentState {
    pending: Mutex<Option<PendingInvitation>>,
}

#[derive(Clone, Debug)]
struct PendingInvitation {
    token: String,
    invitation: SignedInvitation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentProfile {
    version: u16,
    host_node_id: String,
    peer_node_id: String,
    grants: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_invitation: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InvitationSummary {
    host_node_id: String,
    peer_node_id: String,
    expires_at_unix: u64,
    grants: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnrollmentStatus {
    enrolled: bool,
    host_node_id: Option<String>,
    peer_node_id: Option<String>,
    grants: Vec<&'static str>,
    pending: Option<InvitationSummary>,
}

#[derive(Clone, Debug)]
pub struct ConnectionEnrollment {
    pub host_node_id: PublicKey,
    pub grants: InvitationGrants,
    pub pending_invitation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OpenedInvitationSource {
    DeepLinkToken(String),
    File(PathBuf),
}

fn now_unix() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is before the Unix epoch".to_string())
}

fn grant_names(grants: InvitationGrants) -> Vec<&'static str> {
    let mut names = Vec::with_capacity(3);
    if grants.contains(InvitationGrants::VIEW) {
        names.push("view");
    }
    if grants.contains(InvitationGrants::POINTER_KEYBOARD) {
        names.push("pointer_keyboard");
    }
    if grants.contains(InvitationGrants::GAMEPAD) {
        names.push("gamepad");
    }
    names
}

fn public_key(bytes: [u8; 32], name: &str) -> Result<PublicKey, String> {
    PublicKey::from_bytes(&bytes).map_err(|error| format!("Invalid {name} node ID: {error}"))
}

fn summary(invitation: &SignedInvitation) -> Result<InvitationSummary, String> {
    Ok(InvitationSummary {
        host_node_id: public_key(invitation.claims.host_node_id, "host")?.to_string(),
        peer_node_id: public_key(invitation.claims.intended_peer_id, "Portal")?.to_string(),
        expires_at_unix: invitation.claims.expires_at_unix,
        grants: grant_names(invitation.claims.grants),
    })
}

fn validate_current(invitation: &SignedInvitation, now: u64) -> Result<(), String> {
    invitation
        .verify()
        .map_err(|error| format!("Invalid invitation signature: {error}"))?;
    if now < invitation.claims.issued_at_unix.saturating_sub(60) {
        return Err("Invitation was issued too far in the future".to_string());
    }
    if now > invitation.claims.expires_at_unix {
        return Err("Invitation has expired".to_string());
    }
    Ok(())
}

fn decode_verified(token: String) -> Result<PendingInvitation, String> {
    if token.len() > MAX_INVITATION_TOKEN_LEN || token.trim() != token {
        return Err("Invitation is oversized or contains surrounding whitespace".to_string());
    }
    let invitation = SignedInvitation::decode(&token)
        .map_err(|error| format!("Malformed invitation: {error}"))?;
    Ok(PendingInvitation { token, invitation })
}

fn decode_pending(token: String) -> Result<PendingInvitation, String> {
    let pending = decode_verified(token)?;
    validate_current(&pending.invitation, now_unix()?)?;
    Ok(pending)
}

fn config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map_err(|error| format!("Could not resolve Portal config directory: {error}"))
}

fn profile_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_dir(app)?.join(PROFILE_FILE))
}

fn secure_config_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create Portal config directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect Portal config directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Portal config path must be a real directory".to_string());
    }
    #[cfg(unix)]
    {
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Portal config directory is not owned by the current user".to_string());
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not secure Portal config directory: {error}"))?;
    }
    Ok(())
}

fn read_bounded_file(path: &Path, maximum: u64, require_private: bool) -> Result<Vec<u8>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    #[cfg(unix)]
    if require_private
        && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0)
    {
        return Err("Portal enrollment profile has unsafe ownership or permissions".to_string());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    Ok(bytes)
}

fn invitation_token_from_file(path: &Path) -> Result<String, String> {
    if path.extension().and_then(|value| value.to_str()) != Some("goq-invite") {
        return Err("Invitation file must end in .goq-invite".to_string());
    }
    let bytes = read_bounded_file(path, MAX_IMPORT_BYTES, false)?;
    let token = std::str::from_utf8(&bytes)
        .map_err(|_| "Invitation file is not UTF-8".to_string())?
        .trim_end_matches(['\r', '\n']);
    if token.len() != bytes.len() && bytes.len() - token.len() > 2 {
        return Err("Invitation file has unexpected trailing data".to_string());
    }
    Ok(token.to_string())
}

fn opened_invitation_source(url: &url::Url) -> Result<OpenedInvitationSource, String> {
    if url.scheme() == "goq" {
        if url.host_str() != Some("invite")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Invitation deep link shape is invalid".to_string());
        }
        let mut segments = url
            .path_segments()
            .ok_or_else(|| "Invitation deep link has no token".to_string())?
            .filter(|segment| !segment.is_empty());
        let token = segments
            .next()
            .ok_or_else(|| "Invitation deep link has no token".to_string())?;
        if segments.next().is_some() {
            return Err("Invitation deep link has extra path segments".to_string());
        }
        Ok(OpenedInvitationSource::DeepLinkToken(token.to_string()))
    } else if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| "Invitation file URL is invalid".to_string())?;
        Ok(OpenedInvitationSource::File(path))
    } else {
        Err("Only goq invitation links and invitation files are accepted".to_string())
    }
}

fn load_profile(path: &Path) -> Result<Option<EnrollmentProfile>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect enrollment profile: {error}")),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("Enrollment profile must not be a symlink".to_string());
        }
        Ok(_) => {}
    }
    let bytes = read_bounded_file(path, MAX_PROFILE_BYTES, true)?;
    let profile: EnrollmentProfile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Enrollment profile is malformed: {error}"))?;
    validate_profile(&profile)?;
    Ok(Some(profile))
}

fn validate_profile(profile: &EnrollmentProfile) -> Result<InvitationGrants, String> {
    if profile.version != PROFILE_VERSION {
        return Err("Enrollment profile version is unsupported".to_string());
    }
    profile
        .host_node_id
        .parse::<PublicKey>()
        .map_err(|error| format!("Enrollment host ID is invalid: {error}"))?;
    profile
        .peer_node_id
        .parse::<PublicKey>()
        .map_err(|error| format!("Enrollment peer ID is invalid: {error}"))?;
    let grants = InvitationGrants::new(profile.grants)
        .map_err(|error| format!("Enrollment grants are invalid: {error}"))?;
    if let Some(token) = &profile.pending_invitation {
        // A durable pending token may have expired after Sigil committed
        // enrollment but before Portal cleared it. Keep validating its exact
        // signed shape so the connection path can attempt the narrow,
        // ticket-free crash recovery as the same peer.
        let pending = decode_verified(token.clone())?;
        let invitation = &pending.invitation.claims;
        if public_key(invitation.host_node_id, "host")?.to_string() != profile.host_node_id
            || public_key(invitation.intended_peer_id, "Portal")?.to_string()
                != profile.peer_node_id
            || invitation.grants != grants
        {
            return Err("Pending invitation does not match its enrollment profile".to_string());
        }
    }
    Ok(grants)
}

fn write_profile(path: &Path, profile: &EnrollmentProfile) -> Result<(), String> {
    validate_profile(profile)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Enrollment profile has no parent directory".to_string())?;
    secure_config_dir(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("Enrollment profile must not be a symlink".to_string());
    }
    let bytes = serde_json::to_vec(profile)
        .map_err(|error| format!("Could not encode enrollment profile: {error}"))?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err("Enrollment profile exceeds its bounded size".to_string());
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("Could not name enrollment profile update: {error}"))?;
    let temporary = parent.join(format!(
        ".{PROFILE_FILE}.{:016x}.tmp",
        u64::from_be_bytes(random)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let result = (|| -> Result<(), String> {
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("Could not create enrollment profile: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write enrollment profile: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not sync enrollment profile: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not activate enrollment profile: {error}"))?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not sync Portal config directory: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn stage_token(state: &EnrollmentState, token: String) -> Result<InvitationSummary, String> {
    let pending = decode_pending(token)?;
    let result = summary(&pending.invitation)?;
    let mut slot = state
        .pending
        .lock()
        .map_err(|_| "Invitation staging state is unavailable".to_string())?;
    if slot.is_some() {
        return Err("Another invitation is already awaiting confirmation".to_string());
    }
    *slot = Some(pending);
    Ok(result)
}

#[tauri::command]
pub fn portal_enrollment_status(
    app: AppHandle,
    state: State<'_, super::state::AppState>,
) -> Result<EnrollmentStatus, String> {
    let profile = load_profile(&profile_path(&app)?)?;
    let pending = state
        .enrollment
        .pending
        .lock()
        .map_err(|_| "Invitation staging state is unavailable".to_string())?
        .as_ref()
        .map(|pending| summary(&pending.invitation))
        .transpose()?;
    let Some(profile) = profile else {
        return Ok(EnrollmentStatus {
            enrolled: false,
            host_node_id: None,
            peer_node_id: None,
            grants: Vec::new(),
            pending,
        });
    };
    let grants = validate_profile(&profile)?;
    Ok(EnrollmentStatus {
        enrolled: true,
        host_node_id: Some(profile.host_node_id),
        peer_node_id: Some(profile.peer_node_id),
        grants: grant_names(grants),
        pending,
    })
}

#[tauri::command]
pub fn portal_import_invitation_file(
    path: String,
    state: State<'_, super::state::AppState>,
) -> Result<InvitationSummary, String> {
    let path = PathBuf::from(path);
    stage_token(&state.enrollment, invitation_token_from_file(&path)?)
}

fn ensure_profile_can_be_replaced(existing: Option<&EnrollmentProfile>) -> Result<(), String> {
    if existing.is_some_and(|profile| profile.pending_invitation.is_none()) {
        return Err(
            "Portal is already enrolled; revoke the Sigil enrollment and explicitly reset Portal before replacing it"
                .to_string(),
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn portal_confirm_invitation(
    app: AppHandle,
    state: State<'_, super::state::AppState>,
) -> Result<EnrollmentStatus, String> {
    // Connection setup holds this guard across profile snapshot, redemption,
    // and exact cleanup. Refuse to mutate durable enrollment concurrently.
    let _connection_serial = state.client_connection_serial.try_lock().map_err(|_| {
        "A Portal connection or enrollment update is already in progress".to_string()
    })?;
    let mut pending_slot = state
        .enrollment
        .pending
        .lock()
        .map_err(|_| "Invitation staging state is unavailable".to_string())?;
    let pending = pending_slot
        .as_ref()
        .cloned()
        .ok_or_else(|| "No invitation is awaiting confirmation".to_string())?;
    validate_current(&pending.invitation, now_unix()?)?;
    let path = profile_path(&app)?;
    let existing = load_profile(&path)?;
    ensure_profile_can_be_replaced(existing.as_ref())?;
    let claims = &pending.invitation.claims;
    let profile = EnrollmentProfile {
        version: PROFILE_VERSION,
        host_node_id: public_key(claims.host_node_id, "host")?.to_string(),
        peer_node_id: public_key(claims.intended_peer_id, "Portal")?.to_string(),
        grants: claims.grants.bits(),
        pending_invitation: Some(pending.token),
    };
    write_profile(&path, &profile)?;
    pending_slot.take();
    drop(pending_slot);
    drop(_connection_serial);
    portal_enrollment_status(app, state)
}

#[tauri::command]
pub fn portal_cancel_invitation(state: State<'_, super::state::AppState>) -> Result<(), String> {
    state
        .enrollment
        .pending
        .lock()
        .map_err(|_| "Invitation staging state is unavailable".to_string())?
        .take();
    Ok(())
}

#[tauri::command]
pub async fn portal_reset_enrollment(
    app: AppHandle,
    state: State<'_, super::state::AppState>,
    expected_host_node_id: String,
) -> Result<(), String> {
    let _connection_serial = state.client_connection_serial.try_lock().map_err(|_| {
        "A Portal connection or enrollment update is already in progress".to_string()
    })?;
    if state.client_connection_active.load(Ordering::SeqCst) {
        return Err("Disconnect before resetting Portal enrollment".to_string());
    }
    let path = profile_path(&app)?;
    let profile =
        load_profile(&path)?.ok_or_else(|| "Portal has no enrollment to reset".to_string())?;
    if expected_host_node_id != profile.host_node_id {
        return Err("Enrollment changed before reset confirmation".to_string());
    }
    fs::remove_file(&path)
        .map_err(|error| format!("Could not remove Portal enrollment: {error}"))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not sync Portal enrollment reset: {error}"))?;
    }
    state
        .enrollment
        .pending
        .lock()
        .map_err(|_| "Invitation staging state is unavailable".to_string())?
        .take();
    Ok(())
}

pub fn stage_opened_url(app: &AppHandle, url: &url::Url) -> Result<(), String> {
    let state = app.state::<super::state::AppState>();
    let summary = match opened_invitation_source(url)? {
        OpenedInvitationSource::DeepLinkToken(token) => stage_token(&state.enrollment, token)?,
        OpenedInvitationSource::File(path) => {
            stage_token(&state.enrollment, invitation_token_from_file(&path)?)?
        }
    };
    app.emit("invitation-pending", summary)
        .map_err(|error| format!("Could not announce invitation: {error}"))?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    Ok(())
}

pub fn connection_enrollment(
    app: &AppHandle,
    actual_peer: PublicKey,
) -> Result<ConnectionEnrollment, String> {
    let profile = load_profile(&profile_path(app)?)?
        .ok_or_else(|| "Portal is not enrolled with a Sigil host".to_string())?;
    let grants = validate_profile(&profile)?;
    if profile.peer_node_id != actual_peer.to_string() {
        return Err(
            "Security key does not match the Portal peer named by the invitation".to_string(),
        );
    }
    Ok(ConnectionEnrollment {
        host_node_id: profile
            .host_node_id
            .parse()
            .map_err(|error| format!("Enrollment host ID is invalid: {error}"))?,
        grants,
        pending_invitation: profile.pending_invitation,
    })
}

fn clear_exact_pending_invitation(
    profile: &mut EnrollmentProfile,
    expected_invitation: &str,
) -> Result<(), String> {
    match profile.pending_invitation.as_deref() {
        Some(actual) if actual == expected_invitation => {
            profile.pending_invitation = None;
            Ok(())
        }
        Some(_) => Err("Portal enrollment changed while its invitation was redeeming".to_string()),
        None => {
            Err("Portal invitation was already cleared before redemption completed".to_string())
        }
    }
}

pub fn mark_invitation_redeemed(app: &AppHandle, expected_invitation: &str) -> Result<(), String> {
    let path = profile_path(app)?;
    let Some(mut profile) = load_profile(&path)? else {
        return Err("Portal enrollment disappeared during connection".to_string());
    };
    clear_exact_pending_invitation(&mut profile, expected_invitation)?;
    write_profile(&path, &profile)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use sigil_protocol::InvitationClaims;

    use super::*;

    fn token(now: u64) -> String {
        let host = SigningKey::from_bytes(&[7; 32]);
        let peer = SigningKey::from_bytes(&[9; 32]);
        let claims = InvitationClaims::new(
            host.verifying_key().to_bytes(),
            peer.verifying_key().to_bytes(),
            now - 1,
            now + 300,
            1,
            [3; 32],
            InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
        )
        .unwrap();
        SignedInvitation::issue(claims, &[7; 32]).unwrap().encode()
    }

    #[test]
    fn staged_summary_never_contains_the_raw_ticket() {
        let now = now_unix().unwrap();
        let state = EnrollmentState::default();
        let raw = token(now);
        let summary = stage_token(&state, raw.clone()).unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains(&raw));
        assert!(json.contains("gamepad"));
    }

    #[test]
    fn profile_round_trip_is_private_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(PROFILE_FILE);
        #[cfg(unix)]
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let invitation = SignedInvitation::decode(&token(now_unix().unwrap())).unwrap();
        let profile = EnrollmentProfile {
            version: PROFILE_VERSION,
            host_node_id: public_key(invitation.claims.host_node_id, "host")
                .unwrap()
                .to_string(),
            peer_node_id: public_key(invitation.claims.intended_peer_id, "peer")
                .unwrap()
                .to_string(),
            grants: invitation.claims.grants.bits(),
            pending_invitation: Some(invitation.encode()),
        };
        write_profile(&path, &profile).unwrap();
        assert_eq!(load_profile(&path).unwrap().unwrap().grants, profile.grants);
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn malformed_profile_and_second_pending_invitation_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(PROFILE_FILE);
        fs::write(&path, b"{} trailing").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_profile(&path).is_err());

        let state = EnrollmentState::default();
        let raw = token(now_unix().unwrap());
        stage_token(&state, raw.clone()).unwrap();
        assert!(stage_token(&state, raw).is_err());
    }

    #[test]
    fn expired_durable_ticket_remains_available_for_ticket_free_recovery() {
        let invitation = SignedInvitation::decode(&token(1_000)).unwrap();
        let profile = EnrollmentProfile {
            version: PROFILE_VERSION,
            host_node_id: public_key(invitation.claims.host_node_id, "host")
                .unwrap()
                .to_string(),
            peer_node_id: public_key(invitation.claims.intended_peer_id, "peer")
                .unwrap()
                .to_string(),
            grants: invitation.claims.grants.bits(),
            pending_invitation: Some(invitation.encode()),
        };
        assert!(validate_profile(&profile).is_ok());
        assert!(decode_pending(profile.pending_invitation.unwrap()).is_err());
    }

    #[test]
    fn redeemed_profile_requires_explicit_reset_before_replacement() {
        let invitation = SignedInvitation::decode(&token(now_unix().unwrap())).unwrap();
        let mut profile = EnrollmentProfile {
            version: PROFILE_VERSION,
            host_node_id: public_key(invitation.claims.host_node_id, "host")
                .unwrap()
                .to_string(),
            peer_node_id: public_key(invitation.claims.intended_peer_id, "peer")
                .unwrap()
                .to_string(),
            grants: invitation.claims.grants.bits(),
            pending_invitation: Some(invitation.encode()),
        };
        assert!(ensure_profile_can_be_replaced(Some(&profile)).is_ok());
        let expected = profile.pending_invitation.clone().unwrap();
        assert!(clear_exact_pending_invitation(&mut profile, "different-ticket").is_err());
        assert_eq!(
            profile.pending_invitation.as_deref(),
            Some(expected.as_str())
        );
        clear_exact_pending_invitation(&mut profile, &expected).unwrap();
        assert!(ensure_profile_can_be_replaced(Some(&profile)).is_err());
    }

    #[test]
    fn strict_deep_link_shape_accepts_only_one_bounded_token_path() {
        let raw = token(now_unix().unwrap());
        let accepted = url::Url::parse(&format!("goq://invite/{raw}")).unwrap();
        assert_eq!(
            opened_invitation_source(&accepted).unwrap(),
            OpenedInvitationSource::DeepLinkToken(raw.clone())
        );

        let rejected = [
            format!("goq://other/{raw}"),
            "goq://invite".to_string(),
            format!("goq://invite/{raw}/extra"),
            format!("goq://invite/{raw}?unexpected=1"),
            format!("goq://invite/{raw}#unexpected"),
            format!("goq://user@invite/{raw}"),
            format!("goq://invite:123/{raw}"),
            format!("https://invite/{raw}"),
        ];
        for value in rejected {
            let url = url::Url::parse(&value).unwrap();
            assert!(
                opened_invitation_source(&url).is_err(),
                "unexpected invitation URL accepted: {value}"
            );
        }
    }

    #[test]
    fn file_ingress_accepts_only_bounded_exact_invitation_text() {
        let temp = tempfile::tempdir().unwrap();
        let raw = token(now_unix().unwrap());
        for (name, suffix) in [
            ("exact.goq-invite", ""),
            ("newline.goq-invite", "\n"),
            ("crlf.goq-invite", "\r\n"),
        ] {
            let path = temp.path().join(name);
            fs::write(&path, format!("{raw}{suffix}")).unwrap();
            assert_eq!(invitation_token_from_file(&path).unwrap(), raw);
        }

        let file_url = url::Url::from_file_path(temp.path().join("exact.goq-invite")).unwrap();
        assert_eq!(
            opened_invitation_source(&file_url).unwrap(),
            OpenedInvitationSource::File(temp.path().join("exact.goq-invite"))
        );
    }

    #[test]
    fn file_ingress_rejects_unsafe_or_malformed_sources() {
        let temp = tempfile::tempdir().unwrap();
        let raw = token(now_unix().unwrap());

        let wrong_extension = temp.path().join("invitation.txt");
        fs::write(&wrong_extension, &raw).unwrap();
        assert!(invitation_token_from_file(&wrong_extension).is_err());

        let trailing = temp.path().join("trailing.goq-invite");
        fs::write(&trailing, format!("{raw}\n\n\n")).unwrap();
        assert!(invitation_token_from_file(&trailing).is_err());

        let invalid_utf8 = temp.path().join("invalid-utf8.goq-invite");
        fs::write(&invalid_utf8, [0xff, 0xfe]).unwrap();
        assert!(invitation_token_from_file(&invalid_utf8).is_err());

        let oversized = temp.path().join("oversized.goq-invite");
        fs::write(&oversized, vec![b'x'; MAX_IMPORT_BYTES as usize + 1]).unwrap();
        assert!(invitation_token_from_file(&oversized).is_err());

        let directory = temp.path().join("directory.goq-invite");
        fs::create_dir(&directory).unwrap();
        assert!(invitation_token_from_file(&directory).is_err());

        #[cfg(unix)]
        {
            let target = temp.path().join("target.goq-invite");
            let symlink = temp.path().join("symlink.goq-invite");
            fs::write(&target, raw).unwrap();
            std::os::unix::fs::symlink(target, &symlink).unwrap();
            assert!(invitation_token_from_file(&symlink).is_err());
        }
    }
}
