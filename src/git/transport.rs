use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use git2::{
    AutotagOption, Config, Cred, CredentialHelper, CredentialType, FetchOptions, ProxyOptions,
    RemoteCallbacks, Repository,
};
use snafu::{ResultExt, whatever};

use crate::Result;
use crate::shell::Shell;

pub(super) struct Transport {
    pub shell: Shell,
    fetch_with_cli: bool,
}

impl Transport {
    pub fn new(shell: Shell, fetch_with_cli: bool) -> Self {
        Self {
            shell,
            fetch_with_cli,
        }
    }

    pub fn validate_url(&self, url: &str, allow_local: bool) -> Result<()> {
        if !allow_local && is_local_url(url) {
            whatever!("refusing local Git submodule URL `{url}` in a network repository");
        }
        Ok(())
    }

    pub fn fetch(
        &self,
        repository: &Repository,
        url: &str,
        refspecs: &[&str],
        allow_local: bool,
    ) -> Result<()> {
        self.validate_url(url, allow_local)?;
        if self.fetch_with_cli {
            self.fetch_cli(repository, url, refspecs)
        } else {
            self.fetch_library(repository, url, refspecs, allow_local)
        }
    }

    fn fetch_library(
        &self,
        repository: &Repository,
        url: &str,
        refspecs: &[&str],
        allow_local: bool,
    ) -> Result<()> {
        let config = repository
            .config()
            .whatever_context("could not load Git configuration")?;
        let mut authentication = Authentication::new(config);
        let progress = self.shell.progress("Downloading");
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |url, username, allowed| {
            authentication.credentials(url, username, allowed)
        });
        callbacks.transfer_progress(|statistics| {
            progress.update(&statistics);
            true
        });
        let mut options = FetchOptions::new();
        options
            .remote_callbacks(callbacks)
            .download_tags(AutotagOption::None);
        let mut proxy = ProxyOptions::new();
        proxy.auto();
        options.proxy_options(proxy);
        let mut remote = repository
            .remote_anonymous(url)
            .whatever_context("could not configure Git fetch")?;
        self.validate_url(
            remote
                .url()
                .whatever_context("Git remote URL is not valid UTF-8")?,
            allow_local,
        )?;
        let result = remote.fetch(refspecs, Some(&mut options), Some("typm: fetch"));
        drop(options);
        progress.finish();
        result.with_whatever_context(|error| {
            let message = error.message().to_ascii_lowercase();
            if message.contains("sha256") || message.contains("sha-256") || message.contains("objectformat") || message.contains("object format") {
                "SHA-256 Git repositories are not supported by typm's git2 backend".to_owned()
            } else {
                format!("Git fetch failed for {url}; for system Git authentication, set net.git-fetch-with-cli = true")
            }
        })
    }

    fn fetch_cli(&self, repository: &Repository, url: &str, refspecs: &[&str]) -> Result<()> {
        let mut command = git_command();
        command.arg("--git-dir").arg(repository.path()).args([
            "-c",
            "gc.auto=0",
            "fetch",
            "--force",
            "--no-tags",
            "--no-recurse-submodules",
        ]);
        if self.shell.is_quiet() {
            command.arg("--quiet");
        } else if self.shell.is_terminal() {
            command.arg("--progress");
        }
        command.arg("--").arg(url).args(refspecs);
        command.stdin(Stdio::inherit()).stdout(Stdio::piped());
        if !self.shell.is_quiet() && self.shell.is_terminal() {
            command.stderr(Stdio::inherit());
        } else {
            command.stderr(Stdio::piped());
        }
        let output = command
            .output()
            .whatever_context("could not run Git; install git or disable net.git-fetch-with-cli")?;
        if !output.status.success() {
            whatever!(
                "Git fetch failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

struct Authentication {
    config: Config,
    attempts: usize,
    attempted: BTreeSet<(String, &'static str, String)>,
}

impl Authentication {
    fn new(config: Config) -> Self {
        Self {
            config,
            attempts: 0,
            attempted: BTreeSet::new(),
        }
    }

    fn credentials(
        &mut self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
    ) -> std::result::Result<Cred, git2::Error> {
        self.attempts += 1;
        if self.attempts > 8 {
            return Err(authentication_failed());
        }
        let mut helper = CredentialHelper::new(url);
        helper.config(&self.config);
        if username.is_some() {
            helper.username(username);
        }
        let username = username.or(helper.username.as_deref()).unwrap_or("git");
        if allowed.contains(CredentialType::USERNAME) && self.claim(url, "username", username) {
            return Cred::username(username);
        }
        if allowed.contains(CredentialType::SSH_KEY)
            && self.claim(url, "agent", username)
            && let Ok(credential) = Cred::ssh_key_from_agent(username)
        {
            return Ok(credential);
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT)
            && self.claim(url, "helper", username)
            && let Some((username, password)) = helper.execute()
        {
            return Cred::userpass_plaintext(&username, &password);
        }
        if allowed.contains(CredentialType::DEFAULT) && self.claim(url, "default", username) {
            return Cred::default();
        }
        Err(authentication_failed())
    }

    fn claim(&mut self, url: &str, kind: &'static str, username: &str) -> bool {
        self.attempted
            .insert((url.to_owned(), kind, username.to_owned()))
    }
}

fn authentication_failed() -> git2::Error {
    git2::Error::from_str(
        "Git authentication failed; configure credential.helper or an SSH agent, or set net.git-fetch-with-cli = true",
    )
}

pub(super) fn is_local_url(url: &str) -> bool {
    Path::new(url).is_absolute() || url.starts_with("file://") || !url.contains(':')
}

pub(super) fn git_command() -> Command {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_GRAFT_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(key);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_not_repeated_after_rejection() {
        let mut authentication = Authentication::new(Config::new().unwrap());
        let url = "ssh://git@example.invalid/repo.git";
        assert!(
            authentication
                .credentials(url, Some("git"), CredentialType::USERNAME)
                .is_ok()
        );
        assert!(
            authentication
                .credentials(url, Some("git"), CredentialType::USERNAME)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn credential_helpers_are_used_only_for_allowed_password_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config");
        std::fs::write(&path, "").unwrap();
        let mut config = Config::open(&path).unwrap();
        config
            .set_str(
                "credential.helper",
                "!f() { printf '%s\\n' 'username=typm-test' 'password=unused-test-password'; }; f",
            )
            .unwrap();
        let mut authentication = Authentication::new(config);
        let url = "https://example.invalid/repo.git";
        assert!(
            authentication
                .credentials(url, None, CredentialType::empty())
                .is_err()
        );
        assert!(
            authentication
                .credentials(url, None, CredentialType::USER_PASS_PLAINTEXT)
                .is_ok()
        );
        assert!(
            authentication
                .credentials(url, None, CredentialType::USER_PASS_PLAINTEXT)
                .is_err()
        );
    }

    #[test]
    fn network_repositories_cannot_fetch_local_submodules() {
        let transport = Transport::new(Shell::new(true), false);
        for url in ["file:///tmp/repo", "/tmp/repo", "../repo"] {
            assert!(transport.validate_url(url, false).is_err());
            assert!(transport.validate_url(url, true).is_ok());
        }
        for url in [
            "https://example.invalid/repo",
            "ssh://git@example.invalid/repo",
            "git@example.invalid:repo",
        ] {
            assert!(transport.validate_url(url, false).is_ok());
        }
    }
}
