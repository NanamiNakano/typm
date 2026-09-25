use clap::{ArgGroup, Parser, Subcommand, ValueHint};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "A package manager for Typst",
    disable_help_subcommand = true
)]
pub struct Args {
    /// Use a specific typm.toml instead of searching parent directories.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub manifest_path: Option<PathBuf>,
    /// Suppress typm status and Git progress (Typst output is unchanged).
    #[arg(short, long)]
    pub quiet: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add a dependency, or refresh an existing dependency's Git source.
    #[command(group(ArgGroup::new("source").args(["git", "path"]).required(true)))]
    #[command(group(ArgGroup::new("selector").args(["branch", "tag", "rev"])))]
    Add {
        /// Package name; inferred when the source contains only one package.
        name: Option<String>,
        #[arg(long, value_name = "URL")]
        git: Option<String>,
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        path: Option<PathBuf>,
        #[arg(long, requires = "git")]
        branch: Option<String>,
        #[arg(long, requires = "git")]
        tag: Option<String>,
        #[arg(long, requires = "git")]
        rev: Option<String>,
        #[arg(long, default_value = "typm")]
        namespace: String,
    },
    /// Remove direct dependencies and their unused transitive package links.
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },
    /// Download missing dependencies and repair links, preserving locked Git commits.
    Sync,
    /// Update all Git dependencies within their declared refs and refresh package links.
    Update,
    /// Generate shell completions for typm and its wrapped Typst commands.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(external_subcommand)]
    Typst(Vec<OsString>),
}
