//! Tree actor for phase 1 of target git-model §3, §6, §7 and §18.
//!
//! One exclusively locked actor owns a repository, private checkouts and a
//! byte-call WAL. The recording milestone supports flat regular files and
//! full-lineage saves on main and forks. The actor is the implementation
//! foundation for the landing referee and sync work still due in phase 1.
//! Every call is fsynced before acknowledgement (deliberately stronger and
//! slower than the target's group commit). Checkouts are disposable caches;
//! the branches are authoritative. See docs/target-architecture/phase-1.md.

mod pages;
pub mod queue;
mod referee;
mod sync;
mod wal;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::file_lock::{FileLock, Mode};
use crate::paths::RepoPath;
use crate::tree_git::{RefUpdate, TreeEntry, TreeGit};
use crate::tree_reconcile::LineMap;

const FORMAT: &str = "agentfs-trees-1\n";
// Read state created by the first two development commits without changing
// its journal, branches or history. New state uses the domain name above.
const LEGACY_FORMAT: &str = "agentfs-prototype-1\n";
const QUEUE_SEQUENCE: &str = "refs/agentfs/queue-sequence";
type Result<T> = std::result::Result<T, Error>;
type Undecided = (BTreeSet<String>, BTreeSet<(MergeFileIdentity, u64)>);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    TreeGit(#[from] crate::tree_git::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Reconcile(#[from] crate::tree_reconcile::Error),
    #[error("tree volume is busy")]
    Busy,
    #[error("not a tree state area (or unsupported format)")]
    Format,
    #[error("tree supports flat, git-safe UTF-8 names only")]
    Name,
    #[error("writer must be a nonempty single-line label")]
    Writer,
    #[error("no such tree, file or handle")]
    NotFound,
    #[error("file already exists")]
    Exists,
    #[error("file offset or journal record is too large")]
    TooLarge,
    #[error("journal corruption at byte {0}; operator recovery required")]
    CorruptJournal(usize),
    #[error("actor stopped after a recording/save failure; reopen to recover")]
    Stopped,
}

impl Error {
    /// The errno a file call answers with for this error.
    pub fn errno(&self) -> i32 {
        match self {
            Self::NotFound => libc::ENOENT,
            Self::Exists => libc::EEXIST,
            Self::Name | Self::Writer => libc::EINVAL,
            Self::TooLarge => libc::EFBIG,
            Self::Busy => libc::EBUSY,
            Self::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            _ => libc::EIO,
        }
    }
}

/// Calls name byte positions, never lines. Append finds EOF at save time.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Call {
    Write { offset: u64, bytes: Vec<u8> },
    Append(Vec<u8>),
    Truncate(u64),
}

impl Call {
    fn apply(&self, bytes: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Write {
                offset,
                bytes: data,
            } => {
                let offset = usize::try_from(*offset).map_err(|_| Error::TooLarge)?;
                let end = offset.checked_add(data.len()).ok_or(Error::TooLarge)?;
                // A zero-length pwrite past EOF does not extend a file.
                if !data.is_empty() {
                    bytes.resize(bytes.len().max(end), 0);
                    bytes[offset..end].copy_from_slice(data);
                }
            }
            Self::Append(data) => {
                bytes.len().checked_add(data.len()).ok_or(Error::TooLarge)?;
                bytes.extend_from_slice(data);
            }
            Self::Truncate(size) => {
                let size = usize::try_from(*size).map_err(|_| Error::TooLarge)?;
                bytes.resize(size, 0);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    seq: u64,
    event: Event,
}

#[derive(Clone, Serialize, Deserialize)]
enum Event {
    Fork {
        name: String,
        parent: String,
        at: String,
    },
    Open {
        tree: String,
        path: String,
        writer: String,
        object: u64,
        create: bool,
    },
    AnonymousOpen {
        tree: String,
        writer: String,
    },
    FirstLink {
        tree: String,
        object: u64,
        path: String,
        writer: String,
    },
    Call {
        handle: u64,
        call: Call,
    },
    /// The bytes a page write changed (ledger R1.4; `pages`).
    PageWrite {
        handle: u64,
        runs: Vec<pages::Run>,
    },
    Close {
        handle: u64,
    },
    Rename {
        tree: String,
        from: String,
        to: String,
        writer: String,
    },
    Unlink {
        tree: String,
        path: String,
        writer: String,
    },
}

#[derive(Clone)]
struct Tree {
    names: BTreeMap<String, u64>,
    anonymous: BTreeSet<u64>,
    bytes: BTreeMap<u64, Vec<u8>>,
    lines: BTreeMap<u64, LineMap>,
    identities: BTreeMap<u64, MergeFileIdentity>,
    namespace: BTreeMap<MergeFileIdentity, NameHistory>,
    fresh_names: BTreeSet<u64>,
    head: Oid,
    committed: u64,
}

// Merge identity follows an atomic replacement; the object number follows an
// open handle. Keep the types distinct even though both are allocated from
// the same durable event sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
struct MergeFileIdentity(u64);

/// A file's name versions, including deletion, travel in the same commit as
/// its content. Each history is keyed by event number, which is volume-wide.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct NameHistory {
    head: u64,
    versions: BTreeMap<u64, NameVersion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NameVersion {
    path: Option<String>,
    over: Option<u64>,
    replaced_by: Option<MergeFileIdentity>,
}

impl NameHistory {
    fn created(seq: u64, path: String) -> Self {
        Self {
            head: seq,
            versions: [(
                seq,
                NameVersion {
                    path: Some(path),
                    over: None,
                    replaced_by: None,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn advance(&mut self, seq: u64, path: Option<String>, replaced_by: Option<MergeFileIdentity>) {
        self.versions.insert(
            seq,
            NameVersion {
                path,
                over: Some(self.head),
                replaced_by,
            },
        );
        self.head = seq;
    }
}

struct Handle {
    tree: String,
    writer: String,
    object: u64,
    bytes: Vec<u8>,
    /// This open's calls, each with its journal number.
    calls: Vec<(u64, Call)>,
}

struct SyncPlan {
    target: Option<Oid>,
    snapshot: referee::Snapshot,
    remaining: bool,
    keys: BTreeSet<queue::Key>,
}

#[derive(Serialize, Deserialize)]
struct ObjectTable {
    committed: u64,
    files: BTreeMap<MergeFileIdentity, u64>,
}

enum QueueAction {
    Landing(Oid),
    Sync(Oid, SyncPlan),
}

/// Result of a release or crash-recovered release.
#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    Unchanged,
    Commit(Oid),
    /// An old handle after unlink/rename-over must never reach a replacement.
    OrphanDropped,
}

/// Process-local counters for observing save and queue costs. Durations are
/// measurements only and never participate in ordering or conflict decisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Metrics {
    pub saves: u64,
    pub save_nanos: u64,
    pub max_save_nanos: u64,
    pub takes: u64,
    pub take_nanos: u64,
    pub max_take_nanos: u64,
    pub landings: u64,
    pub landing_nanos: u64,
    pub syncs: u64,
    pub sync_nanos: u64,
    pub waits: u64,
    pub wait_nanos: u64,
    pub max_wait_nanos: u64,
    pub queued: u64,
}

/// A single synchronous actor. Sharing requires a mutex; a second process
/// cannot open this volume. No method exposes its state area to workloads.
pub struct Volume {
    root: PathBuf,
    repo: TreeGit,
    wal: wal::Wal,
    trees: BTreeMap<String, Tree>,
    handles: BTreeMap<u64, Handle>,
    legacy_commits: BTreeMap<(String, u64), Oid>,
    legacy_snapshots: BTreeMap<Oid, referee::Snapshot>,
    next: u64,
    stopped: bool,
    metrics: Metrics,
    waiting_since: BTreeMap<String, Instant>,
    #[allow(dead_code)] // agentfs-eh5.3 reads it.
    pages: BTreeMap<(String, u64), pages::ObjectPages>,
    _lock: Arc<FileLock>,
}

impl Volume {
    /// Initialize an empty directory or recover a tree volume before serving.
    /// Recovery closes every abandoned write-open exactly once.
    pub fn open(root: &Path) -> Result<Self> {
        crate::history::init_libgit2();
        if !root.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)?;
        }
        let root = root.canonicalize()?;
        let lock = Arc::new(
            FileLock::try_take(&root.join("actor.lock"), Mode::Exclusive)?.ok_or(Error::Busy)?,
        );
        let format = root.join("format");
        if !format.exists() {
            if fs::read_dir(&root)?.any(|e| e.map_or(true, |e| e.file_name() != "actor.lock")) {
                return Err(Error::Format);
            }
            fs::write(&format, FORMAT)?;
            File::open(&format)?.sync_all()?;
            File::open(&root)?.sync_all()?;
        }
        let stored_format = fs::read_to_string(format)?;
        if stored_format != FORMAT && stored_format != LEGACY_FORMAT {
            return Err(Error::Format);
        }
        // Never grant another user access to a newly initialized state area.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        if root.join("history.git").exists() {
            remove_stale_locks(&root.join("history.git"))?;
        }
        let repo = if root.join("history.git").exists() {
            TreeGit::open_with_lease(root.join("history.git"), Arc::clone(&lock))?
        } else {
            TreeGit::init_with_lease(root.join("history.git"), Arc::clone(&lock))?
        };
        let head = match repo.head("refs/heads/main")? {
            Some(oid) => oid,
            None => {
                let tree = repo.write_tree(&[])?;
                let head = repo.write_commit(tree, &[], "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: main\nAgentfs-Event: 0\n")?;
                repo.ref_transaction(&[RefUpdate {
                    refname: "refs/heads/main".into(),
                    new: Some(head),
                    expected_old: None,
                }])?;
                head
            }
        };
        let legacy_commits = legacy_commit_index(&repo)?;
        let (wal, records) = wal::Wal::open(&root.join("calls.wal"))?;
        let mut volume = Self {
            root,
            repo,
            wal,
            trees: BTreeMap::new(),
            handles: BTreeMap::new(),
            legacy_commits,
            legacy_snapshots: BTreeMap::new(),
            next: records.len() as u64 + 1,
            stopped: false,
            metrics: Metrics::default(),
            waiting_since: BTreeMap::new(),
            pages: BTreeMap::new(),
            _lock: lock,
        };
        let committed = volume.watermark(head)?;
        volume.trees.insert(
            "main".into(),
            Tree {
                names: BTreeMap::new(),
                anonymous: BTreeSet::new(),
                bytes: BTreeMap::new(),
                lines: BTreeMap::new(),
                identities: BTreeMap::new(),
                namespace: BTreeMap::new(),
                fresh_names: BTreeSet::new(),
                head,
                committed,
            },
        );
        if volume.repo.head("refs/agentfs/reconcile-used")?.is_some() {
            volume.recover_snapshots(&records)?;
        } else {
            for record in records {
                volume.apply(record)?;
            }
        }
        let abandoned: Vec<_> = volume.handles.keys().copied().collect();
        for handle in abandoned {
            volume.close(handle)?;
        }
        for name in volume.tree_names() {
            volume.checkout(&name)?;
        }
        volume.metrics.queued = volume.repo.list_refs("refs/queue/")?.len() as u64;
        volume.progress()?;
        Ok(volume)
    }

    pub fn tree_names(&self) -> Vec<String> {
        self.trees.keys().cloned().collect()
    }

    /// Snapshot measurements without exposing them through a workload view.
    pub fn metrics(&self) -> Metrics {
        let mut metrics = self.metrics;
        for since in self.waiting_since.values() {
            let nanos = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
            metrics.wait_nanos = metrics.wait_nanos.saturating_add(nanos);
            metrics.max_wait_nanos = metrics.max_wait_nanos.max(nanos);
        }
        metrics
    }

    /// A failed actor must not keep answering reads from stale state.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn head(&self, tree: &str) -> Result<Oid> {
        Ok(self.tree(tree)?.head)
    }

    /// Fork IDs are numeric and never reused. Forking a fork is not exposed yet.
    pub fn fork(&mut self) -> Result<String> {
        let name = format!("fork/{}", self.next);
        let at = self.head("main")?.to_string();
        self.record(Event::Fork {
            name: name.clone(),
            parent: "main".into(),
            at,
        })?;
        Ok(name)
    }

    /// Saved bytes; an independent reader never sees an unfinished save.
    pub fn read(&self, tree: &str, path: &str) -> Result<&[u8]> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?)
    }

    /// Stable object IDs for a view's inode table (not merge identities).
    pub fn files(&self, tree: &str) -> Result<Vec<(String, u64, usize)>> {
        let tree = self.tree(tree)?;
        Ok(tree
            .names
            .iter()
            .map(|(p, o)| (p.clone(), *o, tree.bytes[o].len()))
            .collect())
    }

    /// Open a private write buffer. Creation is itself a recorded namespace call.
    /// The returned number is persisted before it can be used and never repeats.
    pub fn write_open(
        &mut self,
        tree: &str,
        path: &str,
        writer: &str,
        create: bool,
    ) -> Result<u64> {
        name(path)?;
        writer_label(writer)?;
        let object = self.tree(tree)?.names.get(path).copied();
        if object.is_none() && !create {
            return Err(Error::NotFound);
        }
        let handle = self.next;
        self.record(Event::Open {
            tree: tree.into(),
            path: path.into(),
            writer: writer.into(),
            object: object.unwrap_or(handle),
            create: object.is_none(),
        })?;
        Ok(handle)
    }

    /// Create an unnamed open object. Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: &str, writer: &str) -> Result<u64> {
        writer_label(writer)?;
        self.tree(tree)?;
        let handle = self.next;
        self.record(Event::AnonymousOpen {
            tree: tree.into(),
            writer: writer.into(),
        })?;
        Ok(handle)
    }

    /// Link an anonymous object for the first time, preserving its identity.
    pub fn first_link(&mut self, tree: &str, object: u64, path: &str, writer: &str) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        let t = self.tree(tree)?;
        if t.names.contains_key(path) {
            return Err(Error::Exists);
        }
        if !t.anonymous.contains(&object) {
            return Err(Error::NotFound);
        }
        self.record(Event::FirstLink {
            tree: tree.into(),
            object,
            path: path.into(),
            writer: writer.into(),
        })?;
        Ok(())
    }

    /// Whether this object has never had a name in this tree.
    pub fn is_anonymous(&self, tree: &str, object: u64) -> Result<bool> {
        Ok(self.tree(tree)?.anonymous.contains(&object))
    }

    /// An open's stable object number, including before its first link.
    pub fn open_object(&self, handle: u64) -> Result<u64> {
        Ok(self.handles.get(&handle).ok_or(Error::NotFound)?.object)
    }

    pub fn read_open(&self, handle: u64) -> Result<&[u8]> {
        Ok(&self.handles.get(&handle).ok_or(Error::NotFound)?.bytes)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        let call = if append {
            Call::Append(bytes.to_vec())
        } else {
            Call::Write {
                offset,
                bytes: bytes.to_vec(),
            }
        };
        self.call(handle, call)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.call(handle, Call::Truncate(size))
    }

    fn call(&mut self, handle: u64, call: Call) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        // Check both the writer's view and replay on the head before journaling.
        let mut bytes = h.bytes.clone();
        call.apply(&mut bytes)?;
        let mut replay = self.tree(&h.tree)?.bytes[&h.object].clone();
        for c in h.calls.iter().map(|(_, c)| c).chain(std::iter::once(&call)) {
            c.apply(&mut replay)?;
        }
        self.record(Event::Call { handle, call })?;
        Ok(())
    }

    /// fsync/FLUSH records durability, not publication. Every call is already durable.
    pub fn sync(&self) -> Result<()> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.wal.sync()
    }

    /// RELEASE publishes calls against this tree's head, never the stale open base.
    /// On failure the WAL remains intact; stop and reopen before serving again.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        if !self.handles.contains_key(&handle) {
            return Err(Error::NotFound);
        }
        self.record(Event::Close { handle })
    }

    pub fn rename(&mut self, tree: &str, from: &str, to: &str, writer: &str) -> Result<()> {
        name(from)?;
        name(to)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(from) {
            return Err(Error::NotFound);
        }
        if from != to {
            self.record(Event::Rename {
                tree: tree.into(),
                from: from.into(),
                to: to.into(),
                writer: writer.into(),
            })?;
        }
        Ok(())
    }

    pub fn unlink(&mut self, tree: &str, path: &str, writer: &str) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(path) {
            return Err(Error::NotFound);
        }
        self.record(Event::Unlink {
            tree: tree.into(),
            path: path.into(),
            writer: writer.into(),
        })?;
        Ok(())
    }

    fn tree(&self, tree: &str) -> Result<&Tree> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.trees.get(tree).ok_or(Error::NotFound)
    }

    fn snapshot_of(tree: &Tree) -> referee::Snapshot {
        let files = tree
            .names
            .values()
            .map(|object| {
                (
                    tree.identities[object],
                    referee::File {
                        object: *object,
                        lines: tree.lines[object].clone(),
                    },
                )
            })
            .collect();
        referee::Snapshot {
            files,
            names: tree.namespace.clone(),
        }
    }

    // Once queue takes can introduce objects, replay cannot reconstruct a
    // tree from direct calls alone. Start from each durable branch snapshot,
    // recover the opens that crossed its watermark, then replay later calls.
    fn recover_snapshots(&mut self, records: &[Record]) -> Result<()> {
        if let Some(last_legacy) = self.legacy_commits.keys().map(|(_, seq)| *seq).max() {
            for record in records.iter().filter(|r| r.seq <= last_legacy) {
                self.apply(record.clone())?;
            }
        }
        self.trees.clear();
        self.handles.clear();
        let branches = self.repo.list_refs("refs/heads/")?;
        let mut watermarks = BTreeMap::new();
        for (branch, head) in branches {
            let Some(name) = branch.strip_prefix("refs/heads/") else {
                continue;
            };
            if name != "main" && !name.starts_with("fork/") {
                continue;
            }
            let mut committed = self.watermark(head)?;
            let mut snapshot = self.snapshot_at(head)?;
            if let Some(saved) = self.restore_object_table(name, &mut snapshot)? {
                committed = saved;
            }
            self.trees.insert(
                name.into(),
                Tree {
                    names: BTreeMap::new(),
                    anonymous: BTreeSet::new(),
                    bytes: BTreeMap::new(),
                    lines: BTreeMap::new(),
                    identities: BTreeMap::new(),
                    namespace: BTreeMap::new(),
                    fresh_names: BTreeSet::new(),
                    head,
                    committed,
                },
            );
            self.install_snapshot(name, snapshot, head)?;
            watermarks.insert(name.to_owned(), committed);
        }
        let mut handle_trees = BTreeMap::new();
        for record in records {
            match &record.event {
                Event::Open {
                    tree,
                    writer,
                    object,
                    ..
                } => {
                    handle_trees.insert(record.seq, tree.clone());
                    if record.seq <= *watermarks.get(tree).unwrap_or(&0) {
                        let bytes = self
                            .trees
                            .get(tree)
                            .and_then(|t| t.bytes.get(object))
                            .cloned()
                            .unwrap_or_default();
                        self.handles.insert(
                            record.seq,
                            Handle {
                                tree: tree.clone(),
                                writer: writer.clone(),
                                object: *object,
                                bytes,
                                calls: Vec::new(),
                            },
                        );
                    }
                }
                Event::AnonymousOpen { tree, writer } => {
                    handle_trees.insert(record.seq, tree.clone());
                    if record.seq <= *watermarks.get(tree).unwrap_or(&0) {
                        let bytes = self
                            .trees
                            .get(tree)
                            .and_then(|t| t.bytes.get(&record.seq))
                            .cloned()
                            .unwrap_or_default();
                        self.handles.insert(
                            record.seq,
                            Handle {
                                tree: tree.clone(),
                                writer: writer.clone(),
                                object: record.seq,
                                bytes,
                                calls: Vec::new(),
                            },
                        );
                    }
                }
                Event::Call { handle, call } => {
                    if let Some(h) = self.handles.get_mut(handle)
                        && record.seq <= *watermarks.get(&h.tree).unwrap_or(&0)
                    {
                        call.apply(&mut h.bytes)?;
                        h.calls.push((record.seq, call.clone()));
                    }
                }
                Event::PageWrite { handle, runs } => {
                    if let Some(h) = self.handles.get(handle)
                        && record.seq <= *watermarks.get(&h.tree).unwrap_or(&0)
                    {
                        self.pages_note_writeback(record.seq, *handle, runs)?;
                    }
                }
                Event::Close { handle } => {
                    if let Some(h) = self.handles.get(handle)
                        && record.seq <= *watermarks.get(&h.tree).unwrap_or(&0)
                    {
                        self.handles.remove(handle);
                    }
                }
                _ => {}
            }
        }
        for record in records {
            let tree = match &record.event {
                Event::Fork { name, .. } => {
                    if !self.trees.contains_key(name) {
                        self.apply(record.clone())?;
                    }
                    continue;
                }
                Event::Open { tree, .. }
                | Event::AnonymousOpen { tree, .. }
                | Event::FirstLink { tree, .. }
                | Event::Rename { tree, .. }
                | Event::Unlink { tree, .. } => tree,
                Event::Call { handle, .. }
                | Event::PageWrite { handle, .. }
                | Event::Close { handle } => handle_trees
                    .get(handle)
                    .ok_or(Error::CorruptJournal(record.seq as usize))?,
            };
            if record.seq > *watermarks.get(tree).unwrap_or(&0) {
                self.apply(record.clone())?;
            }
        }
        Ok(())
    }

    fn snapshot_at(&self, head: Oid) -> Result<referee::Snapshot> {
        let commit = self.repo.read_commit(head)?;
        let is_envelope = std::str::from_utf8(&commit.message).is_ok_and(|message| {
            message
                .lines()
                .any(|line| line == "Agentfs-Format: envelope-1")
        });
        if !is_envelope {
            // The initial empty commit precedes the envelope format.
            if self.repo.list_tree(commit.tree)?.is_empty() {
                return Ok(referee::Snapshot {
                    files: BTreeMap::new(),
                    names: BTreeMap::new(),
                });
            }
            return self
                .legacy_snapshots
                .get(&head)
                .cloned()
                .ok_or(Error::Format);
        }
        let entries = self.repo.list_tree(commit.tree)?;
        let metadata = entries
            .iter()
            .find(|e| e.name == b"metadata.json")
            .ok_or(Error::Format)?;
        let Some(namespace) = entries.iter().find(|e| e.name == b"namespace.json") else {
            return self
                .legacy_snapshots
                .get(&head)
                .cloned()
                .ok_or(Error::Format);
        };
        let by_path: BTreeMap<String, (u64, MergeFileIdentity, LineMap)> =
            serde_json::from_slice(&self.repo.read_blob(metadata.oid)?)?;
        let names = serde_json::from_slice(&self.repo.read_blob(namespace.oid)?)?;
        let files = by_path
            .into_values()
            .map(|(object, id, lines)| (id, referee::File { object, lines }))
            .collect();
        Ok(referee::Snapshot { files, names })
    }

    fn snapshot_tree(&self, snapshot: &referee::Snapshot) -> Result<Oid> {
        let mut files = Vec::new();
        let mut metadata = BTreeMap::new();
        for (id, file) in &snapshot.files {
            let history = snapshot.names.get(id).ok_or(Error::Format)?;
            let path = history
                .versions
                .get(&history.head)
                .and_then(|version| version.path.as_ref())
                .ok_or(Error::Format)?;
            let bytes = file.lines.bytes();
            files.push(TreeEntry {
                name: path.as_bytes().to_vec(),
                mode: 0o100644,
                oid: self.repo.write_blob(&bytes)?,
            });
            metadata.insert(path, (file.object, id, &file.lines));
        }
        let files = self.repo.write_tree(&files)?;
        let metadata = self.repo.write_blob(&serde_json::to_vec(&metadata)?)?;
        let namespace = self
            .repo
            .write_blob(&serde_json::to_vec(&snapshot.names)?)?;
        Ok(self.repo.write_tree(&[
            TreeEntry {
                name: b"files".to_vec(),
                mode: 0o040000,
                oid: files,
            },
            TreeEntry {
                name: b"metadata.json".to_vec(),
                mode: 0o100644,
                oid: metadata,
            },
            TreeEntry {
                name: b"namespace.json".to_vec(),
                mode: 0o100644,
                oid: namespace,
            },
        ])?)
    }

    fn object_table_update(
        &self,
        tree: &str,
        snapshot: &referee::Snapshot,
        committed: u64,
        updates: &mut Vec<RefUpdate>,
    ) -> Result<()> {
        let name = format!("refs/agentfs/objects/{tree}");
        let files: BTreeMap<MergeFileIdentity, u64> = snapshot
            .files
            .iter()
            .map(|(id, file)| (*id, file.object))
            .collect();
        let oid = self
            .repo
            .write_blob(&serde_json::to_vec(&ObjectTable { committed, files })?)?;
        updates.push(RefUpdate {
            expected_old: self.repo.head(&name)?,
            refname: name,
            new: Some(oid),
        });
        Ok(())
    }

    fn restore_object_table(
        &self,
        tree: &str,
        snapshot: &mut referee::Snapshot,
    ) -> Result<Option<u64>> {
        let name = format!("refs/agentfs/objects/{tree}");
        if let Some(oid) = self.repo.head(&name)? {
            let table: ObjectTable = serde_json::from_slice(&self.repo.read_blob(oid)?)?;
            for (id, file) in &mut snapshot.files {
                file.object = *table.files.get(id).ok_or(Error::Format)?;
            }
            return Ok(Some(table.committed));
        }
        Ok(None)
    }

    fn install_snapshot(
        &mut self,
        tree: &str,
        snapshot: referee::Snapshot,
        head: Oid,
    ) -> Result<()> {
        let t = self.trees.get_mut(tree).ok_or(Error::NotFound)?;
        t.names.clear();
        t.fresh_names.clear();
        for (id, file) in snapshot.files {
            let history = snapshot.names.get(&id).ok_or(Error::Format)?;
            let path = history
                .versions
                .get(&history.head)
                .and_then(|version| version.path.clone())
                .ok_or(Error::Format)?;
            if history
                .versions
                .get(&history.head)
                .is_some_and(|version| version.over.is_none())
            {
                t.fresh_names.insert(file.object);
            }
            t.names.insert(path, file.object);
            t.bytes.insert(file.object, file.lines.bytes());
            t.lines.insert(file.object, file.lines);
            t.identities.insert(file.object, id);
        }
        t.namespace = snapshot.names;
        t.head = head;
        Ok(())
    }

    fn changed_keys(before: &referee::Snapshot, after: &referee::Snapshot) -> BTreeSet<queue::Key> {
        let ids: BTreeSet<_> = before
            .names
            .keys()
            .chain(after.names.keys())
            .chain(before.files.keys())
            .chain(after.files.keys())
            .copied()
            .collect();
        let mut keys = BTreeSet::new();
        for id in ids {
            let old = before.names.get(&id);
            let new = after.names.get(&id);
            if old.map(|h| h.head) == new.map(|h| h.head)
                && before.files.get(&id).map(|file| &file.lines)
                    == after.files.get(&id).map(|file| &file.lines)
            {
                continue;
            }
            keys.insert(queue::Key::File(id.0));
            for history in [old, new].into_iter().flatten() {
                if let Some(path) = history
                    .versions
                    .get(&history.head)
                    .and_then(|v| v.path.as_ref())
                {
                    keys.insert(queue::Key::Name(path.clone()));
                }
            }
        }
        keys
    }

    fn open_keys(&self, tree: &str) -> BTreeSet<queue::Key> {
        let mut keys = BTreeSet::new();
        if let Some(t) = self.trees.get(tree) {
            for handle in self.handles.values().filter(|h| h.tree == tree) {
                if !t.names.values().any(|object| *object == handle.object) {
                    continue;
                }
                if let Some(id) = t.identities.get(&handle.object) {
                    keys.insert(queue::Key::File(id.0));
                }
                for (path, object) in &t.names {
                    if *object == handle.object {
                        keys.insert(queue::Key::Name(path.clone()));
                    }
                }
            }
        }
        keys
    }

    fn event_owners(&self, start: Oid) -> Result<BTreeMap<u64, Oid>> {
        let mut todo = vec![start];
        let mut seen = BTreeSet::new();
        let mut owners = BTreeMap::new();
        while let Some(oid) = todo.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let commit = self.repo.read_commit(oid)?;
            todo.extend(commit.parents);
            if let Ok(message) = std::str::from_utf8(&commit.message)
                && message.lines().any(|line| line == "Agentfs-Cause: Direct")
                && let Some(seq) = message
                    .lines()
                    .find_map(|line| line.strip_prefix("Agentfs-Event: "))
                    .and_then(|s| s.parse::<u64>().ok())
            {
                owners.insert(seq, oid);
            }
        }
        Ok(owners)
    }

    fn undecided(
        &self,
        fork: &referee::Snapshot,
        fork_head: Oid,
        upstream: Oid,
    ) -> Result<Undecided> {
        let owners = self.event_owners(fork_head)?;
        let mut lines = BTreeSet::new();
        let mut names = BTreeSet::new();
        for file in fork.files.values() {
            for line in &file.lines.lines {
                let seq = line
                    .version
                    .split_once(':')
                    .and_then(|(s, _)| s.parse::<u64>().ok())
                    .ok_or(Error::Format)?;
                let owner = owners.get(&seq).ok_or(Error::Format)?;
                if !self.repo.is_ancestor(*owner, upstream)? {
                    lines.insert(line.version.clone());
                }
            }
        }
        for (id, history) in &fork.names {
            let owner = owners.get(&history.head).ok_or(Error::Format)?;
            if !self.repo.is_ancestor(*owner, upstream)? {
                names.insert((*id, history.head));
            }
        }
        Ok((lines, names))
    }

    fn prior_decisions(
        &self,
        mut head: Oid,
    ) -> Result<BTreeMap<referee::Version, referee::Version>> {
        let mut result = BTreeMap::new();
        loop {
            let commit = self.repo.read_commit(head)?;
            if let Ok(message) = std::str::from_utf8(&commit.message) {
                for json in message
                    .lines()
                    .filter_map(|line| line.strip_prefix("Agentfs-Collisions: "))
                {
                    let collisions: Vec<referee::Collision> = serde_json::from_str(json)?;
                    for collision in collisions {
                        result.entry(collision.lost).or_insert(collision.winner);
                    }
                }
            }
            match commit.parents.first() {
                Some(parent) => head = *parent,
                None => break,
            }
        }
        Ok(result)
    }

    fn reconcile_marker(&self, updates: &mut Vec<RefUpdate>) -> Result<()> {
        let name = "refs/agentfs/reconcile-used";
        if self.repo.head(name)?.is_none() {
            updates.push(RefUpdate {
                refname: name.into(),
                new: Some(self.repo.write_blob(b"1")?),
                expected_old: None,
            });
        }
        Ok(())
    }

    fn take_landing(&mut self, tree: &str, reference: &str, incoming: Oid) -> Result<()> {
        let started = Instant::now();
        let current = self.tree(tree)?.head;
        let incoming_commit = self.repo.read_commit(incoming)?;
        let base = *incoming_commit.parents.first().ok_or(Error::Format)?;
        let decision = referee::judge(
            &Self::snapshot_of(self.tree(tree)?),
            &self.snapshot_at(base)?,
            &self.snapshot_at(incoming)?,
            &self.prior_decisions(current)?,
        )?;
        let content = self.snapshot_tree(&decision.snapshot)?;
        let seq = self.tree(tree)?.committed;
        let mut message = format!(
            "Landing\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Landing\nAgentfs-Tree: {tree}\nAgentfs-Event: {seq}\nAgentfs-Format: envelope-1\n"
        );
        if !decision.collisions.is_empty() {
            message.push_str(&format!(
                "Agentfs-Collisions: {}\n",
                serde_json::to_string(&decision.collisions)?
            ));
        }
        let head = self
            .repo
            .write_commit(content, &[current, incoming], &message)?;
        let mut updates = vec![
            RefUpdate {
                refname: format!("refs/heads/{tree}"),
                new: Some(head),
                expected_old: Some(current),
            },
            RefUpdate {
                refname: reference.into(),
                new: None,
                expected_old: Some(incoming),
            },
        ];
        if !decision.collisions.is_empty() {
            let name = format!("refs/collisions/{tree}/{incoming}");
            if self.repo.head(&name)?.is_none() {
                updates.push(RefUpdate {
                    refname: name,
                    new: Some(incoming),
                    expected_old: None,
                });
            }
        }
        self.reconcile_marker(&mut updates)?;
        self.object_table_update(tree, &decision.snapshot, seq, &mut updates)?;
        let enqueued = self.enqueue_moves(tree, head, &mut updates)?;
        crate::failpoints::hit("tree.after_objects_durable")?;
        self.repo.ref_transaction(&updates)?;
        crate::failpoints::hit("tree.after_refs_durable")?;
        self.install_snapshot(tree, decision.snapshot, head)?;
        self.metrics.queued = self
            .metrics
            .queued
            .saturating_sub(1)
            .saturating_add(enqueued);
        self.checkout(tree)?;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.takes = self.metrics.takes.saturating_add(1);
        self.metrics.take_nanos = self.metrics.take_nanos.saturating_add(nanos);
        self.metrics.max_take_nanos = self.metrics.max_take_nanos.max(nanos);
        self.metrics.landings = self.metrics.landings.saturating_add(1);
        self.metrics.landing_nanos = self.metrics.landing_nanos.saturating_add(nanos);
        Ok(())
    }

    fn plan_sync(&self, tree: &str, open: &BTreeSet<queue::Key>) -> Result<SyncPlan> {
        let fork = self.tree(tree)?;
        let fork_snapshot = Self::snapshot_of(fork);
        let mut todo = Vec::new();
        let mut cursor = self.head("main")?;
        while !self.repo.is_ancestor(cursor, fork.head)? {
            todo.push(cursor);
            let commit = self.repo.read_commit(cursor)?;
            let Some(parent) = commit.parents.first() else {
                break;
            };
            cursor = *parent;
        }
        todo.reverse();
        let mut current = fork_snapshot.clone();
        let mut target = None;
        let mut keys = BTreeSet::new();
        let mut remaining = false;
        for oid in todo {
            let parent = self.snapshot_at(oid)?;
            let (lines, names) = self.undecided(&fork_snapshot, fork.head, oid)?;
            let next = sync::sync(&parent, &fork_snapshot, &lines, &names)?;
            let changes = Self::changed_keys(&current, &next);
            if !changes.is_disjoint(open) {
                remaining = true;
                if target.is_none() {
                    keys = changes;
                }
                break;
            }
            keys.extend(changes);
            current = next;
            target = Some(oid);
        }
        Ok(SyncPlan {
            target,
            snapshot: current,
            remaining,
            keys,
        })
    }

    fn pending_from(&self, tree: &str) -> Result<bool> {
        for (_, oid) in self.repo.list_refs("refs/queue/main/")? {
            let commit = self.repo.read_commit(oid)?;
            if std::str::from_utf8(&commit.message).is_ok_and(|m| {
                m.lines()
                    .any(|line| line == format!("Agentfs-Tree: {tree}"))
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn take_sync(&mut self, tree: &str, reference: &str, value: Oid, plan: SyncPlan) -> Result<()> {
        let started = Instant::now();
        let current = self.head(tree)?;
        let mut updates = Vec::new();
        let mut new_head = None;
        let installed = plan.snapshot;
        if let Some(target) = plan.target {
            let parent = self.snapshot_at(target)?;
            let head = if Self::changed_keys(&parent, &installed).is_empty()
                && !self.pending_from(tree)?
                && self.repo.is_ancestor(current, target)?
            {
                target
            } else {
                let content = self.snapshot_tree(&installed)?;
                let seq = self.tree(tree)?.committed;
                let message = format!(
                    "Sync\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Sync\nAgentfs-Tree: {tree}\nAgentfs-Event: {seq}\nAgentfs-Format: envelope-1\n"
                );
                self.repo
                    .write_commit(content, &[current, target], &message)?
            };
            updates.push(RefUpdate {
                refname: format!("refs/heads/{tree}"),
                new: Some(head),
                expected_old: Some(current),
            });
            new_head = Some(head);
        }
        if new_head.is_some() {
            self.object_table_update(tree, &installed, self.tree(tree)?.committed, &mut updates)?;
        }
        if !plan.remaining {
            updates.push(RefUpdate {
                refname: reference.into(),
                new: None,
                expected_old: Some(value),
            });
        }
        self.reconcile_marker(&mut updates)?;
        crate::failpoints::hit("tree.after_objects_durable")?;
        self.repo.ref_transaction(&updates)?;
        crate::failpoints::hit("tree.after_refs_durable")?;
        if let Some(head) = new_head {
            self.install_snapshot(tree, installed, head)?;
            self.checkout(tree)?;
        }
        if !plan.remaining {
            self.metrics.queued = self.metrics.queued.saturating_sub(1);
        }
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.takes = self.metrics.takes.saturating_add(1);
        self.metrics.take_nanos = self.metrics.take_nanos.saturating_add(nanos);
        self.metrics.max_take_nanos = self.metrics.max_take_nanos.max(nanos);
        self.metrics.syncs = self.metrics.syncs.saturating_add(1);
        self.metrics.sync_nanos = self.metrics.sync_nanos.saturating_add(nanos);
        Ok(())
    }

    /// Take at most one eligible queue entry. Callers can release the actor
    /// mutex between calls, so opens do not wait behind a waiting entry.
    pub fn progress(&mut self) -> Result<bool> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let result = self.progress_one();
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn progress_one(&mut self) -> Result<bool> {
        let mut trees = self.tree_names();
        trees.sort_by_key(|name| name != "main");
        for tree in trees {
            let entries = self.repo.list_refs(&format!("refs/queue/{tree}/"))?;
            if entries.is_empty() {
                continue;
            }
            let open = self.open_keys(&tree);
            let mut plans = Vec::new();
            let mut actions = Vec::new();
            for (_, oid) in &entries {
                if tree == "main" {
                    let commit = self.repo.read_commit(*oid)?;
                    let base = *commit.parents.first().ok_or(Error::Format)?;
                    let source = std::str::from_utf8(&commit.message)
                        .ok()
                        .and_then(|m| m.lines().find_map(|l| l.strip_prefix("Agentfs-Tree: ")))
                        .ok_or(Error::Format)?
                        .to_owned();
                    let keys =
                        Self::changed_keys(&self.snapshot_at(base)?, &self.snapshot_at(*oid)?);
                    plans.push(queue::Plan {
                        source,
                        files: keys,
                    });
                    actions.push(QueueAction::Landing(*oid));
                } else {
                    let sync = self.plan_sync(&tree, &open)?;
                    plans.push(queue::Plan {
                        source: "main".into(),
                        files: sync.keys.clone(),
                    });
                    actions.push(QueueAction::Sync(*oid, sync));
                }
            }
            let selected = queue::eligible(&plans, &open);
            let blocked = selected.unwrap_or(entries.len());
            for (reference, _) in entries.iter().take(blocked) {
                if !self.waiting_since.contains_key(reference) {
                    self.waiting_since.insert(reference.clone(), Instant::now());
                    self.metrics.waits = self.metrics.waits.saturating_add(1);
                }
            }
            if let Some(index) = selected {
                let reference = &entries[index].0;
                if let Some(since) = self.waiting_since.remove(reference) {
                    let nanos = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    self.metrics.wait_nanos = self.metrics.wait_nanos.saturating_add(nanos);
                    self.metrics.max_wait_nanos = self.metrics.max_wait_nanos.max(nanos);
                }
                match actions.swap_remove(index) {
                    QueueAction::Landing(oid) => self.take_landing(&tree, reference, oid)?,
                    QueueAction::Sync(oid, plan) => self.take_sync(&tree, reference, oid, plan)?,
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn record(&mut self, event: Event) -> Result<Saved> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let started = Instant::now();
        let record = Record {
            seq: self.next,
            event,
        };
        if let Err(e) = self.wal.append(&record) {
            self.stopped = true;
            return Err(e);
        }
        if let Err(e) = crate::failpoints::hit("tree.after_wal_before_apply") {
            self.stopped = true;
            return Err(e.into());
        }
        self.next += 1;
        let result = self.apply(record);
        if matches!(result, Ok(Saved::Commit(_))) {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.metrics.saves = self.metrics.saves.saturating_add(1);
            self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
            self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
        }
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn apply(&mut self, record: Record) -> Result<Saved> {
        let seq = record.seq;
        let mut save = None;
        let mut open_trailer = None;
        let mut outcome = Saved::Unchanged;
        match record.event {
            Event::Fork { name, parent, at } => {
                let mut tree = self.tree(&parent)?.clone();
                for object in std::mem::take(&mut tree.anonymous) {
                    tree.bytes.remove(&object);
                    tree.lines.remove(&object);
                    tree.identities.remove(&object);
                    tree.fresh_names.remove(&object);
                }
                let branch = format!("refs/heads/{name}");
                tree.head = match self.repo.head(&branch)? {
                    Some(oid) => oid,
                    None => {
                        let oid = Oid::from_str(&at)?;
                        self.repo.ref_transaction(&[RefUpdate {
                            refname: branch.clone(),
                            new: Some(oid),
                            expected_old: None,
                        }])?;
                        oid
                    }
                };
                tree.committed = self.watermark(tree.head)?;
                self.repo
                    .set_config(&format!("branch.{name}.remote"), ".")?;
                self.repo.set_config(
                    &format!("branch.{name}.merge"),
                    &format!("refs/heads/{parent}"),
                )?;
                self.trees.insert(name.clone(), tree);
                self.checkout(&name)?;
            }
            Event::Open {
                tree,
                path,
                writer,
                object,
                create,
            } => {
                if create {
                    let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                    t.names.insert(path.clone(), object);
                    t.bytes.insert(object, Vec::new());
                    t.lines.insert(object, LineMap::default());
                    t.identities.insert(object, MergeFileIdentity(object));
                    t.namespace.insert(
                        MergeFileIdentity(object),
                        NameHistory::created(seq, path.clone()),
                    );
                    t.fresh_names.insert(object);
                    save = Some((tree.clone(), writer.clone(), "Create"));
                }
                let bytes = self.tree(&tree)?.bytes[&object].clone();
                self.handles.insert(
                    seq,
                    Handle {
                        tree,
                        writer,
                        object,
                        bytes,
                        calls: Vec::new(),
                    },
                );
            }
            Event::AnonymousOpen { tree, writer } => {
                let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                t.anonymous.insert(seq);
                t.bytes.insert(seq, Vec::new());
                t.lines.insert(seq, LineMap::default());
                t.identities.insert(seq, MergeFileIdentity(seq));
                t.fresh_names.insert(seq);
                self.handles.insert(
                    seq,
                    Handle {
                        tree,
                        writer,
                        object: seq,
                        bytes: Vec::new(),
                        calls: Vec::new(),
                    },
                );
            }
            Event::FirstLink {
                tree,
                object,
                path,
                writer,
            } => {
                let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                if !t.anonymous.remove(&object) {
                    return Err(Error::NotFound);
                }
                t.namespace.insert(
                    t.identities[&object],
                    NameHistory::created(seq, path.clone()),
                );
                t.names.insert(path, object);
                save = Some((tree, writer, "Create"));
            }
            Event::Call { handle, call } => {
                let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
                call.apply(&mut h.bytes)?;
                h.calls.push((seq, call.clone()));
                let (tree, object) = (h.tree.clone(), h.object);
                self.pages_note_call(&tree, object, &call)?;
            }
            Event::PageWrite { handle, runs } => {
                self.pages_note_writeback(seq, handle, &runs)?;
            }
            Event::Close { handle } => {
                let h = self.handles.remove(&handle).ok_or(Error::NotFound)?;
                let calls = self.pages_calls_for_close(&h);
                let (saved_tree, saved_object) = (h.tree.clone(), h.object);
                let tree = self.trees.get_mut(&h.tree).ok_or(Error::NotFound)?;
                if tree.anonymous.remove(&h.object) {
                    tree.bytes.remove(&h.object);
                    tree.lines.remove(&h.object);
                    tree.identities.remove(&h.object);
                    tree.fresh_names.remove(&h.object);
                } else if !tree.names.values().any(|o| *o == h.object) {
                    outcome = Saved::OrphanDropped;
                } else if !calls.is_empty() {
                    let mut bytes = tree.bytes[&h.object].clone();
                    for call in &calls {
                        call.apply(&mut bytes)?;
                    }
                    let mut index = 0_u64;
                    let mut next_id = || {
                        index += 1;
                        format!("{seq}:{index}")
                    };
                    let lines = if calls.iter().all(|call| matches!(call, Call::Append(_))) {
                        let suffix: Vec<u8> = calls
                            .iter()
                            .flat_map(|call| match call {
                                Call::Append(bytes) => bytes.as_slice(),
                                _ => unreachable!(),
                            })
                            .copied()
                            .collect();
                        tree.lines[&h.object].append(&suffix, &mut next_id)?
                    } else {
                        tree.lines[&h.object].align(&bytes, &mut next_id)?
                    };
                    tree.lines.insert(h.object, lines);
                    tree.bytes.insert(h.object, bytes);
                    open_trailer = Some(handle);
                    save = Some((h.tree, h.writer, "Direct"));
                }
                self.pages_saved(&saved_tree, saved_object)?;
            }
            Event::Rename {
                tree,
                from,
                to,
                writer,
            } => {
                let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                let object = t.names.remove(&from).ok_or(Error::NotFound)?;
                if let Some(replaced) = t.names.get(&to).copied()
                    && t.fresh_names.contains(&object)
                {
                    let source_id = t.identities[&object];
                    t.namespace
                        .get_mut(&source_id)
                        .ok_or(Error::Format)?
                        .advance(seq, None, None);
                    let mut index = 0_u64;
                    let lines = t.lines[&replaced].align(&t.bytes[&object], || {
                        index += 1;
                        format!("{seq}:{index}")
                    })?;
                    t.lines.insert(object, lines);
                    t.identities.insert(object, t.identities[&replaced]);
                    if !t.fresh_names.contains(&replaced) {
                        t.fresh_names.remove(&object);
                    }
                } else {
                    let source_id = t.identities[&object];
                    if let Some(replaced) = t.names.get(&to).copied() {
                        let replaced_id = t.identities[&replaced];
                        t.namespace
                            .get_mut(&replaced_id)
                            .ok_or(Error::Format)?
                            .advance(seq, None, Some(source_id));
                    }
                    t.namespace
                        .get_mut(&source_id)
                        .ok_or(Error::Format)?
                        .advance(seq, Some(to.clone()), None);
                    t.fresh_names.remove(&object);
                }
                t.names.insert(to, object);
                save = Some((tree, writer, "Rename"));
            }
            Event::Unlink { tree, path, writer } => {
                let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                let object = t.names.remove(&path).ok_or(Error::NotFound)?;
                t.namespace
                    .get_mut(&t.identities[&object])
                    .ok_or(Error::Format)?
                    .advance(seq, None, None);
                save = Some((tree, writer, "Unlink"));
            }
        }
        if let Some((tree, writer, cause)) = save {
            if let Some(head) = self.legacy_commits.get(&(tree.clone(), seq)).copied() {
                self.legacy_snapshots
                    .insert(head, Self::snapshot_of(self.tree(&tree)?));
            }
            if seq > self.tree(&tree)?.committed {
                outcome = Saved::Commit(self.commit(&tree, &writer, cause, seq, open_trailer)?);
            }
        }
        Ok(outcome)
    }

    fn watermark(&self, head: Oid) -> Result<u64> {
        let commit = self.repo.read_commit(head)?;
        std::str::from_utf8(&commit.message)
            .ok()
            .and_then(|m| m.lines().find_map(|l| l.strip_prefix("Agentfs-Event: ")))
            .and_then(|n| n.parse().ok())
            .ok_or(Error::Format)
    }

    fn commit(
        &mut self,
        name: &str,
        writer: &str,
        cause: &str,
        seq: u64,
        open: Option<u64>,
    ) -> Result<Oid> {
        let t = self.tree(name)?;
        let mut files = Vec::new();
        for (path, object) in &t.names {
            let blob = self.repo.write_blob(&t.bytes[object])?;
            files.push(TreeEntry {
                name: path.as_bytes().to_vec(),
                mode: 0o100644,
                oid: blob,
            });
        }
        let files = self.repo.write_tree(&files)?;
        let mut metadata = BTreeMap::new();
        for (path, object) in &t.names {
            metadata.insert(path, (object, t.identities[object], &t.lines[object]));
        }
        let blob = self.repo.write_blob(&serde_json::to_vec(&metadata)?)?;
        let namespace = self.repo.write_blob(&serde_json::to_vec(&t.namespace)?)?;
        let tree = self.repo.write_tree(&[
            TreeEntry {
                name: b"files".to_vec(),
                mode: 0o040000,
                oid: files,
            },
            TreeEntry {
                name: b"metadata.json".to_vec(),
                mode: 0o100644,
                oid: blob,
            },
            TreeEntry {
                name: b"namespace.json".to_vec(),
                mode: 0o100644,
                oid: namespace,
            },
        ])?;
        let mut message = format!(
            "{cause} by {writer}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: Direct\nAgentfs-Tree: {name}\nAgentfs-Event: {seq}\nAgentfs-Format: envelope-1\n"
        );
        if let Some(open) = open {
            message.push_str(&format!("Agentfs-Open: {open}\n"));
        }
        let head = self.repo.write_commit(tree, &[t.head], &message)?;
        let branch = format!("refs/heads/{name}");
        let mut updates = vec![RefUpdate {
            refname: branch,
            new: Some(head),
            expected_old: Some(t.head),
        }];
        self.object_table_update(name, &Self::snapshot_of(t), seq, &mut updates)?;
        let enqueued = self.enqueue_moves(name, head, &mut updates)?;
        crate::failpoints::hit("tree.after_objects_durable")?;
        self.repo.ref_transaction(&updates)?;
        crate::failpoints::hit("tree.after_refs_durable")?;
        let t = self.trees.get_mut(name).ok_or(Error::NotFound)?;
        t.head = head;
        t.committed = seq;
        self.metrics.queued = self.metrics.queued.saturating_add(enqueued);
        crate::failpoints::hit("tree.before_checkout")?;
        self.checkout(name)?;
        crate::failpoints::hit("tree.after_checkout")?;
        Ok(head)
    }

    // Queue refs and the branch move are committed by the same reftable
    // transaction. The persisted counter supplies order across restart and is
    // independent of commit dates and journal event numbers.
    fn enqueue_moves(&self, tree: &str, head: Oid, updates: &mut Vec<RefUpdate>) -> Result<u64> {
        let destinations: Vec<(String, Oid)> = if tree == "main" {
            let sync = self.repo.write_blob(b"sync")?;
            self.trees
                .keys()
                .filter(|name| name.as_str() != "main")
                .map(|name| (name.clone(), sync))
                .collect()
        } else {
            vec![("main".to_owned(), head)]
        };
        if destinations.is_empty() {
            return Ok(0);
        }
        let old = self.repo.head(QUEUE_SEQUENCE)?;
        let mut sequence: u64 = match old {
            Some(oid) => std::str::from_utf8(&self.repo.read_blob(oid)?)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Format)?,
            None => 0,
        };
        for (destination, oid) in &destinations {
            sequence = sequence.checked_add(1).ok_or(Error::TooLarge)?;
            updates.push(RefUpdate {
                refname: format!("refs/queue/{destination}/{sequence:020}"),
                new: Some(*oid),
                expected_old: None,
            });
        }
        let value = self.repo.write_blob(sequence.to_string().as_bytes())?;
        updates.push(RefUpdate {
            refname: QUEUE_SEQUENCE.into(),
            new: Some(value),
            expected_old: old,
        });
        Ok(destinations.len() as u64)
    }

    fn checkout(&self, name: &str) -> Result<()> {
        let dir = self.root.join("trees").join(name);
        fs::create_dir_all(&dir)?;
        // Rebuild from the BRANCH, never replay's intermediate in-memory state.
        let commit = self.repo.read_commit(self.tree(name)?.head)?;
        // The trailer distinguishes legacy roots even if a workload used our names.
        let has_envelope = std::str::from_utf8(&commit.message)
            .is_ok_and(|m| m.lines().any(|l| l == "Agentfs-Format: envelope-1"));
        let tree = if has_envelope {
            self.repo
                .list_tree(commit.tree)?
                .into_iter()
                .find(|e| e.name == b"files")
                .ok_or(Error::Format)?
                .oid
        } else {
            commit.tree
        };
        self.repo.checkout_tree(tree, &dir)?;
        Ok(())
    }
}

// Existing recording commits predate durable namespace maps. Rebuild their
// snapshots from the same WAL used for object recovery, keyed by the commit's
// event trailer. New commits carry both maps themselves.
fn legacy_commit_index(repo: &TreeGit) -> Result<BTreeMap<(String, u64), Oid>> {
    let mut todo: Vec<Oid> = repo
        .list_refs("refs/heads/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    let mut seen = BTreeSet::new();
    let mut result = BTreeMap::new();
    while let Some(oid) = todo.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let commit = repo.read_commit(oid)?;
        todo.extend(commit.parents);
        if let Ok(message) = std::str::from_utf8(&commit.message) {
            let has_envelope = message
                .lines()
                .any(|line| line == "Agentfs-Format: envelope-1");
            if has_envelope
                && repo
                    .list_tree(commit.tree)?
                    .iter()
                    .any(|e| e.name == b"namespace.json")
            {
                continue;
            }
            let tree = message
                .lines()
                .find_map(|line| line.strip_prefix("Agentfs-Tree: "));
            let seq = message
                .lines()
                .find_map(|line| line.strip_prefix("Agentfs-Event: "))
                .and_then(|s| s.parse::<u64>().ok());
            if let (Some(tree), Some(seq)) = (tree, seq) {
                result.insert((tree.to_owned(), seq), oid);
            }
        }
    }
    Ok(result)
}

// The exclusive actor lock is held first; no other writer can own these.
fn remove_stale_locks(gitdir: &Path) -> io::Result<()> {
    fn refs(dir: &Path) -> io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                refs(&entry.path())?;
            } else if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|s| s == "lock")
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    refs(&gitdir.join("refs"))?;
    for name in [
        "HEAD.lock",
        "config.lock",
        "packed-refs.lock",
        "index.lock",
        "reftable/tables.list.lock",
    ] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.contains('/') {
        return Err(Error::Name);
    }
    Ok(())
}

fn writer_label(writer: &str) -> Result<()> {
    if writer.is_empty() || writer.len() > 256 || writer.chars().any(char::is_control) {
        return Err(Error::Writer);
    }
    Ok(())
}
