#[cfg(any(target_os = "linux", test))]
use std::mem::size_of;

#[cfg(any(target_os = "linux", test))]
use anyhow::{Result, bail};

#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_XATTR_VERSION: u32 = 0x0002;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_ENTRY_BYTES: usize = 8;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_REQUIRED_ENTRIES: usize = 5;
#[cfg(any(target_os = "linux", test))]
pub(crate) const POSIX_ACL_REQUIRED_BYTES: usize =
    size_of::<u32>() + POSIX_ACL_ENTRY_BYTES * POSIX_ACL_REQUIRED_ENTRIES;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_UNDEFINED_ID: u32 = u32::MAX;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_USER_OBJ: u16 = 0x01;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_USER: u16 = 0x02;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_GROUP_OBJ: u16 = 0x04;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_MASK: u16 = 0x10;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_OTHER: u16 = 0x20;
#[cfg(any(target_os = "linux", test))]
const POSIX_ACL_READ_WRITE: u16 = 0x06;

/// Validate the only extended uinput ACL shape Sigil accepts.
///
/// Linux stores POSIX ACL xattrs as one little-endian version followed by
/// fixed-size entries. Requiring the canonical five-entry form prevents a
/// configured one-user exception from admitting extra users or groups.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn validate_single_user_access_acl(
    bytes: &[u8],
    expected_uid: u32,
    expected_mode: u32,
) -> Result<()> {
    if bytes.len() != POSIX_ACL_REQUIRED_BYTES {
        bail!(
            "configured uinput access ACL must contain exactly one named user and no named groups"
        );
    }
    let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if version != POSIX_ACL_XATTR_VERSION {
        bail!("configured uinput access ACL has an unsupported Linux ACL version");
    }

    let expected_entries = [
        (
            POSIX_ACL_USER_OBJ,
            ((expected_mode >> 6) & 0o7) as u16,
            POSIX_ACL_UNDEFINED_ID,
        ),
        (POSIX_ACL_USER, POSIX_ACL_READ_WRITE, expected_uid),
        (
            POSIX_ACL_GROUP_OBJ,
            ((expected_mode >> 3) & 0o7) as u16,
            POSIX_ACL_UNDEFINED_ID,
        ),
        (
            POSIX_ACL_MASK,
            ((expected_mode >> 3) & 0o7) as u16,
            POSIX_ACL_UNDEFINED_ID,
        ),
        (
            POSIX_ACL_OTHER,
            (expected_mode & 0o7) as u16,
            POSIX_ACL_UNDEFINED_ID,
        ),
    ];

    for (index, (expected_tag, expected_permissions, expected_id)) in
        expected_entries.into_iter().enumerate()
    {
        let offset = size_of::<u32>() + index * POSIX_ACL_ENTRY_BYTES;
        let tag = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let permissions = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
        let id = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]);
        if permissions & !0o7 != 0 {
            bail!("configured uinput access ACL contains invalid permission bits");
        }
        if tag != expected_tag || permissions != expected_permissions || id != expected_id {
            bail!(
                "configured uinput access ACL does not match the exact one-user owner/group/mode contract"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl_entry(tag: u16, permissions: u16, id: u32) -> [u8; POSIX_ACL_ENTRY_BYTES] {
        let mut bytes = [0_u8; POSIX_ACL_ENTRY_BYTES];
        bytes[0..2].copy_from_slice(&tag.to_le_bytes());
        bytes[2..4].copy_from_slice(&permissions.to_le_bytes());
        bytes[4..8].copy_from_slice(&id.to_le_bytes());
        bytes
    }

    fn one_user_access_acl(uid: u32, mode: u32) -> Vec<u8> {
        let entries = [
            acl_entry(
                POSIX_ACL_USER_OBJ,
                ((mode >> 6) & 0o7) as u16,
                POSIX_ACL_UNDEFINED_ID,
            ),
            acl_entry(POSIX_ACL_USER, POSIX_ACL_READ_WRITE, uid),
            acl_entry(
                POSIX_ACL_GROUP_OBJ,
                ((mode >> 3) & 0o7) as u16,
                POSIX_ACL_UNDEFINED_ID,
            ),
            acl_entry(
                POSIX_ACL_MASK,
                ((mode >> 3) & 0o7) as u16,
                POSIX_ACL_UNDEFINED_ID,
            ),
            acl_entry(POSIX_ACL_OTHER, (mode & 0o7) as u16, POSIX_ACL_UNDEFINED_ID),
        ];
        let mut bytes = Vec::with_capacity(POSIX_ACL_REQUIRED_BYTES);
        bytes.extend_from_slice(&POSIX_ACL_XATTR_VERSION.to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes
    }

    #[test]
    fn exact_one_user_access_acl_is_accepted() {
        let bytes = one_user_access_acl(1000, 0o660);
        assert_eq!(bytes.len(), POSIX_ACL_REQUIRED_BYTES);
        validate_single_user_access_acl(&bytes, 1000, 0o660).unwrap();
    }

    #[test]
    fn one_user_access_acl_rejects_wrong_principal_and_permissions() {
        let bytes = one_user_access_acl(1000, 0o660);
        assert!(validate_single_user_access_acl(&bytes, 1001, 0o660).is_err());

        for permission_offset in [6, 14, 22, 30, 38] {
            let mut changed = bytes.clone();
            changed[permission_offset] ^= 0x01;
            assert!(validate_single_user_access_acl(&changed, 1000, 0o660).is_err());
        }

        let mut invalid_permissions = bytes;
        invalid_permissions[14] = 0x08;
        assert!(validate_single_user_access_acl(&invalid_permissions, 1000, 0o660).is_err());
    }

    #[test]
    fn one_user_access_acl_rejects_extra_duplicate_and_named_group_entries() {
        let bytes = one_user_access_acl(1000, 0o660);

        let mut extra = bytes.clone();
        extra.extend_from_slice(&acl_entry(POSIX_ACL_USER, POSIX_ACL_READ_WRITE, 1001));
        assert!(validate_single_user_access_acl(&extra, 1000, 0o660).is_err());

        let mut duplicate_user = bytes.clone();
        duplicate_user[20..22].copy_from_slice(&POSIX_ACL_USER.to_le_bytes());
        assert!(validate_single_user_access_acl(&duplicate_user, 1000, 0o660).is_err());

        let mut named_group = bytes;
        named_group[20..22].copy_from_slice(&0x08_u16.to_le_bytes());
        assert!(validate_single_user_access_acl(&named_group, 1000, 0o660).is_err());
    }

    #[test]
    fn one_user_access_acl_rejects_bad_version_and_non_exact_lengths() {
        let bytes = one_user_access_acl(1000, 0o660);

        let mut bad_version = bytes.clone();
        bad_version[0..4].copy_from_slice(&3_u32.to_le_bytes());
        assert!(validate_single_user_access_acl(&bad_version, 1000, 0o660).is_err());
        assert!(validate_single_user_access_acl(&bytes[..bytes.len() - 1], 1000, 0o660).is_err());

        let mut oversized = bytes;
        oversized.push(0);
        assert!(validate_single_user_access_acl(&oversized, 1000, 0o660).is_err());
    }
}
