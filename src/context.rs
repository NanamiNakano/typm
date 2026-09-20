use crate::Result;
use crate::files::{read_optional, require_regular_file_if_present};
use crate::shell::Shell;
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt, whatever};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub struct Context {
    pub cwd: PathBuf,
    pub home: PathBuf,
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

#[derive(Default)]
struct Environment {
    home: Option<OsString>,
    cache_root: Option<OsString>,
    git_fetch_with_cli: Option<OsString>,
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
        let environment = Environment {
            home: std::env::var_os("TYPM_HOME"),
            cache_root: std::env::var_os("TYPM_CACHE_DIR"),
            git_fetch_with_cli: std::env::var_os("TYPM_NET_GIT_FETCH_WITH_CLI"),
        };
        let default_home = BaseDirs::new().map(|base| base.home_dir().join(".typm"));
        let legacy_cache =
            ProjectDirs::from("", "", "typm").map(|project| project.cache_dir().to_owned());
        Self::from_environment(cwd, environment, default_home, legacy_cache, quiet)
    }

    fn from_environment(
        cwd: PathBuf,
        environment: Environment,
        default_home: Option<PathBuf>,
        legacy_cache: Option<PathBuf>,
        quiet: bool,
    ) -> Result<Self> {
        let explicit_home = environment.home.is_some();
        let home = match environment.home {
            Some(path) => environment_path("TYPM_HOME", &path, &cwd)?,
            None => {
                default_home.whatever_context("could not find a home directory; set TYPM_HOME")?
            }
        };
        let cache_root = match environment.cache_root {
            Some(path) => environment_path("TYPM_CACHE_DIR", &path, &cwd)?,
            None if explicit_home => home.clone(),
            None => legacy_cache
                .filter(|path| path.join("git/db").is_dir())
                .unwrap_or_else(|| home.clone()),
        };
        let mut context = Self {
            cwd,
            home,
            cache_root,
            config: Config::default(),
            shell: Shell::new(quiet),
        };
        context.config = Config::load(
            &context.home,
            &context.cwd,
            environment.git_fetch_with_cli.as_deref(),
        )?;
        Ok(context)
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        directory: tempfile::TempDir,
        cwd: PathBuf,
        default_home: PathBuf,
        legacy_cache: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let cwd = directory.path().join("workspace/nested");
            let default_home = directory.path().join("user/.typm");
            let legacy_cache = directory.path().join("legacy-cache");
            fs::create_dir_all(&cwd).unwrap();
            Self {
                directory,
                cwd,
                default_home,
                legacy_cache,
            }
        }

        fn context(&self, environment: Environment) -> Result<Context> {
            Context::from_environment(
                self.cwd.clone(),
                environment,
                Some(self.default_home.clone()),
                Some(self.legacy_cache.clone()),
                true,
            )
        }

        fn write_config(&self, path: &Path, contents: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn initialization_does_not_create_home_or_cache() {
        let fixture = Fixture::new();
        let context = fixture.context(Environment::default()).unwrap();
        assert_eq!(context.home, fixture.default_home);
        assert_eq!(context.cache_root, fixture.default_home);
        assert!(!context.home.exists());
        assert!(!context.config.net.git_fetch_with_cli);
    }

    #[test]
    fn explicit_paths_are_relative_to_invocation_and_override_legacy_cache() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.legacy_cache.join("git/db")).unwrap();
        let context = fixture
            .context(Environment {
                home: Some("typm-home".into()),
                ..Environment::default()
            })
            .unwrap();
        assert_eq!(context.home, fixture.cwd.join("typm-home"));
        assert_eq!(context.cache_root, context.home);
        let context = fixture
            .context(Environment {
                home: Some("typm-home".into()),
                cache_root: Some("custom-cache".into()),
                ..Environment::default()
            })
            .unwrap();
        assert_eq!(context.cache_root, fixture.cwd.join("custom-cache"));
        assert!(!context.cache_root.exists());
    }

    #[test]
    fn explicit_cache_keeps_default_home_configuration() {
        let fixture = Fixture::new();
        fixture.write_config(
            &fixture.default_home.join("config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        );
        let context = fixture
            .context(Environment {
                cache_root: Some("custom-cache".into()),
                ..Environment::default()
            })
            .unwrap();
        assert_eq!(context.home, fixture.default_home);
        assert_eq!(context.cache_root, fixture.cwd.join("custom-cache"));
        assert!(context.config.net.git_fetch_with_cli);
        assert!(!context.cache_root.exists());
    }

    #[test]
    fn reuses_only_legacy_caches_with_git_storage() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.legacy_cache).unwrap();
        let context = fixture.context(Environment::default()).unwrap();
        assert_eq!(context.cache_root, fixture.default_home);
        fs::create_dir_all(fixture.legacy_cache.join("git/db")).unwrap();
        let context = fixture.context(Environment::default()).unwrap();
        assert_eq!(context.cache_root, fixture.legacy_cache);
    }

    #[test]
    fn rejects_empty_home_and_cache_overrides() {
        let fixture = Fixture::new();
        for environment in [
            Environment {
                home: Some(OsString::new()),
                ..Environment::default()
            },
            Environment {
                cache_root: Some(OsString::new()),
                ..Environment::default()
            },
        ] {
            assert!(fixture.context(environment).is_err());
        }
    }

    #[test]
    fn nearer_configuration_and_environment_override_global_values() {
        let fixture = Fixture::new();
        fixture.write_config(
            &fixture.default_home.join("config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        );
        fixture.write_config(
            &fixture.directory.path().join("workspace/.typm/config.toml"),
            "[net]\ngit-fetch-with-cli = false\n",
        );
        assert!(
            !fixture
                .context(Environment::default())
                .unwrap()
                .config
                .net
                .git_fetch_with_cli
        );
        fixture.write_config(
            &fixture.cwd.join(".typm/config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        );
        assert!(
            fixture
                .context(Environment::default())
                .unwrap()
                .config
                .net
                .git_fetch_with_cli
        );
        let context = fixture
            .context(Environment {
                git_fetch_with_cli: Some("false".into()),
                ..Environment::default()
            })
            .unwrap();
        assert!(!context.config.net.git_fetch_with_cli);
    }

    #[test]
    fn unspecified_settings_do_not_reset_inherited_values() {
        let fixture = Fixture::new();
        fixture.write_config(
            &fixture.default_home.join("config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        );
        fixture.write_config(
            &fixture.cwd.join(".typm/config.toml"),
            "[net]\nfuture-option = 5\n[future]\noption = 'value'\n",
        );
        assert!(
            fixture
                .context(Environment::default())
                .unwrap()
                .config
                .net
                .git_fetch_with_cli
        );
    }

    #[test]
    fn invalid_configuration_reports_its_path() {
        let fixture = Fixture::new();
        let path = fixture.cwd.join(".typm/config.toml");
        fixture.write_config(&path, "[net]\ngit-fetch-with-cli = 'yes'\n");
        let error = fixture.context(Environment::default()).err().unwrap();
        assert!(error.to_string().contains(&path.display().to_string()));
        for value in ["yes", "1", "", "TRUE"] {
            assert!(
                Config::load(
                    &fixture.directory.path().join("absent-home"),
                    fixture.directory.path(),
                    Some(OsStr::new(value)),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn global_configuration_is_not_reapplied_as_an_ancestor() {
        let fixture = Fixture::new();
        let global = fixture.directory.path().join("workspace/.typm");
        fixture.write_config(
            &global.join("config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        );
        fixture.write_config(
            &fixture.directory.path().join(".typm/config.toml"),
            "[net]\ngit-fetch-with-cli = false\n",
        );
        let config = Config::load(&global, &fixture.cwd, None).unwrap();
        assert!(!config.net.git_fetch_with_cli);
    }

    #[test]
    fn cache_lock_is_created_lazily_and_released_on_drop() {
        let fixture = Fixture::new();
        let context = fixture.context(Environment::default()).unwrap();
        assert!(!context.cache_root.exists());
        let guard = context.acquire_cache_lock().unwrap();
        let competing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(context.cache_root.join(".package-cache"))
            .unwrap();
        assert!(matches!(
            competing.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(guard);
        competing.try_lock().unwrap();
    }
}
