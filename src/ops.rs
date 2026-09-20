use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use snafu::{ResultExt, whatever};

use crate::Result;
use crate::context::Context;
use crate::files::read_optional;
use crate::lockfile;
use crate::manifest::{Dependency, ManifestDocument};
use crate::project::Project;
use crate::resolver::{self, Resolver};
use crate::sources::RefreshPolicy;
use crate::typst;

pub struct AddOptions {
    pub name: Option<String>,
    pub dependency: Dependency,
}

pub struct RemoveOptions {
    pub names: Vec<String>,
}

pub struct TypstOptions {
    pub arguments: Vec<OsString>,
    pub explicit_manifest: bool,
}

pub fn add(context: &Context, project: &Project, options: AddOptions) -> Result<i32> {
    let name = options.name.as_deref();
    let _guard = project.acquire()?;
    let mut document = ManifestDocument::read(&project.manifest_path)?;
    let previous_lockfile = lockfile::read(&project.root.join("typm.lock"))?;
    let dependency = prepare_dependency(context, options.dependency, &project.root)?;
    dependency.validate(name)?;

    let refresh = dependency
        .git_source()
        .map_or(RefreshPolicy::Preserve, |source| {
            RefreshPolicy::Source(source.id())
        });
    let mut resolver = Resolver::new(context, previous_lockfile, refresh);
    let name = resolver.infer_name(&dependency, &project.root, name)?;
    document.insert(&name, dependency)?;
    let count = reconcile_packages(project, &document, resolver, true)?;

    context
        .shell
        .status("Added", &format!("{name} ({count} packages linked)"));
    Ok(0)
}

fn prepare_dependency(
    context: &Context,
    mut dependency: Dependency,
    project_root: &Path,
) -> Result<Dependency> {
    let cwd = &context.cwd;
    dependency.git = dependency
        .git
        .map(|location| resolver::normalize_git_location(&location, cwd))
        .transpose()?;
    if let Some(path) = dependency.path {
        let absolute = cwd
            .join(path)
            .canonicalize()
            .whatever_context("cannot find local package directory")?;
        dependency.path = Some(pathdiff::diff_paths(&absolute, project_root).unwrap_or(absolute));
    }
    Ok(dependency)
}

pub fn remove(context: &Context, project: &Project, options: RemoveOptions) -> Result<i32> {
    let _guard = project.acquire()?;
    let mut document = ManifestDocument::read(&project.manifest_path)?;
    let previous_lockfile = lockfile::read(&project.root.join("typm.lock"))?;
    let names: BTreeSet<_> = options.names.into_iter().collect();
    for name in &names {
        document.remove(name)?;
    }

    let resolver = Resolver::new(context, previous_lockfile, RefreshPolicy::Preserve);
    reconcile_packages(project, &document, resolver, true)?;

    context
        .shell
        .status("Removed", &names.into_iter().collect::<Vec<_>>().join(", "));
    Ok(0)
}

pub fn sync(context: &Context, project: &Project) -> Result<i32> {
    let count = prepare_packages(context, project, RefreshPolicy::Preserve)?;
    context
        .shell
        .status("Synced", &format!("{count} packages linked"));
    Ok(0)
}

pub fn update(context: &Context, project: &Project) -> Result<i32> {
    let count = prepare_packages(context, project, RefreshPolicy::All)?;
    context
        .shell
        .status("Updated", &format!("{count} packages linked"));
    Ok(0)
}

pub fn run_typst(context: &Context, project: &Project, options: TypstOptions) -> Result<i32> {
    if read_optional(&project.manifest_path)?.is_some() {
        let count = prepare_packages(context, project, RefreshPolicy::Preserve)?;
        if count > 0 {
            context.shell.status("Ready", &format!("{count} packages"));
        }
    } else if options.explicit_manifest {
        whatever!(
            "manifest does not exist: {}",
            project.manifest_path.display()
        );
    }
    typst::run(&options.arguments, &project.root.join(".typm/packages"))
}

fn prepare_packages(context: &Context, project: &Project, refresh: RefreshPolicy) -> Result<usize> {
    let _guard = project.acquire()?;
    let document = ManifestDocument::read_required(&project.manifest_path)?;
    let previous_lockfile = lockfile::read(&project.root.join("typm.lock"))?;
    let resolver = Resolver::new(context, previous_lockfile, refresh);
    reconcile_packages(project, &document, resolver, false)
}

/// Resolve and commit the current manifest while the caller holds the project lock.
fn reconcile_packages(
    project: &Project,
    document: &ManifestDocument,
    resolver: Resolver<'_>,
    write_manifest: bool,
) -> Result<usize> {
    let resolution = resolver.resolve(&document.manifest, &project.root)?;
    let manifest = write_manifest.then(|| document.text());
    project.update(&resolution.lock, &resolution.links, manifest.as_deref())?;
    Ok(resolution.links.len())
}
