//! Restore and revert: writing content from history back into the tree
//! (design §15).
//!
//! Three phases, all independent of how the tree is reached (the backing
//! tree directly when no mount runs, or the mount point of a running
//! mount), so `agentfs restore` and the daemon run the same code:
//!
//! 1. [`plan`] is pure history: what each path must become.
//!    - **Restore** follows `git restore --source=<commit> -- <path>`: every
//!      entry of the commit at or under the path is written, and every
//!      entry the branch head has there but the commit does not is
//!      removed. Untracked files are never touched.
//!    - **Revert** applies the inverse of a commit's diff against its
//!      first parent (the empty tree for a root commit), without rename
//!      detection. Merge commits are refused.
//!
//!    The commit must be on the volume's branch: the head or one of its
//!    ancestors.
//! 2. [`check`] runs every refusal before anything is written (a restore
//!    plan also lists the paths history already has right, so an unrecorded
//!    change there is refused rather than reported as matching): content
//!    the rules or checks would not record, anything in the tree that is
//!    not in history (never overridable: it would be lost), untracked
//!    files or directories in the way, and, overridable with `force`
//!    because nothing is lost, later changes to a reverted path that are
//!    already in history and files open for writing through a mount.
//!    Steps the tree and the branch head both already match are dropped,
//!    so a re-run is a no-op. Where the tree holds the target but the head
//!    does not (a restore stopped between writing and recording), the step
//!    is kept with nothing to write, so the caller records it (direct
//!    mode); through a mount, which records only what it sees written, it
//!    is refused as a change not in history.
//! 3. [`apply`] re-checks each path just before changing it (it must still
//!    hold what the check saw and, unless `force`, not be open for writing
//!    through a mount), then writes: removals deepest first (pruning
//!    directories they leave empty), then writes shallowest first. A
//!    regular file with one link is rewritten in place, keeping its inode,
//!    owner, other mode bits and extended attributes; a hard-linked file,
//!    a type change or a new file is a new inode (only the named path
//!    changes, as in git).
//!    New files and directories take the owner of their nearest existing
//!    parent when the process may chown, with mode 0644 or 0755.
//!
//! **Git LFS** (design §7): a pointer at a path its commit's managed
//! `.gitattributes` block lists is checked out as the object's content,
//! taken from the local cache or downloaded from a remote
//! ([`crate::lfs::fetch`]) during the check, before anything is written;
//! the tree's file is compared with it by size and SHA-256. The root
//! `.gitattributes` is compared and written without the managed block
//! (only history has it), and one holding nothing but the block is
//! agentfs's own, absent from the tree.
//!
//! Recording is the caller's: in mount mode the writes go through the
//! mount and are recorded like any workload's; in direct mode the CLI
//! records the paths [`apply`] reports.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read};
use std::path::Path;

use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

use crate::checks::Refusal;
use crate::confined::{Current, Dir, FileState, Kind, Owner};
use crate::lfs::fetch::LfsObjects;
use crate::lfs::{Pointer, Store, attributes};
use crate::paths::{PathError, RepoPath};
use crate::record::OperationKind;
use crate::rules::SkipReason;

const MODE_EXECUTABLE: i32 = 0o100755;
const MODE_SYMLINK: i32 = 0o120000;

/// Which paths a restore covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pathspec {
    /// The whole tree (`.`).
    Root,
    /// A file, symlink or directory.
    Path(RepoPath),
}

impl Pathspec {
    /// Parses an operator's path: `.` (or empty, or `/`) is the root;
    /// anything else must be a path git can store. A leading `./` and
    /// trailing `/` are dropped.
    pub fn parse(raw: &str) -> Result<Self, RestoreError> {
        let trimmed = raw.trim_end_matches('/');
        let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
        if trimmed.is_empty() || trimmed == "." {
            return Ok(Self::Root);
        }
        RepoPath::new(trimmed)
            .map(Self::Path)
            .map_err(|e| RestoreError::InvalidPath {
                path: raw.to_owned(),
                error: e,
            })
    }
}

impl fmt::Display for Pathspec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => f.write_str("."),
            Self::Path(p) => write!(f, "{p}"),
        }
    }
}

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Restore { spec: Pathspec, commit: String },
    Revert { commit: String },
}

/// What a history entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File { executable: bool },
    Symlink,
}

/// How a history entry's blob becomes the tree's content (design §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The blob is the content.
    Blob,
    /// The blob is a pointer to this LFS object, a managed path of its
    /// commit: the content comes from the object cache or a remote.
    Lfs(Pointer),
    /// The root `.gitattributes` with agentfs's managed block, which only
    /// history has: the content is the workload's own part, whose blob id
    /// is `user`.
    ManagedAttributes { user: Oid },
}

/// One path's state in history: a blob and how it is checked out.
///
/// Two entries are equal when they check out the same content and kind,
/// whatever blob holds it: a `.gitattributes` whose managed block alone
/// changed is the same file to the tree.
#[derive(Debug, Clone, Copy, Eq)]
pub struct Entry {
    pub oid: Oid,
    pub kind: EntryKind,
    pub stored: Stored,
}

/// What an entry checks out as.
#[derive(PartialEq, Eq)]
enum ContentId {
    Git(Oid),
    Lfs(Pointer),
}

impl Entry {
    fn content_id(&self) -> ContentId {
        match self.stored {
            Stored::Blob => ContentId::Git(self.oid),
            Stored::Lfs(p) => ContentId::Lfs(p),
            Stored::ManagedAttributes { user } => ContentId::Git(user),
        }
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.content_id() == other.content_id()
    }
}

/// One path to change. In a restore plan before [`check`], also every path
/// history already has right (`current == target`), so the tree is
/// verified there too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub path: RepoPath,
    /// The path in the branch head: what the tree must hold now.
    pub current: Option<Entry>,
    /// What the path becomes; `None` removes it.
    pub target: Option<Entry>,
}

impl Step {
    /// A one-word description for the plan listing.
    pub fn action(&self) -> &'static str {
        match (&self.current, &self.target) {
            (_, None) => "remove",
            (
                _,
                Some(Entry {
                    kind: EntryKind::Symlink,
                    ..
                }),
            ) => "symlink",
            (Some(c), Some(t)) if c.content_id() == t.content_id() && c.kind != t.kind => "mode",
            _ => "write",
        }
    }
}

/// What a restore or revert will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub kind: OperationKind,
    /// The commit restored from, or reverted.
    pub source: Oid,
    /// The branch head the plan was made against.
    pub head: Oid,
    /// The commit subject for the operation.
    pub summary: String,
    pub steps: Vec<Step>,
    /// Revert only: paths whose head state is not what the reverted commit
    /// left (changed again since, and recorded).
    pub changed_since: Vec<RepoPath>,
    /// Restore only: the deepest directory removals may prune up to
    /// (exclusive): the parent of the restored path, or the root.
    pub prune_floor: String,
}

/// Why a restore or revert cannot run, or failed.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("{commit:?} is not a commit in the history repository")]
    UnknownCommit { commit: String },
    #[error("commit {commit} is not on the volume's branch {branch}")]
    NotInHistory { commit: String, branch: String },
    #[error("commit {commit} is a merge; revert needs a single parent")]
    MergeCommit { commit: String },
    #[error("nothing at {path} in commit {commit} or in the branch head")]
    NothingAtPath { path: String, commit: String },
    #[error("commit {commit} changes nothing")]
    NothingToRevert { commit: String },
    #[error("a restore request must name a path (`.` for the whole tree)")]
    MissingPath,
    #[error("{path:?} is not a path agentfs records: {error}")]
    InvalidPath { path: String, error: PathError },
    #[error("{path} would not be recorded: {reason}")]
    Unrecordable { path: String, reason: String },
    #[error("{path} differs from history: it holds changes that are not recorded yet")]
    Unrecorded { path: String },
    #[error("{path} is in the way: {what} that is not in history")]
    InTheWay { path: String, what: &'static str },
    #[error("changed again since the reverted commit: {}", list(.paths))]
    ChangedSince { paths: Vec<String> },
    #[error("open for writing through the mount: {}", list(.paths))]
    OpenForWriting { paths: Vec<String> },
    #[error("history is behind the tree: earlier changes are not committed yet")]
    HistoryBehind,
    #[error("{path}: LFS object {oid} cannot be had: {error}")]
    LfsObject {
        path: String,
        oid: String,
        error: String,
    },
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn list(paths: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut out = paths
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOWN {
        out.push_str(&format!(" and {} more", paths.len() - SHOWN));
    }
    out
}

impl RestoreError {
    /// A stable name for the refusal, for the control protocol.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnknownCommit { .. } => "unknown_commit",
            Self::NotInHistory { .. } => "not_in_history",
            Self::MergeCommit { .. } => "merge_commit",
            Self::NothingAtPath { .. } => "nothing_at_path",
            Self::NothingToRevert { .. } => "nothing_to_revert",
            Self::MissingPath => "missing_path",
            Self::InvalidPath { .. } => "invalid_path",
            Self::Unrecordable { .. } => "unrecordable",
            Self::Unrecorded { .. } => "unrecorded",
            Self::InTheWay { .. } => "in_the_way",
            Self::ChangedSince { .. } => "changed_since",
            Self::OpenForWriting { .. } => "open_for_writing",
            Self::HistoryBehind => "history_behind",
            Self::LfsObject { .. } => "lfs_object",
            Self::Git(_) => "git",
            Self::Io(_) => "io",
        }
    }

    /// Whether this is a refusal (the request cannot run as asked), as
    /// opposed to a failure while running it.
    pub fn is_refusal(&self) -> bool {
        !matches!(self, Self::Git(_) | Self::Io(_))
    }

    /// What the operator can do about it.
    pub fn hint(&self) -> Option<&'static str> {
        Some(match self {
            Self::UnknownCommit { .. } => {
                "find the commit with `git --git-dir <state>/git log --follow -- <path>`"
            }
            Self::NotInHistory { .. } => {
                "only commits on the volume's branch can be restored; use plain git for others \
                 (docs/runbooks/recovery-and-undo.md)"
            }
            Self::MergeCommit { .. } => "restore the paths you need from the commit instead",
            Self::NothingAtPath { .. } => "check the path with `git show <commit> --stat`",
            Self::NothingToRevert { .. } => return None,
            Self::MissingPath | Self::InvalidPath { .. } => {
                "give a path relative to the tree root, or `.`"
            }
            Self::Unrecordable { .. } => {
                "the path rules or checks (info/exclude, -agentfs, size limits) keep it out of \
                 history; change them first, or restore it with plain git"
            }
            Self::Unrecorded { .. } => {
                "pause the workload and let its changes be recorded (or record the path with \
                 `agentfs record`), then retry; --force does not override this"
            }
            Self::InTheWay { .. } => {
                "move the untracked file or directory away (or record it), then retry"
            }
            Self::ChangedSince { .. } => {
                "these paths were changed again after that commit; --force reverts them anyway \
                 (the later changes stay in history)"
            }
            Self::OpenForWriting { .. } => {
                "pause the workload first; --force writes anyway (later writes by the workload \
                 are still recorded)"
            }
            Self::HistoryBehind => "check `agentfs status` and the CommitFailed alarms, then retry",
            Self::LfsObject { .. } => {
                "the content is stored as Git LFS and is neither in <state>/lfs/objects nor on \
                 a remote agentfs can reach; check the remotes' LFS endpoints and credentials \
                 (docs/runbooks/stuck-pushes.md)"
            }
            Self::Git(_) | Self::Io(_) => return None,
        })
    }

    /// Whether `--force` overrides it.
    pub fn overridable(&self) -> bool {
        matches!(
            self,
            Self::ChangedSince { .. } | Self::OpenForWriting { .. }
        )
    }
}

/// Every blob and symlink of `tree` under `prefix` (empty for all), by
/// path. Submodule entries are skipped (agentfs never records them). LFS
/// pointers at the tree's managed paths and a root `.gitattributes` with
/// a managed block are entries of their content (see [`Stored`]); a root
/// `.gitattributes` holding only the block is agentfs's own and is left
/// out.
fn entries_under(
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    spec: &Pathspec,
) -> Result<BTreeMap<String, Entry>, git2::Error> {
    let mut out = BTreeMap::new();
    let Some(tree) = tree else {
        return Ok(out);
    };
    let managed = attributes::managed_paths(repo, tree)?;
    let (subtree, base) = match spec {
        Pathspec::Root => (tree.clone(), String::new()),
        Pathspec::Path(p) => match tree.get_path(Path::new(p.as_str())) {
            Ok(entry) => match entry.kind() {
                Some(ObjectType::Tree) => (repo.find_tree(entry.id())?, format!("{p}/")),
                Some(ObjectType::Blob) => {
                    if let Some(e) =
                        entry_for(repo, p.as_str(), entry.filemode(), entry.id(), &managed)?
                    {
                        out.insert(p.as_str().to_owned(), e);
                    }
                    return Ok(out);
                }
                _ => return Ok(out),
            },
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(out),
            Err(e) => return Err(e),
        },
    };
    let mut blobs = Vec::new();
    subtree.walk(TreeWalkMode::PreOrder, |root, e| {
        if e.kind() == Some(ObjectType::Blob)
            && let Ok(name) = e.name()
        {
            blobs.push((format!("{base}{root}{name}"), e.filemode(), e.id()));
        }
        TreeWalkResult::Ok
    })?;
    for (path, mode, id) in blobs {
        if let Some(entry) = entry_for(repo, &path, mode, id, &managed)? {
            out.insert(path, entry);
        }
    }
    Ok(out)
}

/// The entry at `path` of `tree`, whose managed paths are `managed`.
fn entry_at(
    repo: &Repository,
    tree: &git2::Tree<'_>,
    managed: &BTreeSet<RepoPath>,
    path: &str,
) -> Result<Option<Entry>, git2::Error> {
    match tree.get_path(Path::new(path)) {
        Ok(e) if e.kind() == Some(ObjectType::Blob) => {
            entry_for(repo, path, e.filemode(), e.id(), managed)
        }
        Ok(_) => Ok(None),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The entry of the blob `oid` with `mode` at `path`, in a tree whose
/// managed paths are `managed`; `None` for what a restore never writes.
fn entry_for(
    repo: &Repository,
    path: &str,
    mode: i32,
    oid: Oid,
    managed: &BTreeSet<RepoPath>,
) -> Result<Option<Entry>, git2::Error> {
    let kind = match mode {
        MODE_SYMLINK => EntryKind::Symlink,
        MODE_EXECUTABLE => EntryKind::File { executable: true },
        0o100644 | 0o100664 => EntryKind::File { executable: false },
        _ => return Ok(None),
    };
    let mut stored = Stored::Blob;
    if kind != EntryKind::Symlink {
        if path == attributes::ROOT_GITATTRIBUTES {
            let blob = repo.find_blob(oid)?;
            let user = attributes::user_part(blob.content());
            if user.len() != blob.content().len() {
                if user.is_empty() {
                    return Ok(None);
                }
                stored = Stored::ManagedAttributes {
                    user: Oid::hash_object(ObjectType::Blob, &user)?,
                };
            }
        } else if !managed.is_empty() && RepoPath::new(path).is_ok_and(|p| managed.contains(&p)) {
            let blob = repo.find_blob(oid)?;
            if let Some(pointer) = Pointer::parse(blob.content()) {
                stored = Stored::Lfs(pointer);
            }
        }
    }
    Ok(Some(Entry { oid, kind, stored }))
}

fn short(commit: &git2::Commit<'_>) -> String {
    commit
        .as_object()
        .short_id()
        .ok()
        .and_then(|b| b.as_str().ok().map(str::to_owned))
        .unwrap_or_else(|| commit.id().to_string()[..7].to_owned())
}

/// Works out what `request` changes, from history alone (`branch_ref` is
/// the volume's branch).
pub fn plan(repo: &Repository, branch_ref: &str, request: &Request) -> Result<Plan, RestoreError> {
    let wanted = match request {
        Request::Restore { commit, .. } | Request::Revert { commit } => commit,
    };
    // `HEAD` is the volume's branch: the state area's own HEAD names no
    // branch agentfs writes.
    let spec = match wanted.strip_prefix("HEAD") {
        Some(rest) if rest.is_empty() || rest.starts_with(['~', '^', '@', ':']) => {
            format!("{branch_ref}{rest}")
        }
        _ => wanted.clone(),
    };
    let commit = repo
        .revparse_single(&spec)
        .and_then(|o| o.peel_to_commit())
        .map_err(|_| RestoreError::UnknownCommit {
            commit: wanted.clone(),
        })?;
    let not_in_history = || RestoreError::NotInHistory {
        commit: commit.id().to_string(),
        branch: branch_ref.to_owned(),
    };
    let head = match repo.find_reference(branch_ref) {
        Ok(r) => r.peel_to_commit()?,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Err(not_in_history()),
        Err(e) => return Err(e.into()),
    };
    if head.id() != commit.id() && !repo.graph_descendant_of(head.id(), commit.id())? {
        return Err(not_in_history());
    }
    let head_tree = head.tree()?;
    match request {
        Request::Restore { spec, .. } => {
            let target = entries_under(repo, Some(&commit.tree()?), spec)?;
            let current = entries_under(repo, Some(&head_tree), spec)?;
            if target.is_empty() && current.is_empty() {
                return Err(RestoreError::NothingAtPath {
                    path: spec.to_string(),
                    commit: commit.id().to_string(),
                });
            }
            let paths: BTreeSet<&String> = target.keys().chain(current.keys()).collect();
            let mut steps = Vec::new();
            for path in paths {
                // Paths history already has right are kept as steps too, so
                // the check finds unrecorded changes in the tree there
                // instead of reporting that everything matches.
                steps.push(Step {
                    path: RepoPath::new(path.clone()).map_err(git_path)?,
                    current: current.get(path).copied(),
                    target: target.get(path).copied(),
                });
            }
            let prune_floor = match spec {
                Pathspec::Root => String::new(),
                Pathspec::Path(p) => p
                    .as_str()
                    .rsplit_once('/')
                    .map_or(String::new(), |(parent, _)| parent.to_owned()),
            };
            Ok(Plan {
                kind: OperationKind::Restore,
                source: commit.id(),
                head: head.id(),
                summary: format!("restore {spec} from {}", short(&commit)),
                steps,
                changed_since: Vec::new(),
                prune_floor,
            })
        }
        Request::Revert { .. } => {
            if commit.parent_count() > 1 {
                return Err(RestoreError::MergeCommit {
                    commit: commit.id().to_string(),
                });
            }
            let parent_tree = match commit.parent_count() {
                0 => None,
                _ => Some(commit.parent(0)?.tree()?),
            };
            let before = entries_under(repo, parent_tree.as_ref(), &Pathspec::Root)?;
            let after = entries_under(repo, Some(&commit.tree()?), &Pathspec::Root)?;
            let changed: BTreeSet<&String> = before
                .keys()
                .chain(after.keys())
                .filter(|p| before.get(*p) != after.get(*p))
                .collect();
            if changed.is_empty() {
                return Err(RestoreError::NothingToRevert {
                    commit: commit.id().to_string(),
                });
            }
            let head_managed = attributes::managed_paths(repo, &head_tree)?;
            let mut steps = Vec::new();
            let mut changed_since = Vec::new();
            for path in changed {
                let repo_path = RepoPath::new(path.clone()).map_err(git_path)?;
                let in_head = entry_at(repo, &head_tree, &head_managed, path)?;
                let target = before.get(path).copied();
                if in_head != after.get(path).copied() && in_head != target {
                    changed_since.push(repo_path.clone());
                }
                if in_head != target {
                    steps.push(Step {
                        path: repo_path,
                        current: in_head,
                        target,
                    });
                }
            }
            let subject = commit.summary().ok().flatten().unwrap_or("").to_owned();
            Ok(Plan {
                kind: OperationKind::Revert,
                source: commit.id(),
                head: head.id(),
                summary: format!("revert {}: {subject}", short(&commit)),
                steps,
                changed_since,
                prune_floor: String::new(),
            })
        }
    }
}

/// A history path that is not a valid [`RepoPath`] (never written by
/// agentfs).
fn git_path(e: PathError) -> RestoreError {
    RestoreError::Git(git2::Error::from_str(&format!(
        "history holds a path agentfs cannot write: {e}"
    )))
}

/// Whether a path would be recorded: the volume's rules and checks
/// ([`crate::volume::Recorder`] implements it).
pub trait RecordPolicy {
    /// Why a write at `path` (a file, or a symlink to `symlink_target`)
    /// would not be recorded, if it would not.
    fn refuses(&self, path: &RepoPath, symlink_target: Option<&str>) -> io::Result<Option<String>>;
    /// Where a file at `path` holding `size` bytes would be stored when
    /// recorded (design §7).
    fn store_for(&self, path: &RepoPath, size: u64) -> io::Result<Store>;
    /// The content checks for a file holding `content`, stored in `store`.
    fn check_content(&self, path: &RepoPath, content: &[u8], store: Store) -> Option<Refusal>;
    /// The checks for a file of `size` bytes whose content is not read (an
    /// LFS object), stored in `store`.
    fn check_size(&self, path: &RepoPath, size: u64, store: Store) -> Option<Refusal>;
    /// Every member of the fsync-class group `path` belongs to (design
    /// §8), `path` included; empty when it is in no group.
    fn group_of(&self, _path: &RepoPath) -> Vec<RepoPath> {
        Vec::new()
    }
}

impl RecordPolicy for crate::volume::Recorder {
    fn group_of(&self, path: &RepoPath) -> Vec<RepoPath> {
        self.fsync_group(path)
            .map(|g| g.members)
            .unwrap_or_default()
    }

    fn refuses(&self, path: &RepoPath, symlink_target: Option<&str>) -> io::Result<Option<String>> {
        self.would_record(path, symlink_target)
            .map(|r| r.map(|reason: SkipReason| reason.to_string()))
            .map_err(io::Error::other)
    }

    fn store_for(&self, path: &RepoPath, size: u64) -> io::Result<Store> {
        crate::volume::Recorder::store_for(self, path, size).map_err(io::Error::other)
    }

    fn check_content(&self, path: &RepoPath, content: &[u8], store: Store) -> Option<Refusal> {
        self.checks().check_blob_stored(path, content, store)
    }

    fn check_size(&self, path: &RepoPath, size: u64, store: Store) -> Option<Refusal> {
        self.checks().check_size(path, size, store)
    }
}

/// Which files a running workload holds open for writing, and who records
/// the tree's changes.
pub trait Liveness {
    fn open_for_writing(&self, path: &RepoPath) -> bool;
    /// Whether the tree records its own writes (a mount: the FUSE layer
    /// records them), rather than the caller recording what [`apply`]
    /// reports (the backing tree, written directly).
    fn records_its_writes(&self) -> bool;
}

/// No mount: nothing can be open through one, and the caller records.
pub struct NotLive;

impl Liveness for NotLive {
    fn open_for_writing(&self, _: &RepoPath) -> bool {
        false
    }

    fn records_its_writes(&self) -> bool {
        false
    }
}

/// Whether the regular file `f` at `path` holds `entry`'s content
/// (whatever its mode). LFS content is compared by size, then SHA-256.
fn same_content(tree: &Dir, path: &str, f: &FileState, entry: &Entry) -> io::Result<bool> {
    Ok(match entry.stored {
        Stored::Blob => f.oid == entry.oid,
        Stored::ManagedAttributes { user } => f.oid == user,
        Stored::Lfs(p) => f.size == p.size && tree.sha256_at(path)?.0 == p.oid,
    })
}

/// Whether the tree state `current` at `path` is history's `entry`
/// (`None`: nothing).
fn matches(
    repo: &Repository,
    tree: &Dir,
    path: &str,
    current: &Current,
    entry: Option<&Entry>,
) -> Result<bool, RestoreError> {
    Ok(match (current, entry) {
        (Current::Absent, None) => true,
        (
            Current::File(f),
            Some(
                e @ Entry {
                    kind: EntryKind::File { executable },
                    ..
                },
            ),
        ) => f.executable == *executable && same_content(tree, path, f, e)?,
        (
            Current::Symlink { target },
            Some(Entry {
                oid,
                kind: EntryKind::Symlink,
                ..
            }),
        ) => repo.find_blob(*oid)?.content() == target.as_slice(),
        _ => false,
    })
}

fn describe(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "a file",
        Kind::Symlink => "a symlink",
        Kind::Dir => "a directory",
        Kind::Other => "a special file",
        Kind::Absent => "nothing",
    }
}

/// The proper ancestors of `path`, shallowest first.
fn ancestors(path: &str) -> Vec<&str> {
    path.match_indices('/').map(|(i, _)| &path[..i]).collect()
}

/// Brings every member of an fsync-class group (design §8) a step touches
/// into the plan, with its state in the same commit: a restore of
/// `app.db` restores `app.db-wal` and `app.db-journal` with it, and a
/// revert of a commit that changed one member puts every member back to
/// the parent's version, so the tree never pairs one version's database
/// with another's `-wal` or `-journal`. A member that has changed since
/// the reverted commit counts as changed since (`--force`). A member
/// neither history nor the target has is still checked: an untracked
/// file there (a hot journal) refuses the plan.
fn expand_groups(
    mut plan: Plan,
    repo: &Repository,
    policy: &dyn RecordPolicy,
) -> Result<Plan, RestoreError> {
    let have: BTreeSet<RepoPath> = plan.steps.iter().map(|s| s.path.clone()).collect();
    let extra: BTreeSet<RepoPath> = plan
        .steps
        .iter()
        .flat_map(|s| policy.group_of(&s.path))
        .filter(|m| !have.contains(m))
        .collect();
    if extra.is_empty() {
        return Ok(plan);
    }
    let head_tree = repo.find_commit(plan.head)?.tree()?;
    let source = repo.find_commit(plan.source)?;
    let (target_tree, after_tree) = match plan.kind {
        OperationKind::Restore => (Some(source.tree()?), None),
        OperationKind::Revert => {
            let parent = match source.parent_count() {
                0 => None,
                _ => Some(source.parent(0)?.tree()?),
            };
            (parent, Some(source.tree()?))
        }
    };
    let managed = |tree: &git2::Tree<'_>| attributes::managed_paths(repo, tree);
    let head_managed = managed(&head_tree)?;
    let target_managed = match &target_tree {
        Some(t) => managed(t)?,
        None => BTreeSet::new(),
    };
    for path in extra {
        let current = entry_at(repo, &head_tree, &head_managed, path.as_str())?;
        let target = match &target_tree {
            Some(t) => entry_at(repo, t, &target_managed, path.as_str())?,
            None => None,
        };
        if let Some(after_tree) = &after_tree {
            let after = entry_at(repo, after_tree, &managed(after_tree)?, path.as_str())?;
            if current != after && current != target {
                plan.changed_since.push(path.clone());
            }
        }
        plan.steps.push(Step {
            path,
            current,
            target,
        });
    }
    plan.steps.sort_by(|a, b| a.path.cmp(&b.path));
    plan.changed_since.sort();
    Ok(plan)
}

/// Runs every check of `plan` against the tree before anything is
/// written, and returns it without the steps the tree already matches.
/// Every LFS object a step writes is made available in the cache first
/// (`objects`), so a missing one refuses the plan before any write.
pub fn check(
    plan: Plan,
    repo: &Repository,
    policy: &dyn RecordPolicy,
    objects: &dyn LfsObjects,
    tree: &Dir,
    live: &dyn Liveness,
    force: bool,
) -> Result<Plan, RestoreError> {
    let plan = expand_groups(plan, repo, policy)?;
    let removed: BTreeSet<String> = plan
        .steps
        .iter()
        .filter(|s| s.target.is_none())
        .map(|s| s.path.to_string())
        .collect();
    // 1. Everything written must be recordable, and its content at hand.
    for step in &plan.steps {
        let Some(target) = &step.target else { continue };
        if step.current.as_ref() == Some(target) {
            continue; // Nothing is written there.
        }
        let unrecordable = |reason: String| RestoreError::Unrecordable {
            path: step.path.to_string(),
            reason,
        };
        let blob = repo.find_blob(target.oid)?;
        let symlink_target = match target.kind {
            EntryKind::Symlink => Some(String::from_utf8_lossy(blob.content()).into_owned()),
            EntryKind::File { .. } => None,
        };
        if let Some(reason) = policy.refuses(&step.path, symlink_target.as_deref())? {
            return Err(unrecordable(reason));
        }
        if target.kind == EntryKind::Symlink {
            continue;
        }
        let refusal = match target.stored {
            Stored::Blob => {
                let store = policy.store_for(&step.path, blob.size() as u64)?;
                policy.check_content(&step.path, blob.content(), store)
            }
            Stored::ManagedAttributes { .. } => {
                let user = attributes::user_part(blob.content());
                policy.check_content(&step.path, &user, Store::Blob)
            }
            Stored::Lfs(pointer) => {
                objects
                    .ensure(&pointer)
                    .map_err(|e| RestoreError::LfsObject {
                        path: step.path.to_string(),
                        oid: pointer.oid.to_string(),
                        error: e.to_string(),
                    })?;
                let store = policy.store_for(&step.path, pointer.size)?;
                policy.check_size(&step.path, pointer.size, store)
            }
        };
        if let Some(refusal) = refusal {
            return Err(unrecordable(refusal.to_string()));
        }
    }
    // 2. The tree must hold what history says, and nothing untracked may
    //    be in the way.
    let mut kept = Vec::with_capacity(plan.steps.len());
    for step in plan.steps {
        let path = step.path.as_str();
        let mut blocked = false;
        for ancestor in ancestors(path) {
            match tree.kind_at(ancestor)? {
                Kind::Dir => continue,
                Kind::Absent => break,
                kind => {
                    if removed.contains(ancestor) {
                        // Its own step checks it; it is gone before writes.
                        blocked = true;
                        break;
                    }
                    return Err(RestoreError::InTheWay {
                        path: ancestor.to_owned(),
                        what: describe(kind),
                    });
                }
            }
        }
        let current = if blocked {
            Current::Absent
        } else {
            tree.current_entry(&step.path)?
        };
        if matches(repo, tree, path, &current, step.target.as_ref())? {
            if step.current == step.target {
                continue; // Already done, in the tree and in history.
            }
            // The tree already holds the target but history does not: an
            // earlier restore stopped between writing and recording, or the
            // content was put there by hand. Recording it loses nothing
            // (the target is in history already). Directly, the step is
            // kept, with nothing to write, so the caller records it; through
            // a mount, which records only what it sees written, it is a
            // change not in history yet.
            if live.records_its_writes() {
                return Err(RestoreError::Unrecorded {
                    path: path.to_owned(),
                });
            }
            kept.push(step);
            continue;
        }
        if !matches(repo, tree, path, &current, step.current.as_ref())? {
            match (&current, step.current) {
                (Current::Dir, _) => {
                    // A directory where a file goes: only if all of it
                    // is being removed.
                    let under = tree.list_tree(path)?;
                    if step.target.is_none() || under.iter().any(|p| !removed.contains(p.as_str()))
                    {
                        return Err(RestoreError::InTheWay {
                            path: path.to_owned(),
                            what: "a directory with files",
                        });
                    }
                }
                (Current::Other, _) => {
                    return Err(RestoreError::InTheWay {
                        path: path.to_owned(),
                        what: "a special file",
                    });
                }
                (_, None) => {
                    return Err(RestoreError::InTheWay {
                        path: path.to_owned(),
                        what: match current {
                            Current::Symlink { .. } => "a symlink",
                            _ => "a file",
                        },
                    });
                }
                (_, Some(_)) => {
                    return Err(RestoreError::Unrecorded {
                        path: path.to_owned(),
                    });
                }
            }
        }
        kept.push(step);
    }
    // 3. What --force overrides: nothing is lost either way.
    if !force && !plan.changed_since.is_empty() {
        return Err(RestoreError::ChangedSince {
            paths: plan.changed_since.iter().map(ToString::to_string).collect(),
        });
    }
    let open: Vec<String> = kept
        .iter()
        .filter(|s| live.open_for_writing(&s.path))
        .map(|s| s.path.to_string())
        .collect();
    if !force && !open.is_empty() {
        return Err(RestoreError::OpenForWriting { paths: open });
    }
    Ok(Plan {
        steps: kept,
        ..plan
    })
}

/// What [`apply`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Files and symlinks written (content or mode).
    pub written: Vec<RepoPath>,
    /// Paths removed.
    pub removed: Vec<RepoPath>,
}

/// [`apply`] stopped at `path`; `applied` is what it changed before.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {error}")]
pub struct ApplyError {
    pub applied: Applied,
    pub path: String,
    pub error: io::Error,
    /// Whether the re-check passed and [`apply`] had begun changing `path`
    /// (so it may be left part written).
    pub started: bool,
}

impl ApplyError {
    /// The failed step's path if [`apply`] had begun changing it and the
    /// tree no longer holds history's entry there (a write cut short): the
    /// caller records it as it is, so the tree matches history again and a
    /// re-run finishes the job. `None` when nothing there changed, or it
    /// failed its re-check (someone else changed it).
    pub fn left_changed(&self, plan: &Plan, repo: &Repository, tree: &Dir) -> Option<RepoPath> {
        if !self.started {
            return None;
        }
        let step = plan.steps.iter().find(|s| s.path.as_str() == self.path)?;
        let current = tree.current_entry(&step.path).ok()?;
        let unchanged =
            matches(repo, tree, &self.path, &current, step.current.as_ref()).unwrap_or(false);
        (!unchanged).then(|| step.path.clone())
    }
}

/// Why [`apply`] stopped before changing a path: it no longer holds what
/// [`check`] saw, or a workload opened it for writing since.
fn changed_since_check(what: &str) -> io::Error {
    io::Error::other(format!(
        "{what} since the check, so it was left alone; run the command again to check anew"
    ))
}

/// Re-checks `step` just before it is applied: the tree must hold what
/// [`check`] accepted (history's current entry, or already the target; a
/// directory or nothing only where the check allowed it), and, unless
/// `force`, no workload may have opened it for writing since.
fn recheck(
    repo: &Repository,
    tree: &Dir,
    step: &Step,
    live: &dyn Liveness,
    force: bool,
) -> io::Result<Current> {
    if !force && live.open_for_writing(&step.path) {
        return Err(changed_since_check("opened for writing through the mount"));
    }
    let current = tree.current_entry(&step.path)?;
    let accepted = match &current {
        // A directory in the way was checked to hold only removed paths;
        // removing it fails if anything else appeared.
        Current::Dir => step.target.is_some(),
        // Absent where history has it, for a removal, is done already.
        Current::Absent if step.target.is_none() => true,
        _ => {
            let path = step.path.as_str();
            let is = |entry: Option<&Entry>| {
                matches(repo, tree, path, &current, entry).map_err(io::Error::other)
            };
            is(step.current.as_ref())? || is(step.target.as_ref())?
        }
    };
    if !accepted {
        return Err(changed_since_check("changed"));
    }
    Ok(current)
}

/// Writes a checked `plan` into `tree`. Each path is re-checked just
/// before it changes (see [`recheck`]); a workload can still write a file
/// in the moment between that and the write, which is why a live restore
/// should pause the workload. LFS content comes from `objects` (the cache,
/// filled by [`check`]).
pub fn apply(
    plan: &Plan,
    repo: &Repository,
    objects: &dyn LfsObjects,
    tree: &Dir,
    live: &dyn Liveness,
    force: bool,
) -> Result<Applied, ApplyError> {
    let chown = crate::confined::can_chown();
    let mut applied = Applied::default();
    let fail = |applied: &mut Applied, path: &str, error: io::Error, started: bool| ApplyError {
        applied: std::mem::take(applied),
        path: path.to_owned(),
        error,
        started,
    };
    // Directories a write needs are never pruned.
    let needed: BTreeSet<&str> = plan
        .steps
        .iter()
        .filter(|s| s.target.is_some())
        .flat_map(|s| ancestors(s.path.as_str()))
        .collect();

    // Removals, deepest first, then the directories they leave empty.
    let mut removals: Vec<&Step> = plan.steps.iter().filter(|s| s.target.is_none()).collect();
    removals.sort_by_key(|s| std::cmp::Reverse(s.path.components().count()));
    let mut emptied: BTreeSet<String> = BTreeSet::new();
    for step in removals {
        let path = step.path.as_str();
        match recheck(repo, tree, step, live, force) {
            Ok(Current::Absent) => {}
            Ok(_) => {
                if let Err(e) = tree.unlink(path) {
                    return Err(fail(&mut applied, path, e, true));
                }
            }
            Err(e) => return Err(fail(&mut applied, path, e, false)),
        }
        applied.removed.push(step.path.clone());
        for ancestor in ancestors(path) {
            let below_floor = plan.prune_floor.is_empty()
                || ancestor
                    .strip_prefix(plan.prune_floor.as_str())
                    .is_some_and(|r| r.starts_with('/'));
            if below_floor && !needed.contains(ancestor) {
                emptied.insert(ancestor.to_owned());
            }
        }
    }
    let mut emptied: Vec<String> = emptied.into_iter().collect();
    emptied.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for dir in emptied {
        // Only directories left empty: untracked files keep theirs.
        if tree.kind_at(&dir).is_ok_and(|k| k == Kind::Dir)
            && let Err(e) = tree.rmdir(&dir)
        {
            return Err(fail(&mut applied, &dir, e, false));
        }
    }

    // Writes, shallowest first.
    let mut writes: Vec<&Step> = plan.steps.iter().filter(|s| s.target.is_some()).collect();
    writes.sort_by_key(|s| s.path.components().count());
    for step in writes {
        let path = step.path.as_str();
        let Some(target) = step.target else { continue };
        let current = match recheck(repo, tree, step, live, force) {
            Ok(current) => current,
            Err(e) => return Err(fail(&mut applied, path, e, false)),
        };
        if let Err(e) = write_one(repo, objects, tree, path, current, &target, chown) {
            return Err(fail(&mut applied, path, e, true));
        }
        applied.written.push(step.path.clone());
    }
    Ok(applied)
}

/// Creates the missing ancestors of `path`, each owned like its nearest
/// existing parent when `chown`. Returns the owner of `path`'s parent.
fn make_parents(tree: &Dir, path: &str, chown: bool) -> io::Result<Owner> {
    let mut owner = tree.owner_of("")?;
    for ancestor in ancestors(path) {
        match tree.kind_at(ancestor)? {
            Kind::Dir => owner = tree.owner_of(ancestor)?,
            Kind::Absent => tree.create_dir(ancestor, chown.then_some(owner))?,
            kind => {
                return Err(io::Error::other(format!(
                    "{ancestor} is {}, not a directory",
                    describe(kind)
                )));
            }
        }
    }
    Ok(owner)
}

/// A file entry's content, ready to be streamed into the tree.
enum Content<'r> {
    Blob(git2::Blob<'r>),
    Bytes(Vec<u8>),
    /// A cached LFS object.
    Object(std::fs::File, u64),
}

impl Content<'_> {
    fn load<'r>(
        repo: &'r Repository,
        objects: &dyn LfsObjects,
        entry: &Entry,
    ) -> io::Result<Content<'r>> {
        Ok(match entry.stored {
            Stored::Blob => Content::Blob(repo.find_blob(entry.oid).map_err(io::Error::other)?),
            Stored::ManagedAttributes { .. } => {
                let blob = repo.find_blob(entry.oid).map_err(io::Error::other)?;
                Content::Bytes(attributes::user_part(blob.content()))
            }
            Stored::Lfs(pointer) => {
                let path = objects.ensure(&pointer).map_err(io::Error::other)?;
                let file = std::fs::File::open(path)?;
                if file.metadata()?.len() != pointer.size {
                    return Err(io::Error::other(format!(
                        "the cached LFS object {} is not {} bytes",
                        pointer.oid, pointer.size
                    )));
                }
                Content::Object(file, pointer.size)
            }
        })
    }

    fn len(&self) -> u64 {
        match self {
            Content::Blob(b) => b.content().len() as u64,
            Content::Bytes(b) => b.len() as u64,
            Content::Object(_, len) => *len,
        }
    }

    fn reader(&mut self) -> Box<dyn Read + '_> {
        match self {
            Content::Blob(b) => Box::new(b.content()),
            Content::Bytes(b) => Box::new(&b[..]),
            Content::Object(f, _) => Box::new(f),
        }
    }
}

fn write_one(
    repo: &Repository,
    objects: &dyn LfsObjects,
    tree: &Dir,
    path: &str,
    current: Current,
    target: &Entry,
    chown: bool,
) -> io::Result<()> {
    let parent_owner = make_parents(tree, path, chown)?;
    match target.kind {
        EntryKind::File { executable } => {
            if let Current::File(f) = &current
                && f.nlink == 1
            {
                if !same_content(tree, path, f, target)? {
                    let mut content = Content::load(repo, objects, target)?;
                    let len = content.len();
                    tree.write_in_place_from(path, &mut content.reader(), len)?;
                }
                if f.executable != executable {
                    let mode = if executable {
                        f.mode | ((f.mode & 0o444) >> 2)
                    } else {
                        f.mode & !0o111
                    };
                    tree.chmod(path, mode)?;
                }
                return Ok(());
            }
            let mut content = Content::load(repo, objects, target)?;
            let owner = replace(tree, path, &current, parent_owner)?;
            let mode = if executable { 0o755 } else { 0o644 };
            let len = content.len();
            tree.create_file_from(
                path,
                &mut content.reader(),
                len,
                mode,
                chown.then_some(owner),
            )
        }
        EntryKind::Symlink => {
            let blob = repo.find_blob(target.oid).map_err(io::Error::other)?;
            if let Current::Symlink { target } = &current
                && target.as_slice() == blob.content()
            {
                return Ok(());
            }
            let owner = replace(tree, path, &current, parent_owner)?;
            tree.create_symlink(path, blob.content(), chown.then_some(owner))
        }
    }
}

/// Removes what is at `path` so it can be created anew; returns the owner
/// the new inode gets: the replaced file's, else `parent`.
fn replace(tree: &Dir, path: &str, current: &Current, parent: Owner) -> io::Result<Owner> {
    match current {
        Current::Absent => Ok(parent),
        Current::Dir => {
            // Checked to be empty of anything but removed paths, which are
            // gone by now; pruning may have left the directory itself.
            tree.rmdir(path)?;
            Ok(parent)
        }
        Current::File(f) => {
            tree.unlink(path)?;
            Ok((f.uid, f.gid))
        }
        Current::Symlink { .. } | Current::Other => {
            let owner = tree.owner_of(path)?;
            tree.unlink(path)?;
            Ok(owner)
        }
    }
}

#[cfg(test)]
mod tests;
