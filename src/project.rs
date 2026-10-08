//! Project-local packages and the state that records their ownership.

use crate::Result;
use crate::files::{
    atomic_write, ensure_directory, metadata_if_present, read_optional,
    require_directory_if_present, require_regular_file_if_present,
};
use crate::lockfile::Lockfile;
use crate::stubs::PackageView;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedLink {
    /// A path of the form namespace/name/version, relative to .typm/packages.
    pub path: PathBuf,
    pub target: PathBuf,
    /// Inline manifest and source links when the import version needs rewriting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<PackageView>,
    /// Direct project dependencies that make this package reachable.
    pub roots: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkState {
    pub version: u32,
    pub links: Vec<ManagedLink>,
}

impl Default for LinkState {
    fn default() -> Self {
        Self {
            version: 2,
            links: Vec::new(),
        }
    }
}

pub struct Project {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
}

struct ProjectSnapshot {
    lockfile: Option<String>,
    state_contents: Option<String>,
}

struct LinkUpdate {
    removals: Vec<ManagedLink>,
    creations: Vec<ManagedLink>,
    state_contents: String,
}

#[derive(Default)]
struct AppliedLinkChanges {
    removed: Vec<(ManagedLink, PathBuf)>,
    created: Vec<ManagedLink>,
    directories: Vec<PathBuf>,
    state_written: bool,
}

impl AppliedLinkChanges {
    fn ensure_directory(&mut self, path: &Path) -> Result<()> {
        if ensure_directory(path)? {
            self.directories.push(path.to_path_buf());
        }
        Ok(())
    }
}

impl Project {
    pub fn new(manifest_path: PathBuf) -> Result<Self> {
        let manifest_path = if manifest_path.is_absolute() {
            manifest_path
        } else {
            std::env::current_dir()
                .whatever_context("could not determine the working directory")?
                .join(manifest_path)
        };
        let Some(filename) = manifest_path.file_name() else {
            snafu::whatever!("invalid manifest path: {}", manifest_path.display());
        };
        let root = manifest_path
            .parent()
            .expect("an absolute file path has a parent")
            .canonicalize()
            .with_whatever_context(|_| {
                format!(
                    "could not locate project directory for {}",
                    manifest_path.display()
                )
            })?;
        if !root.is_dir() {
            snafu::whatever!("project directory is not a directory: {}", root.display());
        }
        let manifest_path = root.join(filename);
        Ok(Self {
            root,
            manifest_path,
        })
    }

    /// Keep the returned file alive until the complete project update finishes.
    pub fn acquire(&self) -> Result<File> {
        let control = self.root.join(".typm");
        ensure_directory(&control)?;
        let path = control.join("project.lock");
        require_regular_file_if_present(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_whatever_context(|_| format!("could not open project lock {}", path.display()))?;
        file.lock()
            .with_whatever_context(|_| format!("could not lock project {}", self.root.display()))?;
        let ignore = control.join(".gitignore");
        if require_regular_file_if_present(&ignore)?.is_none() {
            atomic_write(&ignore, b"*\n")?;
        }
        Ok(file)
    }

    pub fn read_links(&self) -> Result<LinkState> {
        let control = self.root.join(".typm");
        require_directory_if_present(&control)?;
        let path = control.join("state.toml");
        if require_regular_file_if_present(&path)?.is_none() {
            return Ok(LinkState::default());
        }
        let contents = fs::read_to_string(&path)
            .with_whatever_context(|_| format!("could not read link state {}", path.display()))?;
        let state: LinkState = toml::from_str(&contents)
            .with_whatever_context(|_| format!("invalid link state {}", path.display()))?;
        if !matches!(state.version, 1 | 2)
            || (state.version == 1 && state.links.iter().any(|link| link.view.is_some()))
        {
            snafu::whatever!(
                "unsupported link state version {} in {}",
                state.version,
                path.display()
            );
        }
        index_links(&state.links)?;
        Ok(state)
    }

    /// Stage complete packages and retain replaced entries until every project
    /// file is committed, so rollback never has to reconstruct a source tree.
    pub fn update(
        &self,
        lockfile: &Lockfile,
        links: &[ManagedLink],
        manifest: Option<&str>,
    ) -> Result<()> {
        let snapshot = self.snapshot()?;
        require_regular_file_if_present(&self.manifest_path)?;
        let lock_contents =
            toml::to_string_pretty(lockfile).whatever_context("cannot serialize typm.lock")?;
        let update = self.preflight_links(links)?;
        let staging = tempfile::Builder::new()
            .prefix(".update-")
            .tempdir_in(self.root.join(".typm"))
            .whatever_context("could not create temporary package update")?;
        let mut applied = AppliedLinkChanges::default();
        let mut lockfile_written = false;
        let result = self
            .apply_links(&update, &mut applied, staging.path())
            .and_then(|()| {
                self.write_project_files(
                    (!lockfile.packages.is_empty() || snapshot.lockfile.is_some())
                        .then_some(lock_contents.as_str()),
                    manifest,
                    &mut lockfile_written,
                )
            });
        if let Err(error) = result {
            let mut failures = self.rollback_links(&applied, staging.path());
            if applied.state_written
                && let Err(error) = restore_file(
                    &self.root.join(".typm/state.toml"),
                    snapshot.state_contents.as_deref(),
                )
            {
                failures.push(error.to_string());
            }
            if lockfile_written
                && let Err(error) =
                    restore_file(&self.root.join("typm.lock"), snapshot.lockfile.as_deref())
            {
                failures.push(error.to_string());
            }
            if !failures.is_empty() {
                let backup = staging.keep();
                return Err(error).with_whatever_context(|_| {
                    format!(
                        "rollback incomplete: {}; package backups retained at {}",
                        failures.join("; "),
                        backup.display()
                    )
                });
            }
            return Err(error);
        }
        let active_targets: Vec<_> = links.iter().map(|link| link.target.clone()).collect();
        crate::stubs::cleanup_legacy(&self.root.join(".typm"), &active_targets);
        Ok(())
    }

    fn snapshot(&self) -> Result<ProjectSnapshot> {
        Ok(ProjectSnapshot {
            lockfile: read_optional(&self.root.join("typm.lock"))?,
            state_contents: read_optional(&self.root.join(".typm/state.toml"))?,
        })
    }

    fn write_project_files(
        &self,
        lockfile: Option<&str>,
        manifest: Option<&str>,
        lockfile_written: &mut bool,
    ) -> Result<()> {
        if let Some(contents) = lockfile {
            atomic_write(&self.root.join("typm.lock"), contents.as_bytes())?;
            *lockfile_written = true;
        }
        // The manifest write is the final fallible operation. If it succeeds,
        // the transaction is complete; only the lockfile and links need rollback.
        if let Some(contents) = manifest {
            atomic_write(&self.manifest_path, contents.as_bytes())?;
        }
        Ok(())
    }

    fn preflight_links(&self, desired: &[ManagedLink]) -> Result<LinkUpdate> {
        let previous = self.read_links()?;
        let previous_links = index_links(&previous.links)?;
        let desired_links = index_links(desired)?;
        require_directory_if_present(&self.root.join(".typm/packages"))?;
        let paths: BTreeSet<_> = previous_links
            .keys()
            .chain(desired_links.keys())
            .copied()
            .collect();
        let mut removals = Vec::new();
        let mut creations = Vec::new();
        for relative in paths {
            let before = previous_links.get(relative).copied();
            let after = desired_links.get(relative).copied();
            let exists = self.validate_existing_link(relative, before)?;
            match (before, after) {
                (Some(before), Some(after))
                    if before.target != after.target || before.view != after.view =>
                {
                    if exists {
                        removals.push(before.clone());
                    }
                    creations.push(after.clone());
                }
                (_, Some(after)) if !exists => creations.push(after.clone()),
                (Some(before), None) if exists => removals.push(before.clone()),
                _ => {}
            }
        }
        let next = LinkState {
            version: 2,
            links: desired_links.values().map(|link| (*link).clone()).collect(),
        };
        let state_contents = toml::to_string_pretty(&next)
            .whatever_context("could not serialize project link state")?;
        require_regular_file_if_present(&self.root.join(".typm/state.toml"))?;
        Ok(LinkUpdate {
            removals,
            creations,
            state_contents,
        })
    }

    fn validate_existing_link(&self, relative: &Path, owned: Option<&ManagedLink>) -> Result<bool> {
        self.check_link_parents(relative)?;
        let path = self.root.join(".typm/packages").join(relative);
        let Some(metadata) = metadata_if_present(&path)? else {
            return Ok(false);
        };
        let Some(owned) = owned else {
            snafu::whatever!(
                "refusing to replace unmanaged package path {}",
                path.display()
            );
        };
        if let Some(view) = &owned.view {
            view.validate(&path, &owned.target)?;
            return Ok(true);
        }
        if !metadata.file_type().is_symlink() {
            snafu::whatever!(
                "managed package link was replaced by a file or directory: {}",
                path.display()
            );
        }
        require_link_target(&path, &owned.target)?;
        Ok(true)
    }

    fn apply_links(
        &self,
        update: &LinkUpdate,
        applied: &mut AppliedLinkChanges,
        staging: &Path,
    ) -> Result<()> {
        let packages = self.root.join(".typm/packages");
        applied.ensure_directory(&self.root.join(".typm"))?;
        applied.ensure_directory(&packages)?;
        for (index, link) in update.creations.iter().enumerate() {
            let path = staging.join(format!("new-{index}"));
            if let Some(view) = &link.view {
                view.create(&path, &link.target)?;
            } else {
                create_symlink(&link.target, &path)?;
            }
        }
        for (index, link) in update.removals.iter().enumerate() {
            self.validate_existing_link(&link.path, Some(link))?;
            let path = packages.join(&link.path);
            let backup = staging.join(format!("old-{index}"));
            fs::rename(&path, &backup).with_whatever_context(|_| {
                format!("could not back up managed package {}", path.display())
            })?;
            applied.removed.push((link.clone(), backup));
        }
        for (index, link) in update.creations.iter().enumerate() {
            self.ensure_link_parents(&link.path, applied)?;
            let path = packages.join(&link.path);
            require_absent(&path)?;
            fs::rename(staging.join(format!("new-{index}")), &path).with_whatever_context(
                |_| format!("could not install managed package {}", path.display()),
            )?;
            applied.created.push(link.clone());
        }
        atomic_write(
            &self.root.join(".typm/state.toml"),
            update.state_contents.as_bytes(),
        )?;
        applied.state_written = true;
        Ok(())
    }

    fn rollback_links(&self, applied: &AppliedLinkChanges, staging: &Path) -> Vec<String> {
        let packages = self.root.join(".typm/packages");
        let mut failures = Vec::new();
        for (index, link) in applied.created.iter().enumerate().rev() {
            let path = packages.join(&link.path);
            let result = self
                .validate_existing_link(&link.path, Some(link))
                .and_then(|exists| {
                    if exists {
                        fs::rename(&path, staging.join(format!("discard-{index}")))
                            .with_whatever_context(|_| {
                                format!("could not roll back package {}", path.display())
                            })?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                failures.push(error.to_string());
            }
        }
        for (link, backup) in applied.removed.iter().rev() {
            let path = packages.join(&link.path);
            let result = self
                .check_link_parents(&link.path)
                .and_then(|()| require_absent(&path))
                .and_then(|()| {
                    fs::rename(backup, &path).with_whatever_context(|_| {
                        format!("could not restore package {}", path.display())
                    })
                });
            if let Err(error) = result {
                failures.push(error.to_string());
            }
        }
        remove_empty_created_directories(&applied.directories);
        failures
    }

    fn check_link_parents(&self, relative: &Path) -> Result<()> {
        let mut parent = self.root.join(".typm");
        require_directory_if_present(&parent)?;
        parent.push("packages");
        require_directory_if_present(&parent)?;
        for component in relative
            .parent()
            .expect("validated package path")
            .components()
        {
            parent.push(component);
            require_directory_if_present(&parent)?;
        }
        Ok(())
    }

    fn ensure_link_parents(&self, relative: &Path, applied: &mut AppliedLinkChanges) -> Result<()> {
        let mut parent = self.root.join(".typm");
        applied.ensure_directory(&parent)?;
        parent.push("packages");
        applied.ensure_directory(&parent)?;
        for component in relative
            .parent()
            .expect("validated package path")
            .components()
        {
            parent.push(component);
            applied.ensure_directory(&parent)?;
        }
        Ok(())
    }
}

fn index_links(links: &[ManagedLink]) -> Result<BTreeMap<&Path, &ManagedLink>> {
    let mut result = BTreeMap::new();
    for link in links {
        let components: Vec<_> = link.path.components().collect();
        if components.len() != 3
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            snafu::whatever!(
                "invalid package link path {}: expected namespace/name/version",
                link.path.display()
            );
        }
        if !link.target.is_absolute() {
            snafu::whatever!(
                "package link target must be absolute: {}",
                link.target.display()
            );
        }
        if let Some(view) = &link.view {
            view.validate_definition()?;
        }
        if result.insert(link.path.as_path(), link).is_some() {
            snafu::whatever!("duplicate package link path {}", link.path.display());
        }
    }
    Ok(result)
}

fn require_link_target(path: &Path, expected: &Path) -> Result<()> {
    let actual = fs::read_link(path).with_whatever_context(|_| {
        format!("could not read managed package link {}", path.display())
    })?;
    if actual != expected {
        snafu::whatever!(
            "managed package link was changed: {} (expected {}, found {})",
            path.display(),
            expected.display(),
            actual.display()
        );
    }
    Ok(())
}

fn create_symlink(target: &Path, path: &Path) -> Result<()> {
    crate::files::create_symlink(target, path, true)
}

fn require_absent(path: &Path) -> Result<()> {
    if metadata_if_present(path)?.is_some() {
        snafu::whatever!(
            "refusing to replace existing package path {}",
            path.display()
        );
    }
    Ok(())
}

fn restore_file(path: &Path, original: Option<&str>) -> Result<()> {
    if let Some(contents) = original {
        return atomic_write(path, contents.as_bytes());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_whatever_context(|_| format!("cannot roll back {}", path.display()))
        }
    }
}

fn remove_empty_created_directories(directories: &[PathBuf]) {
    for directory in directories.iter().rev() {
        if fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            let _ = fs::remove_dir(directory);
        }
    }
}
