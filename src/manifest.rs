use crate::Result;
use crate::files::read_optional;
use crate::git::GitSourceSpec;
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, whatever};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

fn default_namespace() -> String {
    "typm".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

impl Dependency {
    pub fn validate(&self, name: Option<&str>) -> Result<()> {
        if self.git.is_some() == self.path.is_some() {
            whatever!("dependency must specify exactly one of `git` or `path`");
        }
        if self
            .path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            whatever!("dependency path cannot be empty");
        }
        let spec = format!("@{}/{}", self.namespace, name.unwrap_or("package"));
        spec.parse::<typst_syntax::package::VersionlessPackageSpec>()
            .map_err(|error| std::io::Error::other(error.to_string()))
            .with_whatever_context(|_| format!("invalid dependency identity `{spec}`"))?;
        if let Some(source) = self.git_source() {
            source.validate()?;
        } else if self.branch.is_some() || self.tag.is_some() || self.rev.is_some() {
            whatever!("branch, tag, and rev require a Git dependency");
        }
        Ok(())
    }

    pub fn git_source(&self) -> Option<GitSourceSpec> {
        Some(GitSourceSpec {
            git: self.git.clone()?,
            branch: self.branch.clone(),
            tag: self.tag.clone(),
            rev: self.rev.clone(),
        })
    }

    fn to_inline_table(&self) -> Result<InlineTable> {
        let mut entry = InlineTable::new();
        if let Some(git) = &self.git {
            entry.insert("git", Value::from(git.as_str()));
        }
        if let Some(path) = &self.path {
            let Some(path) = path.to_str() else {
                whatever!("local dependency paths in TOML must be valid UTF-8");
            };
            entry.insert("path", Value::from(path));
        }
        for (key, value) in [
            ("branch", &self.branch),
            ("tag", &self.tag),
            ("rev", &self.rev),
        ] {
            if let Some(value) = value {
                entry.insert(key, Value::from(value.as_str()));
            }
        }
        if self.namespace != "typm" {
            entry.insert("namespace", Value::from(self.namespace.as_str()));
        }
        Ok(entry)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        for (name, dependency) in &self.dependencies {
            dependency
                .validate(Some(name))
                .with_whatever_context(|_| format!("invalid dependency `{name}`"))?;
        }
        Ok(())
    }
}

pub struct ManifestDocument {
    pub manifest: Manifest,
    document: DocumentMut,
}

impl ManifestDocument {
    pub fn read(path: &Path) -> Result<Self> {
        let text = read_optional(path)?.unwrap_or_default();
        Self::parse(path, &text)
    }

    pub fn read_required(path: &Path) -> Result<Self> {
        let Some(text) = read_optional(path)? else {
            whatever!("manifest does not exist: {}", path.display());
        };
        Self::parse(path, &text)
    }

    fn parse(path: &Path, text: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(text)
            .with_whatever_context(|_| format!("cannot parse {}", path.display()))?;
        manifest.validate()?;
        let document = text
            .parse::<DocumentMut>()
            .with_whatever_context(|_| format!("cannot edit {}", path.display()))?;
        Ok(Self { manifest, document })
    }

    pub fn insert(&mut self, name: &str, dependency: Dependency) -> Result<()> {
        let entry = dependency.to_inline_table()?;
        let dependencies = self.dependencies();
        if let Some(existing) = dependencies.get_mut(name).and_then(Item::as_table_like_mut) {
            update_dependency_entry(existing, &entry);
        } else {
            dependencies.insert(name, Item::Value(Value::InlineTable(entry)));
        }
        self.manifest
            .dependencies
            .insert(name.to_owned(), dependency);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.manifest.dependencies.remove(name).is_none() {
            whatever!("dependency `{name}` is not in the manifest");
        }
        self.dependencies().remove(name);
        Ok(())
    }

    fn dependencies(&mut self) -> &mut dyn TableLike {
        self.document
            .entry("dependencies")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .expect("validated dependencies table")
    }

    pub fn text(&self) -> String {
        self.document.to_string()
    }
}

fn update_dependency_entry(existing: &mut dyn TableLike, replacement: &InlineTable) {
    for key in ["git", "path", "branch", "tag", "rev", "namespace"] {
        if let Some(value) = replacement.get(key) {
            let mut value = value.clone();
            if let Some(previous) = existing.get(key).and_then(Item::as_value) {
                *value.decor_mut() = previous.decor().clone();
            }
            existing.insert(key, Item::Value(value));
        } else {
            existing.remove(key);
        }
    }
}

pub fn discover(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(cwd.join(path));
    }
    for directory in cwd.ancestors() {
        let path = directory.join("typm.toml");
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_whatever_context(|_| format!("cannot inspect {}", path.display()));
            }
        }
    }
    Ok(cwd.join("typm.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> ManifestDocument {
        ManifestDocument::parse(Path::new("typm.toml"), text).unwrap()
    }

    fn local_dependency(path: &str) -> Dependency {
        Dependency {
            git: None,
            path: Some(PathBuf::from(path)),
            branch: None,
            tag: None,
            rev: None,
            namespace: "typm".to_owned(),
        }
    }

    #[test]
    fn inserting_and_removing_dependencies_preserves_manifest_comments() {
        let original = "# keep this comment\n[dependencies] # keep this too\n";
        let mut document = document(original);
        let dependency = Dependency {
            namespace: "company".to_owned(),
            ..local_dependency("../custom")
        };

        document.insert("custom", dependency.clone()).unwrap();

        let updated = document.text();
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("# keep this too"));
        let manifest: Manifest = toml::from_str(&updated).unwrap();
        assert_eq!(manifest.dependencies["custom"], dependency);
        assert_eq!(document.manifest.dependencies["custom"], dependency);

        document.remove("custom").unwrap();

        assert_eq!(document.text(), original);
        assert!(document.manifest.dependencies.is_empty());
    }

    #[test]
    fn readding_a_dependency_preserves_its_trailing_comment() {
        let mut document = document(
            "[dependencies]\nlocal = { path = \"../local/local\" } # keep the dependency rationale\n",
        );
        let dependency = local_dependency("../updated-local");

        document.insert("local", dependency.clone()).unwrap();

        let updated = document.text();
        assert!(updated.contains("# keep the dependency rationale"));
        let manifest: Manifest = toml::from_str(&updated).unwrap();
        assert_eq!(manifest.dependencies["local"], dependency);
        assert_eq!(document.manifest.dependencies["local"], dependency);
    }
}
