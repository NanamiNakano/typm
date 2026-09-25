//! Project-local package links and the state that records their ownership.

use crate::Result;
use crate::files::{
    atomic_write, ensure_directory, metadata_if_present, read_optional,
    require_directory_if_present, require_regular_file_if_present,
};
use crate::lockfile::Lockfile;
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
            version: 1,
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
    links: LinkState,
}

struct LinkUpdate {
    removals: Vec<ManagedLink>,
    creations: Vec<ManagedLink>,
    state_contents: String,
}

#[derive(Default)]
struct AppliedLinkChanges {
    removed: Vec<ManagedLink>,
    created: Vec<ManagedLink>,
    directories: Vec<PathBuf>,
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
        if state.version != 1 {
            snafu::whatever!(
                "unsupported link state version {} in {}",
                state.version,
                path.display()
            );
        }
        index_links(&state.links)?;
        Ok(state)
    }

    /// File replacements are atomic individually. Roll back ordinary I/O failures
    /// across the manifest, lockfile and links while holding the project lock.
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
        self.reconcile(links)?;
        let mut lockfile_written = false;
        let result = self.write_project_files(
            (!lockfile.packages.is_empty() || snapshot.lockfile.is_some())
                .then_some(lock_contents.as_str()),
            manifest,
            &mut lockfile_written,
        );
        if let Err(error) = result {
            let failures = self.restore_snapshot(&snapshot, lockfile_written);
            if !failures.is_empty() {
                return Err(error).with_whatever_context(|_| {
                    format!("rollback incomplete: {}", failures.join("; "))
                });
            }
            return Err(error);
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<ProjectSnapshot> {
        Ok(ProjectSnapshot {
            lockfile: read_optional(&self.root.join("typm.lock"))?,
            links: self.read_links()?,
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

    fn restore_snapshot(&self, snapshot: &ProjectSnapshot, lockfile_written: bool) -> Vec<String> {
        let mut failures = Vec::new();
        if lockfile_written
            && let Err(error) =
                restore_file(&self.root.join("typm.lock"), snapshot.lockfile.as_deref())
        {
            failures.push(error.to_string());
        }
        if let Err(error) = self.reconcile(&snapshot.links.links) {
            failures.push(error.to_string());
        }
        failures
    }

    /// Reconcile links under the project lock. An existing link is owned only
    /// when its literal target matches the target recorded in state.toml.
    fn reconcile(&self, desired: &[ManagedLink]) -> Result<()> {
        let update = self.preflight_links(desired)?;
        let mut applied = AppliedLinkChanges::default();
        if let Err(error) = self.apply_links(&update, &mut applied) {
            let rollback_errors = self.rollback_links(&applied);
            if !rollback_errors.is_empty() {
                snafu::whatever!(
                    "{error}; could not fully restore package links: {}",
                    rollback_errors.join("; ")
                );
            }
            return Err(error);
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
                (Some(before), Some(after)) if before.target != after.target => {
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
            version: 1,
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
        if !metadata.file_type().is_symlink() {
            snafu::whatever!(
                "managed package link was replaced by a file or directory: {}",
                path.display()
            );
        }
        require_link_target(&path, &owned.target)?;
        Ok(true)
    }

    fn apply_links(&self, update: &LinkUpdate, applied: &mut AppliedLinkChanges) -> Result<()> {
        let packages = self.root.join(".typm/packages");
        applied.ensure_directory(&self.root.join(".typm"))?;
        applied.ensure_directory(&packages)?;
        for link in &update.removals {
            self.check_link_parents(&link.path)?;
            let path = packages.join(&link.path);
            require_link_target(&path, &link.target)?;
            remove_symlink(&path)?;
            applied.removed.push(link.clone());
        }
        for link in &update.creations {
            self.ensure_link_parents(&link.path, applied)?;
            create_symlink(&link.target, &packages.join(&link.path))?;
            applied.created.push(link.clone());
        }
        atomic_write(
            &self.root.join(".typm/state.toml"),
            update.state_contents.as_bytes(),
        )
    }

    fn rollback_links(&self, applied: &AppliedLinkChanges) -> Vec<String> {
        let packages = self.root.join(".typm/packages");
        let mut failures = Vec::new();
        for link in applied.created.iter().rev() {
            let path = packages.join(&link.path);
            let result = self
                .check_link_parents(&link.path)
                .and_then(|()| require_link_target(&path, &link.target))
                .and_then(|()| remove_symlink(&path));
            if let Err(error) = result {
                failures.push(error.to_string());
            }
        }
        for link in applied.removed.iter().rev() {
            let result = self
                .check_link_parents(&link.path)
                .and_then(|()| create_symlink(&link.target, &packages.join(&link.path)));
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

fn remove_symlink(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let result = fs::remove_dir(path);
    #[cfg(not(windows))]
    let result = fs::remove_file(path);
    result.with_whatever_context(|_| {
        format!("could not remove managed package link {}", path.display())
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Project) {
        let directory = tempfile::tempdir().unwrap();
        let project = Project::new(directory.path().join("typm.toml")).unwrap();
        (directory, project)
    }

    fn link(project: &Project, name: &str) -> ManagedLink {
        ManagedLink {
            path: PathBuf::from(format!("local/{name}/1.0.0")),
            target: project.root.join(format!("source-{name}")),
            roots: BTreeSet::from([name.to_string()]),
        }
    }

    #[cfg(unix)]
    #[test]
    fn acquire_refuses_symlink_or_directory_at_gitignore() {
        let (_directory, project) = setup();
        let control = project.root.join(".typm");
        fs::create_dir(&control).unwrap();
        let ignore = control.join(".gitignore");
        let original = project.root.join("custom-ignore");
        fs::write(&original, "custom rules\n").unwrap();
        create_symlink(&original, &ignore).unwrap();
        assert!(
            project
                .acquire()
                .unwrap_err()
                .to_string()
                .contains(".gitignore")
        );
        assert_eq!(fs::read_link(&ignore).unwrap(), original);
        assert_eq!(fs::read_to_string(&original).unwrap(), "custom rules\n");

        fs::remove_file(&ignore).unwrap();
        fs::create_dir(&ignore).unwrap();
        fs::write(ignore.join("keep.txt"), "user data").unwrap();
        assert!(
            project
                .acquire()
                .unwrap_err()
                .to_string()
                .contains(".gitignore")
        );
        assert_eq!(
            fs::read_to_string(ignore.join("keep.txt")).unwrap(),
            "user data"
        );
    }

    #[test]
    fn rejects_traversal_before_creating_directories() {
        let (_directory, project) = setup();
        let mut desired = link(&project, "example");
        desired.path = PathBuf::from("../outside/version");
        assert!(project.reconcile(&[desired]).is_err());
        assert!(!project.root.join(".typm").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_changed_links_and_preserves_state() {
        let (_directory, project) = setup();
        let desired = link(&project, "example");
        project.reconcile(std::slice::from_ref(&desired)).unwrap();
        let destination = project.root.join(".typm/packages").join(&desired.path);
        fs::remove_file(&destination).unwrap();
        create_symlink(&project.root.join("user-package"), &destination).unwrap();
        assert!(project.reconcile(&[]).is_err());
        assert_eq!(
            fs::read_link(destination).unwrap(),
            project.root.join("user-package")
        );
        assert_eq!(project.read_links().unwrap().links, vec![desired]);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_unmanaged_symlink_even_when_its_target_matches() {
        let (_directory, project) = setup();
        let desired = link(&project, "example");
        let destination = project.root.join(".typm/packages").join(&desired.path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        create_symlink(&desired.target, &destination).unwrap();
        assert!(project.reconcile(std::slice::from_ref(&desired)).is_err());
        assert_eq!(fs::read_link(destination).unwrap(), desired.target);
        assert!(!project.root.join(".typm/state.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_parent_and_control_directories() {
        let (directory, project) = setup();
        let elsewhere = directory.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        create_symlink(&elsewhere, &project.root.join(".typm")).unwrap();
        assert!(project.acquire().is_err());
        assert!(project.reconcile(&[link(&project, "example")]).is_err());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        fs::remove_file(project.root.join(".typm")).unwrap();
        fs::create_dir_all(project.root.join(".typm/packages")).unwrap();
        create_symlink(&elsewhere, &project.root.join(".typm/packages/local")).unwrap();
        assert!(project.reconcile(&[link(&project, "example")]).is_err());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rolls_back_removal_when_new_link_cannot_be_created() {
        let (_directory, project) = setup();
        let before = link(&project, "before");
        project.reconcile(std::slice::from_ref(&before)).unwrap();
        let after = link(&project, "after");
        let mut invalid = link(&project, "z-invalid");
        invalid.target = project.root.join("x".repeat(16384));
        assert!(project.reconcile(&[after.clone(), invalid]).is_err());
        assert_eq!(
            fs::read_link(project.root.join(".typm/packages").join(&before.path)).unwrap(),
            before.target
        );
        assert!(
            fs::symlink_metadata(project.root.join(".typm/packages").join(&after.path)).is_err()
        );
        assert_eq!(project.read_links().unwrap().links, vec![before]);
    }
}
