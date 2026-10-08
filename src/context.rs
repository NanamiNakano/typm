use crate::Result;
use crate::files::{read_optional, require_regular_file_if_present};
use crate::shell::Shell;
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt, whatever};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub struct Context {
    pub cwd: PathBuf,
    pub cache_root: PathBuf,
    pub config: Config,
    pub shell: Shell,
}

#[derive(Debug, Default)]
pub struct Config {
    pub net: NetConfig,
}

#[derive(Debug, Default)]
pub struct NetConfig {
    pub git_fetch_with_cli: bool,
}

#[derive(Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    net: NetworkSettings,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct NetworkSettings {
    git_fetch_with_cli: Option<bool>,
}

impl Context {
    pub fn new(quiet: bool) -> Result<Self> {
        let cwd = std::env::current_dir().whatever_context("cannot determine current directory")?;
        let home = std::env::var_os("TYPM_HOME");
        let cache_root = std::env::var_os("TYPM_CACHE_DIR");
        let git_fetch_with_cli = std::env::var_os("TYPM_NET_GIT_FETCH_WITH_CLI");
        let default_home = BaseDirs::new().map(|base| base.home_dir().join(".typm"));
        let legacy_cache =
            ProjectDirs::from("", "", "typm").map(|project| project.cache_dir().to_owned());
        let explicit_home = home.is_some();
        let home = match home {
            Some(path) => environment_path("TYPM_HOME", &path, &cwd)?,
            None => {
                default_home.whatever_context("could not find a home directory; set TYPM_HOME")?
            }
        };
        let cache_root = match cache_root {
            Some(path) => environment_path("TYPM_CACHE_DIR", &path, &cwd)?,
            None if explicit_home => home.clone(),
            None => legacy_cache
                .filter(|path| path.join("git/db").is_dir())
                .unwrap_or_else(|| home.clone()),
        };
        let config = Config::load(&home, &cwd, git_fetch_with_cli.as_deref())?;
        Ok(Self {
            cwd,
            cache_root,
            config,
            shell: Shell::new(quiet),
        })
    }

    /// Keep the returned file alive while reading or updating shared Git storage.
    pub fn acquire_cache_lock(&self) -> Result<File> {
        fs::create_dir_all(&self.cache_root).with_whatever_context(|_| {
            format!(
                "could not create cache directory {}",
                self.cache_root.display()
            )
        })?;
        let path = self.cache_root.join(".package-cache");
        require_regular_file_if_present(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_whatever_context(|_| format!("could not open cache lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                self.shell
                    .status("Blocking", "waiting for the package cache lock");
                file.lock().with_whatever_context(|_| {
                    format!("could not lock package cache {}", self.cache_root.display())
                })?;
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).with_whatever_context(|_| {
                    format!("could not lock package cache {}", self.cache_root.display())
                });
            }
        }
        Ok(file)
    }
}

impl Config {
    fn load(home: &Path, cwd: &Path, environment_override: Option<&OsStr>) -> Result<Self> {
        let mut config = Self::default();
        let mut loaded = BTreeSet::new();
        for path in config_paths(home, cwd) {
            let Some(contents) = read_optional(&path)? else {
                continue;
            };
            let identity = path.canonicalize().with_whatever_context(|_| {
                format!("cannot resolve configuration file {}", path.display())
            })?;
            if !loaded.insert(identity) {
                continue;
            }
            let settings: ConfigFile = toml::from_str(&contents)
                .with_whatever_context(|_| format!("invalid configuration {}", path.display()))?;
            if let Some(value) = settings.net.git_fetch_with_cli {
                config.net.git_fetch_with_cli = value;
            }
        }
        if let Some(value) = environment_override {
            config.net.git_fetch_with_cli = match value.to_str() {
                Some("true") => true,
                Some("false") => false,
                _ => whatever!("TYPM_NET_GIT_FETCH_WITH_CLI must be `true` or `false`"),
            };
        }
        Ok(config)
    }
}

fn config_paths(home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![home.join("config.toml")];
    let ancestors: Vec<_> = cwd.ancestors().collect();
    paths.extend(
        ancestors
            .into_iter()
            .rev()
            .map(|directory| directory.join(".typm/config.toml")),
    );
    paths
}

fn environment_path(name: &str, value: &OsStr, cwd: &Path) -> Result<PathBuf> {
    if value.is_empty() {
        whatever!("{name} must not be empty");
    }
    Ok(cwd.join(value))
}
