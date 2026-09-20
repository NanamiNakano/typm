//! Portable Git dependency pins. Local working trees are never pinned.

use crate::Result;
use crate::files::read_optional;
use crate::git::GitSourceSpec;
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, whatever};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSource {
    pub source: GitSourceSpec,
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub source: String,
    pub directory: PathBuf,
    pub name: String,
    pub version: String,
    pub namespace: String,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default)]
    pub sources: BTreeMap<String, LockedSource>,
    #[serde(default)]
    pub packages: BTreeMap<String, LockedPackage>,
    /// Git entrypoints reached through each direct dependency's local graph.
    #[serde(default)]
    pub roots: BTreeMap<String, Vec<String>>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: 1,
            sources: BTreeMap::new(),
            packages: BTreeMap::new(),
            roots: BTreeMap::new(),
        }
    }
}

impl Lockfile {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            whatever!("unsupported typm.lock format version {}", self.version);
        }
        for (id, source) in &self.sources {
            source.source.validate()?;
            if source.source.id() != *id {
                whatever!("invalid source identity in typm.lock");
            }
            if !matches!(source.commit.len(), 40 | 64)
                || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                whatever!("invalid locked Git commit `{}`", source.commit);
            }
        }
        for package in self.packages.values() {
            if !self.sources.contains_key(&package.source) {
                whatever!("typm.lock references an unknown Git source");
            }
        }
        for id in self.roots.values().flatten().chain(
            self.packages
                .values()
                .flat_map(|package| &package.dependencies),
        ) {
            if !self.packages.contains_key(id) {
                whatever!("typm.lock references an unknown package `{id}`");
            }
        }
        Ok(())
    }
}

pub fn read(path: &Path) -> Result<Lockfile> {
    let Some(text) = read_optional(path)? else {
        return Ok(Lockfile::default());
    };
    let lock: Lockfile = toml::from_str(&text)
        .with_whatever_context(|_| format!("cannot parse {}", path.display()))?;
    lock.validate()?;
    Ok(lock)
}
