//! Source providers own package acquisition, repository boundaries, and source identities.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use snafu::{ResultExt, whatever};

use crate::Result;
use crate::context::Context;
use crate::git::{Cache, Checkout, GitSourceSpec};
use crate::lockfile::{LockedSource, Lockfile};
use crate::manifest::{Dependency, ManifestDocument};
use crate::package::{self, Package};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceId {
    Git(String),
    Path(PathBuf),
}

pub struct LocatedPackage {
    pub package: Package,
    pub source: SourceId,
}

pub struct PackageIdentity {
    pub location_id: String,
    pub pin: Option<PackagePin>,
    source_key: String,
}

impl PackageIdentity {
    pub fn package_id(&self, namespace: &str) -> String {
        hash_parts(&[&self.source_key, namespace])
    }
}

pub struct PackagePin {
    pub source_id: String,
    pub source: LockedSource,
    pub directory: PathBuf,
}

enum PackageRequest<'a> {
    Discover {
        expected: Option<&'a str>,
        namespace: &'a str,
    },
    Path {
        path: &'a Path,
        base: &'a Path,
        namespace: &'a str,
    },
}

impl PackageRequest<'_> {
    fn namespace(&self) -> &str {
        match self {
            Self::Discover { namespace, .. } | Self::Path { namespace, .. } => namespace,
        }
    }
}

trait Source {
    fn package(&self, request: PackageRequest<'_>) -> Result<Package>;
    fn manifest(&self, package_root: &Path) -> Result<ManifestDocument>;
    fn identity(&self, package: &Package) -> Result<PackageIdentity>;
}

struct GitSource {
    specification: GitSourceSpec,
    checkout: Checkout,
}

impl Source for GitSource {
    fn package(&self, request: PackageRequest<'_>) -> Result<Package> {
        match request {
            PackageRequest::Discover {
                expected,
                namespace,
            } => package::discover(&self.checkout.root, expected, namespace),
            PackageRequest::Path {
                path,
                base,
                namespace,
            } => {
                if path.is_absolute() {
                    whatever!(
                        "Git package declares an absolute local dependency path `{}`; use a relative path inside the repository or a Git dependency",
                        path.display()
                    );
                }
                let root = resolve_dependency_path(path, base)?;
                if !root.starts_with(&self.checkout.root) {
                    whatever!(
                        "path dependency `{}` escapes the pinned Git repository; use a Git dependency for packages outside the checkout",
                        path.display()
                    );
                }
                package::read(&root, namespace)
            }
        }
    }

    fn manifest(&self, package_root: &Path) -> Result<ManifestDocument> {
        let manifest_path = package_root.join("typm.toml");
        if manifest_path
            .try_exists()
            .whatever_context("cannot inspect package dependency manifest")?
        {
            let target = manifest_path
                .canonicalize()
                .whatever_context("cannot resolve package dependency manifest")?;
            if !target.starts_with(&self.checkout.root) {
                whatever!(
                    "dependency manifest escapes the pinned Git repository: {}",
                    manifest_path.display()
                );
            }
        }
        ManifestDocument::read(&manifest_path)
    }

    fn identity(&self, package: &Package) -> Result<PackageIdentity> {
        let directory = package
            .root
            .strip_prefix(&self.checkout.root)
            .whatever_context("Git package directory escapes its checkout")?;
        let location_id = hash_parts(&[
            &self.specification.git,
            &self.checkout.commit,
            &directory.to_string_lossy(),
        ]);
        let source_id = self.specification.id();
        Ok(PackageIdentity {
            source_key: hash_parts(&[&source_id, &location_id]),
            location_id,
            pin: Some(PackagePin {
                source_id,
                source: LockedSource {
                    source: self.specification.clone(),
                    commit: self.checkout.commit.clone(),
                },
                directory: directory.into(),
            }),
        })
    }
}

struct PathSource {
    root: PathBuf,
}

impl Source for PathSource {
    fn package(&self, request: PackageRequest<'_>) -> Result<Package> {
        package::read(&self.root, request.namespace())
    }

    fn manifest(&self, package_root: &Path) -> Result<ManifestDocument> {
        ManifestDocument::read(&package_root.join("typm.toml"))
    }

    fn identity(&self, package: &Package) -> Result<PackageIdentity> {
        let location_id = hash_parts(&["local", &package.root.to_string_lossy()]);
        Ok(PackageIdentity {
            source_key: location_id.clone(),
            location_id,
            pin: None,
        })
    }
}

pub enum RefreshPolicy {
    Preserve,
    Source(String),
    All,
}

impl RefreshPolicy {
    fn refreshes(&self, source: &str) -> bool {
        match self {
            Self::Preserve => false,
            Self::Source(id) => id == source,
            Self::All => true,
        }
    }
}

pub struct SourceMap<'context> {
    context: &'context Context,
    cache: Option<Cache>,
    providers: BTreeMap<SourceId, Box<dyn Source>>,
    previous_pins: BTreeMap<String, LockedSource>,
    refresh: RefreshPolicy,
    cache_lock: Option<File>,
}

impl<'context> SourceMap<'context> {
    pub fn new(
        context: &'context Context,
        previous_lock: Lockfile,
        refresh: RefreshPolicy,
    ) -> Self {
        Self {
            context,
            cache: None,
            providers: BTreeMap::new(),
            previous_pins: previous_lock.sources,
            refresh,
            cache_lock: None,
        }
    }

    pub fn locate(
        &mut self,
        dependency: &Dependency,
        base: &Path,
        parent: Option<&SourceId>,
        expected: Option<&str>,
    ) -> Result<LocatedPackage> {
        dependency.validate(expected)?;
        let (source, request) = if let Some(mut specification) = dependency.git_source() {
            specification.git = normalize_git_location(&specification.git, base)?;
            let source = self.load_git(specification)?;
            (
                source,
                PackageRequest::Discover {
                    expected,
                    namespace: &dependency.namespace,
                },
            )
        } else {
            let path = dependency.path.as_ref().expect("validated path source");
            if let Some(source @ SourceId::Git(_)) = parent {
                (
                    source.clone(),
                    PackageRequest::Path {
                        path,
                        base,
                        namespace: &dependency.namespace,
                    },
                )
            } else {
                let source = self.load_path(resolve_dependency_path(path, base)?);
                (
                    source,
                    PackageRequest::Discover {
                        expected,
                        namespace: &dependency.namespace,
                    },
                )
            }
        };
        let package = self.provider(&source).package(request)?;
        if let Some(expected) = expected
            && package.name != expected
        {
            whatever!(
                "dependency `{expected}` points to package `{}`",
                package.name
            );
        }
        Ok(LocatedPackage { package, source })
    }

    pub fn identity(&self, located: &LocatedPackage) -> Result<PackageIdentity> {
        self.provider(&located.source).identity(&located.package)
    }

    pub fn manifest(&self, source: &SourceId, package_root: &Path) -> Result<ManifestDocument> {
        self.provider(source).manifest(package_root)
    }

    fn provider(&self, source: &SourceId) -> &dyn Source {
        self.providers
            .get(source)
            .expect("source registered before package lookup")
            .as_ref()
    }

    fn load_git(&mut self, specification: GitSourceSpec) -> Result<SourceId> {
        let id = specification.id();
        let source_id = SourceId::Git(id.clone());
        if self.providers.contains_key(&source_id) {
            return Ok(source_id);
        }
        if self.cache_lock.is_none() {
            self.cache_lock = Some(self.context.acquire_cache_lock()?);
        }
        if self.cache.is_none() {
            self.cache = Some(Cache::new(self.context)?);
        }
        let locked = self
            .previous_pins
            .get(&id)
            .filter(|locked| locked.source == specification);
        let checkout = self.cache.as_ref().expect("cache initialized").resolve(
            &specification,
            locked.map(|locked| locked.commit.as_str()),
            self.refresh.refreshes(&id),
        )?;
        self.providers.insert(
            source_id.clone(),
            Box::new(GitSource {
                specification,
                checkout,
            }),
        );
        Ok(source_id)
    }

    fn load_path(&mut self, root: PathBuf) -> SourceId {
        let source_id = SourceId::Path(root.clone());
        self.providers
            .entry(source_id.clone())
            .or_insert_with(|| Box::new(PathSource { root }));
        source_id
    }
}

fn resolve_dependency_path(path: &Path, base: &Path) -> Result<PathBuf> {
    base.join(path).canonicalize().with_whatever_context(|_| {
        format!(
            "cannot locate path dependency {}",
            base.join(path).display()
        )
    })
}

fn hash_parts(parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}

/// Git URLs and SSH shorthand are preserved. Filesystem sources are interpreted
/// relative to their declaring manifest (relative to cwd for CLI --git).
pub fn normalize_git_location(location: &str, base: &Path) -> Result<String> {
    if location.contains(':') {
        return Ok(location.to_owned());
    }
    let path = std::path::absolute(base.join(location))
        .whatever_context("cannot resolve Git repository path")?;
    let Some(path) = path.to_str() else {
        whatever!("Git repository paths must be valid UTF-8");
    };
    Ok(path.to_owned())
}
