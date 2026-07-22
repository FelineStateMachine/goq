use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

pub fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            builder
                .create(path)
                .with_context(|| format!("creating private state directory {}", path.display()))?;
            validate_private_directory(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspecting private state directory {}", path.display())),
    }
}

pub fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting private state directory {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "private state directory must not be a symlink"
    );
    ensure!(metadata.is_dir(), "private state path must be a directory");
    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "private state directory is accessible by group or others"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "private state directory has the wrong owner"
        );
    }
    Ok(())
}

#[derive(Debug)]
pub enum LockAcquireError {
    Busy,
    Unsafe(anyhow::Error),
}

impl std::fmt::Display for LockAcquireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => {
                formatter.write_str("another Sigil daemon already owns this state directory")
            }
            Self::Unsafe(error) => write!(formatter, "unsafe lock storage: {error}"),
        }
    }
}

impl std::error::Error for LockAcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Busy => None,
            Self::Unsafe(error) => Some(error.as_ref()),
        }
    }
}

pub fn try_open_lifetime_lock(
    directory: &Path,
    file_name: &str,
) -> std::result::Result<File, LockAcquireError> {
    validate_private_directory(directory).map_err(LockAcquireError::Unsafe)?;
    let path = directory.join(file_name);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(&path)
        .with_context(|| format!("opening lifecycle lock {}", path.display()))
        .map_err(LockAcquireError::Unsafe)?;
    validate_private_file(&file, &path).map_err(LockAcquireError::Unsafe)?;

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(LockAcquireError::Busy);
            }
            return Err(LockAcquireError::Unsafe(error.into()));
        }
    }

    Ok(file)
}

pub fn read_bounded(
    directory: &Path,
    file_name: &str,
    maximum_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    validate_private_directory(directory)?;
    let path = directory.join(file_name);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening private state {}", path.display()));
        }
    };
    validate_private_file(&file, &path)?;
    let length = file.metadata()?.len();
    ensure!(
        length <= maximum_bytes,
        "private state file exceeds its fixed size bound"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum_bytes,
        "private state file exceeds its fixed size bound"
    );
    ensure!(!bytes.is_empty(), "private state file is empty");
    Ok(Some(bytes))
}

pub fn atomic_write(
    directory: &Path,
    file_name: &str,
    bytes: &[u8],
    maximum_bytes: u64,
) -> Result<()> {
    let mut terminated = Vec::with_capacity(bytes.len().saturating_add(1));
    terminated.extend_from_slice(bytes);
    terminated.push(b'\n');
    atomic_write_exact(directory, file_name, &terminated, maximum_bytes)
}

pub fn atomic_write_exact(
    directory: &Path,
    file_name: &str,
    bytes: &[u8],
    maximum_bytes: u64,
) -> Result<()> {
    validate_private_directory(directory)?;
    ensure!(
        bytes.len() as u64 <= maximum_bytes,
        "private state file exceeds its fixed size bound"
    );
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).context("generating private state temporary-file name")?;
    let temporary = directory.join(format!(
        ".{file_name}.{:016x}.tmp",
        u64::from_be_bytes(random)
    ));
    let destination = directory.join(file_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(&temporary).with_context(|| {
        format!(
            "creating private state temporary file {}",
            temporary.display()
        )
    })?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn remove_file_if_exists(directory: &Path, file_name: &str) -> Result<bool> {
    validate_private_directory(directory)?;
    let path = directory.join(file_name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "private state cleanup target is not a regular file"
            );
            #[cfg(unix)]
            {
                ensure!(
                    metadata.mode() & 0o077 == 0,
                    "private state cleanup target is accessible by group or others"
                );
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "private state cleanup target has the wrong owner"
                );
            }
            fs::remove_file(&path)
                .with_context(|| format!("removing private state {}", path.display()))?;
            File::open(directory)?.sync_all()?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting private state {}", path.display()))
        }
    }
}

fn validate_private_file(file: &File, path: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn lifetime_lock_is_exclusive_and_reacquirable() {
        let directory = private_directory();
        let first = try_open_lifetime_lock(directory.path(), "daemon-v1.lock").unwrap();
        assert!(matches!(
            try_open_lifetime_lock(directory.path(), "daemon-v1.lock"),
            Err(LockAcquireError::Busy)
        ));
        drop(first);
        try_open_lifetime_lock(directory.path(), "daemon-v1.lock").unwrap();
    }

    #[test]
    fn atomic_state_round_trips_with_a_fixed_bound() {
        let directory = private_directory();
        atomic_write(directory.path(), "status.json", b"{\"ok\":true}", 64).unwrap();
        assert_eq!(
            read_bounded(directory.path(), "status.json", 64)
                .unwrap()
                .unwrap(),
            b"{\"ok\":true}\n"
        );
        assert!(read_bounded(directory.path(), "status.json", 4).is_err());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(directory.path().join("status.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn exact_atomic_state_preserves_bytes_and_cleanup_is_durable() {
        let directory = private_directory();
        atomic_write_exact(directory.path(), "config.toml", b"x = 1\n\n", 64).unwrap();
        assert_eq!(
            read_bounded(directory.path(), "config.toml", 64)
                .unwrap()
                .unwrap(),
            b"x = 1\n\n"
        );
        assert!(remove_file_if_exists(directory.path(), "config.toml").unwrap());
        assert!(!remove_file_if_exists(directory.path(), "config.toml").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_permissive_state() {
        let directory = private_directory();
        let target = directory.path().join("target");
        fs::write(&target, b"state").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, directory.path().join("status.json")).unwrap();
        assert!(read_bounded(directory.path(), "status.json", 64).is_err());

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            try_open_lifetime_lock(directory.path(), "daemon-v1.lock"),
            Err(LockAcquireError::Unsafe(_))
        ));
    }
}
