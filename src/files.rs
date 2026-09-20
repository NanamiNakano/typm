use crate::Result;
use snafu::ResultExt;
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Write};
use std::path::Path;

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => snafu::whatever!(
            "expected a regular file, refusing to follow or replace {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_whatever_context(|_| format!("cannot inspect {}", path.display()));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_contents_and_skips_identical_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.toml");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[cfg(unix)]
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
