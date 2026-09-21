use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SHELLS: [&str; 5] = ["bash", "zsh", "fish", "powershell", "elvish"];

struct Fixture {
    temp: TempDir,
    project: PathBuf,
    home: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        Self {
            project,
            home: temp.path().join("home"),
            cache: temp.path().join("cache"),
            temp,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_typm"));
        command
            .current_dir(&self.project)
            .env("PATH", "")
            .env("TYPM_HOME", &self.home)
            .env("TYPM_CACHE_DIR", &self.cache)
            .env_remove("TYPM_NET_GIT_FETCH_WITH_CLI");
        command
    }

    fn script(&self, shell: &str) -> String {
        let output = success(
            self.command()
                .args(["completions", shell])
                .output()
                .unwrap(),
        );
        assert!(output.stderr.is_empty(), "{shell}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
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

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if path.is_dir() {
                entries.insert(relative, None);
                visit(root, &path, entries);
            } else {
                entries.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn every_shell_contains_typm_and_typst_without_an_installation_or_project() {
    let fixture = Fixture::new();
    let before = snapshot(fixture.temp.path());
    for shell in SHELLS {
        let script = fixture.script(shell);
        let registration = match shell {
            "bash" => "complete -F _typm",
            "zsh" => "#compdef typm",
            "fish" => "complete -c typm",
            "powershell" => "Register-ArgumentCompleter -Native -CommandName",
            "elvish" => "edit:completion:arg-completer[typm]",
            _ => unreachable!(),
        };
        assert!(script.contains(registration), "{shell}: {script}");
        if shell == "powershell" {
            assert!(
                script.contains("-CommandName 'typm'") || script.contains("-CommandName typm"),
                "{script}"
            );
        }
        for definition in [
            "add",
            "remove",
            "sync",
            "update",
            "completions",
            "manifest-path",
            "compile",
            "watch",
            "query",
            "fonts",
            "font-path",
            "diagnostic-format",
        ] {
            assert!(
                script.contains(definition),
                "{shell} completion is missing {definition}"
            );
        }
    }
    assert_eq!(snapshot(fixture.temp.path()), before);
}

#[test]
fn generation_ignores_invalid_configuration_manifests_and_environment() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.home).unwrap();
    fs::create_dir(fixture.project.join(".typm")).unwrap();
    for path in [
        fixture.home.join("config.toml"),
        fixture.project.join(".typm/config.toml"),
        fixture.project.join("typm.toml"),
        fixture.project.join("typm.lock"),
    ] {
        fs::write(path, "this is not valid TOML [").unwrap();
    }
    let before = snapshot(fixture.temp.path());
    for shell in SHELLS {
        let output = success(
            fixture
                .command()
                .env("TYPM_NET_GIT_FETCH_WITH_CLI", "not-a-boolean")
                .args(["--manifest-path", "missing.toml", "completions", shell])
                .output()
                .unwrap(),
        );
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    success(
        fixture
            .command()
            .env("TYPM_HOME", "")
            .env("TYPM_CACHE_DIR", "")
            .args(["completions", "bash"])
            .output()
            .unwrap(),
    );
    assert_eq!(snapshot(fixture.temp.path()), before);
}

#[test]
fn missing_and_unknown_shells_report_cli_errors() {
    let fixture = Fixture::new();
    for args in [vec!["completions"], vec!["completions", "unknown-shell"]] {
        let output = fixture.command().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("SHELL"), "{error}");
        if args.len() == 1 {
            assert!(error.contains("typm completions"), "{error}");
        } else {
            assert!(error.contains("invalid value 'unknown-shell'"), "{error}");
            for shell in SHELLS {
                assert!(error.contains(shell), "{error}");
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

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
    fn generation_does_not_run_an_available_typst() {
        let fixture = Fixture::new();
        let bin = fixture.temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let typst = bin.join("typst");
        fs::write(
            &typst,
            "#!/bin/sh\nprintf invoked > \"$TYPM_TEST_LOG\"\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&typst, fs::Permissions::from_mode(0o755)).unwrap();
        let before = snapshot(fixture.temp.path());
        for shell in SHELLS {
            success(
                fixture
                    .command()
                    .env("PATH", &bin)
                    .env("TYPM_TEST_LOG", fixture.temp.path().join("typst.log"))
                    .args(["completions", shell])
                    .output()
                    .unwrap(),
            );
        }
        assert_eq!(snapshot(fixture.temp.path()), before);
    }

    #[test]
    fn closed_stdout_reports_an_error_without_panicking() {
        let fixture = Fixture::new();
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        let stdout: OwnedFd = writer.into();
        let output = fixture
            .command()
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
    fn bash_suggests_native_and_forwarded_commands_options_and_values() {
        let Some(bash) = shell_path("bash") else {
            return;
        };
        let fixture = Fixture::new();
        let script = fixture.temp.path().join("typm.bash");
        fs::write(&script, fixture.script("bash")).unwrap();
        let complete = |words: &[&str]| bash_suggestions(&bash, &script, words);
        let root = complete(&["typm", ""]);
        for command in [
            "add",
            "remove",
            "rm",
            "sync",
            "update",
            "completions",
            "compile",
            "c",
            "watch",
            "w",
            "query",
            "fonts",
            "help",
        ] {
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
        for command in ["compile", "c", "watch", "w"] {
            let flags = complete(&["typm", command, "--"]);
            assert!(flags.contains("--format"), "{command}: {flags:?}");
            assert!(flags.contains("--root"), "{command}: {flags:?}");
            assert_eq!(
                flags.contains("--ignore-embedded-fonts"),
                cfg!(feature = "embedded-fonts"),
                "{command}: {flags:?}"
            );
            for flag in ["--no-serve", "--no-reload", "--port"] {
                assert_eq!(
                    flags.contains(flag),
                    matches!(command, "watch" | "w") && cfg!(feature = "http-server"),
                    "{command} {flag}: {flags:?}"
                );
            }
            let formats = complete(&["typm", command, "--format", ""]);
            for format in ["pdf", "png", "svg"] {
                assert!(formats.contains(format), "{command}: {formats:?}");
            }
        }
        let update = complete(&["typm", "update", "--"]);
        assert!(update.contains("--help"));
        assert!(!update.contains("--force"));
        assert!(!update.contains("--revert"));
        let shells = complete(&["typm", "completions", ""]);
        for shell in SHELLS {
            assert!(shells.contains(shell), "{shells:?}");
        }
        let help = complete(&["typm", "help", ""]);
        for command in ["compile", "watch", "query", "fonts", "update"] {
            assert!(help.contains(command), "{help:?}");
        }
        assert!(
            !help.contains("add"),
            "help is forwarded to Typst: {help:?}"
        );
    }

    #[test]
    fn generated_bash_and_zsh_scripts_pass_shell_syntax_checks() {
        let fixture = Fixture::new();
        for name in ["bash", "zsh"] {
            let Some(shell) = shell_path(name) else {
                continue;
            };
            let script = fixture.temp.path().join(format!("typm.{name}"));
            fs::write(&script, fixture.script(name)).unwrap();
            success(Command::new(shell).arg("-n").arg(script).output().unwrap());
        }
    }
}
