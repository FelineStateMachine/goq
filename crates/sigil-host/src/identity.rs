use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use iroh::SecretKey;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const SECRET_KEY_LEN: usize = 32;

pub fn init(path: &Path) -> Result<SecretKey> {
    ensure_parent(path)?;
    reject_symlink(path)?;

    let secret = SecretKey::generate();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options
        .open(path)
        .with_context(|| format!("refusing to overwrite identity {}", path.display()))?;
    file.write_all(&secret.to_bytes())
        .with_context(|| format!("writing identity {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing identity {}", path.display()))?;

    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting identity permissions on {}", path.display()))?;

    validate_metadata(path)?;
    Ok(secret)
}

pub fn load(path: &Path) -> Result<SecretKey> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).with_context(|| {
        format!(
            "opening identity {} without following symlinks",
            path.display()
        )
    })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting opened identity {}", path.display()))?;
    validate_open_metadata(path, &metadata)?;
    let mut bytes = [0_u8; SECRET_KEY_LEN];
    file.read_exact(&mut bytes)
        .with_context(|| format!("reading identity {}", path.display()))?;
    let mut extra = [0_u8; 1];
    ensure!(
        file.read(&mut extra)? == 0,
        "identity {} must contain exactly {SECRET_KEY_LEN} bytes",
        path.display()
    );
    Ok(SecretKey::from_bytes(&bytes))
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if !parent.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder
            .create(parent)
            .with_context(|| format!("creating identity directory {}", parent.display()))?;
    }

    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspecting identity directory {}", parent.display()))?;
    ensure!(metadata.is_dir(), "identity parent is not a directory");
    ensure!(
        !metadata.file_type().is_symlink(),
        "identity parent is a symlink"
    );

    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "identity directory {} must not be accessible by group or other users",
            parent.display()
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "identity directory {} is not owned by the current user",
            parent.display()
        );
    }

    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("identity {} must not be a symlink", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting identity {}", path.display())),
    }
}

fn validate_metadata(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting identity {}", path.display()))?;
    validate_open_metadata(path, &metadata)
}

fn validate_open_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure!(metadata.is_file(), "identity must be a regular file");
    ensure!(
        metadata.len() == SECRET_KEY_LEN as u64,
        "identity {} must be exactly {SECRET_KEY_LEN} bytes",
        path.display()
    );

    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "identity {} must not be readable or writable by group or other users",
            path.display()
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "identity {} is not owned by the current user",
            path.display()
        );
    }

    Ok(())
}

pub fn display_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_round_trips_identity() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("private");
        #[cfg(unix)]
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(not(unix))]
        fs::create_dir(&directory).unwrap();
        let path = directory.join("host.key");

        let created = init(&path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(created.public(), loaded.public());

        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn refuses_to_overwrite_identity() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("host.key");
        init(&path).unwrap();
        assert!(init(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_overly_permissive_identity() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("host.key");
        init(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let target = temp.path().join("target.key");
        init(&target).unwrap();
        let link = temp.path().join("link.key");
        symlink(&target, &link).unwrap();
        assert!(load(&link).is_err());
    }
}
