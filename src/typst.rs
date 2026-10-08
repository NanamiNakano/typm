use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use clap::Parser;
use snafu::ResultExt;
use typst_args::{CliArguments, Input, OutputFormat};

use crate::Result;

pub fn apply_outdir(
    arguments: &mut Vec<OsString>,
    cwd: &Path,
    project_root: &Path,
    outdir: &Path,
) -> Result<()> {
    // Inspect a copy so the original arguments, including options from newer
    // Typst versions that this schema cannot parse, are still forwarded intact.
    let Ok(cli) = CliArguments::try_parse_from(
        std::iter::once(OsStr::new("typst")).chain(arguments.iter().map(OsString::as_os_str)),
    ) else {
        return Ok(());
    };
    let args = match cli.command {
        typst_args::Command::Compile(command) => command.args,
        typst_args::Command::Watch(command) => command.args,
        _ => return Ok(()),
    };
    if args.output.is_some() {
        return Ok(());
    }
    let Input::Path(input) = args.input else {
        return Ok(());
    };

    // Prefer the source's lexical path so symlinks within the project keep their
    // own output paths. Normalize parent components before joining the outdir.
    let mut absolute = PathBuf::new();
    for component in cwd.join(&input).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                absolute.pop();
            }
            _ => absolute.push(component),
        }
    }
    // The project root is canonical. An absolute source may use a symlink alias
    // or a different Windows path prefix for the same directory.
    if !absolute.starts_with(project_root) {
        absolute = absolute.canonicalize().unwrap_or(absolute);
    }
    let relative = absolute
        .strip_prefix(project_root)
        .with_whatever_context(|_| {
            format!(
                "cannot apply outdir to input outside project: {}; specify an explicit output path",
                input.display()
            )
        })?;
    let extension = match args.format.unwrap_or(OutputFormat::Pdf) {
        OutputFormat::Pdf => "pdf",
        OutputFormat::Png => "png",
        OutputFormat::Svg => "svg",
        OutputFormat::Html => "html",
        OutputFormat::Bundle => "",
    };
    let output = project_root
        .join(outdir)
        .join(relative)
        .with_extension(extension);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_whatever_context(|_| {
            format!("could not create output directory {}", parent.display())
        })?;
    }
    // A trailing option such as --open can consume an optional value. Keep the
    // generated path positional, reusing the delimiter if one already exists.
    if !arguments.iter().any(|argument| argument == "--") {
        arguments.push("--".into());
    }
    arguments.push(output.into_os_string());
    Ok(())
}

pub fn run(arguments: &[OsString], packages: &Path) -> Result<i32> {
    let mut command = Command::new("typst");
    command.args(arguments).env("TYPST_PACKAGE_PATH", packages);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
            .whatever_context("could not run Typst; install typst and ensure it is on PATH")
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .whatever_context("could not run Typst; install typst and ensure it is on PATH")?;
        Ok(status.code().unwrap_or(1))
    }
}
