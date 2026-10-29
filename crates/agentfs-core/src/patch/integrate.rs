//! Merging a patch branch onto the branch head (design §12, §19.2 item
//! 11; bead agentfs-mia.4).
//!
//! A patch is a branch: a line of single-parent commits whose oldest
//! commit's parent is on the volume's branch (the head or an ancestor of
//! it), its **base**. A one-commit patch, today's only shape, is a branch
//! of one whose base is its parent. [`PatchBranch`] is what the queue
//! reads from the patch's tip; an [`Integrator`] merges it onto the head
//! and writes the merged tree's objects, nothing else: no ref, no file of
//! the backing tree, no journal record. The queue then lands the merged
//! tree exactly as it lands a patch today (the restore engine, one atomic
//! record, one commit carrying `Agentfs-Patch: <tip>` and `Agentfs-Inbox`).
//!
//! The one implementation in this bead, [`GitMergeTree`], runs the `git`
//! program (AGENTS.md principle 4, design §17.3):
//! `git merge-tree --write-tree --merge-base=<base> <head> <tip>` in the
//! state area's git directory, with the environment `repack` uses (an empty
//! one, the repository's own config only), `-c core.fsync=objects`, so
//! the merged objects are durable before the record names them, and `-c
//! attr.tree=<the empty tree>`, so only the operator's
//! `$GIT_DIR/info/attributes` steers a merge (AGENTS.md principle 3; a bare
//! repository otherwise reads `HEAD:.gitattributes` since git 2.42). For a
//! one-commit patch that is the same three-way merge as libgit2's
//! cherry-pick against its parent, which the queue used before.
//! Bead agentfs-mia.3 adds the rebase strategy as a second implementation.

use std::path::PathBuf;

use git2::Oid;

use super::BlockReason;
use crate::paths::RepoPath;

/// A patch as the queue reads it from its tip (see the module docs).
///
/// Built only by `PatchBranch::read` (bead agentfs-mia.4), which refuses
/// what the queue does not land: no merge base with the head
/// ([`BlockReason::ParentNotInHistory`]), a commit with more than one
/// parent between base and tip ([`BlockReason::MergeCommit`]), a root
/// commit or a gitlink or a path agentfs does not record in the diff
/// ([`BlockReason::Unsupported`]), more than
/// [`super::MAX_PATCH_PATHS`] paths ([`BlockReason::TooLarge`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchBranch {
    /// The commit the inbox ref names.
    pub tip: Oid,
    /// The merge base of the tip and the head the branch was read
    /// against: the last commit of the volume's branch the patch builds
    /// on.
    pub base: Oid,
    /// The patch's own commits, base exclusive, tip inclusive, oldest
    /// first; never empty.
    pub commits: Vec<Oid>,
    /// The paths the diff from `base` to `tip` changes, sorted, each once.
    pub paths: Vec<RepoPath>,
}

/// Why a patch branch is not one the queue lands: the reason, a message
/// for the status and the alarm, and the paths involved, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: BlockReason,
    pub message: String,
    pub paths: Vec<String>,
}

/// What merging a patch onto the head gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrated {
    /// A clean merge: the merged tree, its objects written and durable.
    Clean { tree: Oid },
    /// The merge conflicts in these paths (sorted, each once). Nothing a
    /// conflict wrote is used.
    Conflict { paths: Vec<String> },
}

/// Why a merge could not be judged at all (never a conflict).
#[derive(Debug, thiserror::Error)]
pub enum IntegrateError {
    /// The `git` program could not be started.
    #[error("running git: {0}")]
    Spawn(#[source] std::io::Error),
    /// `git` exited with a status other than clean or conflicted.
    #[error("`git {args}` failed ({status}): {stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    /// `git` succeeded but its output is not what this build reads.
    #[error("reading the output of `git {args}`: {detail}")]
    BadOutput { args: String, detail: String },
    #[error(transparent)]
    Git(#[from] git2::Error),
}

/// Merges a patch branch onto a branch head (see the module docs).
///
/// Invariants every implementation keeps: it writes objects only (never a
/// ref, the backing tree or the journal); it is deterministic for the same
/// head, patch and repository config, so a run after a crash merges to the
/// same tree; a conflict is [`Integrated::Conflict`], never an error.
pub trait Integrator {
    /// Merges `patch` onto `head` (a commit of the volume's branch, of
    /// which `patch.base` is the head or an ancestor).
    fn integrate(&self, head: Oid, patch: &PatchBranch) -> Result<Integrated, IntegrateError>;
}

/// The merge strategy: `git merge-tree --write-tree` against the patch's
/// base (see the module docs).
#[derive(Debug, Clone)]
pub struct GitMergeTree {
    /// The `git` program, looked up in `repack::REPACK_PATH`.
    #[allow(dead_code)] // read by its `Integrator` impl (bead agentfs-mia.4)
    git: PathBuf,
    /// The state area's git directory.
    #[allow(dead_code)] // read by its `Integrator` impl (bead agentfs-mia.4)
    git_dir: PathBuf,
}
