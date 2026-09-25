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
            .join(".typm/packages/typm")
            .join(name)
            .join(version)
    }

    fn lock(&self) -> String {
        fs::read_to_string(self.project.join("typm.lock")).unwrap()
    }

    fn assert_packages(&self, linked: &[(&str, &str)], locked: &[(&str, &str)]) {
        let paths = |packages: &[(&str, &str)]| {
            packages
                .iter()
                .map(|(name, version)| format!("typm/{name}/{version}"))
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
                assert_eq!(
                    fs::read_link(&installed).unwrap(),
                    Path::new(link["target"].as_str().unwrap())
                );
                assert!(installed.join("lib.typ").is_file());
                path.to_owned()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(state_paths, paths(linked));
        for (name, version) in linked {
            assert_eq!(package_version(&self.link(name, version)), *version);
        }
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

fn package_source(installed: &Path) -> PathBuf {
    fs::canonicalize(installed.join("lib.typ"))
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn package_version(path: &Path) -> String {
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(path.join("typst.toml")).unwrap()).unwrap();
    manifest["package"]["version"].as_str().unwrap().to_owned()
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
fn package_stubs_preserve_local_and_cached_sources_and_manifest_formatting() {
    for git in [false, true] {
        let f = Fixture::new();
        let repo = git.then(|| f.repo("styled"));
        let source = repo
            .as_ref()
            .map(|repo| repo.path.clone())
            .unwrap_or_else(|| f.local("styled"));
        package(&source, "styled", "1.2.3");
        let manifest = "# Keep release 1.2.3 and this formatting.\n[package]\nname='styled'\nversion = '1.2.3' # published version\nentrypoint = \"lib.typ\"\ndescription = 'Release 1.2.3'\nauthors = [\"Package Author\"]\n\n[tool.example]\nversion = '1.2.3'\n";
        fs::write(source.join("typst.toml"), manifest).unwrap();
        fs::create_dir(source.join("assets")).unwrap();
        fs::write(source.join("assets/data.txt"), "package asset\n").unwrap();
        fs::write(source.join("README.md"), "Original package\n").unwrap();
        if let Some(repo) = &repo {
            repo.commit();
            f.ok(&["add", "--git", &repo.url()]);
        } else {
            f.ok(&["add", "--path", source.to_str().unwrap()]);
        }

        let installed = f.link("styled", "0.0.0");
        assert!(
            fs::symlink_metadata(&installed)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let stub = fs::canonicalize(installed.join("typst.toml")).unwrap();
        assert!(stub.starts_with(f.project.join(".typm")));
        let expected = manifest.replacen("version = '1.2.3'", "version = '0.0.0'", 1);
        assert_eq!(
            fs::read_to_string(installed.join("typst.toml")).unwrap(),
            expected
        );
        assert_eq!(
            fs::read_to_string(source.join("typst.toml")).unwrap(),
            manifest
        );
        let backing = package_source(&installed);
        assert_eq!(
            fs::read_to_string(backing.join("typst.toml")).unwrap(),
            manifest
        );
        if git {
            assert!(backing.starts_with(&f.cache));
            f.assert_packages(&[("styled", "0.0.0")], &[("styled", "1.2.3")]);
        } else {
            assert_eq!(backing, source);
        }
        for name in ["typst.toml", "lib.typ", "assets", "README.md"] {
            let entry = installed.join(name);
            assert!(
                fs::symlink_metadata(&entry)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            if name != "typst.toml" {
                assert_eq!(
                    fs::canonicalize(entry).unwrap(),
                    fs::canonicalize(backing.join(name)).unwrap()
                );
            }
        }
        assert!(!f.link("styled", "1.2.3").exists());
        assert!(!f.project.join(".typm/packages/local").exists());
        if let Some(repo) = &repo {
            assert!(repo.git(&["status", "--porcelain"]).is_empty());
        }
    }
}

#[test]
fn local_stub_links_stay_live_and_refresh_manifest_and_top_level_entries() {
    let f = Fixture::new();
    let source = f.local("live");
    fs::create_dir(source.join("assets")).unwrap();
    fs::write(source.join("assets/value.txt"), "original\n").unwrap();
    fs::write(source.join("obsolete.txt"), "removed later\n").unwrap();
    f.ok(&["add", "--path", source.to_str().unwrap()]);
    let installed = f.link("live", "0.0.0");
    fs::write(source.join("lib.typ"), "#let value = \"live edit\"\n").unwrap();
    fs::write(source.join("assets/value.txt"), "updated\n").unwrap();
    fs::write(source.join("assets/new.txt"), "new nested asset\n").unwrap();
    assert_eq!(
        fs::read_to_string(installed.join("lib.typ")).unwrap(),
        "#let value = \"live edit\"\n"
    );
    assert_eq!(
        fs::read_to_string(installed.join("assets/value.txt")).unwrap(),
        "updated\n"
    );
    assert_eq!(
        fs::read_to_string(installed.join("assets/new.txt")).unwrap(),
        "new nested asset\n"
    );
    fs::write(source.join("new.typ"), "#let added = true\n").unwrap();
    fs::remove_file(source.join("obsolete.txt")).unwrap();
    f.ok(&["sync"]);
    assert_eq!(
        fs::read_to_string(installed.join("new.typ")).unwrap(),
        "#let added = true\n"
    );
    assert!(
        fs::symlink_metadata(installed.join("new.typ"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::symlink_metadata(installed.join("obsolete.txt")).is_err());

    let original_manifest = fs::read_to_string(source.join("typst.toml")).unwrap();
    let newer_version = original_manifest.replace("1.0.0", "2.0.0");
    let refreshed_manifest = format!("{newer_version}description = 'Updated package metadata'\n");
    fs::write(source.join("typst.toml"), &refreshed_manifest).unwrap();
    f.ok(&["sync"]);
    assert_eq!(
        fs::read_to_string(source.join("typst.toml")).unwrap(),
        refreshed_manifest
    );
    assert_eq!(
        fs::read_to_string(installed.join("typst.toml")).unwrap(),
        refreshed_manifest.replace("2.0.0", "0.0.0")
    );
    assert!(!f.link("live", "1.0.0").exists());
    assert!(!f.link("live", "2.0.0").exists());
}

#[test]
fn sync_migrates_old_managed_links_without_keeping_import_aliases() {
    let f = Fixture::new();
    let repo = f.repo("migrated");
    package(&repo.path, "migrated", "1.0.0");
    let pinned = repo.commit();
    f.ok(&[
        "add",
        "--git",
        &repo.url(),
        "--branch",
        "main",
        "--namespace",
        "local",
    ]);
    let previous_link = f.project.join(".typm/packages/local/migrated/0.0.0");
    let source = package_source(&previous_link);
    fs::remove_file(&previous_link).unwrap();
    dependencies(&f.project, &git_dependency("migrated", &repo));
    let old_link = f.project.join(".typm/packages/local/migrated/1.0.0");
    fs::create_dir_all(old_link.parent().unwrap()).unwrap();
    symlink(&source, &old_link).unwrap();
    fs::write(
        f.project.join(".typm/state.toml"),
        format!("version = 1\n\n[[links]]\npath = 'local/migrated/1.0.0'\ntarget = {:?}\nroots = ['migrated']\n", source.to_str().unwrap()),
    ).unwrap();
    package(&repo.path, "migrated", "2.0.0");
    let upstream = repo.commit();

    f.ok(&["sync"]);
    assert!(fs::symlink_metadata(&old_link).is_err());
    let installed = f.link("migrated", "0.0.0");
    assert_eq!(package_source(&installed), source);
    assert_eq!(package_version(&installed), "0.0.0");
    assert_eq!(package_version(&source), "1.0.0");
    assert!(f.lock().contains(&pinned));
    assert!(!f.lock().contains(&upstream));
    f.assert_packages(&[("migrated", "0.0.0")], &[("migrated", "1.0.0")]);
    assert!(!f.link("migrated", "1.0.0").exists());
    assert!(
        !f.project
            .join(".typm/packages/local/migrated/0.0.0")
            .exists()
    );
}

#[test]
fn direct_and_transitive_links_follow_promotions_demotions_and_removal() {
    for version in ["1.0.0", "0.0.0"] {
        let f = Fixture::new();
        let shared = f.local("shared");
        package(&shared, "shared", version);
        let front = f.local("front");
        dependencies(&front, "shared = { path = '../shared' }");
        f.ok(&["add", "--path", front.to_str().unwrap()]);
        let direct = f.link("shared", "0.0.0");
        let transitive = f.link("shared", version);
        assert_eq!(fs::read_link(&transitive).unwrap(), shared);
        if version != "0.0.0" {
            assert!(!direct.exists());
        }

        f.ok(&["add", "--path", shared.to_str().unwrap()]);
        assert_eq!(package_source(&direct), shared);
        assert_eq!(package_version(&direct), "0.0.0");
        assert_eq!(fs::read_link(&transitive).unwrap(), shared);
        assert_eq!(package_version(&transitive), version);

        f.ok(&["remove", "shared"]);
        if version != "0.0.0" {
            assert!(fs::symlink_metadata(&direct).is_err());
        }
        assert_eq!(fs::read_link(&transitive).unwrap(), shared);
        f.ok(&["add", "--path", shared.to_str().unwrap()]);
        f.ok(&["remove", "front"]);
        if version != "0.0.0" {
            assert!(fs::symlink_metadata(&transitive).is_err());
        }
        assert_eq!(package_source(&direct), shared);
        f.ok(&["remove", "shared"]);
        assert!(fs::symlink_metadata(&direct).is_err());
        assert_eq!(package_version(&shared), version);
        assert!(shared.join("lib.typ").is_file());
    }
}

#[test]
fn direct_alias_conflicting_with_a_distinct_genuine_zero_source_preserves_project() {
    let f = Fixture::new();
    let shared = f.local("shared");
    let zero = f.temp.path().join("zero-version");
    package(&zero, "shared", "0.0.0");
    let front = f.local("front");
    dependencies(
        &front,
        &format!("shared = {{ path = {:?} }}", zero.to_str().unwrap()),
    );
    f.ok(&["add", "--path", shared.to_str().unwrap()]);
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();
    let state = fs::read(f.project.join(".typm/state.toml")).unwrap();
    let installed = f.link("shared", "0.0.0");
    let target = fs::read_link(&installed).unwrap();
    let error = failure(f.run(&["add", "--path", front.to_str().unwrap()]));
    assert!(error.contains("shared") && error.contains("front"));
    assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
    assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
    assert_eq!(fs::read_link(installed).unwrap(), target);
    assert!(!f.link("front", "0.0.0").exists());
    assert_eq!(package_version(&shared), "1.0.0");
    assert_eq!(package_version(&zero), "0.0.0");
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
    assert!(f.project.join(".typm/.gitignore").is_file());
    for (name, version, target) in [
        ("helpers", "0.0.0", helpers),
        ("local-leaf", "1.0.0", local_leaf),
    ] {
        assert_eq!(package_source(&f.link(name, version)), target);
    }
    for (name, version) in [("front", "0.0.0"), ("leaf", "1.0.0")] {
        let link = f.link(name, version);
        assert!(link.join("lib.typ").is_file());
        assert!(package_source(&link).starts_with(&f.cache));
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
        ".typm/packages/typm/dependency/0.0.0",
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

        let checkout = package_source(&f.link("locked", "0.0.0"));
        fs::remove_dir_all(checkout).unwrap();
        fs::remove_file(f.link("locked", "0.0.0")).unwrap();
        let offline = repo.path.with_extension("offline");
        fs::rename(&repo.path, &offline).unwrap();
        sync();
        assert_eq!(f.lock(), lock);
        assert_eq!(
            fs::read_to_string(f.link("locked", "0.0.0").join("lib.typ")).unwrap(),
            "#let value = \"locked\"\n"
        );

        fs::rename(offline, &repo.path).unwrap();
        fs::remove_dir_all(&f.cache).unwrap();
        sync();
        assert_eq!(f.lock(), lock);
        assert_eq!(
            fs::read_to_string(f.link("locked", "0.0.0").join("lib.typ")).unwrap(),
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
    for (name, version) in [
        ("front", "0.0.0"),
        ("leaf", "2.0.0"),
        ("local-git", "2.0.0"),
    ] {
        assert!(f.link(name, version).join("lib.typ").is_file());
        assert_eq!(
            package_version(&package_source(&f.link(name, version))),
            "2.0.0"
        );
        assert!(fs::symlink_metadata(f.link(name, "1.0.0")).is_err());
    }
    assert_eq!(package_source(&f.link("helpers", "0.0.0")), helpers);
    assert_eq!(package_version(&helpers), "2.0.0");
    assert!(!f.link("helpers", "1.0.0").exists());
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
            assert!(f.link(name, "0.0.0").join("lib.typ").is_file());
        }
        quiet("update");
        for name in ["branched", "default", "named", "moved"] {
            assert!(
                f.link(name, "0.0.0").join("lib.typ").is_file(),
                "{transport}: {name}"
            );
            assert_eq!(
                package_version(&package_source(&f.link(name, "0.0.0"))),
                "2.0.0"
            );
        }
        for name in ["tagged", "pinned"] {
            assert!(f.link(name, "0.0.0").join("lib.typ").is_file());
            assert_eq!(
                package_version(&package_source(&f.link(name, "0.0.0"))),
                "1.0.0"
            );
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
    let link = fs::read_link(f.link("existing", "0.0.0")).unwrap();
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
        assert_eq!(fs::read_link(f.link("existing", "0.0.0")).unwrap(), link);
        assert_eq!(
            package_version(&package_source(&f.link("existing", "0.0.0"))),
            "1.0.0"
        );
    }

    f.local("collision");
    dependencies(
        &f.project,
        &format!("{valid_dependencies}\ncollision = {{ path = '../local/collision' }}"),
    );
    let collision = f.link("collision", "0.0.0");
    fs::create_dir_all(&collision).unwrap();
    fs::write(collision.join("keep.txt"), "user-owned data").unwrap();
    let error = failure(f.run(&["update"]));
    assert!(error.contains("unmanaged package path"));
    assert_eq!(f.lock(), lock);
    assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
    assert_eq!(fs::read_link(f.link("existing", "0.0.0")).unwrap(), link);
    assert_eq!(
        fs::read_to_string(collision.join("keep.txt")).unwrap(),
        "user-owned data"
    );

    assert!(!f.log.exists());
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
    let alpha = package_source(&f.link("alpha", "0.0.0"));
    let beta = package_source(&f.link("beta", "1.0.0"));
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
    assert!(!f.link("left", "0.0.0").exists());
    assert!(f.link("right", "0.0.0").is_dir());
    assert!(f.link("shared", "1.0.0").is_dir());
    f.ok(&["rm", "right"]);
    assert!(!f.link("right", "0.0.0").exists());
    assert!(!f.link("shared", "1.0.0").exists());
    assert!(f.cache.is_dir());
}

#[test]
fn commands_reconcile_manual_manifest_edits() {
    for command in ["add", "remove", "sync", "update", "compile"] {
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
        assert_eq!(package_source(&f.link("retargeted", "0.0.0")), original);
        package(&pinned.path, "pinned", "2.0.0");
        let refreshed_commit = pinned.commit();
        let retained = format!(
            "# Keep this manual edit.\n[dependencies]\n{} # selected revision\n{}\nretargeted = {{ path = '../local/retargeted-replacement' }}\nmanual = {{ path = '../local/manual' }}\n",
            selected_at(&older),
            git_dependency("pinned", &pinned),
        );
        let mut edited = retained.clone();
        if command == "remove" {
            edited.push_str("removed = { path = '../local/removed' }\n");
        }
        fs::write(f.project.join("typm.toml"), &edited).unwrap();
        fs::rename(&deleted.path, deleted.path.with_extension("offline")).unwrap();
        fs::remove_dir_all(removed).unwrap();

        match command {
            "add" => {
                f.ok(&["add", "--path", f.local("added").to_str().unwrap()]);
            }
            "remove" => {
                f.ok(&[command, "removed"]);
            }
            "compile" => {
                f.ok(&[command, "main.typ"]);
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
        let mut linked = vec![
            ("selected", "0.0.0"),
            ("pinned", "0.0.0"),
            ("retargeted", "0.0.0"),
            ("manual", "0.0.0"),
        ];
        if command == "add" {
            linked.push(("added", "0.0.0"));
        }
        f.assert_packages(&linked, &locked);
        assert_eq!(package_source(&f.link("retargeted", "0.0.0")), replacement);
        assert_eq!(package_source(&f.link("manual", "0.0.0")), manual);
        for path in [
            f.link("selected", "2.0.0"),
            f.link("deleted", "0.0.0"),
            f.link("removed", "0.0.0"),
        ] {
            assert!(
                fs::symlink_metadata(&path).is_err(),
                "{command}: {}",
                path.display()
            );
        }
        let lock = f.lock();
        assert!(lock.contains(&older));
        for commit in [&newer, &deleted_commit] {
            assert!(!lock.contains(commit));
        }
        assert_eq!(lock.contains(&pinned_commit), command != "update");
        assert_eq!(lock.contains(&refreshed_commit), command == "update");
        let manifest = fs::read_to_string(f.project.join("typm.toml")).unwrap();
        if command == "add" {
            assert!(manifest.starts_with(&retained));
        } else {
            assert_eq!(manifest, retained);
        }
        assert_eq!(f.log.exists(), command == "compile");
    }
}

#[test]
fn removal_resolves_edited_local_versions_and_transitives_preserving_shared_packages() {
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
    linked.extend([("local", "0.0.0"), ("keeper", "0.0.0")]);
    f.assert_packages(&linked, &locked);
    for path in [f.link("removed", "0.0.0"), f.link("obsolete", "1.0.0")] {
        assert!(fs::symlink_metadata(path).is_err());
    }
    let lock = f.lock();
    for commit in [&older, &newer, &added_commit] {
        assert!(lock.contains(commit));
    }
    assert!(!lock.contains(&obsolete_commit));
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
    let links = [f.link("existing", "0.0.0"), f.link("removed", "0.0.0")].map(|path| {
        let target = fs::read_link(&path).unwrap();
        (path, target)
    });
    dependencies(
        &f.project,
        &format!("{valid}\nzmissing = {{ path = '../missing' }}"),
    );
    let manifest = fs::read(f.project.join("typm.toml")).unwrap();

    for command in ["add", "remove", "sync", "update", "compile"] {
        let output = match command {
            "add" => f.run(&[command, "--path", added.to_str().unwrap()]),
            "remove" => f.run(&[command, "removed"]),
            "compile" => f.run(&[command, "main.typ"]),
            _ => f.run(&[command]),
        };
        let error = failure(output);
        assert!(error.contains("zmissing"));
        assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
        assert_eq!(fs::read(f.project.join("typm.lock")).unwrap(), lock);
        assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
        for (path, target) in &links {
            assert_eq!(fs::read_link(path).unwrap(), *target);
        }
        assert!(fs::symlink_metadata(f.link("added", "0.0.0")).is_err());
        assert!(
            !f.log.exists(),
            "{command} must not invoke Typst after failed resolution"
        );
    }
}

#[test]
fn conflicting_sources_leave_the_existing_project_unchanged() {
    for second_is_git in [true, false] {
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
        let second_dependency = if second_is_git {
            git_dependency("shared", &second)
        } else {
            format!("shared = {{ path = {:?} }}", second.path.to_str().unwrap())
        };
        dependencies(&right, &second_dependency);
        f.ok(&["add", "--path", left.to_str().unwrap()]);
        let manifest = fs::read(f.project.join("typm.toml")).unwrap();
        let lock = f.lock();
        let state = fs::read(f.project.join(".typm/state.toml")).unwrap();
        let target = fs::read_link(f.link("shared", "1.0.0")).unwrap();
        let error = failure(f.run(&["add", "--path", right.to_str().unwrap()]));
        assert!(error.contains("shared") && error.contains("left") && error.contains("right"));
        assert!(error.contains("@typm/shared:1.0.0"));
        assert_eq!(fs::read(f.project.join("typm.toml")).unwrap(), manifest);
        assert_eq!(f.lock(), lock);
        assert_eq!(fs::read(f.project.join(".typm/state.toml")).unwrap(), state);
        assert_eq!(fs::read_link(f.link("shared", "1.0.0")).unwrap(), target);
        assert!(!f.link("right", "0.0.0").exists());
    }
}

#[test]
fn different_transitive_versions_coexist_and_local_cycles_are_reported() {
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
    assert_eq!(fs::canonicalize(f.link("shared", "1.0.0")).unwrap(), v1);
    assert_eq!(fs::canonicalize(f.link("shared", "2.0.0")).unwrap(), v2);
    assert!(!f.link("shared", "0.0.0").exists());
    f.ok(&["add", "--path", v1.to_str().unwrap()]);
    assert_eq!(package_source(&f.link("shared", "0.0.0")), v1);
    assert_eq!(fs::canonicalize(f.link("shared", "1.0.0")).unwrap(), v1);
    assert_eq!(fs::canonicalize(f.link("shared", "2.0.0")).unwrap(), v2);

    dependencies(&first, "second = { path = \"../second\" }");
    dependencies(&second, "first = { path = \"../first\" }");
    let error = failure(f.run(&["compile", "main.typ"]));
    assert!(error.contains("cycle") && error.contains("first") && error.contains("second"));
    assert!(!f.log.exists(), "Typst must not run on resolution failure");
}

#[test]
fn explicit_local_namespace_overrides_the_typm_default() {
    let f = Fixture::new();
    let legacy = f.local("legacy");
    let current = f.local("current");
    let first = f.temp.path().join("first-shared");
    let second = f.temp.path().join("second-shared");
    package(&first, "shared", "1.0.0");
    package(&second, "shared", "2.0.0");
    dependencies(
        &legacy,
        &format!(
            "shared = {{ path = {:?}, namespace = 'local' }}",
            first.to_str().unwrap()
        ),
    );
    dependencies(
        &current,
        &format!("shared = {{ path = {:?} }}", second.to_str().unwrap()),
    );
    f.ok(&["add", "--path", legacy.to_str().unwrap()]);
    f.ok(&["add", "--path", current.to_str().unwrap()]);
    let explicit = f.project.join(".typm/packages/local/shared/1.0.0");
    let default = f.link("shared", "2.0.0");
    assert_eq!(package_source(&explicit), first);
    assert_eq!(package_source(&default), second);
    assert_eq!(package_version(&explicit), "1.0.0");
    assert_eq!(package_version(&default), "2.0.0");
    f.ok(&[
        "add",
        "--path",
        first.to_str().unwrap(),
        "--namespace",
        "local",
    ]);
    let manifest = fs::read_to_string(f.project.join("typm.toml")).unwrap();
    let parsed: toml::Value = toml::from_str(&manifest).unwrap();
    assert_eq!(
        parsed["dependencies"]["shared"]["namespace"].as_str(),
        Some("local")
    );
    let direct = f.project.join(".typm/packages/local/shared/0.0.0");
    assert_eq!(package_source(&direct), first);
    assert!(!f.link("shared", "0.0.0").exists());
    f.ok(&["sync"]);
    assert_eq!(
        fs::read_to_string(f.project.join("typm.toml")).unwrap(),
        manifest
    );
    assert_eq!(package_source(&direct), first);
    assert_eq!(package_source(&explicit), first);
    f.ok(&["remove", "shared"]);
    assert!(fs::symlink_metadata(direct).is_err());
    assert_eq!(package_source(&explicit), first);
    f.ok(&["remove", "legacy"]);
    assert!(fs::symlink_metadata(explicit).is_err());
    assert!(default.join("lib.typ").is_file());
}

#[test]
fn a_dangling_user_lockfile_symlink_survives_failed_add() {
    let f = Fixture::new();
    let local = f.local("local");
    let lock = f.project.join("typm.lock");
    let missing_target = f.temp.path().join("missing-user-lock.toml");
    symlink(&missing_target, &lock).unwrap();
    failure(f.run(&["add", "--path", local.to_str().unwrap()]));
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
        package_source(&f.link("concurrent", "0.0.0")),
        package_source(&other.join(".typm/packages/typm/concurrent/0.0.0"))
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
        "#import \"@typm/leaf:1.0.0\": value\n#let text = value\n",
    )
    .unwrap();
    front.commit();
    let helpers = f.local("helpers");
    dependencies(&helpers, &git_dependency("leaf", &leaf));
    fs::write(
        helpers.join("lib.typ"),
        "#import \"@typm/leaf:1.0.0\": value\n#let helper = value\n",
    )
    .unwrap();
    let original_files = [
        ("leaf", "1.0.0", &leaf.path),
        ("front", "0.0.0", &front.path),
        ("helpers", "0.0.0", &helpers),
    ]
    .map(|(name, version, source)| {
        (
            name,
            version,
            source,
            fs::read(source.join("typst.toml")).unwrap(),
            fs::read(source.join("lib.typ")).unwrap(),
        )
    });
    f.ok(&["add", "--git", &front.url(), "--branch", "main"]);
    f.ok(&["add", "--path", helpers.to_str().unwrap()]);

    let preview_cache = f.temp.path().join("typst-cache");
    let cached = preview_cache.join("preview/cached/1.0.0");
    package(&cached, "cached", "1.0.0");
    fs::write(cached.join("lib.typ"), "#let cached = \"cached preview\"\n").unwrap();
    fs::write(f.project.join("main.typ"), "#import \"@typm/front:0.0.0\": text\n#import \"@typm/helpers:0.0.0\": helper\n#import \"@preview/cached:1.0.0\": cached\n#text #helper #cached\n").unwrap();
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
    assert!(!f.link("leaf", "0.0.0").exists());
    for (name, version, source, manifest, contents) in original_files {
        let installed = f.link(name, version);
        let backing = package_source(&installed);
        assert_eq!(fs::read(source.join("typst.toml")).unwrap(), manifest);
        assert_eq!(fs::read(source.join("lib.typ")).unwrap(), contents);
        assert_eq!(fs::read(backing.join("typst.toml")).unwrap(), manifest);
        assert_eq!(fs::read(backing.join("lib.typ")).unwrap(), contents);
        if version != "0.0.0" {
            assert_eq!(fs::canonicalize(installed).unwrap(), backing);
        }
    }
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
    assert!(f.link("without-git", "0.0.0").join("lib.typ").is_file());
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
    assert!(f.link("config-root", "0.0.0").join("lib.typ").is_file());
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
    let target = package_source(&f.link("home", "0.0.0"));
    assert!(target.starts_with(&home));
    assert!(target.ends_with(&commit));
    f.ok(&["add", "--git", &repo.url()]);
    let target = package_source(&f.link("home", "0.0.0"));
    assert!(target.starts_with(&f.cache));
    assert!(target.ends_with(&commit));
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
    let cached = package_source(&f.link("parent", "0.0.0"));
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
