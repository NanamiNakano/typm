//! Immutable project-local package views with a stable import version.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt, whatever};

use crate::Result;
use crate::files::{
    create_symlink, ensure_directory, metadata_if_present, read_optional,
    require_directory_if_present,
};
use crate::package::IMPORT_VERSION;

/// Prepare a view before switching any installed links. Old views are retained
/// so a failed project update can restore its previous links without rebuilding.
pub fn prepare(control: &Path, source: &Path) -> Result<PathBuf> {
    let manifest_path = source.join("typst.toml");
    let original = fs::read_to_string(&manifest_path)
        .with_whatever_context(|_| format!("cannot read {}", manifest_path.display()))?;
    let manifest = manifest_stub(&original)
        .with_whatever_context(|_| format!("cannot create stub for {}", manifest_path.display()))?;
    let entries = source_entries(source)?;
    let mut hash = blake3::Hasher::new();
    for bytes in [
        b"typm-package-view-v1".as_slice(),
        source.as_os_str().as_encoded_bytes(),
        manifest.as_bytes(),
    ] {
        hash_part(&mut hash, bytes);
    }
    for (name, directory) in &entries {
        hash_part(&mut hash, name.as_encoded_bytes());
        hash.update(&[u8::from(*directory)]);
    }

    let stubs = control.join("stubs");
    let destination = stubs.join(hash.finalize().to_hex().as_str());
    ensure_directory(control)?;
    ensure_directory(&stubs)?;
    if metadata_if_present(&destination)?.is_some() {
        validate_view(&destination, source, &manifest, &entries)?;
        return Ok(destination.join("package"));
    }

    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&stubs)
        .whatever_context("could not create temporary package view")?;
    fs::write(staging.path().join("typst.toml"), &manifest)
        .whatever_context("could not write package manifest stub")?;
    let package = staging.path().join("package");
    fs::create_dir(&package).whatever_context("could not create package view")?;
    for (name, directory) in &entries {
        create_symlink(&entry_target(source, name), &package.join(name), *directory)?;
    }
    fs::rename(staging.path(), &destination).with_whatever_context(|_| {
        format!("could not publish package view {}", destination.display())
    })?;
    Ok(destination.join("package"))
}

fn hash_part(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
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

/// File types are part of the view identity because Windows distinguishes file
/// and directory symlinks. Paths remain links to the source, including its links.
fn source_entries(source: &Path) -> Result<BTreeMap<OsString, bool>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(source)
        .with_whatever_context(|_| format!("cannot list package {}", source.display()))?
    {
        let entry = entry.whatever_context("cannot read package directory entry")?;
        let name = entry.file_name();
        if name == ".typm" {
            continue;
        }
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::FileTypeExt;
            let kind = entry
                .file_type()
                .whatever_context("cannot inspect package directory entry")?;
            name != "typst.toml" && (kind.is_dir() || kind.is_symlink_dir())
        };
        #[cfg(not(windows))]
        let directory = name != "typst.toml" && entry.path().is_dir();
        entries.insert(name, directory);
    }
    if !entries.contains_key(std::ffi::OsStr::new("typst.toml")) {
        whatever!("package manifest disappeared from {}", source.display());
    }
    Ok(entries)
}

fn entry_target(source: &Path, name: &std::ffi::OsStr) -> PathBuf {
    if name == "typst.toml" {
        PathBuf::from("../typst.toml")
    } else {
        source.join(name)
    }
}

fn validate_view(
    view: &Path,
    source: &Path,
    manifest: &str,
    entries: &BTreeMap<OsString, bool>,
) -> Result<()> {
    require_directory_if_present(view)?;
    if read_optional(&view.join("typst.toml"))?.as_deref() != Some(manifest) {
        whatever!("generated package manifest was changed: {}", view.display());
    }
    let package = view.join("package");
    require_directory_if_present(&package)?;
    let actual = fs::read_dir(&package)
        .with_whatever_context(|_| format!("cannot read package view {}", package.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()
        .whatever_context("cannot read package view entries")?;
    if actual != entries.keys().cloned().collect() {
        whatever!(
            "generated package view entries were changed: {}",
            package.display()
        );
    }
    for name in entries.keys() {
        let path = package.join(name);
        let target = fs::read_link(&path).with_whatever_context(|_| {
            format!("generated package link was changed: {}", path.display())
        })?;
        if target != entry_target(source, name) {
            whatever!("generated package link was changed: {}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_only_package_version_in_all_table_forms() {
        for original in [
            "# version = '9.8.7'\r\n[package]\r\nname = 'test'\r\nversion = '1.2.3' # keep\r\n[tool]\r\nversion = '9.8.7'\r\n",
            "package = { name = 'test', version = '1.2.3' }\n",
            "package.name = 'test'\npackage.version = '1.2.3'\n",
            "[package]\nversion = '''1.2.3'''\n",
            "[package]\nversion = \"1.2.3\"\n",
        ] {
            assert_eq!(
                manifest_stub(original).unwrap(),
                original.replace("1.2.3", "0.0.0")
            );
        }
    }

    #[cfg(unix)]
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("typst.toml"),
            "[package]\nname = 'example'\nversion = '1.2.3'\nentrypoint = 'lib.typ'\n",
        )
        .unwrap();
        fs::write(source.join("lib.typ"), "#let value = 1\n").unwrap();
        let control = temp.path().join(".typm");
        (temp, control, source)
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_in_generated_directories_and_manifest() {
        for replaced in ["stubs", "view", "package", "manifest"] {
            let (_temp, control, source) = fixture();
            let original = fs::read(source.join("typst.toml")).unwrap();
            let package = prepare(&control, &source).unwrap();
            let view = package.parent().unwrap();
            let path = match replaced {
                "stubs" => control.join("stubs"),
                "view" => view.to_owned(),
                "package" => package.clone(),
                "manifest" => view.join("typst.toml"),
                _ => unreachable!(),
            };
            let target = if replaced == "manifest" {
                fs::remove_file(&path).unwrap();
                source.join("typst.toml")
            } else {
                fs::remove_dir_all(&path).unwrap();
                source.clone()
            };
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(prepare(&control, &source).is_err(), "{replaced}");
            assert_eq!(fs::read_link(&path).unwrap(), target);
            assert_eq!(fs::read(source.join("typst.toml")).unwrap(), original);
            assert_eq!(fs::read_dir(&source).unwrap().count(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_changed_stub_contents_and_entry_targets() {
        let (_temp, control, source) = fixture();
        let package = prepare(&control, &source).unwrap();
        let manifest = package.parent().unwrap().join("typst.toml");
        let original = fs::read(&manifest).unwrap();
        fs::write(&manifest, "user data\n").unwrap();
        assert!(prepare(&control, &source).is_err());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "user data\n");
        fs::write(&manifest, original).unwrap();
        let entry = package.join("lib.typ");
        fs::remove_file(&entry).unwrap();
        std::os::unix::fs::symlink(source.join("typst.toml"), &entry).unwrap();
        assert!(prepare(&control, &source).is_err());
        assert_eq!(fs::read_link(&entry).unwrap(), source.join("typst.toml"));
    }
}
