//! Shared Git databases and commit-addressed checkouts.

mod transport;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use git2::build::{CheckoutBuilder, CloneLocal, RepoBuilder};
use git2::{ErrorCode, Oid, Reference, Repository, SubmoduleUpdate};
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, whatever};

use crate::Result;
use crate::context::Context;
use crate::shell::Shell;
use transport::Transport;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSourceSpec {
    pub git: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

impl GitSourceSpec {
    pub fn validate(&self) -> Result<()> {
        if self.git.is_empty()
            || self.git.starts_with('-')
            || self.git.chars().any(char::is_control)
        {
            whatever!("Git source must be a nonempty URL or filesystem path");
        }
        let selectors = [&self.branch, &self.tag, &self.rev];
        if selectors
            .iter()
            .filter(|selector| selector.is_some())
            .count()
            > 1
        {
            whatever!("a Git dependency accepts only one of branch, tag, or rev");
        }
        for selector in selectors.into_iter().flatten() {
            if selector.is_empty() || selector.chars().any(char::is_control) {
                whatever!(
                    "Git branch, tag, and rev selectors must be nonempty and contain no control characters"
                );
            }
        }
        for (kind, value) in [("heads", &self.branch), ("tags", &self.tag)] {
            if let Some(value) = value {
                validate_named_ref(kind, value)?;
            }
        }
        Ok(())
    }

    /// A stable source identity that includes the selector, independently of its commit.
    pub fn id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            Some(&self.git),
            self.branch.as_ref(),
            self.tag.as_ref(),
            self.rev.as_ref(),
        ] {
            match part {
                Some(value) => {
                    hasher.update(&[1]);
                    hasher.update(&(value.len() as u64).to_le_bytes());
                    hasher.update(value.as_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        hasher.finalize().to_hex().to_string()
    }

    fn requested_ref(&self) -> String {
        if let Some(branch) = &self.branch {
            format!("refs/heads/{branch}")
        } else if let Some(tag) = &self.tag {
            format!("refs/tags/{tag}")
        } else {
            self.rev.clone().unwrap_or_else(|| "HEAD".into())
        }
    }

    fn cache_ref(&self) -> String {
        format!("refs/typm/sources/{}", self.id())
    }

    fn allows_local_submodules(&self) -> bool {
        Path::new(&self.git).is_absolute() || self.git.starts_with("file://")
    }
}

#[derive(Debug, Clone)]
pub struct Checkout {
    pub root: PathBuf,
    pub commit: String,
}

pub struct Cache {
    root: PathBuf,
    transport: Transport,
}

impl Cache {
    pub fn new(context: &Context) -> Result<Self> {
        Self::configured(
            context.cache_root.clone(),
            context.shell.clone(),
            context.config.net.git_fetch_with_cli,
        )
    }

    fn configured(root: PathBuf, shell: Shell, fetch_with_cli: bool) -> Result<Self> {
        for directory in ["git/db", "git/checkouts", "git/locks"] {
            fs::create_dir_all(root.join(directory)).with_whatever_context(|_| {
                format!("could not create cache directory {}", root.display())
            })?;
        }
        let root = root
            .canonicalize()
            .whatever_context("could not resolve cache directory")?;
        Ok(Self {
            root,
            transport: Transport::new(shell, fetch_with_cli),
        })
    }

    #[cfg(test)]
    fn at(root: PathBuf, quiet: bool) -> Result<Self> {
        Self::configured(root, Shell::new(quiet), false)
    }

    pub fn resolve(
        &self,
        source: &GitSourceSpec,
        locked: Option<&str>,
        refresh: bool,
    ) -> Result<Checkout> {
        source.validate()?;
        validate_locked_commit(locked)?;
        let repository_id = blake3::hash(source.git.as_bytes()).to_hex().to_string();
        let _lock = self.lock_repository(&repository_id)?;
        let database_path = self.root.join("git/db").join(&repository_id);
        let checkout_parent = self.root.join("git/checkouts").join(&repository_id);
        fs::create_dir_all(&checkout_parent)
            .whatever_context("could not create Git checkout directory")?;

        if !refresh && let Some(commit) = locked {
            let root = checkout_parent.join(commit);
            if checkout_complete(&root, commit) {
                return Ok(Checkout {
                    root,
                    commit: commit.to_owned(),
                });
            }
        }
        reject_sha256_revision(locked.filter(|_| !refresh))?;
        reject_sha256_revision(source.rev.as_deref())?;
        let database = GitDatabase::open_or_create(&database_path, source)?;
        let remote = GitRemote {
            source,
            transport: &self.transport,
        };
        let commit = remote.resolve(&database, locked, refresh)?;
        database.retain_commit(commit)?;
        let commit = commit.to_string();
        let root = checkout_parent.join(&commit);
        if !checkout_complete(&root, &commit) {
            Checkout::create(&database, &root, &remote, &commit)?;
        }
        Ok(Checkout { root, commit })
    }

    fn lock_repository(&self, id: &str) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("git/locks").join(format!("{id}.lock")))
            .whatever_context("could not open the Git cache lock")?;
        file.lock()
            .whatever_context("could not lock the Git cache")?;
        Ok(file)
    }
}

struct GitDatabase {
    path: PathBuf,
    repository: Repository,
}

impl GitDatabase {
    fn open_or_create(path: &Path, source: &GitSourceSpec) -> Result<Self> {
        if !path.join("HEAD").is_file() {
            Self::create(path, source)?;
        }
        reject_sha256_repository(path)?;
        let repository = Repository::open_bare(path)
            .with_whatever_context(|_| format!("could not open Git database {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            repository,
        })
    }

    fn create(path: &Path, source: &GitSourceSpec) -> Result<()> {
        remove_incomplete(path)?;
        let staging = tempfile::Builder::new()
            .prefix(".typm-db-")
            .tempdir_in(path.parent().expect("database has parent"))
            .whatever_context("could not stage the Git database")?;
        {
            let repository = Repository::init_bare(staging.path())
                .whatever_context("could not initialize the Git cache")?;
            repository
                .remote("origin", &source.git)
                .whatever_context("could not configure the Git repository URL")?;
        }
        fs::rename(staging.path(), path).whatever_context("could not publish the Git database")?;
        Ok(())
    }

    fn commit(&self, revision: &str) -> Result<Option<Oid>> {
        resolve_commit(&self.repository, revision)
    }

    fn cache_revision(&self, source: &GitSourceSpec, commit: Oid) -> Result<()> {
        self.repository
            .reference(&source.cache_ref(), commit, true, "typm: resolve source")
            .whatever_context("could not cache the selected Git revision")?;
        Ok(())
    }

    fn invalidate_source_ref(&self, source: &GitSourceSpec) -> Result<()> {
        match self.repository.find_reference(&source.cache_ref()) {
            Ok(mut reference) => reference
                .delete()
                .whatever_context("could not clear the cached Git source reference"),
            Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
            Err(error) => {
                Err(error).whatever_context("could not inspect the cached Git source reference")
            }
        }
    }

    fn retain_commit(&self, commit: Oid) -> Result<()> {
        self.repository
            .reference(
                &format!("refs/typm/commits/{commit}"),
                commit,
                true,
                "typm: retain locked commit",
            )
            .whatever_context("could not retain the cached Git commit")?;
        Ok(())
    }
}

struct GitRemote<'a> {
    source: &'a GitSourceSpec,
    transport: &'a Transport,
}

impl GitRemote<'_> {
    fn resolve(&self, database: &GitDatabase, locked: Option<&str>, refresh: bool) -> Result<Oid> {
        if let Some(locked) = locked.filter(|_| !refresh) {
            if let Some(commit) = database.commit(locked)? {
                return Ok(commit);
            }
            return self.fetch_locked(database, locked);
        }
        if let Some(revision) = self
            .source
            .rev
            .as_deref()
            .filter(|rev| is_commit_prefix(rev))
            && let Some(commit) = database.commit(revision)?
            && commit
                .to_string()
                .starts_with(&revision.to_ascii_lowercase())
        {
            database.cache_revision(self.source, commit)?;
            return Ok(commit);
        }
        self.fetch_source(database)
    }

    fn fetch_source(&self, database: &GitDatabase) -> Result<Oid> {
        self.updating();
        let requested = self.source.requested_ref();
        let source_ref = self.source.cache_ref();
        let refspec = format!("+{requested}:{source_ref}");
        database.invalidate_source_ref(self.source)?;
        let fetched = self.fetch(database, &[&refspec]);
        let selected_commit = database.commit(&source_ref)?;
        if (fetched.is_err() || selected_commit.is_none()) && self.source.rev.is_some() {
            self.fetch_advertised(database)?;
            let commit = database.commit(&requested)?.with_whatever_context(|| {
                format!(
                    "could not resolve Git revision {requested} in {}",
                    self.source.git
                )
            })?;
            database.cache_revision(self.source, commit)?;
            return Ok(commit);
        }
        fetched?;
        selected_commit.with_whatever_context(|| {
            format!(
                "Git reference {requested} in {} does not identify a commit",
                self.source.git
            )
        })
    }

    fn fetch_locked(&self, database: &GitDatabase, locked: &str) -> Result<Oid> {
        self.updating();
        if self.fetch(database, &[locked]).is_err() || database.commit(locked)?.is_none() {
            let refspec = format!(
                "+{}:{}",
                self.source.requested_ref(),
                self.source.cache_ref()
            );
            let _ = self.fetch(database, &[&refspec]);
            if database.commit(locked)?.is_none() {
                self.fetch_advertised(database).with_whatever_context(|_| {
                    format!(
                        "could not download locked commit {locked} from {}",
                        self.source.git
                    )
                })?;
            }
        }
        database.commit(locked)?.with_whatever_context(|| {
            format!(
                "locked Git commit {locked} is unavailable in {}; the lockfile was not changed",
                self.source.git
            )
        })
    }

    fn fetch_advertised(&self, database: &GitDatabase) -> Result<()> {
        self.fetch(
            database,
            &[
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
                "+HEAD:refs/typm/default",
            ],
        )
    }

    fn fetch(&self, database: &GitDatabase, refspecs: &[&str]) -> Result<()> {
        self.transport
            .fetch(&database.repository, &self.source.git, refspecs, true)
    }

    fn updating(&self) {
        self.transport
            .shell
            .status("Updating", &format!("git repository `{}`", self.source.git));
    }
}

impl Checkout {
    fn create(
        database: &GitDatabase,
        destination: &Path,
        remote: &GitRemote<'_>,
        commit: &str,
    ) -> Result<()> {
        remove_incomplete(destination)?;
        let staging = tempfile::Builder::new()
            .prefix(".typm-checkout-")
            .tempdir_in(destination.parent().expect("checkout has parent"))
            .whatever_context("could not stage the Git checkout")?;
        {
            let repository = clone_database(database, staging.path())?;
            repository
                .remote_set_url("origin", &remote.source.git)
                .whatever_context("could not restore the checkout origin URL")?;
            checkout_commit(&repository, parse_commit(commit)?)?;
            initialize_submodules(
                &repository,
                remote.transport,
                remote.source.allows_local_submodules(),
                0,
            )?;
        }
        publish_checkout(staging.path(), destination, commit)
    }
}

fn clone_database(database: &GitDatabase, checkout: &Path) -> Result<Repository> {
    let source = database
        .path
        .to_str()
        .whatever_context("Git cache paths must be valid UTF-8")?;
    let mut no_checkout = CheckoutBuilder::new();
    no_checkout.dry_run();
    RepoBuilder::new()
        .clone_local(CloneLocal::Local)
        .with_checkout(no_checkout)
        .clone(source, checkout)
        .whatever_context("could not create the cached Git checkout")
}

fn checkout_commit(repository: &Repository, commit: Oid) -> Result<()> {
    let object = repository
        .find_commit(commit)
        .whatever_context("could not find the selected Git commit")?;
    repository
        .checkout_tree(object.as_object(), Some(CheckoutBuilder::new().force()))
        .whatever_context("could not check out the selected Git commit")?;
    repository
        .set_head_detached(commit)
        .whatever_context("could not detach the Git checkout at the selected commit")?;
    Ok(())
}

fn initialize_submodules(
    repository: &Repository,
    transport: &Transport,
    allow_local: bool,
    depth: usize,
) -> Result<()> {
    if depth > 256 {
        whatever!("Git submodules exceed 256 nested repositories");
    }
    let submodules = repository
        .submodules()
        .whatever_context("could not inspect Git submodules")?;
    for mut submodule in submodules {
        if submodule.update_strategy() == SubmoduleUpdate::None {
            continue;
        }
        let name = submodule
            .name()
            .whatever_context("Git submodule name is not valid UTF-8")?
            .to_owned();
        let url = initialize_submodule_url(repository, &mut submodule, &name)?;
        transport.validate_url(&url, allow_local)?;
        let commit = submodule
            .index_id()
            .with_whatever_context(|| format!("Git submodule `{name}` has no pinned commit"))?;
        let subrepository = match submodule.open() {
            Ok(repository) => repository,
            Err(error) if error.code() == ErrorCode::NotFound => submodule
                .repo_init(true)
                .whatever_context("could not initialize Git submodule repository")?,
            Err(error) => {
                return Err(error).whatever_context("could not open Git submodule repository");
            }
        };
        subrepository
            .config()
            .whatever_context("could not load Git submodule configuration")?
            .set_str("remote.origin.url", &url)
            .whatever_context("could not configure the Git submodule origin")?;
        if subrepository.find_commit(commit).is_err() {
            transport
                .shell
                .status("Updating", &format!("git submodule `{name}`"));
            let fetched =
                transport.fetch(&subrepository, &url, &[&commit.to_string()], allow_local);
            if fetched.is_err() || subrepository.find_commit(commit).is_err() {
                transport.fetch(
                    &subrepository,
                    &url,
                    &[
                        "+refs/heads/*:refs/remotes/origin/*",
                        "+refs/tags/*:refs/tags/*",
                        "+HEAD:refs/typm/default",
                    ],
                    allow_local,
                )?;
            }
        }
        checkout_commit(&subrepository, commit)?;
        initialize_submodules(
            &subrepository,
            transport,
            allow_local && transport::is_local_url(&url),
            depth + 1,
        )?;
    }
    Ok(())
}

fn initialize_submodule_url(
    repository: &Repository,
    submodule: &mut git2::Submodule<'_>,
    name: &str,
) -> Result<String> {
    let declared_url = submodule
        .url()
        .whatever_context("Git submodule URL is not valid UTF-8")?
        .whatever_context("Git submodule has no repository URL")?
        .to_owned();
    submodule
        .init(false)
        .whatever_context("could not initialize Git submodule configuration")?;
    let mut config = repository
        .config()
        .whatever_context("could not load Git configuration")?;
    let key = format!("submodule.{name}.url");
    if declared_url.starts_with("./") || declared_url.starts_with("../") {
        let origin = repository
            .find_remote("origin")
            .whatever_context("could not find the parent repository origin")?;
        let origin_url = origin
            .url()
            .whatever_context("Git repository origin URL is not valid UTF-8")?;
        if origin_url.starts_with("file://") {
            let mut base = url::Url::parse(origin_url)
                .whatever_context("invalid parent repository file URL")?;
            if !base.path().ends_with('/') {
                base.path_segments_mut()
                    .ok()
                    .whatever_context("could not resolve relative Git submodule URL")?
                    .push("");
            }
            let resolved = base
                .join(&declared_url)
                .whatever_context("could not resolve relative Git submodule URL")?;
            config
                .set_str(&key, resolved.as_str())
                .whatever_context("could not configure the Git submodule URL")?;
        }
    }
    config
        .get_string(&key)
        .whatever_context("Git submodule has no repository URL")
}

fn resolve_commit(repository: &Repository, revision: &str) -> Result<Option<Oid>> {
    match repository.revparse_single(&format!("{revision}^{{commit}}")) {
        Ok(object) => Ok(Some(object.id())),
        Err(error)
            if matches!(
                error.code(),
                ErrorCode::NotFound | ErrorCode::InvalidSpec | ErrorCode::Ambiguous
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).whatever_context("could not inspect the cached Git revision"),
    }
}

fn parse_commit(commit: &str) -> Result<Oid> {
    reject_sha256_revision(Some(commit))?;
    Oid::from_str(commit).whatever_context("invalid Git commit")
}

fn validate_named_ref(kind: &str, value: &str) -> Result<()> {
    if !Reference::is_valid_name(&format!("refs/{kind}/{value}")) {
        whatever!("invalid Git {kind} name: {value}");
    }
    Ok(())
}

fn validate_locked_commit(locked: Option<&str>) -> Result<()> {
    if let Some(commit) = locked
        && !is_full_commit(commit)
    {
        whatever!("invalid locked Git commit: {commit}");
    }
    Ok(())
}

fn reject_sha256_revision(revision: Option<&str>) -> Result<()> {
    if let Some(revision) = revision
        && revision.len() > 40
        && is_commit_prefix(revision)
    {
        whatever!("SHA-256 Git repositories are not supported by typm's git2 backend");
    }
    Ok(())
}

fn reject_sha256_repository(path: &Path) -> Result<()> {
    if let Ok(config) = git2::Config::open(&path.join("config"))
        && let Ok(format) = config.get_string("extensions.objectformat")
        && format.eq_ignore_ascii_case("sha256")
    {
        whatever!("SHA-256 Git repositories are not supported by typm's git2 backend");
    }
    Ok(())
}

fn publish_checkout(staging: &Path, destination: &Path, commit: &str) -> Result<()> {
    let mut marker = File::create(staging.join(".git/typm-complete"))
        .whatever_context("could not mark the Git checkout complete")?;
    marker
        .write_all(commit.as_bytes())
        .whatever_context("could not write Git completion marker")?;
    marker
        .sync_all()
        .whatever_context("could not flush Git completion marker")?;
    drop(marker);
    fs::rename(staging, destination).whatever_context("could not publish the Git checkout")?;
    Ok(())
}

fn checkout_complete(path: &Path, commit: &str) -> bool {
    if !fs::read_to_string(path.join(".git/typm-complete")).is_ok_and(|marker| marker == commit) {
        return false;
    }
    if commit.len() == 64 {
        return fs::read_to_string(path.join(".git/HEAD")).is_ok_and(|head| head.trim() == commit);
    }
    Repository::open(path)
        .and_then(|repository| Ok(repository.head()?.target()))
        .is_ok_and(|head| head.is_some_and(|head| head.to_string() == commit))
}

fn is_full_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_commit_prefix(value: &str) -> bool {
    (4..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_incomplete(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).with_whatever_context(|_| {
                format!(
                    "could not remove incomplete Git cache entry {}",
                    path.display()
                )
            })?;
        }
        Ok(_) => whatever!(
            "refusing to replace unexpected cache entry {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).whatever_context("could not inspect Git cache entry"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(path: &Path, args: &[&str]) -> String {
        let output = transport::git_command()
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=typm test",
                "-c",
                "user.email=typm@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn cached_commits_are_pinned_and_rebuilt_without_the_remote() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(remote.join("lib.typ"), "#let answer = 42\n").unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "--quiet", "-m", "first"]);
        let first = git(&remote, &["rev-parse", "HEAD"]);
        let source = GitSourceSpec {
            git: remote.to_str().unwrap().to_owned(),
            branch: Some("main".into()),
            tag: None,
            rev: None,
        };
        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        let checkout = cache.resolve(&source, None, false).unwrap();
        assert_eq!(checkout.commit, first);
        fs::write(remote.join("lib.typ"), "#let answer = 43\n").unwrap();
        git(&remote, &["commit", "--quiet", "-am", "second"]);
        let second = git(&remote, &["rev-parse", "HEAD"]);
        assert_eq!(cache.resolve(&source, None, false).unwrap().commit, second);
        assert_eq!(
            cache.resolve(&source, Some(&first), false).unwrap().commit,
            first
        );
        assert_ne!(
            cache.resolve(&source, Some(&first), true).unwrap().commit,
            first
        );
        fs::remove_dir_all(&remote).unwrap();
        fs::remove_dir_all(&checkout.root).unwrap();
        let checkout = cache.resolve(&source, Some(&first), false).unwrap();
        assert_eq!(
            fs::read_to_string(checkout.root.join("lib.typ")).unwrap(),
            "#let answer = 42\n"
        );
        let revision_source = GitSourceSpec {
            git: source.git.clone(),
            branch: None,
            tag: None,
            rev: Some(first[..10].to_owned()),
        };
        assert_eq!(
            cache.resolve(&revision_source, None, true).unwrap().commit,
            first
        );
    }

    #[test]
    fn selectors_have_distinct_identities_and_invalid_refspecs_are_rejected() {
        let mut source = GitSourceSpec {
            git: "https://example.invalid/repo.git".into(),
            branch: None,
            tag: None,
            rev: None,
        };
        let head = source.id();
        source.branch = Some("main".into());
        let branch = source.id();
        source.branch = None;
        source.tag = Some("main".into());
        assert_ne!(head, branch);
        assert_ne!(branch, source.id());
        source.tag = Some("bad:refs/heads/injected".into());
        assert!(source.validate().is_err());
    }

    #[test]
    fn abbreviated_revisions_and_named_remote_refs_fetch() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(remote.join("lib.typ"), "first\n").unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "--quiet", "-m", "first"]);
        let first = git(&remote, &["rev-parse", "HEAD"]);
        git(&remote, &["update-ref", "refs/pull/7/head", &first]);
        fs::write(remote.join("lib.typ"), "second\n").unwrap();
        git(&remote, &["commit", "--quiet", "-am", "second"]);

        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        let mut source = GitSourceSpec {
            git: remote.to_str().unwrap().to_owned(),
            branch: None,
            tag: None,
            rev: Some(first[..10].to_owned()),
        };
        assert_eq!(cache.resolve(&source, None, false).unwrap().commit, first);
        source.rev = Some("refs/pull/7/head".to_owned());
        assert_eq!(cache.resolve(&source, None, false).unwrap().commit, first);
    }

    fn repository_fixture(path: &Path) -> String {
        fs::create_dir(path).unwrap();
        git(path, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(path.join("lib.typ"), "first\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "--quiet", "-m", "first"]);
        git(path, &["rev-parse", "HEAD"])
    }

    fn source_fixture(path: &Path) -> GitSourceSpec {
        GitSourceSpec {
            git: path.to_str().unwrap().to_owned(),
            branch: Some("main".into()),
            tag: None,
            rev: None,
        }
    }

    #[test]
    fn nested_submodules_remain_pinned_after_checkout_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let leaf = temporary.path().join("leaf");
        let leaf_commit = repository_fixture(&leaf);
        let middle = temporary.path().join("middle");
        repository_fixture(&middle);
        git(
            &middle,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                "../leaf",
                "nested",
            ],
        );
        git(&middle, &["commit", "--quiet", "-am", "nested submodule"]);
        let parent = temporary.path().join("parent");
        repository_fixture(&parent);
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                "../middle",
                "middle",
            ],
        );
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                "../leaf",
                "ignored",
            ],
        );
        git(
            &parent,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.ignored.update",
                "none",
            ],
        );
        git(
            &parent,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.ignored.url",
                "../missing",
            ],
        );
        git(&parent, &["commit", "--quiet", "-am", "submodules"]);
        fs::write(leaf.join("lib.typ"), "second\n").unwrap();
        git(&leaf, &["commit", "--quiet", "-am", "advance leaf"]);

        for (cli, file_url) in [(false, false), (false, true), (true, false), (true, true)] {
            let cache = Cache::configured(
                temporary.path().join(format!("cache-{cli}-{file_url}")),
                Shell::new(true),
                cli,
            )
            .unwrap();
            let mut source = source_fixture(&parent);
            if file_url {
                source.git = url::Url::from_file_path(&parent).unwrap().to_string();
            }
            let checkout = cache.resolve(&source, None, false).unwrap();
            let nested_path = checkout.root.join("middle/nested");
            assert_eq!(
                fs::read_to_string(nested_path.join("lib.typ")).unwrap(),
                "first\n"
            );
            assert_eq!(
                Repository::open(&nested_path)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target()
                    .unwrap()
                    .to_string(),
                leaf_commit
            );
            assert!(!checkout.root.join("ignored/lib.typ").exists());
        }
    }

    #[test]
    fn system_git_databases_are_reused_without_the_remote() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote");
        let commit = repository_fixture(&remote);
        let source = source_fixture(&remote);
        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        let repository_id = blake3::hash(source.git.as_bytes()).to_hex().to_string();
        let database = cache.root.join("git/db").join(repository_id);
        fs::create_dir(&database).unwrap();
        git(&database, &["init", "--quiet", "--bare"]);
        git(&database, &["remote", "add", "origin", &source.git]);
        git(
            &database,
            &[
                "fetch",
                "--quiet",
                "origin",
                &format!("+refs/heads/main:refs/typm/commits/{commit}"),
            ],
        );
        fs::remove_dir_all(remote).unwrap();
        let checkout = cache.resolve(&source, Some(&commit), false).unwrap();
        assert_eq!(
            fs::read_to_string(checkout.root.join("lib.typ")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn completed_sha256_checkouts_are_reusable_but_cannot_be_rebuilt() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        let source = source_fixture(&temporary.path().join("missing"));
        let repository_id = blake3::hash(source.git.as_bytes()).to_hex().to_string();
        let commit = "a".repeat(64);
        let root = cache
            .root
            .join("git/checkouts")
            .join(repository_id)
            .join(&commit);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/typm-complete"), &commit).unwrap();
        fs::write(root.join(".git/HEAD"), format!("{commit}\n")).unwrap();
        assert_eq!(
            cache.resolve(&source, Some(&commit), false).unwrap().root,
            root
        );
        fs::remove_dir_all(root).unwrap();
        let error = cache.resolve(&source, Some(&commit), false).unwrap_err();
        assert!(
            snafu::Report::from_error(error)
                .to_string()
                .contains("SHA-256")
        );
    }

    #[test]
    fn a_completed_checkout_with_a_changed_head_is_repaired() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote");
        let commit = repository_fixture(&remote);
        let source = source_fixture(&remote);
        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        let checkout = cache.resolve(&source, None, false).unwrap();
        fs::write(checkout.root.join("lib.typ"), "changed\n").unwrap();
        git(
            &checkout.root,
            &["commit", "--quiet", "-am", "changed cached head"],
        );
        fs::remove_dir_all(remote).unwrap();
        let repaired = cache.resolve(&source, Some(&commit), false).unwrap();
        assert_eq!(
            fs::read_to_string(repaired.root.join("lib.typ")).unwrap(),
            "first\n"
        );
        assert_eq!(git(&repaired.root, &["rev-parse", "HEAD"]), commit);
    }

    #[test]
    fn refreshing_a_deleted_branch_does_not_reuse_its_cached_tip() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote");
        let commit = repository_fixture(&remote);
        git(&remote, &["branch", "temporary"]);
        let mut source = source_fixture(&remote);
        source.branch = Some("temporary".into());
        let cache = Cache::at(temporary.path().join("cache"), true).unwrap();
        cache.resolve(&source, None, false).unwrap();
        git(&remote, &["branch", "-D", "temporary"]);
        assert!(cache.resolve(&source, Some(&commit), true).is_err());
        assert_eq!(
            cache.resolve(&source, Some(&commit), false).unwrap().commit,
            commit
        );
    }
}
