//! Reading Typst package metadata and discovering packages inside a checkout.
use std::fs;
use std::path::{Path, PathBuf};

use snafu::{ResultExt, whatever};
use typst_syntax::package::{PackageManifest, PackageSpec};
use walkdir::WalkDir;

use crate::Result;

/// The stable version exposed in imports of direct project dependencies.
pub const IMPORT_VERSION: &str = "0.0.0";

#[derive(Debug, Clone)]
pub struct Package {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
}

pub fn read(root: &Path, namespace: &str) -> Result<Package> {
    let root = root
        .canonicalize()
        .with_whatever_context(|_| format!("cannot find package directory {}", root.display()))?;
    let manifest_path = root.join("typst.toml");
    let manifest_target = manifest_path
        .canonicalize()
        .with_whatever_context(|_| format!("package is missing {}", manifest_path.display()))?;
    if !manifest_target.starts_with(&root) {
        whatever!(
            "package manifest escapes its package directory: {}",
            manifest_path.display()
        );
    }
    let contents = fs::read_to_string(&manifest_path)
        .with_whatever_context(|_| format!("cannot read {}", manifest_path.display()))?;
    let manifest: PackageManifest = toml::from_str(&contents).with_whatever_context(|_| {
        format!("invalid package manifest {}", manifest_path.display())
    })?;
    let name = manifest.package.name.to_string();
    let version = manifest.package.version.to_string();
    let spec = format!("@{namespace}/{name}:{version}");
    spec.parse::<PackageSpec>()
        .map_err(|error| std::io::Error::other(error.to_string()))
        .with_whatever_context(|_| format!("invalid Typst package identity `{spec}`"))?;
    let entry = Path::new(manifest.package.entrypoint.as_str());
    if entry.is_absolute() {
        whatever!("package `{name}` entrypoint must be relative to its package directory");
    }
    let entry = root
        .join(entry)
        .canonicalize()
        .with_whatever_context(|_| format!("missing entrypoint for package `{name}`"))?;
    if !entry.starts_with(&root) || !entry.is_file() {
        whatever!("package `{name}` entrypoint must be a file inside its package directory");
    }
    Ok(Package {
        root,
        name,
        version,
    })
}

pub fn discover(root: &Path, name: Option<&str>, namespace: &str) -> Result<Package> {
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != ".typm")
    {
        let entry = entry
            .with_whatever_context(|_| format!("cannot search Git checkout {}", root.display()))?;
        if entry.file_name() != "typst.toml" || !entry.file_type().is_file() {
            continue;
        }
        let directory = entry.path().parent().expect("manifest has a parent");
        match read(directory, namespace) {
            Ok(package) => candidates.push(package),
            Err(error) => failures.push(format!(
                "{}: {}",
                directory.display(),
                snafu::Report::from_error(error)
            )),
        }
    }
    let all = candidates
        .iter()
        .map(|package| {
            format!(
                "{} ({})",
                package.name,
                package
                    .root
                    .strip_prefix(root)
                    .unwrap_or(&package.root)
                    .display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    candidates.retain(|package| name.is_none_or(|name| package.name == name));
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => whatever!(
            "no matching package{} in Git repository; candidates: {}{}",
            name.map(|name| format!(" `{name}`")).unwrap_or_default(),
            if all.is_empty() { "none" } else { &all },
            if failures.is_empty() {
                String::new()
            } else {
                format!("\n{}", failures.join("\n"))
            }
        ),
        _ => whatever!("ambiguous Git package; specify a unique package name; candidates: {all}"),
    }
}
