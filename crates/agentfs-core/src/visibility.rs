//! The visibility gate of the sandbox overlay: which names the overlay
//! shows, and where a name it creates or moves may go (design §1.1, §4).
//!
//! The sandbox overlay is a second mount point of a volume that shows only
//! the paths the operator's rules would record if they were new
//! (*recordable* paths). This module is its one judge. It is a pure
//! decision over the volume's [`Rules`]: it holds no rule set of its own,
//! never asks whether a path is tracked, never looks at reads or writes on
//! open files and knows nothing about mounts, callers or errno values.
//!
//! - **Show.** An existing file or symlink is shown when its path is
//!   recordable; a directory when the rules could record something beneath
//!   it ([`Rules::excludes_dir`] is false); a special file never. A path in
//!   an fsync-class group is shown only when every member of its group is
//!   recordable, so a group is shown whole or not at all.
//! - **Place a new name.** A recordable file goes to the shared tree; a file
//!   name the rules skip goes to the overlay's private tmp layer; a
//!   directory goes to the tree when the rules could record beneath it and
//!   is refused otherwise. Symlinks, hard links and special files are
//!   always refused.
//! - **Place a move.** A directory never moves (the caller answers
//!   "cross-device", so tools copy and delete file by file). A symlink never
//!   moves. A shown tree file moves only to a recordable name. A file in
//!   the tmp layer moves into the tree under a recordable name, and stays
//!   in the tmp layer otherwise.
//!
//! Invariants, which the unit tests pin:
//!
//! - **Fail closed.** A rules error answers [`Visibility::Hidden`] or
//!   [`Placement::Refused`], never [`Visibility::Shown`] or
//!   [`Placement::Tree`].
//! - **Existence-blind.** Every answer is a function of the path and the
//!   kind alone. The gate never asks whether a name exists, so the answer
//!   for a hidden name cannot depend on it. (Nested-repository detection,
//!   which reads the tree, can only hide.)
//! - **Rules are fixed while mounted.** The gate reads the [`Rules`] built
//!   when the volume opened; changing a rule file takes a remount. A volume
//!   that reads rules from its tree (`agentfs.respectTreeRules`) cannot
//!   have a gate ([`Gate::new`]): the sandbox could edit the rules behind
//!   its own view.

use std::sync::Arc;

use crate::paths::RepoPath;
use crate::rules::Rules;

/// What an existing entry is, as a lookup or a listing sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingKind {
    File,
    /// Shown when its path is recordable: readable and removable, never
    /// moved.
    Symlink,
    /// Shown when the rules could record something beneath it.
    Directory,
    /// A device, FIFO or socket: never shown.
    Special,
}

/// What a create-like operation would make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    File,
    Directory,
    /// Always refused.
    Symlink,
    /// Always refused, whatever the source: one content under two names.
    Hardlink,
    /// A device, FIFO or socket: always refused.
    Special,
}

/// The source of a rename, as the overlay shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// An entry of this kind in the shared tree.
    Tree(&'a RepoPath, ExistingKind),
    /// A file in the overlay's private tmp layer.
    TmpLayer(&'a RepoPath),
}

/// Whether an existing name is shown.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Hidden(Hidden),
}

/// Why a name is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    /// The rules would not record it (or, for a directory, anything
    /// beneath it).
    NotRecordable,
    /// The rules could not decide (an I/O error probing for a nested
    /// repository): hidden, failing closed.
    RulesError,
}

/// Where a new or moved name goes.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A recordable name in the shared tree.
    Tree,
    /// A file name the rules skip: in the overlay's private tmp layer,
    /// never recorded.
    TmpLayer,
    /// A directory move: refused so that tools fall back to copying and
    /// deleting file by file.
    CrossDevice,
    Refused(Denial),
}

/// Why a placement is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The kind is never placed: a symlink, hard link or special file, or a
    /// move of a symlink.
    Kind,
    /// A directory the rules could record nothing beneath, or a move of a
    /// tree file to a name the rules skip.
    NotRecordable,
    /// The source of a move is not shown.
    SourceHidden,
    /// The rules could not decide: refused, failing closed.
    RulesError,
}

/// Why a gate cannot be built for a volume.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// `agentfs.respectTreeRules = true`: rule files in the tree would be
    /// editable through the overlay.
    #[error(
        "the sandbox overlay needs agentfs.respectTreeRules = false: rule files read from the \
         tree could be edited through the overlay"
    )]
    TreeRules,
}

/// The visibility gate for one volume. `Send + Sync`; cheap to clone.
#[derive(Debug, Clone)]
pub struct Gate {
    rules: Arc<Rules>,
}

impl Gate {
    /// The gate over the volume's `rules`. Refuses a volume whose rules
    /// read tree rule files ([`GateError::TreeRules`]).
    pub fn new(rules: Arc<Rules>) -> Result<Self, GateError> {
        let _ = rules;
        todo!("sandbox overlay slice 1")
    }

    /// Whether any entry at `path` could be shown, whatever its kind:
    /// [`Visibility::Hidden`] means every kind is hidden, so a caller can
    /// answer for the name without looking at the tree.
    pub fn may_show(&self, path: &RepoPath) -> Visibility {
        let _ = (&self.rules, path);
        todo!("sandbox overlay slice 1")
    }

    /// Whether the existing entry at `path`, of kind `kind`, is shown.
    pub fn show(&self, path: &RepoPath, kind: ExistingKind) -> Visibility {
        let _ = (path, kind);
        todo!("sandbox overlay slice 1")
    }

    /// Where a new entry of kind `kind` at `path` goes.
    pub fn place_new(&self, path: &RepoPath, kind: NewKind) -> Placement {
        let _ = (path, kind);
        todo!("sandbox overlay slice 1")
    }

    /// Where a rename of `from` to `to` goes. A tree source that is not
    /// shown is refused ([`Denial::SourceHidden`]): the gate never trusts
    /// that its caller checked. An exchange is two moves, each of which
    /// must place in the tree.
    pub fn place_move(&self, from: Source<'_>, to: &RepoPath) -> Placement {
        let _ = (from, to);
        todo!("sandbox overlay slice 1")
    }
}
