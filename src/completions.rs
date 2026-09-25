use std::collections::BTreeSet;
use std::io::Write;

use clap::{Command, CommandFactory};
use clap_complete::Shell;
use snafu::ResultExt;

use crate::Result;
use crate::cli::Args;

/// This tree is only for completion generation; execution must keep forwarding
/// Typst arguments unchanged, including options unknown to the bundled schema.
fn command() -> Command {
    let mut command = Args::command();
    let native_names: BTreeSet<_> = command
        .get_subcommands()
        .flat_map(|subcommand| {
            std::iter::once(subcommand.get_name()).chain(subcommand.get_all_aliases())
        })
        .map(str::to_owned)
        .collect();

    // Build under the wrapper name so Clap's cached binary names and generated
    // help subtree refer to typm, including `typm help update` (forwarded).
    let mut typst = typst_args::CliArguments::command()
        .name("typm")
        .bin_name("typm");
    typst.build();
    for subcommand in typst.get_subcommands() {
        if std::iter::once(subcommand.get_name())
            .chain(subcommand.get_all_aliases())
            .any(|name| native_names.contains(name))
        {
            continue;
        }
        command = command.subcommand(subcommand.clone());
    }
    command
}

pub(crate) fn generate(shell: Shell, output: &mut dyn Write) -> Result<i32> {
    // clap_complete's infallible generator can panic on writer errors. Generate
    // in memory, then report stdout failures through typm's normal error path.
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command(), "typm", &mut script);
    output
        .write_all(&script)
        .whatever_context("could not write shell completions")?;
    Ok(0)
}
