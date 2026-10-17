//! Creating a fork (design §19.1, "Forked volumes"): from a source's state
//! area, read without the source's lock and with no credentials, so the
//! CSI controller can run it inside CreateVolume.
//!
//! [`create`] reads the source's settings and branch head (the base). It
//! refuses a source that is a fork or that has no fork remote. It then
//! makes the fork's state area: the identity in the fork's own config,
//! the source's settings files as its includes, the objects of base's
//! history copied in (no alternates, so a fork outlives its source), the
//! fork's history ref at base, and base's tree written into the fork's
//! backing tree before anything can mount it. It is idempotent per name:
//! a fork that already exists with the same source is returned as it is,
//! with the base it was created with.

use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use git2::{Oid, Repository};

use crate::config::{ConfigError, Settings, validate_name};
use crate::confined::Dir;
use crate::history::HistoryError;
use crate::lfs::fetch::LfsObjects;
use crate::lfs::{LfsError, ObjectCache, Pointer, Store};
use crate::paths::RepoPath;
use crate::restore::{self, NotLive, Pathspec, RecordPolicy, Request, RestoreError};
use crate::volume::{self, StateDir, VolumeError};

use super::identity::{ForkIdentity, PushTarget};

/// What to create.
#[derive(Debug, Clone, Copy)]
pub struct CreateRequest<'a> {
    /// The source's state area, only read: no lock, no commit, never
    /// paused.
    pub source: &'a StateDir,
    /// The fork's state area: new, or this fork's from an earlier try.
    pub fork: &'a StateDir,
    /// The fork's backing tree: empty, or this fork's from an earlier try.
    pub tree: &'a Path,
    /// The fork's volume name, unique at create: [`unique_name`] from the
    /// CLI, the class's `volumeName` (which names `{pvName}`) under CSI.
    pub name: &'a str,
    /// The fork's `include.path` entries: the files the source's settings
    /// come from (under CSI, its profile's config), read at every open.
    pub includes: &'a [PathBuf],
    /// Where the fork's history goes: the source's
    /// [`Settings::fork_refs`] (`agentfs.forkRefs`), which the CLI and the
    /// CSI controller pass. Written into the fork's identity, so it is
    /// fixed at create; a fork that already exists keeps its own.
    pub push_target: PushTarget,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("{} is not a volume's state area", path.display())]
    SourceNotFound { path: PathBuf },
    #[error("reading the source's settings: {0}")]
    SourceSettings(ConfigError),
    #[error("reading the source's history: {0}")]
    SourceUnreadable(HistoryError),
    #[error("volume {volume} has no history yet: there is nothing to fork")]
    SourceHasNoHistory { volume: String },
    #[error("volume {volume} is itself a fork; forks of forks are not supported")]
    SourceIsFork { volume: String },
    #[error(
        "volume {volume} has no fork remote: forks push to its patch remote, or to the one \
         remote with agentfsForkRemote = true (design §19.1)"
    )]
    NoForkRemote { volume: String },
    #[error(
        "volume {volume} sets {keys:?} in its own config; a fork reads settings only from \
         included files: move them to a fragment named by include.path"
    )]
    SourceSettingsNotIncluded { volume: String, keys: Vec<String> },
    #[error("{name:?} is not a valid fork name: {why}")]
    BadName { name: String, why: String },
    #[error("the fork's state area already belongs to {found}; nothing was changed")]
    Exists { found: String },
    #[error("LFS object {oid} of {path} is not in the source's cache; nothing to fork it from")]
    LfsObjectMissing { path: String, oid: String },
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Creates the fork `request` names, or returns the one an earlier call
/// created with the same name and source (see the module docs). Each step
/// is idempotent, so a retry (a CreateVolume retried after a crash)
/// resumes where the last try stopped.
pub fn create(request: &CreateRequest<'_>) -> Result<ForkIdentity, CreateError> {
    let name = request.name;
    validate_name("agentfs.volume", name).map_err(|e| CreateError::BadName {
        name: name.to_owned(),
        why: e.to_string(),
    })?;
    let source = read_source(request.source)?;
    let wanted = ForkIdentity {
        fork: name.to_owned(),
        source: source.volume.clone(),
        base: source.base,
        push_target: request.push_target,
    };
    volume::init_fork(request.fork, &wanted, request.includes).map_err(|e| match e {
        VolumeError::OtherVolume { found, .. } | VolumeError::OtherFork { found, .. } => {
            CreateError::Exists { found }
        }
        e => CreateError::Volume(e),
    })?;
    // The identity the fork has: an earlier try's base wins.
    let settings = Settings::load(&request.fork.git()).map_err(HistoryError::from)?;
    let identity = settings.fork.clone().ok_or_else(|| CreateError::Exists {
        found: settings.volume.clone(),
    })?;
    let fork = Repository::open_bare(request.fork.git()).map_err(HistoryError::from)?;
    let history_ref = identity.history_ref();
    let head = match fork.refname_to_id(&history_ref) {
        Ok(head) => Some(head),
        Err(e) if e.code() == git2::ErrorCode::NotFound => None,
        Err(e) => return Err(HistoryError::from(e).into()),
    };
    match head {
        None => {
            copy_objects(&source.repo, &fork, identity.base)?;
            copy_rules(request.source, request.fork)?;
            fork.reference(&history_ref, identity.base, false, "agentfs: fork created")
                .map_err(HistoryError::from)?;
            sync_parents(&request.fork.git(), &history_ref)?;
        }
        // The fork has recorded since it was created: its tree is its own.
        Some(head) if head != identity.base => return Ok(identity),
        Some(_) => {}
    }
    write_tree(request, &fork, &identity)?;
    Ok(identity)
}

/// The push target new forks of the source at `source` get: its
/// [`Settings::fork_refs`] (`agentfs.forkRefs`), read without its lock.
/// What the CLI and the CSI controller pass as
/// [`CreateRequest::push_target`].
pub fn push_target(source: &StateDir) -> Result<PushTarget, CreateError> {
    Ok(source_settings(source)?.fork_refs)
}

/// The source's settings, without its lock: [`CreateError::SourceNotFound`]
/// when `source` is not a volume's state area.
fn source_settings(source: &StateDir) -> Result<Settings, CreateError> {
    let git = source.git();
    if !git.join("config").is_file() {
        return Err(CreateError::SourceNotFound {
            path: source.root().to_path_buf(),
        });
    }
    crate::history::init_libgit2();
    Settings::load(&git).map_err(CreateError::SourceSettings)
}

/// The source as a fork reads it: settings and branch head, without its
/// lock.
struct Source {
    volume: String,
    base: Oid,
    repo: Repository,
}

fn read_source(state: &StateDir) -> Result<Source, CreateError> {
    let git = state.git();
    let settings = source_settings(state)?;
    let volume = settings.volume.clone();
    if settings.is_fork() {
        return Err(CreateError::SourceIsFork { volume });
    }
    if settings.fork_remote.is_none() {
        return Err(CreateError::NoForkRemote { volume });
    }
    let unreadable = |e: git2::Error| CreateError::SourceUnreadable(e.into());
    let repo = Repository::open_bare(&git).map_err(unreadable)?;
    let base = match repo.find_reference(&settings.branch_ref) {
        Ok(r) => r.peel_to_commit().map_err(unreadable)?.id(),
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(CreateError::SourceHasNoHistory { volume });
        }
        Err(e) => return Err(unreadable(e)),
    };
    Ok(Source { volume, base, repo })
}

/// Every object reachable from `base`, packed from the source into the
/// fork's own object store: no alternates, so a fork outlives its source.
fn copy_objects(source: &Repository, fork: &Repository, base: Oid) -> Result<(), CreateError> {
    let unreadable = |e: git2::Error| CreateError::SourceUnreadable(e.into());
    let mut walk = source.revwalk().map_err(unreadable)?;
    walk.push(base).map_err(unreadable)?;
    let mut pack = source.packbuilder().map_err(unreadable)?;
    pack.insert_walk(&mut walk).map_err(unreadable)?;
    let odb = fork.odb().map_err(HistoryError::from)?;
    let mut writer = odb.packwriter().map_err(HistoryError::from)?;
    let mut write_error = None;
    let packed = pack.foreach(|chunk| match writer.write_all(chunk) {
        Ok(()) => true,
        Err(e) => {
            write_error = Some(e);
            false
        }
    });
    if let Some(e) = write_error {
        return Err(e.into());
    }
    packed.map_err(unreadable)?;
    writer.commit().map_err(HistoryError::from)?;
    if !odb.exists(base) {
        return Err(CreateError::SourceUnreadable(HistoryError::Io(
            io::Error::other(format!(
                "commit {base} is not in the fork after copying the source's objects"
            )),
        )));
    }
    Ok(())
}

/// The source's `info/exclude` and `info/attributes` (design §4.3): the
/// fork records by the same operator rules.
fn copy_rules(source: &StateDir, fork: &StateDir) -> Result<(), CreateError> {
    for name in ["exclude", "attributes"] {
        let from = source.git().join("info").join(name);
        let to = fork.git().join("info").join(name);
        match std::fs::read(from) {
            Ok(content) => std::fs::write(to, content)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(CreateError::SourceUnreadable(HistoryError::Io(e))),
        }
    }
    Ok(())
}

/// Syncs the directories above the ref `name` in `git`.
pub(super) fn sync_parents(git: &Path, name: &str) -> io::Result<()> {
    let mut dir = git.join(name);
    while dir.pop() && dir.starts_with(git) {
        std::fs::File::open(&dir)?.sync_all()?;
    }
    Ok(())
}

/// Base's tree into the fork's backing tree ([`materialize`]), its LFS
/// content from the source's cache.
fn write_tree(
    request: &CreateRequest<'_>,
    fork: &Repository,
    identity: &ForkIdentity,
) -> Result<(), CreateError> {
    let objects = SourceObjects {
        source: ObjectCache::at(request.source.lfs())?,
        fork: ObjectCache::at(request.fork.lfs())?,
    };
    materialize(
        fork,
        &identity.history_ref(),
        identity.base,
        request.tree,
        &objects,
    )
    .map_err(restore_error)
}

/// `commit`'s tree into a new worktree's backing tree `tree` (empty, or
/// what an earlier try wrote), through the restore engine's confined
/// writer: beneath the tree's root, never through a symlink, and nothing
/// recorded, since the tree then equals history. `commit` must be on
/// `branch_ref` in `repo`; `objects` gives the LFS content. The one tree
/// writer of a fork ([`create`]) and a started volume
/// ([`super::start::start_from`]).
pub(super) fn materialize(
    repo: &Repository,
    branch_ref: &str,
    commit: Oid,
    tree: &Path,
    objects: &dyn LfsObjects,
) -> Result<(), RestoreError> {
    std::fs::create_dir_all(tree)?;
    let mut plan = match restore::plan(
        repo,
        branch_ref,
        &Request::Restore {
            spec: Pathspec::Root,
            commit: commit.to_string(),
        },
    ) {
        Ok(plan) => plan,
        // An empty tree: nothing to write.
        Err(RestoreError::NothingAtPath { .. }) => return Ok(()),
        Err(e) => return Err(e),
    };
    // From nothing: the tree is new, and holds at most what an earlier
    // try wrote.
    for step in &mut plan.steps {
        step.current = None;
    }
    let dir = Dir::open(tree)?;
    let plan = restore::check(plan, repo, &Materialize, objects, &dir, &NotLive, true)?;
    restore::apply(&plan, repo, objects, &dir, &NotLive, true)
        .map_err(|e| RestoreError::Io(io::Error::other(format!("writing {commit}'s tree: {e}"))))?;
    Ok(())
}

fn restore_error(e: RestoreError) -> CreateError {
    match e {
        RestoreError::LfsObject { path, oid, .. } => CreateError::LfsObjectMissing { path, oid },
        RestoreError::Git(e) => CreateError::History(e.into()),
        RestoreError::Io(e) => CreateError::Io(e),
        e => CreateError::Io(io::Error::other(format!("writing base's tree: {e}"))),
    }
}

/// History as it is: base's tree was recorded by the source, so nothing
/// in it is refused or re-stored.
struct Materialize;

impl RecordPolicy for Materialize {
    fn refuses(&self, _: &RepoPath, _: Option<&str>) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn store_for(&self, _: &RepoPath, _: u64) -> io::Result<Store> {
        Ok(Store::Blob)
    }

    fn check_content(&self, _: &RepoPath, _: &[u8], _: Store) -> Option<crate::checks::Refusal> {
        None
    }

    fn check_size(&self, _: &RepoPath, _: u64, _: Store) -> Option<crate::checks::Refusal> {
        None
    }
}

/// LFS content for the fork's tree: the fork's own cache, filled from the
/// source's (verified as it is copied). An object the source lacks is
/// missing: a fork never downloads.
struct SourceObjects {
    source: ObjectCache,
    fork: ObjectCache,
}

impl LfsObjects for SourceObjects {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if self.fork.contains(pointer) {
            return Ok(self.fork.path(&pointer.oid));
        }
        if !self.source.contains(pointer) {
            return Err(LfsError::local(format!(
                "LFS object {} is not in the source's cache",
                pointer.oid
            )));
        }
        let copied = self
            .fork
            .insert_file(&self.source.path(&pointer.oid))
            .map_err(|e| LfsError::local(format!("copying LFS object {}: {e}", pointer.oid)))?;
        if copied != *pointer {
            return Err(LfsError::local(format!(
                "LFS object {} in the source's cache does not match its pointer",
                pointer.oid
            )));
        }
        Ok(self.fork.path(&pointer.oid))
    }
}

/// `<prefix>-<8 random hex digits>`: a fork name the CLI makes unique on
/// the remote by construction. The result is a valid volume name, or
/// [`CreateError::BadName`].
pub fn unique_name(prefix: &str) -> Result<String, CreateError> {
    let mut bytes = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let name = format!("{prefix}-{:08x}", u32::from_be_bytes(bytes));
    validate_name("agentfs.volume", &name).map_err(|e| CreateError::BadName {
        name: name.clone(),
        why: e.to_string(),
    })?;
    Ok(name)
}

/// The keys a CLI source may set in its own config: its identity, and its
/// start point when it was started from another branch (F8 S1). Every
/// other setting must come from an include, which the fork reads too.
const OWN_KEYS: [&str; 5] = [
    "agentfs.volume",
    "agentfs.branch",
    "agentfs.start.remote",
    "agentfs.start.branch",
    "agentfs.start.commit",
];

/// Calls `f` with each entry of `config`: its name, lowercased as git
/// compares names, and its value.
fn for_each_entry(
    config: &git2::Config,
    mut f: impl FnMut(String, String),
) -> Result<(), git2::Error> {
    let mut entries = config.entries(None)?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let Ok(name) = entry.name() else { continue };
        let value = String::from_utf8_lossy(entry.value_bytes()).into_owned();
        f(name.to_ascii_lowercase(), value);
    }
    Ok(())
}

/// The source's `include.path` entries, the files a CLI fork reads its
/// settings from (design §11.5). Refuses
/// [`CreateError::SourceSettingsNotIncluded`] when the source's own config
/// sets any `agentfs.*`, `remote.*`, `lfs.*` or `http.*` key other than
/// `agentfs.volume` and `agentfs.branch`: the fork would not see it.
pub fn settings_files(source: &StateDir) -> Result<Vec<PathBuf>, CreateError> {
    let git = source.git();
    let path = git.join("config");
    if !path.is_file() {
        return Err(CreateError::SourceNotFound {
            path: source.root().to_path_buf(),
        });
    }
    crate::history::init_libgit2();
    let settings = Settings::load(&git).map_err(CreateError::SourceSettings)?;
    let git_error = |e: git2::Error| CreateError::SourceSettings(ConfigError::Git(e));
    // libgit2 expands includes in place, so a key the own file sets is one
    // whose value no included file gives.
    let mut included = std::collections::BTreeSet::new();
    for fragment in &settings.includes {
        let config = git2::Config::open(fragment).map_err(git_error)?;
        for_each_entry(&config, |key, value| {
            included.insert((key, value));
        })
        .map_err(git_error)?;
    }
    let config = git2::Config::open(&path).map_err(git_error)?;
    let mut keys = Vec::new();
    for_each_entry(&config, |key, value| {
        let setting = ["agentfs.", "remote.", "lfs.", "http."]
            .iter()
            .any(|p| key.starts_with(p));
        let own = setting
            && !OWN_KEYS.contains(&key.as_str())
            && !included.contains(&(key.clone(), value));
        if own {
            keys.push(key);
        }
    })
    .map_err(git_error)?;
    if !keys.is_empty() {
        keys.sort();
        keys.dedup();
        return Err(CreateError::SourceSettingsNotIncluded {
            volume: settings.volume,
            keys,
        });
    }
    Ok(settings.includes)
}
