//! What the journal stores: one record per change boundary (design §3.1).
//!
//! Records hold absolute state ("this path now has this content"), never
//! deltas, so applying a record twice gives the same tree (design §3.3).

use serde::{Deserialize, Serialize};

use crate::paths::RepoPath;
use crate::pending::PendingId;

/// The FUSE event (or other cause) that ended a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Close,
    Rename,
    Unlink,
    Rmdir,
    Link,
    Symlink,
    Setattr,
    Fsync,
    /// Recorded late, when the last handle or mapping went away.
    Release,
    /// Recorded late, by the periodic sweep of files that stay open.
    Sweep,
    /// An operator restore (design §15).
    Restore,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Rename => "rename",
            Self::Unlink => "unlink",
            Self::Rmdir => "rmdir",
            Self::Link => "link",
            Self::Symlink => "symlink",
            Self::Setattr => "setattr",
            Self::Fsync => "fsync",
            Self::Release => "release",
            Self::Sweep => "sweep",
            Self::Restore => "restore",
        }
    }
}

/// The process that made the change, as FUSE reports it (IDs are in the
/// mount's own namespaces).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

/// One path's new absolute state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// `path` is a regular file holding the frozen content `pending`.
    Write {
        path: RepoPath,
        pending: PendingId,
        executable: bool,
    },
    /// `path` is a symlink to `target`.
    Symlink { path: RepoPath, target: String },
    /// `path`, and anything recorded beneath it, no longer exists.
    Remove { path: RepoPath },
}

impl Change {
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Write { path, .. } | Self::Symlink { path, .. } | Self::Remove { path } => path,
        }
    }

    pub fn pending(&self) -> Option<PendingId> {
        match self {
            Self::Write { pending, .. } => Some(*pending),
            _ => None,
        }
    }
}

/// A change boundary as given to the journal, before it has a sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub trigger: Trigger,
    pub caller: Caller,
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// Applied together: a rename is a removal plus a write.
    pub changes: Vec<Change>,
}

/// A durable journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    #[serde(flatten)]
    pub draft: Draft,
}

impl Record {
    pub fn pending_ids(&self) -> impl Iterator<Item = PendingId> + '_ {
        self.draft.changes.iter().filter_map(Change::pending)
    }
}
