#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_typm"));
    command
        .current_dir(directory)
        .env("PATH", "")
        .env("TYPM_HOME", directory.join("home"))
        .env("TYPM_CACHE_DIR", directory.join("cache"))
        .env_remove("TYPM_NET_GIT_FETCH_WITH_CLI");
    command
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

fn shell_path(name: &str) -> Option<PathBuf> {
    let mut directories = vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")];
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    let path = directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| path.is_file());
    if path.is_none() {
        eprintln!("skipping {name} shell check: executable is not installed");
    }
    path
}

#[test]
fn closed_stdout_reports_an_error_without_panicking() {
    let directory = tempfile::tempdir().unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let stdout: OwnedFd = writer.into();
    let output = command(directory.path())
        .args(["completions", "bash"])
        .stdout(stdout)
        .output()
        .unwrap();
    assert!(output.status.code().is_some_and(|code| code != 0));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("completion"), "{error}");
    assert!(!error.contains("panicked"), "{error}");
}

fn bash_suggestions(bash: &Path, script: &Path, words: &[&str]) -> BTreeSet<String> {
    let output = success(
        Command::new(bash)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                // compopt only works during interactive completion; it does not affect candidates.
                r#"source "$1" || exit
shift
compopt() { :; }
COMP_WORDS=("$@")
COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
COMP_LINE="${COMP_WORDS[*]}"
COMP_POINT=${#COMP_LINE}
_typm typm "${COMP_WORDS[COMP_CWORD]}" "${COMP_WORDS[COMP_CWORD - 1]}" || exit
if [ "${#COMPREPLY[@]}" -gt 0 ]; then
    printf '%s\n' "${COMPREPLY[@]}"
fi"#,
                "completion-test",
            ])
            .arg(script)
            .args(words)
            .current_dir(script.parent().unwrap())
            .env_remove("BASH_ENV")
            .output()
            .unwrap(),
    );
    assert!(output.stderr.is_empty(), "{words:?}: {output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn bash_merges_native_and_forwarded_commands_without_conflicts() {
    let Some(bash) = shell_path("bash") else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("typm.bash");
    let output = success(
        command(directory.path())
            .args(["completions", "bash"])
            .output()
            .unwrap(),
    );
    fs::write(&script, output.stdout).unwrap();
    let complete = |words: &[&str]| bash_suggestions(&bash, &script, words);
    let root = complete(&["typm", ""]);
    for command in ["add", "update", "compile", "watch", "help"] {
        assert!(root.contains(command), "missing {command}: {root:?}");
    }
    let flags = complete(&["typm", "--"]);
    assert!(flags.contains("--manifest-path"));
    assert!(flags.contains("--quiet"));
    assert!(!flags.contains("--color"));
    assert!(!flags.contains("--cert"));
    let add = complete(&["typm", "add", "--"]);
    assert!(add.contains("--git"));
    assert!(add.contains("--path"));
    let compile = complete(&["typm", "compile", "--"]);
    assert!(compile.contains("--format"));
    assert!(compile.contains("--root"));
    let update = complete(&["typm", "update", "--"]);
    assert!(update.contains("--help"));
    assert!(!update.contains("--force"));
    assert!(!update.contains("--revert"));
    let help = complete(&["typm", "help", ""]);
    for command in ["compile", "watch", "query", "fonts", "update"] {
        assert!(help.contains(command), "{help:?}");
    }
    assert!(
        !help.contains("add"),
        "help is forwarded to Typst: {help:?}"
    );
}
