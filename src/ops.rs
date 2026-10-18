use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use snafu::{ResultExt, whatever};

use crate::Result;
use crate::context::Context;
use crate::files::read_optional;
use crate::lockfile;
use crate::manifest::{Dependency, Manifest, ManifestDocument};
use crate::project::{ManagedLink, Project};
use crate::resolver::Resolver;
use crate::sources::{RefreshPolicy, normalize_git_location};
use crate::typst;

pub struct AddOptions {
    pub name: Option<String>,
    pub dependency: Dependency,
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
    let count = reconcile_packages(project, &document, resolver, true, false)?;

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
        .map(|location| normalize_git_location(&location, cwd))
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

pub fn remove(context: &Context, project: &Project, names: Vec<String>) -> Result<i32> {
    let _guard = project.acquire()?;
    let mut document = ManifestDocument::read(&project.manifest_path)?;
    let previous_lockfile = lockfile::read(&project.root.join("typm.lock"))?;
    let names: BTreeSet<_> = names.into_iter().collect();
    for name in &names {
        document.remove(name)?;
    }

    let resolver = Resolver::new(context, previous_lockfile, RefreshPolicy::Preserve);
    reconcile_packages(project, &document, resolver, true, false)?;

    context
        .shell
        .status("Removed", &names.into_iter().collect::<Vec<_>>().join(", "));
    Ok(0)
}

pub fn sync(context: &Context, project: &Project, force: bool) -> Result<i32> {
    let (count, _) = prepare_packages(context, project, RefreshPolicy::Preserve, force)?;
    context
        .shell
        .status("Synced", &format!("{count} packages linked"));
    Ok(0)
}

pub fn update(context: &Context, project: &Project) -> Result<i32> {
    let (count, _) = prepare_packages(context, project, RefreshPolicy::All, false)?;
    context
        .shell
        .status("Updated", &format!("{count} packages linked"));
    Ok(0)
}

pub fn run_typst(context: &Context, project: &Project, mut options: TypstOptions) -> Result<i32> {
    if read_optional(&project.manifest_path)?.is_some() {
        let (count, manifest) = prepare_packages(context, project, RefreshPolicy::Preserve, false)?;
        if count > 0 {
            context.shell.status("Ready", &format!("{count} packages"));
        }
        if let Some(outdir) = manifest.options.outdir {
            typst::apply_outdir(&mut options.arguments, &context.cwd, &project.root, &outdir)?;
        }
    } else if options.explicit_manifest {
        whatever!(
            "manifest does not exist: {}",
            project.manifest_path.display()
        );
    }
    typst::run(&options.arguments, &project.root.join(".typm/packages"))
}

fn prepare_packages(
    context: &Context,
    project: &Project,
    refresh: RefreshPolicy,
    force: bool,
) -> Result<(usize, Manifest)> {
    let _guard = project.acquire()?;
    let document = ManifestDocument::read_required(&project.manifest_path)?;
    let previous_lockfile = lockfile::read(&project.root.join("typm.lock"))?;
    let resolver = Resolver::new(context, previous_lockfile, refresh);
    let count = reconcile_packages(project, &document, resolver, false, force)?;
    Ok((count, document.manifest))
}

/// Resolve and commit the current manifest while the caller holds the project lock.
fn reconcile_packages(
    project: &Project,
    document: &ManifestDocument,
    resolver: Resolver<'_>,
    write_manifest: bool,
    force: bool,
) -> Result<usize> {
    let resolution = resolver.resolve(&document.manifest, &project.root)?;
    let mut links = Vec::new();
    for import in resolution.imports {
        let view = if import.version == import.package.version {
            None
        } else {
            Some(crate::stubs::PackageView::prepare(&import.package.root)?)
        };
        links.push(ManagedLink {
            path: PathBuf::from(import.namespace)
                .join(import.package.name)
                .join(import.version),
            target: import.package.root,
            view,
            roots: import.roots,
        });
    }
    let manifest = write_manifest.then(|| document.text());
    project.update(&resolution.lock, &links, manifest.as_deref(), force)?;
    Ok(links.len())
}
