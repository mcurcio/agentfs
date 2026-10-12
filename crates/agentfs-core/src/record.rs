//! What the journal stores: one record per change boundary (design §3.1).
//!
//! Records hold absolute state ("this path now has this content"), never
//! deltas, so applying a record twice gives the same tree (design §3.3).

use serde::{Deserialize, Serialize};

use crate::lfs::Store;
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
    /// Recorded by an fsync-class group's trailing timer, at most one
    /// `agentfs.coalesce` after its last change (design §8).
    Interval,
    /// An operator restore (design §15).
    Restore,
    /// A patch from the queue, applied at mount (design §12).
    Patch,
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
            Self::Interval => "interval",
            Self::Restore => "restore",
            Self::Patch => "patch",
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
        /// Where the content is stored, decided when it was recorded
        /// (design §7). Absent from older journals, and from blob writes.
        #[serde(default, skip_serializing_if = "Store::is_blob")]
        store: Store,
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

/// What an operator operation does (design §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Paths brought back to their state in a commit.
    Restore,
    /// A commit's changes undone.
    Revert,
    /// A patch from the queue applied onto the branch (design §12).
    Patch,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Revert => "revert",
            Self::Patch => "patch",
        }
    }

    /// The commit trailer naming the operation's source commit.
    pub fn trailer(self) -> &'static str {
        match self {
            Self::Restore => "Agentfs-Restore",
            Self::Revert => "Agentfs-Revert",
            Self::Patch => "Agentfs-Patch",
        }
    }

    /// The trigger of the operation's records: `restore` for restores and
    /// reverts, `patch` for patches.
    pub fn trigger(self) -> Trigger {
        match self {
            Self::Restore | Self::Revert => Trigger::Restore,
            Self::Patch => Trigger::Patch,
        }
    }
}

/// A patch commit's author, kept as the git author of the commit that
/// applies it (design §9, §12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchAuthor {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time_s: i64,
    /// The author's UTC offset, in minutes.
    pub offset_min: i32,
}

/// An operator restore or revert that records belong to (design §15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Tells one operation's records from the next one's.
    pub id: u64,
    pub kind: OperationKind,
    /// The full id of the commit restored from, or reverted.
    pub source: String,
    /// The commit subject, e.g. `restore src from 1a2b3c4`.
    pub summary: String,
    /// The operator who asked for it: the author of its commit, unless
    /// `author` is set.
    pub caller: Caller,
    /// A patch's inbox number (design §12): its commit carries
    /// `Agentfs-Inbox`. Absent from older journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<u64>,
    /// A patch's author, the git author of its commit. Absent from older
    /// journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<PatchAuthor>,
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
    /// The operator operation this record is part of, if any. Absent from
    /// older journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    /// All or nothing: if any change is refused when committed, the whole
    /// record is dropped. Set for fsync-class groups, whose files are only
    /// consistent together (design §8). Absent from older journals.
    #[serde(default, skip_serializing_if = "is_false")]
    pub atomic: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_without_patch_fields_read_and_write_as_before() {
        let old = r#"{"id":3,"kind":"restore","source":"abc","summary":"restore a","caller":{"uid":1,"gid":2,"pid":3}}"#;
        let op: Operation = serde_json::from_str(old).unwrap();
        assert_eq!(op.inbox, None);
        assert_eq!(op.author, None);
        assert_eq!(op.kind.trigger(), Trigger::Restore);
        assert_eq!(serde_json::to_string(&op).unwrap(), old);
        let patch = Operation {
            kind: OperationKind::Patch,
            inbox: Some(4),
            author: Some(PatchAuthor {
                name: "Ada".into(),
                email: "ada@example.com".into(),
                time_s: 5,
                offset_min: -60,
            }),
            ..op
        };
        let json = serde_json::to_string(&patch).unwrap();
        assert!(json.contains(r#""kind":"patch""#), "{json}");
        assert!(json.contains(r#""inbox":4"#), "{json}");
        assert_eq!(serde_json::from_str::<Operation>(&json).unwrap(), patch);
        assert_eq!(patch.kind.trigger(), Trigger::Patch);
        assert_eq!(patch.kind.trailer(), "Agentfs-Patch");
    }

    #[test]
    fn the_store_is_journaled_only_for_lfs_and_defaults_to_blob() {
        let write = |store| Change::Write {
            path: RepoPath::new("a").unwrap(),
            pending: PendingId(7),
            executable: false,
            store,
        };
        let blob = serde_json::to_string(&write(Store::Blob)).unwrap();
        assert_eq!(
            blob,
            r#"{"op":"write","path":"a","pending":7,"executable":false}"#
        );
        let lfs = serde_json::to_string(&write(Store::Lfs)).unwrap();
        assert!(lfs.ends_with(r#","store":"lfs"}"#), "{lfs}");
        // A journal written before LFS existed.
        let old: Change = serde_json::from_str(&blob).unwrap();
        assert_eq!(old, write(Store::Blob));
        assert_eq!(
            serde_json::from_str::<Change>(&lfs).unwrap(),
            write(Store::Lfs)
        );
    }

    #[test]
    fn atomic_is_journaled_only_when_set() {
        let mut draft = Draft {
            trigger: Trigger::Interval,
            caller: Caller {
                uid: 1,
                gid: 1,
                pid: 1,
            },
            time_ms: 5,
            changes: vec![],
            operation: None,
            atomic: false,
        };
        let plain = serde_json::to_string(&draft).unwrap();
        assert!(!plain.contains("atomic"), "{plain}");
        assert!(plain.contains(r#""trigger":"interval""#), "{plain}");
        assert_eq!(serde_json::from_str::<Draft>(&plain).unwrap(), draft);
        draft.atomic = true;
        let atomic = serde_json::to_string(&draft).unwrap();
        assert!(atomic.ends_with(r#","atomic":true}"#), "{atomic}");
        assert_eq!(serde_json::from_str::<Draft>(&atomic).unwrap(), draft);
    }
}
