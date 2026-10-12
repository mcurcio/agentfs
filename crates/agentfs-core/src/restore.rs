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
//! 2. [`check`] runs every refusal before anything is written: content
//!    the rules or checks would not record, anything in the tree that is
//!    not in history (never overridable: it would be lost), untracked
//!    files or directories in the way, and, overridable with `force`
//!    because nothing is lost, later changes to a reverted path that are
//!    already in history and files open for writing through a mount.
//!    Steps the tree already matches are dropped, so a re-run is a no-op.
//! 3. [`apply`] writes: removals deepest first (pruning directories they
//!    leave empty), then writes shallowest first. A regular file with one
//!    link is rewritten in place, keeping its inode, owner, other mode
//!    bits and extended attributes; a hard-linked file, a type change or a
//!    new file is a new inode (only the named path changes, as in git).
//!    New files and directories take the owner of their nearest existing
//!    parent when the process may chown, with mode 0644 or 0755.
//!
//! Recording is the caller's: in mount mode the writes go through the
//! mount and are recorded like any workload's; in direct mode the CLI
//! records the paths [`apply`] reports.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;

use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

use crate::checks::Refusal;
use crate::confined::{Current, Dir, Kind, Owner};
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

/// One path's state in history: a blob and how it is checked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub oid: Oid,
    pub kind: EntryKind,
}

/// One path to change.
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
            (Some(c), Some(t)) if c.oid == t.oid && c.kind != t.kind => "mode",
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
            Self::InvalidPath { .. } => "invalid_path",
            Self::Unrecordable { .. } => "unrecordable",
            Self::Unrecorded { .. } => "unrecorded",
            Self::InTheWay { .. } => "in_the_way",
            Self::ChangedSince { .. } => "changed_since",
            Self::OpenForWriting { .. } => "open_for_writing",
            Self::HistoryBehind => "history_behind",
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
            Self::InvalidPath { .. } => "give a path relative to the tree root, or `.`",
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
/// path. Submodule entries are skipped (agentfs never records them).
fn entries_under(
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    spec: &Pathspec,
) -> Result<BTreeMap<String, Entry>, git2::Error> {
    let mut out = BTreeMap::new();
    let Some(tree) = tree else {
        return Ok(out);
    };
    let (subtree, base) = match spec {
        Pathspec::Root => (tree.clone(), String::new()),
        Pathspec::Path(p) => match tree.get_path(Path::new(p.as_str())) {
            Ok(entry) => match entry.kind() {
                Some(ObjectType::Tree) => (repo.find_tree(entry.id())?, format!("{p}/")),
                Some(ObjectType::Blob) => {
                    if let Some(e) = entry_of(entry.filemode(), entry.id()) {
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
    subtree.walk(TreeWalkMode::PreOrder, |root, e| {
        if e.kind() == Some(ObjectType::Blob)
            && let (Ok(name), Some(entry)) = (e.name(), entry_of(e.filemode(), e.id()))
        {
            out.insert(format!("{base}{root}{name}"), entry);
        }
        TreeWalkResult::Ok
    })?;
    Ok(out)
}

fn entry_of(mode: i32, oid: Oid) -> Option<Entry> {
    let kind = match mode {
        MODE_SYMLINK => EntryKind::Symlink,
        MODE_EXECUTABLE => EntryKind::File { executable: true },
        0o100644 | 0o100664 => EntryKind::File { executable: false },
        _ => return None,
    };
    Some(Entry { oid, kind })
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
    let commit = repo
        .revparse_single(wanted)
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
                let (c, t) = (current.get(path).copied(), target.get(path).copied());
                if c != t {
                    steps.push(Step {
                        path: RepoPath::new(path.clone()).map_err(git_path)?,
                        current: c,
                        target: t,
                    });
                }
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
            let mut steps = Vec::new();
            let mut changed_since = Vec::new();
            for path in changed {
                let repo_path = RepoPath::new(path.clone()).map_err(git_path)?;
                let in_head = head_tree
                    .get_path(Path::new(path))
                    .ok()
                    .and_then(|e| entry_of(e.filemode(), e.id()));
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
    /// The content checks for a file holding `content`.
    fn check_content(&self, path: &RepoPath, content: &[u8]) -> Option<Refusal>;
}

impl RecordPolicy for crate::volume::Recorder {
    fn refuses(&self, path: &RepoPath, symlink_target: Option<&str>) -> io::Result<Option<String>> {
        self.would_record(path, symlink_target)
            .map(|r| r.map(|reason: SkipReason| reason.to_string()))
            .map_err(io::Error::other)
    }

    fn check_content(&self, path: &RepoPath, content: &[u8]) -> Option<Refusal> {
        self.checks().check_blob(path, content)
    }
}

/// Which files a running workload holds open for writing.
pub trait Liveness {
    fn open_for_writing(&self, path: &RepoPath) -> bool;
}

/// No mount: nothing can be open through one.
pub struct NotLive;

impl Liveness for NotLive {
    fn open_for_writing(&self, _: &RepoPath) -> bool {
        false
    }
}

/// Whether the tree state `current` is history's `entry` (`None`: nothing).
fn matches(
    repo: &Repository,
    current: &Current,
    entry: Option<&Entry>,
) -> Result<bool, RestoreError> {
    Ok(match (current, entry) {
        (Current::Absent, None) => true,
        (
            Current::File(f),
            Some(Entry {
                oid,
                kind: EntryKind::File { executable },
            }),
        ) => f.oid == *oid && f.executable == *executable,
        (
            Current::Symlink { target },
            Some(Entry {
                oid,
                kind: EntryKind::Symlink,
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

/// Runs every check of `plan` against the tree before anything is
/// written, and returns it without the steps the tree already matches.
pub fn check(
    plan: Plan,
    repo: &Repository,
    policy: &dyn RecordPolicy,
    tree: &Dir,
    live: &dyn Liveness,
    force: bool,
) -> Result<Plan, RestoreError> {
    let removed: BTreeSet<String> = plan
        .steps
        .iter()
        .filter(|s| s.target.is_none())
        .map(|s| s.path.to_string())
        .collect();
    // 1. Everything written must be recordable.
    for step in &plan.steps {
        let Some(target) = &step.target else { continue };
        let blob = repo.find_blob(target.oid)?;
        let symlink_target = match target.kind {
            EntryKind::Symlink => Some(String::from_utf8_lossy(blob.content()).into_owned()),
            EntryKind::File { .. } => None,
        };
        if let Some(reason) = policy.refuses(&step.path, symlink_target.as_deref())? {
            return Err(RestoreError::Unrecordable {
                path: step.path.to_string(),
                reason,
            });
        }
        if target.kind != EntryKind::Symlink
            && let Some(refusal) = policy.check_content(&step.path, blob.content())
        {
            return Err(RestoreError::Unrecordable {
                path: step.path.to_string(),
                reason: refusal.to_string(),
            });
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
        if matches(repo, &current, step.target.as_ref())? {
            continue; // Already done: a re-run is a no-op.
        }
        if !matches(repo, &current, step.current.as_ref())? {
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
}

/// Writes a checked `plan` into `tree`.
pub fn apply(plan: &Plan, repo: &Repository, tree: &Dir) -> Result<Applied, ApplyError> {
    let chown = crate::confined::can_chown();
    let mut applied = Applied::default();
    let fail = |applied: &mut Applied, path: &str, error: io::Error| ApplyError {
        applied: std::mem::take(applied),
        path: path.to_owned(),
        error,
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
        match tree.kind_at(path) {
            Ok(Kind::Absent) => {}
            Ok(_) => {
                if let Err(e) = tree.unlink(path) {
                    return Err(fail(&mut applied, path, e));
                }
            }
            Err(e) => return Err(fail(&mut applied, path, e)),
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
            return Err(fail(&mut applied, &dir, e));
        }
    }

    // Writes, shallowest first.
    let mut writes: Vec<&Step> = plan.steps.iter().filter(|s| s.target.is_some()).collect();
    writes.sort_by_key(|s| s.path.components().count());
    for step in writes {
        let path = step.path.as_str();
        let Some(target) = step.target else { continue };
        if let Err(e) = write_one(repo, tree, path, step, &target, chown) {
            return Err(fail(&mut applied, path, e));
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

fn write_one(
    repo: &Repository,
    tree: &Dir,
    path: &str,
    step: &Step,
    target: &Entry,
    chown: bool,
) -> io::Result<()> {
    let parent_owner = make_parents(tree, path, chown)?;
    let blob = repo.find_blob(target.oid).map_err(io::Error::other)?;
    let current = tree.current_entry(&step.path)?;
    match target.kind {
        EntryKind::File { executable } => {
            if let Current::File(f) = &current
                && f.nlink == 1
            {
                if f.oid != target.oid {
                    tree.write_in_place(path, blob.content())?;
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
            let owner = replace(tree, path, &current, parent_owner)?;
            let mode = if executable { 0o755 } else { 0o644 };
            tree.create_file(path, blob.content(), mode, chown.then_some(owner))
        }
        EntryKind::Symlink => {
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
