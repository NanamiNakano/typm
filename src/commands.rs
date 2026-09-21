use crate::Result;
use crate::cli::{Args, Command};
use crate::completions;
use crate::context::Context;
use crate::manifest::{self, Dependency};
use crate::ops::{self, AddOptions, RemoveOptions, TypstOptions};
use crate::project::Project;

pub fn run(args: Args) -> Result<i32> {
    if let Command::Completions { shell } = args.command {
        return completions::generate(shell, &mut std::io::stdout().lock());
    }
    let context = Context::new(args.quiet)?;
    let explicit_manifest = args.manifest_path.is_some();
    let project = Project::new(manifest::discover(
        args.manifest_path.as_deref(),
        &context.cwd,
    )?)?;
    match args.command {
        Command::Add {
            name,
            git,
            path,
            branch,
            tag,
            rev,
            namespace,
        } => ops::add(
            &context,
            &project,
            AddOptions {
                name,
                dependency: Dependency {
                    git,
                    path,
                    branch,
                    tag,
                    rev,
                    namespace,
                },
            },
        ),
        Command::Remove { names } => ops::remove(&context, &project, RemoveOptions { names }),
        Command::Sync => ops::sync(&context, &project),
        Command::Update => ops::update(&context, &project),
        Command::Completions { .. } => unreachable!("completions are handled before project setup"),
        Command::Typst(arguments) => ops::run_typst(
            &context,
            &project,
            TypstOptions {
                arguments,
                explicit_manifest,
            },
        ),
    }
}
