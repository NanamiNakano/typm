//! Project-local package directories with an inline manifest and source links.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, whatever};

use crate::Result;
use crate::files::{create_symlink, read_optional, require_directory_if_present};
use crate::package::IMPORT_VERSION;

/// The complete generated contents are recorded so ownership and rollback do
/// not depend on the source package still having its original contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageView {
    manifest: String,
    entries: Vec<SourceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SourceEntry {
    name: OsString,
    directory: bool,
}

impl PackageView {
    /// Read a package without changing either the source or the project.
    pub fn prepare(source: &Path) -> Result<Self> {
        let manifest_path = source.join("typst.toml");
        let original = fs::read_to_string(&manifest_path)
            .with_whatever_context(|_| format!("cannot read {}", manifest_path.display()))?;
        let manifest = manifest_stub(&original).with_whatever_context(|_| {
            format!("cannot create stub for {}", manifest_path.display())
        })?;
        let entries = source_entries(source)?;
        Ok(Self { manifest, entries })
    }

    /// State is untrusted input: entry names must stay immediately inside the
    /// package directory and must not replace the inline manifest.
    pub fn validate_definition(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for entry in &self.entries {
            let mut components = Path::new(&entry.name).components();
            if !matches!(components.next(), Some(Component::Normal(name)) if name == entry.name)
                || components.next().is_some()
                || entry.name == "typst.toml"
                || entry.name == ".typm"
            {
                whatever!("invalid package view entry name: {:?}", entry.name);
            }
            if !names.insert(&entry.name) {
                whatever!("duplicate package view entry name: {:?}", entry.name);
            }
        }
        Ok(())
    }

    /// Create a new directory. The caller stages it and handles rollback if a
    /// write fails part way through construction.
    pub fn create(&self, path: &Path, source: &Path) -> Result<()> {
        self.validate_definition()?;
        fs::create_dir(path).with_whatever_context(|_| {
            format!("could not create package view {}", path.display())
        })?;
        fs::write(path.join("typst.toml"), &self.manifest)
            .whatever_context("could not write package manifest stub")?;
        for entry in &self.entries {
            create_symlink(
                &source.join(&entry.name),
                &path.join(&entry.name),
                entry.directory,
            )?;
        }
        Ok(())
    }

    /// Check only the generated directory, without inspecting the contents of
    /// its source. Changed or deleted source files do not invalidate ownership.
    pub fn validate(&self, path: &Path, source: &Path) -> Result<()> {
        self.validate_definition()?;
        require_directory_if_present(path)?;
        let mut expected: BTreeSet<_> = self
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        expected.insert(OsString::from("typst.toml"));
        require_entries(path, &expected)?;
        if read_optional(&path.join("typst.toml"))?.as_deref() != Some(&self.manifest) {
            whatever!("generated package manifest was changed: {}", path.display());
        }
        for entry in &self.entries {
            require_source_link(
                &path.join(&entry.name),
                &source.join(&entry.name),
                entry.directory,
            )?;
        }
        Ok(())
    }
}

fn manifest_stub(original: &str) -> Result<String> {
    let document =
        toml_edit::Document::parse(original).whatever_context("invalid Typst package manifest")?;
    let version = document
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml_edit::Item::as_value)
        .filter(|value| value.is_str())
        .and_then(toml_edit::Value::span)
        .whatever_context("package manifest must contain a string package.version")?;
    // Replace only the parsed value token, preserving the rest of the original
    // document byte for byte, including comments, CRLFs, and unknown fields.
    let token = &original[version.clone()];
    let quote = if token.starts_with("\"\"\"") {
        "\"\"\""
    } else if token.starts_with("'''") {
        "'''"
    } else if token.starts_with('\'') {
        "'"
    } else {
        "\""
    };
    let mut stub = original.to_owned();
    stub.replace_range(version, &format!("{quote}{IMPORT_VERSION}{quote}"));
    Ok(stub)
}

/// Windows distinguishes file and directory symlinks, including dangling ones.
fn source_entries(source: &Path) -> Result<Vec<SourceEntry>> {
    let mut entries = Vec::new();
    let mut manifest_present = false;
    for entry in fs::read_dir(source)
        .with_whatever_context(|_| format!("cannot list package {}", source.display()))?
    {
        let entry = entry.whatever_context("cannot read package directory entry")?;
        let name = entry.file_name();
        if name == "typst.toml" {
            manifest_present = true;
            continue;
        }
        if name == ".typm" {
            continue;
        }
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::FileTypeExt;
            let kind = entry
                .file_type()
                .whatever_context("cannot inspect package directory entry")?;
            kind.is_dir() || kind.is_symlink_dir()
        };
        #[cfg(not(windows))]
        let directory = entry.path().is_dir();
        entries.push(SourceEntry { name, directory });
    }
    if !manifest_present {
        whatever!("package manifest disappeared from {}", source.display());
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn require_entries(path: &Path, expected: &BTreeSet<OsString>) -> Result<()> {
    let actual = fs::read_dir(path)
        .with_whatever_context(|_| format!("cannot read package view {}", path.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()
        .whatever_context("cannot read package view entries")?;
    if &actual != expected {
        whatever!(
            "generated package view entries were changed: {}",
            path.display()
        );
    }
    Ok(())
}

fn require_source_link(path: &Path, expected: &Path, directory: bool) -> Result<()> {
    let target = fs::read_link(path).with_whatever_context(|_| {
        format!("generated package link was changed: {}", path.display())
    })?;
    if target != expected {
        whatever!("generated package link was changed: {}", path.display());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        let kind = fs::symlink_metadata(path)
            .whatever_context("cannot inspect generated package link")?
            .file_type();
        if kind.is_symlink_dir() != directory {
            whatever!(
                "generated package link type was changed: {}",
                path.display()
            );
        }
    }
    #[cfg(not(windows))]
    let _ = directory;
    Ok(())
}

/// Remove unused old views still matching their original content hash,
/// including historical views retained by earlier updates. Run after commit.
pub fn cleanup_legacy(control: &Path, active_targets: &[PathBuf]) {
    let stubs = control.join("stubs");
    if require_directory_if_present(control).is_err()
        || require_directory_if_present(&stubs).is_err()
    {
        return;
    }
    let Ok(entries) = fs::read_dir(&stubs) else {
        return;
    };
    for entry in entries.flatten() {
        let view = entry.path();
        if active_targets
            .iter()
            .any(|target| target.starts_with(&view))
        {
            continue;
        }
        if let Ok(view) = validate_legacy(control, &view.join("package")) {
            let _ = fs::remove_dir_all(view);
        }
    }
    // Unknown or modified views leave the directory nonempty and are retained.
    let _ = fs::remove_dir(stubs);
}

fn validate_legacy(control: &Path, target: &Path) -> Result<PathBuf> {
    let stubs = control.join("stubs");
    let relative = target
        .strip_prefix(&stubs)
        .whatever_context("not a legacy package view")?;
    let components: Vec<_> = relative.components().collect();
    let [Component::Normal(hash_name), Component::Normal(package)] = components.as_slice() else {
        whatever!("not a legacy package view");
    };
    let hash_name = hash_name
        .to_str()
        .whatever_context("invalid legacy view hash")?;
    if *package != OsStr::new("package")
        || hash_name.len() != 64
        || !hash_name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        whatever!("not a legacy package view");
    }
    require_directory_if_present(control)?;
    require_directory_if_present(&stubs)?;
    let view = stubs.join(hash_name);
    require_directory_if_present(&view)?;
    require_directory_if_present(target)?;
    require_entries(
        &view,
        &BTreeSet::from(["package".into(), "typst.toml".into()]),
    )?;
    let manifest = read_optional(&view.join("typst.toml"))?
        .whatever_context("missing legacy package manifest")?;
    if manifest_stub(&manifest)? != manifest {
        whatever!("legacy package manifest was changed");
    }
    let mut entries = BTreeMap::new();
    let mut source = None;
    for entry in fs::read_dir(target).whatever_context("cannot read legacy package view")? {
        let entry = entry.whatever_context("cannot read legacy package entry")?;
        let name = entry.file_name();
        if name == ".typm" {
            whatever!("unexpected legacy package entry");
        }
        if name == "typst.toml" {
            require_source_link(&entry.path(), Path::new("../typst.toml"), false)?;
            entries.insert(name, false);
            continue;
        }
        let link =
            fs::read_link(entry.path()).whatever_context("legacy package link was changed")?;
        let parent = link
            .parent()
            .whatever_context("invalid legacy package link")?;
        if !parent.is_absolute() || link.file_name() != Some(name.as_os_str()) {
            whatever!("invalid legacy package link");
        }
        if source.as_deref().is_some_and(|source| source != parent) {
            whatever!("legacy package link was changed");
        }
        source = Some(parent.to_path_buf());
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::FileTypeExt;
            entry
                .file_type()
                .whatever_context("cannot inspect legacy package link")?
                .is_symlink_dir()
        };
        #[cfg(not(windows))]
        let directory = link.is_dir();
        entries.insert(name, directory);
    }
    if !entries.contains_key(OsStr::new("typst.toml")) {
        whatever!("missing legacy manifest link");
    }
    // A manifest-only view has no source link from which to recover the source
    // path included in its original hash. Retain it rather than guessing.
    let source = source.whatever_context("cannot establish legacy package source")?;
    let mut hash = blake3::Hasher::new();
    for bytes in [
        b"typm-package-view-v1".as_slice(),
        source.as_os_str().as_encoded_bytes(),
        manifest.as_bytes(),
    ] {
        hash_part(&mut hash, bytes);
    }
    for (name, directory) in entries {
        hash_part(&mut hash, name.as_encoded_bytes());
        hash.update(&[u8::from(directory)]);
    }
    if hash.finalize().to_hex().as_str() != hash_name {
        whatever!("legacy package view was changed");
    }
    Ok(view)
}

fn hash_part(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
