#![cfg(unix)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    project: PathBuf,
    cache: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let project = temp.path().join("project");
        let cache = temp.path().join("cache");
        let bin = temp.path().join("bin");
        let log = temp.path().join("typst.log");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("typst");
        fs::write(
            &script,
            r#"#!/bin/sh
{
    printf 'cwd=%s\n' "$PWD"
    printf 'package=%s\n' "$TYPST_PACKAGE_PATH"
    printf 'cache=%s\n' "$TYPST_PACKAGE_CACHE_PATH"
    printf 'argc=%s\n' "$#"
    for arg; do printf 'arg=%s\n' "$arg"; done
} > "$TYPM_TEST_LOG"
printf 'fake-typst-output\n'
exit "${TYPM_TEST_EXIT:-0}"
"#,
        )
        .unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            temp,
            project,
            cache,
            bin,
            log,
        }
    }

    fn command(&self) -> Command {
        let mut paths = vec![self.bin.clone()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_typm"));
        command
            .current_dir(&self.project)
            .env("TYPM_CACHE_DIR", &self.cache)
            .env("TYPM_HOME", self.temp.path().join("home"))
            .env_remove("TYPM_NET_GIT_FETCH_WITH_CLI")
            .env("TYPM_TEST_LOG", &self.log)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("TYPST_PACKAGE_PATH")
            .env_remove("TYPST_PACKAGE_CACHE_PATH");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        success(self.run(args))
    }

    fn link(&self, name: &str, version: &str) -> PathBuf {
        self.project
            .join(".typm/packages/local")
            .join(name)
            .join(version)
    }

    fn lock(&self) -> String {
        let contents = fs::read_to_string(self.project.join("typm.lock")).unwrap();
        toml::from_str::<toml::Value>(&contents).unwrap();
        contents
    }

    fn assert_packages(&self, linked: &[(&str, &str)], locked: &[(&str, &str)]) {
        let paths = |packages: &[(&str, &str)]| {
            packages
                .iter()
                .map(|(name, version)| format!("local/{name}/{version}"))
                .collect::<BTreeSet<_>>()
        };
        let state: toml::Value =
            toml::from_str(&fs::read_to_string(self.project.join(".typm/state.toml")).unwrap())
                .unwrap();
        let state_paths = state["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| {
                let path = link["path"].as_str().unwrap();
                let installed = self.project.join(".typm/packages").join(path);
                assert!(
                    fs::symlink_metadata(&installed)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    fs::read_link(&installed).unwrap(),
                    Path::new(link["target"].as_str().unwrap())
                );
                assert!(installed.join("lib.typ").is_file());
                path.to_owned()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(state_paths, paths(linked));
        let lock: toml::Value = toml::from_str(&self.lock()).unwrap();
        let lock_paths = lock["packages"]
            .as_table()
            .unwrap()
            .values()
            .map(|package| {
                format!(
                    "{}/{}/{}",
                    package["namespace"].as_str().unwrap(),
                    package["name"].as_str().unwrap(),
                    package["version"].as_str().unwrap()
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(lock_paths, paths(locked));
    }

    fn local(&self, name: &str) -> PathBuf {
        let path = self.temp.path().join("local").join(name);
        package(&path, name, "1.0.0");
        path
    }

    fn repo(&self, name: &str) -> Repository {
        Repository::new(self.temp.path().join("repositories").join(name))
    }
}

struct Repository {
    path: PathBuf,
}

impl Repository {
    fn new(path: PathBuf) -> Self {
        fs::create_dir_all(&path).unwrap();
        let repo = Self { path };
        repo.git(&["init", "--initial-branch=main"]);
        repo.git(&["config", "user.name", "typm test"]);
        repo.git(&["config", "user.email", "typm-test@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        String::from_utf8(success(output).stdout)
            .unwrap()
            .trim()
            .to_owned()
    }

    fn commit(&self) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--message", "test fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn url(&self) -> String {
        format!("file://{}", self.path.display())
    }
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "command failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn failure(output: Output) -> String {
    assert!(output.status.code().is_some_and(|code| code != 0));
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn package(path: &Path, name: &str, version: &str) {
    fs::create_dir_all(path).unwrap();
    fs::write(
        path.join("typst.toml"),
        format!("[package]\nname = {name:?}\nversion = {version:?}\nentrypoint = \"lib.typ\"\n"),
    )
    .unwrap();
    fs::write(path.join("lib.typ"), format!("#let value = {name:?}\n")).unwrap();
}

fn dependencies(path: &Path, entries: &str) {
    fs::write(
        path.join("typm.toml"),
        format!("[dependencies]\n{entries}\n"),
    )
    .unwrap();
}

fn git_dependency(name: &str, repo: &Repository) -> String {
    format!("{name} = {{ git = {:?}, branch = \"main\" }}", repo.url())
}

#[test]
fn sync_downloads_and_links_recursive_git_and_local_dependencies_without_typst() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    let leaf_commit = leaf.commit();
    let front = f.repo("front");
    package(&front.path, "front", "1.0.0");
    dependencies(&front.path, &git_dependency("leaf", &leaf));
    let front_commit = front.commit();
    let helpers = f.local("helpers");
    let local_leaf = f.local("local-leaf");
    dependencies(
        &helpers,
        &format!(
            "{}\nlocal-leaf = {{ path = \"../local-leaf\" }}",
            git_dependency("leaf", &leaf)
        ),
    );
    let manifest = format!(
        "# Preserve project formatting.\n[dependencies]\n{} # Git root\nhelpers={{path='../local/helpers'}}\n",
        git_dependency("front", &front)
    );
    fs::write(f.project.join("typm.toml"), &manifest).unwrap();

    let output = f.ok(&["sync"]);
    assert!(output.stdout.is_empty());
    assert!(!f.log.exists(), "sync must not invoke Typst");
    assert!(f.project.join(".typm/.gitignore").is_file());
    for (name, target) in [("helpers", helpers), ("local-leaf", local_leaf)] {
        assert_eq!(fs::canonicalize(f.link(name, "1.0.0")).unwrap(), target);
    }
    for name in ["front", "leaf"] {
        let link = f.link(name, "1.0.0");
        assert!(link.join("lib.typ").is_file());
        assert!(fs::canonicalize(link).unwrap().starts_with(&f.cache));
    }
    let lock = f.lock();
    assert!(lock.contains(&front_commit));
    assert!(lock.contains(&leaf_commit));
    let state = fs::read(f.project.join(".typm/state.toml")).unwrap();
    let output = f.ok(&["--quiet", "sync"]);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(f.lock(), lock);
    assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
    assert_eq!(
        fs::read_to_string(f.project.join("typm.toml")).unwrap(),
        manifest
    );
    assert!(!f.log.exists());
}

#[test]
fn sync_ignores_project_metadata_and_preserves_custom_gitignore() {
    let f = Fixture::new();
    let project_repo = Repository::new(f.project.clone());
    let dependency = f.repo("dependency");
    package(&dependency.path, "dependency", "1.0.0");
    dependency.commit();
    dependencies(&f.project, &git_dependency("dependency", &dependency));
    let metadata = f.project.join(".typm");
    fs::create_dir(&metadata).unwrap();
    fs::write(metadata.join("config.toml"), "# Project configuration\n").unwrap();
    fs::write(metadata.join("notes.txt"), "local metadata\n").unwrap();

    f.ok(&["sync"]);

    let ignored = [
        ".typm/.gitignore",
        ".typm/config.toml",
        ".typm/notes.txt",
        ".typm/packages/local/dependency/1.0.0",
        ".typm/state.toml",
        ".typm/project.lock",
    ];
    let mut args = vec!["check-ignore", "--"];
    args.extend(ignored);
    assert_eq!(project_repo.git(&args), ignored.join("\n"));
    assert_eq!(
        project_repo.git(&["ls-files", "--others", "--exclude-standard"]),
        "typm.lock\ntypm.toml"
    );

    let custom_ignore = "# Preserve project-specific rules.\n/packages/\n";
    fs::write(metadata.join(".gitignore"), custom_ignore).unwrap();
    f.ok(&["sync"]);
    assert_eq!(
        fs::read_to_string(metadata.join(".gitignore")).unwrap(),
        custom_ignore
    );
}

#[test]
fn sync_keeps_pins_and_restores_missing_checkouts_from_cached_and_remote_objects() {
    for transport in ["false", "true"] {
        let f = Fixture::new();
        let repo = f.repo("locked");
        package(&repo.path, "locked", "1.0.0");
        let original = repo.commit();
        dependencies(&f.project, &git_dependency("locked", &repo));
        let sync = || {
            success(
                f.command()
                    .env("TYPM_NET_GIT_FETCH_WITH_CLI", transport)
                    .arg("sync")
                    .output()
                    .unwrap(),
            )
        };
        sync();
        let lock = f.lock();
        fs::write(repo.path.join("lib.typ"), "#let value = \"new upstream\"\n").unwrap();
        let updated = repo.commit();
        sync();
        assert_eq!(f.lock(), lock);
        assert!(lock.contains(&original));
        assert!(!lock.contains(&updated));

        let checkout = fs::canonicalize(f.link("locked", "1.0.0")).unwrap();
        fs::remove_dir_all(checkout).unwrap();
        fs::remove_file(f.link("locked", "1.0.0")).unwrap();
        let offline = repo.path.with_extension("offline");
        fs::rename(&repo.path, &offline).unwrap();
        sync();
        assert_eq!(f.lock(), lock);
        assert_eq!(
            fs::read_to_string(f.link("locked", "1.0.0").join("lib.typ")).unwrap(),
            "#let value = \"locked\"\n"
        );

        fs::rename(offline, &repo.path).unwrap();
        fs::remove_dir_all(&f.cache).unwrap();
        sync();
        assert_eq!(f.lock(), lock);
        assert_eq!(
            fs::read_to_string(f.link("locked", "1.0.0").join("lib.typ")).unwrap(),
            "#let value = \"locked\"\n"
        );
        assert!(!f.log.exists());
    }
}

#[test]
fn update_refreshes_the_full_graph_and_reconciles_versions_and_dependencies() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    let local_git = f.repo("local-git");
    let obsolete = f.repo("obsolete");
    let added = f.repo("added");
    let mut old_commits = Vec::new();
    for (name, repo) in [
        ("leaf", &leaf),
        ("local-git", &local_git),
        ("obsolete", &obsolete),
    ] {
        package(&repo.path, name, "1.0.0");
        old_commits.push(repo.commit());
    }
    package(&added.path, "added", "1.0.0");
    let added_commit = added.commit();
    let front = f.repo("front");
    package(&front.path, "front", "1.0.0");
    dependencies(
        &front.path,
        &format!(
            "{}\n{}",
            git_dependency("leaf", &leaf),
            git_dependency("obsolete", &obsolete)
        ),
    );
    old_commits.push(front.commit());
    let helpers = f.local("helpers");
    dependencies(
        &helpers,
        &format!(
            "{}\n{}",
            git_dependency("leaf", &leaf),
            git_dependency("local-git", &local_git)
        ),
    );
    let manifest = format!(
        "# Keep selectors and comments.\n[dependencies]\n{}\nhelpers={{path='../local/helpers'}}\n",
        git_dependency("front", &front)
    );
    fs::write(f.project.join("typm.toml"), &manifest).unwrap();
    f.ok(&["sync"]);
    let mut new_commits = vec![added_commit];
    for (name, repo) in [("leaf", &leaf), ("local-git", &local_git)] {
        package(&repo.path, name, "2.0.0");
        new_commits.push(repo.commit());
    }
    package(&front.path, "front", "2.0.0");
    dependencies(
        &front.path,
        &format!(
            "{}\n{}",
            git_dependency("leaf", &leaf),
            git_dependency("added", &added)
        ),
    );
    new_commits.push(front.commit());
    package(&helpers, "helpers", "2.0.0");

    let output = f.ok(&["update"]);
    assert!(output.stdout.is_empty());
    assert!(!f.log.exists(), "update must not invoke Typst");
    let lock = f.lock();
    for commit in new_commits {
        assert!(lock.contains(&commit));
    }
    for commit in old_commits {
        assert!(!lock.contains(&commit));
    }
    for name in ["front", "leaf", "local-git", "helpers"] {
        assert!(f.link(name, "2.0.0").join("lib.typ").is_file());
        assert!(fs::symlink_metadata(f.link(name, "1.0.0")).is_err());
    }
    assert_eq!(
        fs::canonicalize(f.link("helpers", "2.0.0")).unwrap(),
        helpers
    );
    assert!(f.link("added", "1.0.0").join("lib.typ").is_file());
    assert!(fs::symlink_metadata(f.link("obsolete", "1.0.0")).is_err());
    assert_eq!(
        fs::read_to_string(f.project.join("typm.toml")).unwrap(),
        manifest
    );
}

#[test]
fn update_respects_git_selectors_and_quiet_output_for_both_transports() {
    for transport in ["false", "true"] {
        let f = Fixture::new();
        let repo = f.repo("selectors");
        let names = ["branched", "default", "named", "tagged", "moved", "pinned"];
        for name in names {
            package(&repo.path.join(name), name, "1.0.0");
        }
        let original = repo.commit();
        repo.git(&["tag", "--annotate", "stable", "--message", "stable tag"]);
        repo.git(&["tag", "--annotate", "moving", "--message", "moving tag"]);
        let url = repo.url();
        dependencies(
            &f.project,
            &format!(
                "branched = {{ git = {url:?}, branch = \"main\" }}\ndefault = {{ git = {url:?} }}\nnamed = {{ git = {url:?}, rev = \"refs/heads/main\" }}\ntagged = {{ git = {url:?}, tag = \"stable\" }}\nmoved = {{ git = {url:?}, tag = \"moving\" }}\npinned = {{ git = {url:?}, rev = {original:?} }}"
            ),
        );
        let manifest = fs::read(f.project.join("typm.toml")).unwrap();
        let quiet = |command| {
            let output = success(
                f.command()
                    .env("TYPM_NET_GIT_FETCH_WITH_CLI", transport)
                    .args(["--quiet", command])
                    .output()
                    .unwrap(),
            );
            assert!(output.stdout.is_empty());
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        quiet("update");
        let initial_lock = f.lock();
        for name in names {
            package(&repo.path.join(name), name, "2.0.0");
        }
        let updated = repo.commit();
        repo.git(&[
            "tag",
            "--force",
            "--annotate",
            "moving",
            "--message",
            "moved tag",
        ]);

        quiet("sync");
        assert_eq!(f.lock(), initial_lock);
        for name in names {
            assert!(f.link(name, "1.0.0").join("lib.typ").is_file());
        }
        quiet("update");
        for name in ["branched", "default", "named", "moved"] {
            assert!(
                f.link(name, "2.0.0").join("lib.typ").is_file(),
                "{transport}: {name}"
            );
            assert!(fs::symlink_metadata(f.link(name, "1.0.0")).is_err());
        }
        for name in ["tagged", "pinned"] {
            assert!(f.link(name, "1.0.0").join("lib.typ").is_file());
            assert!(fs::symlink_metadata(f.link(name, "2.0.0")).is_err());
        }
        assert!(f.lock().contains(&original));
        assert!(f.lock().contains(&updated));
        assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
        assert!(!f.log.exists());
    }
}

#[test]
fn failed_sync_and_update_preserve_project_files_and_links() {
    let f = Fixture::new();
    let repo = f.repo("existing");
    package(&repo.path, "existing", "1.0.0");
    repo.commit();
    let valid_dependencies = git_dependency("existing", &repo);
    dependencies(&f.project, &valid_dependencies);
    f.ok(&["sync"]);
    let lock = f.lock();
    let state = fs::read(f.project.join(".typm/state.toml")).unwrap();
    let link = fs::read_link(f.link("existing", "1.0.0")).unwrap();
    package(&repo.path, "existing", "2.0.0");
    repo.commit();
    let missing = format!("file://{}/missing-repository", f.temp.path().display());
    dependencies(
        &f.project,
        &format!("{valid_dependencies}\nzmissing = {{ git = {missing:?} }}"),
    );
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();
    for command in ["sync", "update"] {
        failure(f.run(&[command]));
        assert_eq!(f.lock(), lock);
        assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
        assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
        assert_eq!(fs::read_link(f.link("existing", "1.0.0")).unwrap(), link);
        assert!(!f.link("existing", "2.0.0").exists());
    }

    dependencies(&f.project, &valid_dependencies);
    let collision = f.link("existing", "2.0.0");
    fs::create_dir_all(&collision).unwrap();
    fs::write(collision.join("keep.txt"), "user-owned data").unwrap();
    let error = failure(f.run(&["update"]));
    assert!(error.contains("unmanaged package path"));
    assert_eq!(f.lock(), lock);
    assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
    assert_eq!(fs::read_link(f.link("existing", "1.0.0")).unwrap(), link);
    assert_eq!(
        fs::read_to_string(collision.join("keep.txt")).unwrap(),
        "user-owned data"
    );

    let invalid_lock = "version = 999\n";
    fs::write(f.project.join("typm.lock"), invalid_lock).unwrap();
    for command in ["sync", "update"] {
        let error = failure(f.run(&[command]));
        assert!(error.contains("unsupported typm.lock format version"));
        assert_eq!(
            fs::read_to_string(f.project.join("typm.lock")).unwrap(),
            invalid_lock
        );
        assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
        assert_eq!(fs::read_link(f.link("existing", "1.0.0")).unwrap(), link);
    }
    assert!(!f.log.exists());
}

#[test]
fn sync_and_update_require_and_discover_manifests_without_running_typst() {
    for command in ["sync", "update"] {
        let f = Fixture::new();
        let error = failure(f.run(&[command]));
        assert!(error.contains("manifest does not exist"));
        assert!(!f.project.join("typm.toml").exists());
        assert!(!f.project.join("typm.lock").exists());
        assert!(!f.project.join(".typm/state.toml").exists());
        let error = failure(f.run(&["--manifest-path", "missing.toml", command]));
        assert!(error.contains("manifest does not exist"));

        let local = f.local("local");
        dependencies(&f.project, "local={path='../local/local'}");
        let nested = f.project.join("documents");
        fs::create_dir(&nested).unwrap();
        success(
            f.command()
                .current_dir(&nested)
                .arg(command)
                .output()
                .unwrap(),
        );
        assert_eq!(fs::canonicalize(f.link("local", "1.0.0")).unwrap(), local);
        assert!(!nested.join(".typm").exists());
        fs::remove_file(f.link("local", "1.0.0")).unwrap();
        let invocation = f.temp.path().join("invocation");
        fs::create_dir(&invocation).unwrap();
        success(
            f.command()
                .current_dir(&invocation)
                .arg("--manifest-path")
                .arg(f.project.join("typm.toml"))
                .arg(command)
                .output()
                .unwrap(),
        );
        assert_eq!(fs::canonicalize(f.link("local", "1.0.0")).unwrap(), local);
        assert!(!invocation.join(".typm").exists());

        let empty = "# An empty project is valid.\n[dependencies]\n";
        fs::write(f.project.join("typm.toml"), empty).unwrap();
        f.ok(&[command]);
        assert_eq!(
            fs::read_to_string(f.project.join("typm.toml")).unwrap(),
            empty
        );
        assert!(!f.project.join("typm.lock").exists());
        assert!(fs::symlink_metadata(f.link("local", "1.0.0")).is_err());
        assert!(failure(f.run(&[command, "unexpected"])).contains("unexpected argument"));
        assert!(!f.log.exists());
    }
}

#[test]
fn git_dependencies_are_recursive_and_locked() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    let leaf_commit = leaf.commit();
    let front = f.repo("front");
    package(&front.path, "front", "1.0.0");
    dependencies(&front.path, &git_dependency("leaf", &leaf));
    fs::write(
        front.path.join("typm.lock"),
        "ignored invalid child lockfile",
    )
    .unwrap();
    let front_commit = front.commit();

    let output = f.ok(&["add", "--git", &front.url(), "--branch", "main"]);
    assert!(output.stdout.is_empty(), "progress must use stderr");
    for name in ["front", "leaf"] {
        let link = f.link(name, "1.0.0");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(link.join("lib.typ").is_file());
        assert!(fs::canonicalize(link).unwrap().starts_with(&f.cache));
    }
    assert!(f.lock().contains(&front_commit));
    assert!(f.lock().contains(&leaf_commit));
    f.ok(&["compile", "main.typ"]);
}

#[test]
fn locked_commits_survive_remote_changes_and_missing_checkouts() {
    let f = Fixture::new();
    let repo = f.repo("locked");
    package(&repo.path, "locked", "1.0.0");
    let original = repo.commit();
    f.ok(&["add", "--git", &repo.url(), "--branch", "main"]);
    let original_lock = f.lock();
    fs::write(repo.path.join("lib.typ"), "#let value = \"changed\"\n").unwrap();
    let changed = repo.commit();
    f.ok(&["compile", "main.typ"]);
    assert_eq!(f.lock(), original_lock);
    assert!(f.lock().contains(&original));
    assert!(!f.lock().contains(&changed));

    let checkout = fs::canonicalize(f.link("locked", "1.0.0")).unwrap();
    assert!(checkout.starts_with(&f.cache));
    fs::remove_dir_all(checkout).unwrap();
    fs::remove_file(f.link("locked", "1.0.0")).unwrap();
    fs::rename(&repo.path, repo.path.with_extension("offline")).unwrap();
    f.ok(&["compile", "main.typ"]);
    assert_eq!(f.lock(), original_lock);
    assert_eq!(
        fs::read_to_string(f.link("locked", "1.0.0").join("lib.typ")).unwrap(),
        "#let value = \"locked\"\n"
    );
}

#[test]
fn projects_share_cached_commits_and_repeated_add_refreshes() {
    let f = Fixture::new();
    let repo = f.repo("shared");
    package(&repo.path, "shared", "1.0.0");
    let original = repo.commit();
    f.ok(&["add", "--git", &repo.url(), "--branch", "main"]);
    fs::write(repo.path.join("lib.typ"), "#let value = \"updated\"\n").unwrap();
    let updated = repo.commit();
    f.ok(&["add", "shared", "--git", &repo.url(), "--branch", "main"]);
    assert!(f.lock().contains(&updated));
    assert!(!f.lock().contains(&original));

    let other = f.temp.path().join("other-project");
    fs::create_dir(&other).unwrap();
    for file in ["typm.toml", "typm.lock"] {
        fs::copy(f.project.join(file), other.join(file)).unwrap();
    }
    fs::rename(&repo.path, repo.path.with_extension("offline")).unwrap();
    success(
        f.command()
            .current_dir(&other)
            .args(["compile", "main.typ"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::canonicalize(other.join(".typm/packages/local/shared/1.0.0")).unwrap(),
        fs::canonicalize(f.link("shared", "1.0.0")).unwrap()
    );
}

#[test]
fn refreshing_a_root_retains_unchanged_transitive_git_pins() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    let leaf_original = leaf.commit();
    let front = f.repo("front");
    package(&front.path, "front", "1.0.0");
    dependencies(&front.path, &git_dependency("leaf", &leaf));
    let front_original = front.commit();
    f.ok(&["add", "--git", &front.url(), "--branch", "main"]);

    fs::write(leaf.path.join("lib.typ"), "#let value = \"updated leaf\"\n").unwrap();
    let leaf_updated = leaf.commit();
    fs::write(
        front.path.join("lib.typ"),
        "#let value = \"updated front\"\n",
    )
    .unwrap();
    let front_updated = front.commit();
    f.ok(&["add", "--git", &front.url(), "--branch", "main"]);

    let lock = f.lock();
    assert!(lock.contains(&front_updated));
    assert!(lock.contains(&leaf_original));
    assert!(!lock.contains(&front_original));
    assert!(!lock.contains(&leaf_updated));
    assert_eq!(
        fs::read_to_string(f.link("leaf", "1.0.0").join("lib.typ")).unwrap(),
        "#let value = \"leaf\"\n"
    );
}

#[test]
fn annotated_tags_revisions_and_default_head_resolve() {
    let f = Fixture::new();
    let repo = f.repo("selected");
    package(&repo.path, "selected", "1.0.0");
    let first = repo.commit();
    repo.git(&["tag", "--annotate", "v1", "--message", "first version"]);
    package(&repo.path, "selected", "2.0.0");
    let second = repo.commit();

    f.ok(&["add", "--git", &repo.url(), "--tag", "v1"]);
    assert!(f.link("selected", "1.0.0").is_dir());
    assert!(f.lock().contains(&first));
    f.ok(&["add", "--git", &repo.url(), "--rev", &first[..10]]);
    assert!(f.lock().contains(&first));
    f.ok(&["add", "--git", &repo.url()]);
    assert!(f.lock().contains(&second));
    assert!(f.link("selected", "2.0.0").is_dir());
    assert!(!f.link("selected", "1.0.0").exists());
}

#[test]
fn monorepo_paths_share_the_pinned_checkout() {
    let f = Fixture::new();
    let repo = f.repo("monorepo");
    package(&repo.path.join("alpha"), "alpha", "1.0.0");
    package(&repo.path.join("beta"), "beta", "1.0.0");
    dependencies(&repo.path.join("alpha"), "beta = { path = \"../beta\" }");
    let commit = repo.commit();

    let error = failure(f.run(&["add", "--git", &repo.url()]));
    assert!(error.contains("alpha") && error.contains("beta"));
    f.ok(&["add", "alpha", "--git", &repo.url()]);
    assert!(f.lock().contains(&commit));
    let alpha = fs::canonicalize(f.link("alpha", "1.0.0")).unwrap();
    let beta = fs::canonicalize(f.link("beta", "1.0.0")).unwrap();
    assert_eq!(alpha.parent(), beta.parent());
}

#[test]
fn relative_git_submodules_are_available_to_transitive_paths() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    leaf.commit();
    let parent = f.repo("parent");
    package(&parent.path, "parent", "1.0.0");
    parent.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "../leaf",
        "vendor/leaf",
    ]);
    dependencies(&parent.path, "leaf = { path = \"vendor/leaf\" }");
    let commit = parent.commit();
    f.ok(&["add", "parent", "--git", &parent.url()]);
    assert!(f.link("leaf", "1.0.0").join("lib.typ").is_file());
    assert!(f.lock().contains(&commit));
}

#[test]
fn quiet_add_keeps_both_output_streams_clean() {
    let f = Fixture::new();
    let repo = f.repo("quiet");
    package(&repo.path, "quiet", "1.0.0");
    repo.commit();
    for transport in ["false", "true"] {
        let output = success(
            f.command()
                .env("TYPM_NET_GIT_FETCH_WITH_CLI", transport)
                .args(["--quiet", "add", "--git", &repo.url()])
                .output()
                .unwrap(),
        );
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn git_path_dependencies_cannot_escape_the_checkout() {
    for kind in ["absolute", "parent", "symlink"] {
        let f = Fixture::new();
        let outside = f.local("outside");
        let repo = f.repo("bad");
        package(&repo.path, "bad", "1.0.0");
        let path = match kind {
            "absolute" => outside.to_str().unwrap(),
            "parent" => "../outside",
            "symlink" => {
                symlink(&outside, repo.path.join("escaped")).unwrap();
                "escaped"
            }
            _ => unreachable!(),
        };
        dependencies(&repo.path, &format!("outside = {{ path = {path:?} }}"));
        repo.commit();
        let error = failure(f.run(&["add", "bad", "--git", &repo.url()]));
        assert!(error.contains("outside"), "{kind}: {error}");
        assert!(!f.project.join("typm.toml").exists(), "{kind}");
        assert!(!f.project.join("typm.lock").exists(), "{kind}");
    }
}

#[test]
fn local_dependencies_stay_live_and_can_depend_on_git() {
    let f = Fixture::new();
    let repo = f.repo("leaf");
    package(&repo.path, "leaf", "1.0.0");
    let commit = repo.commit();
    let alpha = f.local("alpha");
    let beta = f.local("beta");
    dependencies(&alpha, "beta = { path = \"../beta\" }");
    dependencies(&beta, &git_dependency("leaf", &repo));
    f.ok(&["add", "--path", alpha.to_str().unwrap()]);
    assert_eq!(fs::canonicalize(f.link("beta", "1.0.0")).unwrap(), beta);
    assert!(f.lock().contains(&commit));
    fs::write(beta.join("lib.typ"), "#let value = \"edited live\"\n").unwrap();
    assert_eq!(
        fs::read_to_string(f.link("beta", "1.0.0").join("lib.typ")).unwrap(),
        "#let value = \"edited live\"\n"
    );
    package(&beta, "beta", "2.0.0");
    f.ok(&["compile", "main.typ"]);
    assert!(!f.link("beta", "1.0.0").exists());
    assert_eq!(fs::canonicalize(f.link("beta", "2.0.0")).unwrap(), beta);
    assert!(f.lock().contains(&commit));
}

#[test]
fn edited_local_manifests_replace_their_transitive_graph() {
    let f = Fixture::new();
    let first = f.repo("first");
    package(&first.path, "first", "1.0.0");
    let first_commit = first.commit();
    let second = f.repo("second");
    package(&second.path, "second", "1.0.0");
    let second_commit = second.commit();
    let local = f.local("local");
    dependencies(&local, &git_dependency("first", &first));
    f.ok(&["add", "--path", local.to_str().unwrap()]);
    assert!(f.link("first", "1.0.0").is_dir());

    dependencies(&local, &git_dependency("second", &second));
    f.ok(&["compile", "main.typ"]);
    assert!(!f.link("first", "1.0.0").exists());
    assert!(f.link("second", "1.0.0").is_dir());
    assert!(!f.lock().contains(&first_commit));
    assert!(f.lock().contains(&second_commit));
    f.ok(&["remove", "local"]);
    assert!(!f.link("second", "1.0.0").exists());
    assert!(!f.lock().contains(&second_commit));
}

#[test]
fn removal_preserves_shared_transitives_and_does_not_fetch() {
    let f = Fixture::new();
    let shared = f.repo("shared");
    package(&shared.path, "shared", "1.0.0");
    shared.commit();
    let left = f.repo("left");
    let right = f.repo("right");
    for (name, repo) in [("left", &left), ("right", &right)] {
        package(&repo.path, name, "1.0.0");
        dependencies(&repo.path, &git_dependency("shared", &shared));
        repo.commit();
        f.ok(&["add", "--git", &repo.url(), "--branch", "main"]);
    }
    for repo in [&left, &right, &shared] {
        fs::rename(&repo.path, repo.path.with_extension("offline")).unwrap();
    }
    f.ok(&["remove", "left"]);
    assert!(!f.link("left", "1.0.0").exists());
    assert!(f.link("right", "1.0.0").is_dir());
    assert!(f.link("shared", "1.0.0").is_dir());
    f.ok(&["rm", "right"]);
    assert!(!f.link("right", "1.0.0").exists());
    assert!(!f.link("shared", "1.0.0").exists());
    assert!(f.cache.is_dir());
}

#[test]
fn removal_also_prunes_roots_manually_deleted_from_the_manifest() {
    let f = Fixture::new();
    let first = f.repo("first");
    package(&first.path, "first", "1.0.0");
    let first_commit = first.commit();
    let second = f.repo("second");
    package(&second.path, "second", "1.0.0");
    let second_commit = second.commit();
    for repo in [&first, &second] {
        f.ok(&["add", "--git", &repo.url(), "--branch", "main"]);
    }

    dependencies(&f.project, &git_dependency("first", &first));
    for repo in [&first, &second] {
        fs::rename(&repo.path, repo.path.with_extension("offline")).unwrap();
    }
    f.ok(&["remove", "first"]);
    for name in ["first", "second"] {
        assert!(fs::symlink_metadata(f.link(name, "1.0.0")).is_err());
    }
    let lock = f.lock();
    assert!(!lock.contains(&first_commit));
    assert!(!lock.contains(&second_commit));
}

#[test]
fn commands_reconcile_manual_manifest_edits() {
    for command in [
        "add", "remove", "rm", "sync", "update", "compile", "watch", "query", "fonts",
    ] {
        let f = Fixture::new();
        let selected = f.repo("selected");
        package(&selected.path, "selected", "1.0.0");
        let older = selected.commit();
        package(&selected.path, "selected", "2.0.0");
        let newer = selected.commit();
        let deleted = f.repo("deleted");
        package(&deleted.path, "deleted", "1.0.0");
        let deleted_commit = deleted.commit();
        let pinned = f.repo("pinned");
        package(&pinned.path, "pinned", "1.0.0");
        let pinned_commit = pinned.commit();
        let removed = f.local("removed");
        let original = f.local("retargeted");
        let replacement = f.temp.path().join("local/retargeted-replacement");
        package(&replacement, "retargeted", "1.0.0");
        let manual = f.local("manual");
        let selected_at = |commit: &str| {
            format!(
                "selected = {{ git = {:?}, rev = {commit:?} }}",
                selected.url()
            )
        };
        dependencies(
            &f.project,
            &format!(
                "{}\n{}\n{}\nremoved = {{ path = '../local/removed' }}\nretargeted = {{ path = '../local/retargeted' }}",
                selected_at(&newer),
                git_dependency("deleted", &deleted),
                git_dependency("pinned", &pinned),
            ),
        );
        f.ok(&["sync"]);
        assert_eq!(
            fs::canonicalize(f.link("retargeted", "1.0.0")).unwrap(),
            original
        );
        package(&pinned.path, "pinned", "2.0.0");
        let refreshed_commit = pinned.commit();
        let retained = format!(
            "# Keep this manual edit.\n[dependencies]\n{} # selected revision\n{}\nretargeted = {{ path = '../local/retargeted-replacement' }}\nmanual = {{ path = '../local/manual' }}\n",
            selected_at(&older),
            git_dependency("pinned", &pinned),
        );
        let mut edited = retained.clone();
        if matches!(command, "remove" | "rm") {
            edited.push_str("removed = { path = '../local/removed' }\n");
        }
        fs::write(f.project.join("typm.toml"), &edited).unwrap();
        fs::rename(&deleted.path, deleted.path.with_extension("offline")).unwrap();
        fs::remove_dir_all(removed).unwrap();

        match command {
            "add" => {
                f.ok(&["add", "--path", f.local("added").to_str().unwrap()]);
            }
            "remove" | "rm" => {
                f.ok(&[command, "removed"]);
            }
            "compile" | "watch" => {
                f.ok(&[command, "main.typ"]);
            }
            "query" => {
                f.ok(&[command, "main.typ", "<target>"]);
            }
            _ => {
                f.ok(&[command]);
            }
        }

        let pinned_version = if command == "update" {
            "2.0.0"
        } else {
            "1.0.0"
        };
        let locked = [("selected", "1.0.0"), ("pinned", pinned_version)];
        let mut linked = locked.to_vec();
        linked.extend([("retargeted", "1.0.0"), ("manual", "1.0.0")]);
        if command == "add" {
            linked.push(("added", "1.0.0"));
        }
        f.assert_packages(&linked, &locked);
        assert_eq!(
            fs::canonicalize(f.link("retargeted", "1.0.0")).unwrap(),
            replacement
        );
        assert_eq!(fs::canonicalize(f.link("manual", "1.0.0")).unwrap(), manual);
        for (name, version) in [
            ("selected", "2.0.0"),
            ("deleted", "1.0.0"),
            ("removed", "1.0.0"),
        ] {
            assert!(
                fs::symlink_metadata(f.link(name, version)).is_err(),
                "{command}: {name}/{version}"
            );
        }
        let lock = f.lock();
        assert!(lock.contains(&older));
        for commit in [&newer, &deleted_commit] {
            assert!(!lock.contains(commit));
        }
        assert_eq!(lock.contains(&pinned_commit), command != "update");
        assert_eq!(lock.contains(&refreshed_commit), command == "update");
        if command == "update" {
            assert!(fs::symlink_metadata(f.link("pinned", "1.0.0")).is_err());
        }
        let parsed: toml::Value = toml::from_str(&lock).unwrap();
        assert_eq!(parsed["sources"].as_table().unwrap().len(), 2);
        assert_eq!(
            parsed["roots"]
                .as_table()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["pinned", "selected"])
        );
        let manifest = fs::read_to_string(f.project.join("typm.toml")).unwrap();
        if command == "add" {
            assert!(manifest.starts_with(&retained));
        } else {
            assert_eq!(manifest, retained);
        }
        let forwarded = matches!(command, "compile" | "watch" | "query" | "fonts");
        assert_eq!(f.log.exists(), forwarded);
        if forwarded {
            assert!(
                fs::read_to_string(&f.log)
                    .unwrap()
                    .contains(&format!("arg={command}\n"))
            );
        }
    }
}

#[test]
fn removal_resolves_edited_local_versions_and_transitives_preserving_shared_versions() {
    let f = Fixture::new();
    let shared = f.repo("shared");
    package(&shared.path, "shared", "1.0.0");
    let older = shared.commit();
    package(&shared.path, "shared", "2.0.0");
    let newer = shared.commit();
    let obsolete = f.repo("obsolete");
    package(&obsolete.path, "obsolete", "1.0.0");
    let obsolete_commit = obsolete.commit();
    let added = f.repo("added");
    package(&added.path, "added", "1.0.0");
    let added_commit = added.commit();
    let local = f.local("local");
    let keeper = f.local("keeper");
    f.local("removed");
    let shared_at =
        |commit: &str| format!("shared = {{ git = {:?}, rev = {commit:?} }}", shared.url());
    dependencies(
        &local,
        &format!(
            "{}\n{}",
            shared_at(&older),
            git_dependency("obsolete", &obsolete)
        ),
    );
    dependencies(&keeper, &shared_at(&older));
    dependencies(
        &f.project,
        "local = { path = '../local/local' }\nkeeper = { path = '../local/keeper' }\nremoved = { path = '../local/removed' }",
    );
    f.ok(&["sync"]);

    package(&local, "local", "2.0.0");
    dependencies(
        &local,
        &format!("{}\n{}", shared_at(&newer), git_dependency("added", &added)),
    );
    f.ok(&["remove", "removed"]);

    let locked = [("shared", "1.0.0"), ("shared", "2.0.0"), ("added", "1.0.0")];
    let mut linked = locked.to_vec();
    linked.extend([("local", "2.0.0"), ("keeper", "1.0.0")]);
    f.assert_packages(&linked, &locked);
    for name in ["local", "removed", "obsolete"] {
        assert!(fs::symlink_metadata(f.link(name, "1.0.0")).is_err());
    }
    let lock = f.lock();
    for commit in [&older, &newer, &added_commit] {
        assert!(lock.contains(commit));
    }
    assert!(!lock.contains(&obsolete_commit));
    let parsed: toml::Value = toml::from_str(&lock).unwrap();
    assert_eq!(parsed["sources"].as_table().unwrap().len(), 3);
    assert_eq!(parsed["roots"]["keeper"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["roots"]["local"].as_array().unwrap().len(), 2);
    assert!(parsed["roots"].get("removed").is_none());
    let state: toml::Value =
        toml::from_str(&fs::read_to_string(f.project.join(".typm/state.toml")).unwrap()).unwrap();
    for link in state["links"].as_array().unwrap() {
        let path = link["path"].as_str().unwrap();
        let root = if path == "local/shared/1.0.0" || path == "local/keeper/1.0.0" {
            "keeper"
        } else {
            "local"
        };
        assert_eq!(
            link["roots"].as_array().unwrap(),
            &vec![toml::Value::String(root.into())]
        );
    }
}

#[test]
fn commands_preserve_project_and_skip_typst_when_manual_edits_fail_resolution() {
    let f = Fixture::new();
    let existing = f.repo("existing");
    package(&existing.path, "existing", "1.0.0");
    existing.commit();
    f.local("removed");
    let added = f.local("added");
    let valid = format!(
        "{}\nremoved = {{ path = '../local/removed' }}",
        git_dependency("existing", &existing)
    );
    dependencies(&f.project, &valid);
    f.ok(&["sync"]);
    let lock = fs::read(f.project.join("typm.lock")).unwrap();
    let state = fs::read(f.project.join(".typm/state.toml")).unwrap();
    let links =
        ["existing", "removed"].map(|name| (name, fs::read_link(f.link(name, "1.0.0")).unwrap()));
    dependencies(
        &f.project,
        &format!("{valid}\nzmissing = {{ path = '../missing' }}"),
    );
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();

    for command in [
        "add", "remove", "rm", "sync", "update", "compile", "watch", "query", "fonts",
    ] {
        let output = match command {
            "add" => f.run(&[command, "--path", added.to_str().unwrap()]),
            "remove" | "rm" => f.run(&[command, "removed"]),
            "compile" | "watch" => f.run(&[command, "main.typ"]),
            "query" => f.run(&[command, "main.typ", "<target>"]),
            _ => f.run(&[command]),
        };
        let error = failure(output);
        assert!(error.contains("zmissing"));
        assert!(error.contains("cannot locate path dependency"));
        assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
        assert_eq!(fs::read(f.project.join("typm.lock")).unwrap(), lock);
        assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
        for (name, target) in &links {
            assert_eq!(fs::read_link(f.link(name, "1.0.0")).unwrap(), *target);
        }
        f.assert_packages(
            &[("existing", "1.0.0"), ("removed", "1.0.0")],
            &[("existing", "1.0.0")],
        );
        assert!(fs::symlink_metadata(f.link("added", "1.0.0")).is_err());
        assert!(
            !f.log.exists(),
            "{command} must not invoke Typst after failed resolution"
        );
    }
}

#[test]
fn conflicting_sources_leave_the_existing_project_unchanged() {
    let f = Fixture::new();
    let first = f.repo("shared-first");
    let second = f.repo("shared-second");
    package(&first.path, "shared", "1.0.0");
    package(&second.path, "shared", "1.0.0");
    fs::write(
        second.path.join("lib.typ"),
        "#let value = \"second source\"\n",
    )
    .unwrap();
    first.commit();
    second.commit();
    let left = f.local("left");
    let right = f.local("right");
    dependencies(&left, &git_dependency("shared", &first));
    dependencies(&right, &git_dependency("shared", &second));
    f.ok(&["add", "--path", left.to_str().unwrap()]);
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();
    let lock = f.lock();
    let error = failure(f.run(&["add", "--path", right.to_str().unwrap()]));
    assert!(error.contains("shared") && error.contains("left") && error.contains("right"));
    assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
    assert_eq!(f.lock(), lock);
    assert!(!f.link("right", "1.0.0").exists());
}

#[test]
fn failed_download_preserves_manifest_lock_and_links() {
    let f = Fixture::new();
    let repo = f.repo("existing");
    package(&repo.path, "existing", "1.0.0");
    repo.commit();
    f.ok(&["add", "--git", &repo.url()]);
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();
    let lock = f.lock();
    let missing = format!("file://{}/missing-repository", f.temp.path().display());
    failure(f.run(&["add", "missing", "--git", &missing]));
    assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
    assert_eq!(f.lock(), lock);
    assert!(f.link("existing", "1.0.0").join("lib.typ").is_file());
    assert!(!f.link("missing", "1.0.0").exists());
}

#[test]
fn different_versions_can_coexist_and_local_cycles_are_reported() {
    let f = Fixture::new();
    let first = f.local("first");
    let second = f.local("second");
    let v1 = f.temp.path().join("version-one");
    let v2 = f.temp.path().join("version-two");
    package(&v1, "shared", "1.0.0");
    package(&v2, "shared", "2.0.0");
    dependencies(
        &first,
        &format!("shared = {{ path = {:?} }}", v1.to_str().unwrap()),
    );
    dependencies(
        &second,
        &format!("shared = {{ path = {:?} }}", v2.to_str().unwrap()),
    );
    f.ok(&["add", "--path", first.to_str().unwrap()]);
    f.ok(&["add", "--path", second.to_str().unwrap()]);
    assert!(f.link("shared", "1.0.0").is_dir());
    assert!(f.link("shared", "2.0.0").is_dir());

    dependencies(&first, "second = { path = \"../second\" }");
    dependencies(&second, "first = { path = \"../first\" }");
    let error = failure(f.run(&["compile", "main.typ"]));
    assert!(error.contains("cycle") && error.contains("first") && error.contains("second"));
    assert!(!f.log.exists(), "Typst must not run on resolution failure");
}

#[test]
fn custom_namespaces_are_installed_and_removed() {
    let f = Fixture::new();
    let local = f.local("custom");
    f.ok(&[
        "add",
        "--path",
        local.to_str().unwrap(),
        "--namespace",
        "company",
    ]);
    let installed = f.project.join(".typm/packages/company/custom/1.0.0");
    assert!(installed.join("lib.typ").is_file());
    f.ok(&["remove", "custom"]);
    assert_eq!(
        fs::symlink_metadata(&installed).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn a_dangling_user_lockfile_symlink_survives_failed_add() {
    let f = Fixture::new();
    let local = f.local("local");
    let lock = f.project.join("typm.lock");
    let missing_target = f.temp.path().join("missing-user-lock.toml");
    symlink(&missing_target, &lock).unwrap();
    failure(f.run(&["add", "--path", local.to_str().unwrap()]));
    assert!(
        fs::symlink_metadata(&lock)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&lock).unwrap(), missing_target);
    assert!(!f.project.join("typm.toml").exists());
    assert!(!missing_target.exists());
}

#[test]
fn wrapper_preserves_arguments_environment_working_directory_and_exit_status() {
    let f = Fixture::new();
    let local = f.local("local");
    f.ok(&["add", "--path", local.to_str().unwrap()]);
    let nested = f.project.join("documents");
    fs::create_dir(&nested).unwrap();
    let non_utf8 = OsString::from_vec(b"non-utf8-\xff.typ".to_vec());
    let output = f
        .command()
        .current_dir(&nested)
        .env("TYPM_TEST_EXIT", "23")
        .env("TYPST_PACKAGE_PATH", "/old/package/path")
        .env("TYPST_PACKAGE_CACHE_PATH", "/existing/preview/cache")
        .args([
            "compile",
            "input with spaces.typ",
            "--package-path",
            "/explicit/path",
        ])
        .arg(&non_utf8)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"fake-typst-output\n");
    let log = fs::read(&f.log).unwrap();
    let text = String::from_utf8_lossy(&log);
    assert!(text.contains(&format!("cwd={}\n", nested.display())));
    assert!(text.contains(&format!(
        "package={}\n",
        f.project.join(".typm/packages").display()
    )));
    assert!(text.contains("cache=/existing/preview/cache\n"));
    assert!(text.contains(
        "argc=5\narg=compile\narg=input with spaces.typ\narg=--package-path\narg=/explicit/path\n"
    ));
    assert!(log.ends_with(b"arg=non-utf8-\xff.typ\n"));
}

#[test]
fn explicit_manifest_path_and_empty_projects_work() {
    let f = Fixture::new();
    let local = f.local("local");
    f.ok(&["add", "--path", local.to_str().unwrap()]);
    let other = f.temp.path().join("empty-project");
    fs::create_dir(&other).unwrap();
    success(
        f.command()
            .current_dir(&other)
            .arg("--manifest-path")
            .arg(f.project.join("typm.toml"))
            .args(["compile", "main.typ"])
            .output()
            .unwrap(),
    );
    let log = fs::read_to_string(&f.log).unwrap();
    assert!(log.contains(&format!("cwd={}\n", other.display())));
    assert!(log.contains(&format!(
        "package={}\n",
        f.project.join(".typm/packages").display()
    )));
    success(
        f.command()
            .current_dir(&other)
            .args(["compile", "main.typ"])
            .output()
            .unwrap(),
    );
}

#[test]
fn invalid_dependencies_prevent_typst_from_running() {
    let f = Fixture::new();
    dependencies(&f.project, "missing = { path = \"does-not-exist\" }");
    let error = failure(f.run(&["compile", "main.typ"]));
    assert!(error.contains("missing"));
    assert!(!f.log.exists());
}

#[test]
fn unmanaged_installation_paths_are_not_overwritten() {
    let f = Fixture::new();
    let local = f.local("local");
    let collision = f.link("local", "1.0.0");
    fs::create_dir_all(&collision).unwrap();
    fs::write(collision.join("keep.txt"), "user-owned data").unwrap();
    failure(f.run(&["add", "--path", local.to_str().unwrap()]));
    assert_eq!(
        fs::read_to_string(collision.join("keep.txt")).unwrap(),
        "user-owned data"
    );
    assert!(!f.project.join("typm.toml").exists());
}

#[test]
fn concurrent_projects_share_a_cache_safely() {
    let f = Fixture::new();
    let repo = f.repo("concurrent");
    package(&repo.path, "concurrent", "1.0.0");
    let commit = repo.commit();
    dependencies(&f.project, &git_dependency("concurrent", &repo));
    let other = f.temp.path().join("other-project");
    fs::create_dir(&other).unwrap();
    fs::copy(f.project.join("typm.toml"), other.join("typm.toml")).unwrap();
    let first = f
        .command()
        .args(["compile", "main.typ"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = f
        .command()
        .current_dir(&other)
        .args(["compile", "main.typ"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    success(first.wait_with_output().unwrap());
    success(second.wait_with_output().unwrap());
    assert!(f.lock().contains(&commit));
    assert!(
        fs::read_to_string(other.join("typm.lock"))
            .unwrap()
            .contains(&commit)
    );
    assert_eq!(
        fs::canonicalize(f.link("concurrent", "1.0.0")).unwrap(),
        fs::canonicalize(other.join(".typm/packages/local/concurrent/1.0.0")).unwrap()
    );
}

#[test]
fn real_typst_compiles_transitive_packages_and_uses_its_preview_cache() {
    let typst_available = Command::new("typst")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !typst_available {
        eprintln!("skipping real compiler check: typst is not installed");
        return;
    }
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    leaf.commit();
    let front = f.repo("front");
    package(&front.path, "front", "1.0.0");
    dependencies(&front.path, &git_dependency("leaf", &leaf));
    fs::write(
        front.path.join("lib.typ"),
        "#import \"@local/leaf:1.0.0\": value\n#let text = value\n",
    )
    .unwrap();
    front.commit();
    let helpers = f.local("helpers");
    dependencies(&helpers, &git_dependency("leaf", &leaf));
    fs::write(
        helpers.join("lib.typ"),
        "#import \"@local/leaf:1.0.0\": value\n#let helper = value\n",
    )
    .unwrap();
    f.ok(&["add", "--git", &front.url(), "--branch", "main"]);
    f.ok(&["add", "--path", helpers.to_str().unwrap()]);

    let preview_cache = f.temp.path().join("typst-cache");
    let cached = preview_cache.join("preview/cached/1.0.0");
    package(&cached, "cached", "1.0.0");
    fs::write(cached.join("lib.typ"), "#let cached = \"cached preview\"\n").unwrap();
    fs::write(f.project.join("main.typ"), "#import \"@local/front:1.0.0\": text\n#import \"@local/helpers:1.0.0\": helper\n#import \"@preview/cached:1.0.0\": cached\n#text #helper #cached\n").unwrap();
    success(
        f.command()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TYPST_PACKAGE_CACHE_PATH", &preview_cache)
            .args(["compile", "main.typ", "output.pdf"])
            .output()
            .unwrap(),
    );
    assert!(
        fs::read(f.project.join("output.pdf"))
            .unwrap()
            .starts_with(b"%PDF-")
    );
    assert!(cached.join("lib.typ").is_file());
    assert!(!f.project.join(".typm/packages/preview").exists());
}

#[test]
fn git2_prepares_packages_without_a_git_executable() {
    let f = Fixture::new();
    let repo = f.repo("without-git");
    package(&repo.path, "without-git", "1.0.0");
    let commit = repo.commit();
    success(
        f.command()
            .env("PATH", &f.bin)
            .args(["add", "--git", &repo.url()])
            .output()
            .unwrap(),
    );
    assert!(f.lock().contains(&commit));
    success(
        f.command()
            .env("PATH", &f.bin)
            .args(["compile", "main.typ"])
            .output()
            .unwrap(),
    );
    assert!(f.link("without-git", "1.0.0").join("lib.typ").is_file());
}

#[test]
fn configuration_selects_cli_fetch_and_environment_overrides_project_settings() {
    let f = Fixture::new();
    let repo = f.repo("transport");
    package(&repo.path, "transport", "1.0.0");
    repo.commit();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let calls = f.temp.path().join("git-calls");
    let probe = f.bin.join("git");
    fs::write(
        &probe,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TYPM_TEST_GIT_LOG\"\nexec \"$TYPM_TEST_REAL_GIT\" \"$@\"\n",
    ).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    let home = f.temp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "[net]\ngit-fetch-with-cli = true\n",
    )
    .unwrap();
    let run = |setting: Option<&str>| {
        let mut command = f.command();
        command
            .env("TYPM_TEST_GIT_LOG", &calls)
            .env("TYPM_TEST_REAL_GIT", &real_git);
        if let Some(setting) = setting {
            command.env("TYPM_NET_GIT_FETCH_WITH_CLI", setting);
        }
        success(
            command
                .args(["add", "--git", &repo.url(), "--branch", "main"])
                .output()
                .unwrap(),
        )
    };
    run(None);
    assert!(fs::read_to_string(&calls).unwrap().contains("fetch"));
    fs::remove_file(&calls).unwrap();
    fs::write(
        f.project.join(".typm/config.toml"),
        "[net]\ngit-fetch-with-cli = false\n",
    )
    .unwrap();
    run(None);
    assert!(!calls.exists());
    run(Some("true"));
    assert!(fs::read_to_string(&calls).unwrap().contains("fetch"));
    let lock = f.lock();
    fs::remove_file(&calls).unwrap();
    run(Some("false"));
    assert!(!calls.exists());
    assert_eq!(f.lock(), lock);
}

#[test]
fn explicit_manifest_keeps_configuration_relative_to_invocation_directory() {
    let f = Fixture::new();
    let repo = f.repo("config-root");
    package(&repo.path, "config-root", "1.0.0");
    repo.commit();
    let invocation = f.temp.path().join("invocation");
    fs::create_dir_all(invocation.join(".typm")).unwrap();
    fs::write(
        invocation.join(".typm/config.toml"),
        "[net]\ngit-fetch-with-cli = true\n",
    )
    .unwrap();
    fs::create_dir_all(f.project.join(".typm")).unwrap();
    fs::write(
        f.project.join(".typm/config.toml"),
        "[net]\ngit-fetch-with-cli = false\n",
    )
    .unwrap();
    let manifest = f.project.join("typm.toml");
    let error = failure(
        f.command()
            .current_dir(&invocation)
            .env("PATH", &f.bin)
            .args([
                "--manifest-path",
                manifest.to_str().unwrap(),
                "add",
                "--git",
                &repo.url(),
            ])
            .output()
            .unwrap(),
    );
    assert!(error.contains("Git") || error.contains("git"));
    assert!(!manifest.exists());
    success(
        f.command()
            .current_dir(&invocation)
            .env("PATH", &f.bin)
            .env("TYPM_NET_GIT_FETCH_WITH_CLI", "false")
            .args([
                "--manifest-path",
                manifest.to_str().unwrap(),
                "add",
                "--git",
                &repo.url(),
            ])
            .output()
            .unwrap(),
    );
    assert!(f.link("config-root", "1.0.0").join("lib.typ").is_file());
}

#[test]
fn typm_home_holds_new_caches_and_cache_override_wins() {
    let f = Fixture::new();
    let repo = f.repo("home");
    package(&repo.path, "home", "1.0.0");
    let commit = repo.commit();
    let home = f.temp.path().join("home");
    success(
        f.command()
            .env_remove("TYPM_CACHE_DIR")
            .args(["add", "--git", &repo.url()])
            .output()
            .unwrap(),
    );
    let target = fs::read_link(f.link("home", "1.0.0")).unwrap();
    assert!(target.starts_with(&home));
    assert!(target.ends_with(&commit));
    f.ok(&["add", "--git", &repo.url()]);
    let target = fs::read_link(f.link("home", "1.0.0")).unwrap();
    assert!(target.starts_with(&f.cache));
    assert!(target.ends_with(&commit));
}

#[test]
fn invalid_transport_setting_leaves_project_unchanged() {
    let f = Fixture::new();
    let local = f.local("local");
    let error = failure(
        f.command()
            .env("TYPM_NET_GIT_FETCH_WITH_CLI", "sometimes")
            .args(["add", "--path", local.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(error.contains("TYPM_NET_GIT_FETCH_WITH_CLI"));
    assert!(!f.project.join("typm.toml").exists());
    assert!(!f.project.join(".typm/state.toml").exists());
}

#[test]
fn cli_fetch_prepares_nested_submodules_at_their_recorded_commits() {
    let f = Fixture::new();
    let leaf = f.repo("leaf");
    package(&leaf.path, "leaf", "1.0.0");
    let pinned_leaf = leaf.commit();
    let middle = f.repo("middle");
    package(&middle.path, "middle", "1.0.0");
    middle.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "../leaf",
        "vendor/leaf",
    ]);
    dependencies(&middle.path, "leaf = { path = \"vendor/leaf\" }");
    middle.commit();
    let parent = f.repo("parent");
    package(&parent.path, "parent", "1.0.0");
    parent.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "../middle",
        "vendor/middle",
    ]);
    dependencies(&parent.path, "middle = { path = \"vendor/middle\" }");
    parent.commit();
    fs::write(leaf.path.join("lib.typ"), "#let value = \"new upstream\"\n").unwrap();
    leaf.commit();
    success(
        f.command()
            .env("TYPM_NET_GIT_FETCH_WITH_CLI", "true")
            .args(["add", "parent", "--git", &parent.url()])
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(f.link("leaf", "1.0.0").join("lib.typ")).unwrap(),
        "#let value = \"leaf\"\n"
    );
    let cached = fs::read_link(f.link("parent", "1.0.0")).unwrap();
    let nested = Repository {
        path: cached.join("vendor/middle/vendor/leaf"),
    };
    assert_eq!(nested.git(&["rev-parse", "HEAD"]), pinned_leaf);
    let pins = f.lock();
    success(
        f.command()
            .env("PATH", &f.bin)
            .args(["compile", "main.typ"])
            .output()
            .unwrap(),
    );
    assert_eq!(f.lock(), pins);
}
