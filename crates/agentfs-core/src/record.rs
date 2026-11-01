//! What the journal stores: one record per change boundary (design §3.1).
//!
//! Records hold absolute state ("this path now has this content"), never
//! deltas, so applying a record twice gives the same tree (design §3.3).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::identity::Context;
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
    /// An fsync-class group captured because a change through another
    /// entry point was about to touch it (design §8, §9): no record holds
    /// two entry points.
    Cut,
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
            Self::Cut => "cut",
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

/// How a change came into the volume (design §2.4, §9): `main`, one
/// sandbox session, or agentfs itself. Every journal record names exactly
/// one (`Draft::writer`, provenance slice 1), and every commit names its records' one entry
/// point in an `Agentfs-Entry` trailer ([`crate::history::ENTRY_TRAILER`]).
///
/// Rendered `main`, `sandbox/<id>` or `agentfs` by [`fmt::Display`] and
/// parsed back by [`std::str::FromStr`]; journaled, sent on the wire and
/// written to repair queues as that string. Required wherever it appears:
/// a record, wire draft or trailer without one is refused, never read as
/// `main`.
///
/// Never derived from a uid, pid, cgroup or time. A FUSE session is built
/// with its entry point and stamps every request it serves with it; a late
/// record takes the [`Writer`] held on the inode or group; agentfs-core
/// stamps [`EntryPoint::Agentfs`] on what it does itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum EntryPoint {
    /// The volume's main mount point.
    Main,
    /// One sandbox session: one container's mount of the volume (design
    /// §2.4).
    Sandbox(SandboxId),
    /// agentfs itself: an operator operation's records (restore, revert,
    /// patch apply), `agentfs record`, marker commits, and late records
    /// whose writer is unknown.
    Agentfs,
}

impl fmt::Display for EntryPoint {
    /// `main`, `sandbox/<id>` or `agentfs`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => f.write_str("main"),
            Self::Sandbox(id) => write!(f, "sandbox/{}", id.as_str()),
            Self::Agentfs => f.write_str("agentfs"),
        }
    }
}

impl std::str::FromStr for EntryPoint {
    type Err = EntryError;

    /// The inverse of [`fmt::Display`]: exactly `main`, `agentfs`, or
    /// `sandbox/` followed by a valid [`SandboxId`]. Anything else,
    /// including the empty string, is an error.
    fn from_str(s: &str) -> Result<Self, EntryError> {
        match s {
            "main" => Ok(Self::Main),
            "agentfs" => Ok(Self::Agentfs),
            _ => match s.strip_prefix("sandbox/") {
                Some(id) => SandboxId::new(id).map(Self::Sandbox),
                None => Err(EntryError::Unknown(s.to_owned())),
            },
        }
    }
}

impl TryFrom<String> for EntryPoint {
    type Error = EntryError;

    /// [`std::str::FromStr`], for serde.
    fn try_from(s: String) -> Result<Self, EntryError> {
        s.parse()
    }
}

impl From<EntryPoint> for String {
    /// [`fmt::Display`], for serde.
    fn from(entry: EntryPoint) -> String {
        entry.to_string()
    }
}

/// A sandbox session's id: the container runtime's mount ID (design
/// §2.4), never reused within a volume. 1 to [`SandboxId::MAX_LEN`] ASCII
/// letters, digits, `_`, `.` or `-`, starting with a letter or digit, so
/// it is one path component, one mount-source suffix and one trailer value
/// as it is. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SandboxId(Arc<str>);

impl SandboxId {
    /// The longest id: a Docker mount ID is 64 hex digits.
    pub const MAX_LEN: usize = 64;

    /// `id` if it follows the grammar above, else
    /// [`EntryError::BadSandboxId`].
    pub fn new(id: &str) -> Result<Self, EntryError> {
        let bytes = id.as_bytes();
        let valid = (1..=Self::MAX_LEN).contains(&bytes.len())
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if valid {
            Ok(Self(Arc::from(id)))
        } else {
            Err(EntryError::BadSandboxId(id.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a string is not an [`EntryPoint`] or a [`SandboxId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    #[error("{0:?} is not an entry point: expected main, sandbox/<id> or agentfs")]
    Unknown(String),
    #[error(
        "{0:?} is not a sandbox id: 1 to 64 ASCII letters, digits, '_', '.' or '-', starting \
         with a letter or digit"
    )]
    BadSandboxId(String),
}

/// Who made a change (design §9): the process as FUSE reported it, which
/// is evidence only (the author's uid), and the entry point it came
/// through, which a commit is cut by. Held whole wherever a writer is
/// kept for a later record: an inode's dirty state, an fsync-class
/// group's state, a repair queue entry, a move's reconcile duty.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Writer {
    pub caller: Caller,
    pub entry: EntryPoint,
}

impl Writer {
    /// A change agentfs makes itself, as `caller` (an operation's
    /// operator, the `agentfs record` process, or no one).
    pub fn agentfs(caller: Caller) -> Self {
        Self {
            caller,
            entry: EntryPoint::Agentfs,
        }
    }
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
    /// Whatever is recorded at and beneath `from` is now at and beneath
    /// `to`, unchanged, and whatever was recorded at and beneath `to` is
    /// gone: a directory rename, recorded in O(1) (design §3.1). The tree
    /// under `to` is reconciled afterwards ([`crate::reconcile`]). Older
    /// agentfs versions cannot read it: `agentfs flush` before a
    /// downgrade.
    Move { from: RepoPath, to: RepoPath },
}

impl Change {
    /// The path whose state the change sets: for a move, its destination.
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Write { path, .. } | Self::Symlink { path, .. } | Self::Remove { path } => path,
            Self::Move { to, .. } => to,
        }
    }

    /// Every path the change touches: a move's source and destination.
    pub fn paths(&self) -> impl Iterator<Item = &RepoPath> {
        let from = match self {
            Self::Move { from, .. } => Some(from),
            _ => None,
        };
        from.into_iter().chain(std::iter::once(self.path()))
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

/// The pod whose front recorded a change (design §9, §14): its committer
/// identity and pod trailers. Journaled with each record a history service
/// receives from a front, so a replay after a crash commits with the same
/// committer. Single-process mode journals none: the committer is the
/// process's own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origin {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_uid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    pub replica: String,
}

impl Origin {
    /// The pod part of `context` (everything but the volume).
    pub fn from_context(context: &Context) -> Self {
        Self {
            namespace: context.namespace.clone(),
            pod: context.pod.clone(),
            pod_uid: context.pod_uid.clone(),
            node: context.node.clone(),
            replica: context.replica.clone(),
        }
    }

    /// The identity context of this pod on `volume`.
    pub fn context(&self, volume: &str) -> Context {
        Context {
            volume: volume.to_owned(),
            namespace: self.namespace.clone(),
            pod: self.pod.clone(),
            pod_uid: self.pod_uid.clone(),
            node: self.node.clone(),
            replica: self.replica.clone(),
        }
    }
}

/// A change boundary as given to the journal, before it has a sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub trigger: Trigger,
    /// Who made it: the caller and the entry point (journaled as the keys
    /// `caller` and `entry`, both required; design §9). A journal from
    /// before entry points is refused, never read as `main`.
    #[serde(flatten)]
    pub writer: Writer,
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
    /// The front that recorded it, set by the history service (design
    /// §14); `None` in single-process mode, whose journals carry no field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
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

/// A saved repair queue's entry (design §3.5): a path the FUSE layer
/// could not record, the writer that changed it, and the pod it is
/// committed as (`None`: the loading process's own).
pub type QueuedRepair = (Vec<u8>, Writer, Option<Origin>);

/// What a repair queue saved at a store holds ([`read_repairs`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SavedRepairs {
    pub entries: Vec<QueuedRepair>,
    /// A crash left the temporary file of a rewrite ([`repairs_tmp`]):
    /// its complete lines are among `entries`.
    pub had_tmp: bool,
}

/// Where a repair queue saved at `store` is written before it is renamed
/// over it.
pub fn repairs_tmp(store: &Path) -> PathBuf {
    store.with_extension("tmp")
}

/// The repair queue saved at `store` (`meta/repairs` or a front slot's
/// file), with the complete lines of a temporary file a crash left
/// between writing a rewrite and renaming it over the store. A missing
/// file is an empty queue. A repair is idempotent, so an entry read twice
/// costs a sweep record at most.
pub fn read_repairs(store: &Path) -> io::Result<SavedRepairs> {
    let mut saved = SavedRepairs::default();
    match std::fs::read(store) {
        Ok(bytes) => saved.entries = parse_repairs(&bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match std::fs::read(repairs_tmp(store)) {
        Ok(mut bytes) => {
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            bytes.truncate(complete);
            saved.entries.extend(parse_repairs(&bytes));
            saved.had_tmp = true;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(saved)
}

/// The entries of a saved repair queue, the FUSE layer's format: lines
/// `<uid> <gid> <pid> <path in hex>`, after an `@origin <JSON>` line
/// naming the pod its entries are committed as (`@origin -`: none) and an
/// `@entry <entry point>` line naming the entry point they came through.
/// Each header applies to the entry lines after it until the next of its
/// kind. Malformed lines are skipped (a malformed `@origin` line counts as
/// `@origin -`); an entry with no valid `@entry` before it reads as
/// [`EntryPoint::Agentfs`] (writer unknown), never as `main`.
pub fn parse_repairs(bytes: &[u8]) -> Vec<QueuedRepair> {
    let text = String::from_utf8_lossy(bytes);
    let mut origin: Option<Origin> = None;
    let mut entry = EntryPoint::Agentfs;
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@origin ") {
            origin = match rest {
                "-" => None,
                json => serde_json::from_str(json).ok(),
            };
            continue;
        }
        if let Some(rest) = line.strip_prefix("@entry ") {
            entry = rest.parse().unwrap_or(EntryPoint::Agentfs);
            continue;
        }
        if let Some((path, caller)) = parse_repair_entry(line) {
            let writer = Writer {
                caller,
                entry: entry.clone(),
            };
            out.push((path, writer, origin.clone()));
        }
    }
    out
}

fn parse_repair_entry(line: &str) -> Option<(Vec<u8>, Caller)> {
    let mut f = line.split(' ');
    let uid = f.next()?.parse().ok()?;
    let gid = f.next()?.parse().ok()?;
    let pid = f.next()?.parse().ok()?;
    let hex = f.next()?;
    if f.next().is_some() || hex.len() % 2 != 0 {
        return None;
    }
    let path = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((path, Caller { uid, gid, pid }))
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
            writer: Writer {
                caller: Caller {
                    uid: 1,
                    gid: 1,
                    pid: 1,
                },
                entry: EntryPoint::Main,
            },
            time_ms: 5,
            changes: vec![],
            operation: None,
            atomic: false,
            origin: None,
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

    fn origin() -> Origin {
        Origin {
            namespace: "agent".into(),
            pod: "hermes-a".into(),
            pod_uid: "0b6f".into(),
            node: "hawking".into(),
            replica: "hermes-0".into(),
        }
    }

    #[test]
    fn the_origin_is_journaled_only_when_set() {
        let mut draft = Draft {
            trigger: Trigger::Close,
            writer: Writer {
                caller: Caller {
                    uid: 1,
                    gid: 1,
                    pid: 1,
                },
                entry: EntryPoint::Main,
            },
            time_ms: 5,
            changes: vec![],
            operation: None,
            atomic: false,
            origin: None,
        };
        // Single-process journals carry no origin.
        let single = serde_json::to_string(&draft).unwrap();
        assert_eq!(
            single,
            r#"{"trigger":"close","caller":{"uid":1,"gid":1,"pid":1},"entry":"main","time_ms":5,"changes":[]}"#
        );
        assert_eq!(serde_json::from_str::<Draft>(&single).unwrap(), draft);
        draft.origin = Some(origin());
        let front = serde_json::to_string(&draft).unwrap();
        assert!(
            front.ends_with(
                r#","origin":{"namespace":"agent","pod":"hermes-a","pod_uid":"0b6f","node":"hawking","replica":"hermes-0"}}"#
            ),
            "{front}"
        );
        assert_eq!(serde_json::from_str::<Draft>(&front).unwrap(), draft);
        // Empty fields are left out and read back empty.
        draft.origin = Some(Origin {
            replica: "r".into(),
            namespace: String::new(),
            pod: String::new(),
            pod_uid: String::new(),
            node: String::new(),
        });
        let bare = serde_json::to_string(&draft).unwrap();
        assert!(bare.ends_with(r#","origin":{"replica":"r"}}"#), "{bare}");
        assert_eq!(serde_json::from_str::<Draft>(&bare).unwrap(), draft);
    }

    #[test]
    fn entry_points_round_trip_through_their_string() {
        let id = "a".repeat(SandboxId::MAX_LEN);
        for text in [
            "main",
            "agentfs",
            "sandbox/1",
            "sandbox/A.b_c-9",
            &format!("sandbox/{id}"),
        ] {
            let entry: EntryPoint = text.parse().unwrap();
            assert_eq!(entry.to_string(), text);
            assert_eq!(String::from(entry.clone()), text);
            assert_eq!(EntryPoint::try_from(text.to_owned()).unwrap(), entry);
            let json = serde_json::to_string(&entry).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<EntryPoint>(&json).unwrap(), entry);
        }
        assert_eq!(
            "sandbox/x".parse::<EntryPoint>().unwrap(),
            EntryPoint::Sandbox(SandboxId::new("x").unwrap())
        );
        assert_eq!(SandboxId::new("x").unwrap().as_str(), "x");
    }

    #[test]
    fn entry_points_and_sandbox_ids_outside_the_grammar_are_refused() {
        for text in [
            "",
            "Main",
            "sandbox",
            "sandbox:x",
            "agentfs ",
            " main",
            "x/main",
        ] {
            assert_eq!(
                text.parse::<EntryPoint>(),
                Err(EntryError::Unknown(text.to_owned())),
                "{text:?}"
            );
        }
        let long = "a".repeat(SandboxId::MAX_LEN + 1);
        for id in [
            "",
            "-x",
            ".x",
            "_x",
            "a/b",
            "a:b",
            "a b",
            "é",
            long.as_str(),
        ] {
            assert_eq!(
                SandboxId::new(id),
                Err(EntryError::BadSandboxId(id.to_owned())),
                "{id:?}"
            );
            assert_eq!(
                format!("sandbox/{id}").parse::<EntryPoint>(),
                Err(EntryError::BadSandboxId(id.to_owned())),
                "{id:?}"
            );
        }
        assert!(serde_json::from_str::<EntryPoint>(r#""""#).is_err());
    }

    #[test]
    fn a_draft_without_an_entry_point_does_not_parse() {
        let old =
            r#"{"trigger":"close","caller":{"uid":1,"gid":1,"pid":1},"time_ms":5,"changes":[]}"#;
        assert!(serde_json::from_str::<Draft>(old).is_err());
        let empty = r#"{"trigger":"close","caller":{"uid":1,"gid":1,"pid":1},"entry":"","time_ms":5,"changes":[]}"#;
        assert!(serde_json::from_str::<Draft>(empty).is_err());
        let no_caller = r#"{"trigger":"close","entry":"main","time_ms":5,"changes":[]}"#;
        assert!(serde_json::from_str::<Draft>(no_caller).is_err());
        let cut = r#"{"trigger":"cut","caller":{"uid":1,"gid":1,"pid":1},"entry":"sandbox/s","time_ms":5,"changes":[]}"#;
        let draft = serde_json::from_str::<Draft>(cut).unwrap();
        assert_eq!(draft.trigger, Trigger::Cut);
        assert_eq!(Trigger::Cut.as_str(), "cut");
        assert_eq!(serde_json::to_string(&draft).unwrap(), cut);
    }

    #[test]
    fn repair_lines_take_the_entry_point_before_them_and_agentfs_without_one() {
        let saved = format!(
            "1 2 3 61\n@entry main\n4 5 6 62\n@origin {}\n@entry sandbox/s1\n7 8 9 63\n\
             @entry bogus\n1 1 1 64\n@entry agentfs\n2 2 2 65\n",
            serde_json::to_string(&origin()).unwrap()
        );
        let entries = parse_repairs(saved.as_bytes());
        let got: Vec<(Vec<u8>, String, bool)> = entries
            .iter()
            .map(|(path, writer, origin)| {
                (path.clone(), writer.entry.to_string(), origin.is_some())
            })
            .collect();
        assert_eq!(
            got,
            [
                (b"a".to_vec(), "agentfs".to_owned(), false),
                (b"b".to_vec(), "main".to_owned(), false),
                (b"c".to_vec(), "sandbox/s1".to_owned(), true),
                (b"d".to_vec(), "agentfs".to_owned(), true),
                (b"e".to_vec(), "agentfs".to_owned(), true),
            ]
        );
        assert_eq!(
            entries[1].1.caller,
            Caller {
                uid: 4,
                gid: 5,
                pid: 6
            }
        );
    }

    #[test]
    fn an_origin_round_trips_through_a_context() {
        let context = origin().context("vol");
        assert_eq!(context.volume, "vol");
        assert_eq!(context.pod_uid, "0b6f");
        assert_eq!(Origin::from_context(&context), origin());
    }
}
