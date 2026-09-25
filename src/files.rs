use crate::Result;
use snafu::ResultExt;
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Write};
use std::path::Path;

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    if require_regular_file_if_present(path)?.is_none() {
        return Ok(None);
    }
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_whatever_context(|_| format!("cannot read {}", path.display()))
        }
    }
}

pub fn metadata_if_present(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_whatever_context(|_| format!("could not inspect {}", path.display()))
        }
    }
}

pub fn require_directory_if_present(path: &Path) -> Result<()> {
    if let Some(metadata) = metadata_if_present(path)?
        && !metadata.file_type().is_dir()
    {
        snafu::whatever!(
            "refusing to follow a symlink or non-directory at {}",
            path.display()
        );
    }
    Ok(())
}

pub fn require_regular_file_if_present(path: &Path) -> Result<Option<Metadata>> {
    let metadata = metadata_if_present(path)?;
    if let Some(metadata) = metadata.as_ref()
        && !metadata.file_type().is_file()
    {
        snafu::whatever!(
            "refusing to replace a symlink or non-file at {}",
            path.display()
        );
    }
    Ok(metadata)
}

/// Ensure a real directory exists, returning whether it was created.
pub fn ensure_directory(path: &Path) -> Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            require_directory_if_present(path)?;
            Ok(false)
        }
        Err(error) => Err(error)
            .with_whatever_context(|_| format!("could not create directory {}", path.display())),
    }
}

pub fn create_symlink(target: &Path, path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    let result = {
        let _ = directory;
        std::os::unix::fs::symlink(target, path)
    };
    #[cfg(windows)]
    let result = if directory {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    };
    #[cfg(not(any(unix, windows)))]
    let result: std::io::Result<()> = {
        let _ = directory;
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "symbolic links are not supported on this platform",
        ))
    };
    result.with_whatever_context(|error| {
        let mut message = format!("could not link {} to {}", path.display(), target.display());
        if cfg!(windows) && error.kind() == ErrorKind::PermissionDenied {
            message.push_str(
                "; enable Windows Developer Mode or run with permission to create symbolic links",
            );
        }
        message
    })
}

/// Replace a regular file atomically, without leaving a partially written TOML
/// file after interruption. The parent directory must already exist.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let metadata = require_regular_file_if_present(path)?;
    if metadata.is_some() {
        let current = fs::read(path).with_whatever_context(|_| {
            format!("could not read {} before replacement", path.display())
        })?;
        if current == contents {
            return Ok(());
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_whatever_context(|_| {
        format!("could not create temporary file in {}", parent.display())
    })?;
    if let Some(metadata) = metadata {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .with_whatever_context(|_| {
                format!("could not preserve permissions of {}", path.display())
            })?;
    }
    temporary.write_all(contents).with_whatever_context(|_| {
        format!("could not write temporary file for {}", path.display())
    })?;
    temporary.as_file().sync_all().with_whatever_context(|_| {
        format!("could not sync temporary file for {}", path.display())
    })?;
    temporary
        .persist(path)
        .with_whatever_context(|_| format!("could not atomically replace {}", path.display()))?;
    sync_directory_best_effort(parent);
    Ok(())
}

fn sync_directory_best_effort(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_refuses_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        fs::write(&original, "preserve").unwrap();
        let path = directory.path().join("linked");
        std::os::unix::fs::symlink(&original, &path).unwrap();
        assert!(atomic_write(&path, b"replacement").is_err());
        assert_eq!(fs::read_to_string(original).unwrap(), "preserve");
    }
}
