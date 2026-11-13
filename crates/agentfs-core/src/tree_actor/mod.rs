//! The tree actor (git-model §3, §6, §7, §12 and §18; `docs/trees.md`).
//!
//! One exclusively locked actor owns a repository, private checkouts and a
//! journal. Every call is fsynced before acknowledgement (deliberately stronger
//! and slower than the target's group commit). A step's pointer changes are one
//! journal record, the commit point: objects are durable before it, and Git's
//! refs and the private checkouts are copies made after it, rebuilt from the
//! journal at every open and after any failed copy. Nothing decides from a copy.

mod journal;
mod pointers;
pub mod queue;
mod recording;
mod referee;
pub mod stages;
mod tree;
mod tree_id;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::file_lock::{FileLock, Mode};
use crate::tree_git::{RefUpdate, TreeEntry, TreeGit};
use journal::{
    CompactError, Entry, FoldedOpen, Journal, LiveOpen, PointerChange, Record, compact_at, fold,
    parse_tree,
};
use pointers::Pointers;
use recording::{Handle, pages};
use stages::{Stages, Timer, Which};
use tree::{EventNumber, Lines, MergeFileIdentity, Tree};
pub use tree_id::{NotATree, TreeId};

const FORMAT: &str = "agentfs-trees-3\n";
const JOURNAL: &str = "calls.wal";
const INITIALIZING: &str = "initializing";
type Result<T> = std::result::Result<T, Error>;

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
    #[error("{name} names lost object {oid}; operator recovery required")]
    MissingObject { name: String, oid: Oid },
    #[error("pointer {name} is not at the value a step expected")]
    PointerMoved { name: String },
}

impl From<referee::Broken> for Error {
    /// A broken ancestry is damaged history.
    fn from(_: referee::Broken) -> Self {
        Self::Format
    }
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
            Self::TreeGit(crate::tree_git::Error::NoSpace(_)) => libc::ENOSPC,
            Self::Io(e)
            | Self::TreeGit(crate::tree_git::Error::Io(e))
            | Self::Reconcile(crate::tree_reconcile::Error::Io(e)) => {
                e.raw_os_error().unwrap_or(libc::EIO)
            }
            _ => libc::EIO,
        }
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn branch(tree: TreeId) -> String {
    format!("refs/heads/{tree}")
}

fn queue(tree: TreeId) -> String {
    format!("refs/queue/{tree}/")
}

/// The tree a commit's `Agentfs-Tree` trailer names, if it has one.
fn tree_trailer(message: &[u8]) -> Result<Option<TreeId>> {
    std::str::from_utf8(message)
        .ok()
        .and_then(|m| m.lines().find_map(|l| l.strip_prefix("Agentfs-Tree: ")))
        .map(parse_tree)
        .transpose()
}

/// The upstream map as Git's config writes it: each fork's text and its upstream's.
fn upstream_config(upstreams: &BTreeMap<TreeId, TreeId>) -> BTreeMap<String, String> {
    upstreams
        .iter()
        .map(|(fork, upstream)| (fork.to_string(), upstream.to_string()))
        .collect()
}

/// The actor's [`referee::Landed`]: whether a save's commit is in `upstream`'s history. That
/// never changes, and every line one save wrote has that save's commit: ask Git once per commit,
/// not once per line (an 8 MiB file is 8,000 lines).
struct LandedInto<'a> {
    repo: &'a TreeGit,
    owners: &'a BTreeMap<EventNumber, Oid>,
    upstream: Oid,
    reached: BTreeMap<Oid, bool>,
}

impl referee::Landed for LandedInto<'_> {
    fn landed(&mut self, save: EventNumber) -> Result<bool> {
        let owner = *self.owners.get(&save).ok_or(Error::Format)?;
        if let Some(known) = self.reached.get(&owner) {
            return Ok(*known);
        }
        let answer = self.repo.is_ancestor(owner, self.upstream)?;
        self.reached.insert(owner, answer);
        Ok(answer)
    }
}

struct SyncPlan {
    target: Option<Oid>,
    snapshot: referee::Snapshot,
    remaining: bool,
    keys: BTreeSet<queue::Key>,
}

#[derive(Serialize, Deserialize)]
struct ObjectTable {
    files: BTreeMap<MergeFileIdentity, u64>,
}

enum QueueAction {
    Landing(Oid),
    Sync(Oid, SyncPlan),
}

/// What [`Volume::take_next`] did: took one entry, found every entry waiting, or
/// found none queued. Apart from the model's (`agentfs_model::calls::Took`):
/// neither crate depends on the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Took {
    Taken,
    Waiting,
    Empty,
}

/// Result of a release or crash-recovered release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Time and Git children by stage, process-wide (see [`stages`]).
    pub stages: Stages,
    /// The journal file's length now: what a held open file keeps from compaction.
    pub journal_bytes: u64,
}

/// A step built on a copy of its tree and not yet on record.
struct Draft {
    by: String,
    tree: TreeId,
    built: Tree,
    changes: Vec<RefUpdate>,
    closes: Option<u64>,
    opened: Option<LiveOpen>,
    linked: Option<u64>,
    /// A fork this step makes, and its upstream.
    fork: Option<(TreeId, TreeId)>,
    saved: Saved,
    enqueued: u64,
    taken: u64,
}

impl Draft {
    fn new(by: &str, tree: TreeId, built: Tree) -> Self {
        Self {
            by: by.to_owned(),
            tree,
            built,
            changes: Vec::new(),
            closes: None,
            opened: None,
            linked: None,
            fork: None,
            saved: Saved::Unchanged,
            enqueued: 0,
            taken: 0,
        }
    }

    fn entry(&self) -> Entry {
        let hex = |oid: Option<Oid>| oid.map(|oid| oid.to_string());
        Entry::Step {
            by: self.by.clone(),
            closes: self.closes,
            opened: self.opened.as_ref().map(LiveOpen::record),
            linked: self.linked,
            fork: self
                .fork
                .map(|(fork, upstream)| (fork.to_string(), upstream.to_string())),
            pointers: self
                .changes
                .iter()
                .map(|change| PointerChange {
                    name: change.refname.clone(),
                    new: hex(change.new),
                    old: hex(change.expected_old),
                })
                .collect(),
        }
    }
}

/// A single synchronous actor. Sharing requires a mutex; a second process
/// cannot open this volume. No method exposes its state area to workloads.
pub struct Volume {
    root: PathBuf,
    repo: TreeGit,
    journal: Journal,
    /// Runtime pointer reads come from here, never from Git's copy.
    pointers: Pointers,
    /// Each fork's upstream. The root has none.
    upstreams: BTreeMap<TreeId, TreeId>,
    trees: BTreeMap<TreeId, Tree>,
    handles: BTreeMap<u64, Handle>,
    /// Closes that could not save, in arrival order. Their handles count as
    /// open until a later step in their tree saves them, or a restart does.
    pending_closes: Vec<u64>,
    /// Each direct save's commit, by event number (see `rebuild_owners`).
    owners: BTreeMap<EventNumber, Oid>,
    next: u64,
    stopped: bool,
    /// A copy failed: the next step rebuilds refs whole and redoes checkouts.
    copies_stale: bool,
    stale_checkouts: BTreeSet<TreeId>,
    compact_threshold: u64,
    metrics: Metrics,
    waiting_since: BTreeMap<String, Instant>,
    pages: BTreeMap<(TreeId, u64), pages::ObjectPages>,
    /// Objects takes changed since the view last asked (`drain_changed`).
    changed: Vec<(TreeId, u64)>,
    _lock: Arc<FileLock>,
}

// Only a first init deletes, and only under its own marker: a cut one holds
// nothing a call returned for. Any other state area with no `format` or no
// journal is refused by the caller, with nothing removed.
fn prepare_state_area(root: &Path, lock: &Arc<FileLock>) -> Result<()> {
    let marker = root.join(INITIALIZING);
    if marker.exists() {
        for dir in ["history.git", "trees"] {
            if root.join(dir).exists() {
                fs::remove_dir_all(root.join(dir))?;
            }
        }
        for file in [JOURNAL, "format"] {
            match fs::remove_file(root.join(file)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    } else if fs::read_dir(root)?.any(|e| e.map_or(true, |e| e.file_name() != "actor.lock")) {
        return Ok(());
    }
    fs::write(&marker, b"")?;
    File::open(&marker)?.sync_all()?;
    File::open(root)?.sync_all()?;
    let repo = TreeGit::init_with_lease(root.join("history.git"), Arc::clone(lock))?;
    repo.write_config(&BTreeMap::new())?;
    let tree = repo.write_tree(&[])?;
    let head = repo.write_commit(
        tree,
        &[],
        &format!(
            "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: {}\nAgentfs-Event: 0\n",
            TreeId::Root
        ),
    )?;
    Journal::create(
        &root.join(JOURNAL),
        &Record {
            seq: 0,
            at_ns: now_ns(),
            entry: Entry::Checkpoint {
                next_seq: 1,
                pointers: [(branch(TreeId::Root), head.to_string())]
                    .into_iter()
                    .collect(),
                upstreams: BTreeMap::new(),
                opens: Vec::new(),
                page_saved: Vec::new(),
            },
        },
    )?;
    crate::failpoints::hit("tree.init_before_format")?;
    let temporary = root.join("format.tmp");
    fs::write(&temporary, FORMAT)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, root.join("format"))?;
    File::open(root)?.sync_all()?;
    fs::remove_file(&marker)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

// What is lost of any pointer's commit, tree and blobs refuses to serve. The
// second pass, on this failure path only, names the pointer.
fn check_objects(repo: &TreeGit, pointers: &Pointers) -> Result<()> {
    let missing = repo.missing_objects(&pointers.tips())?;
    let Some(first) = missing.first() else {
        return Ok(());
    };
    for (name, tip) in pointers.under("refs/") {
        if let Some(oid) = repo.missing_objects(&[tip])?.first() {
            return Err(Error::MissingObject { name, oid: *oid });
        }
    }
    Err(Error::MissingObject {
        name: "refs/".to_owned(),
        oid: *first,
    })
}

impl Volume {
    /// Initialize an empty directory or recover a tree volume before serving.
    /// Recovery closes every abandoned write-open exactly once. It takes no
    /// queue entries: the view's worker or a caller runs [`Volume::progress`].
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for a state area that is not this format (an older
    /// format included), or whose journal names a tree that is no [`TreeId`],
    /// has no root, gives the root an upstream or a fork an upstream that is no
    /// tree; [`Error::CorruptJournal`] and [`Error::MissingObject`] when the journal
    /// or a pointer's objects are damaged (docs/runbooks/recording-failures.md),
    /// and any failure of a recovery close.
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
        // Never grant another user access to a newly initialized state area.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        prepare_state_area(&root, &lock)?;
        match fs::read_to_string(root.join("format")) {
            Ok(format) if format == FORMAT => {}
            Ok(_) => return Err(Error::Format),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Format),
            Err(e) => return Err(e.into()),
        }
        let (journal, frames) = Journal::open(&root.join(JOURNAL))?;
        let folded = fold(&frames)?;
        drop(frames);
        let history = root.join("history.git");
        remove_stale_locks(&history)?;
        let repo = TreeGit::open_with_lease(&history, Arc::clone(&lock))?;
        // Before any git command: an empty config silently loses core.fsync,
        // and an empty HEAD is "not a git repository".
        repo.write_config(&upstream_config(&folded.upstreams))?;
        check_objects(&repo, &folded.pointers)?;
        repo.rebuild_refs(&folded.pointers.creates())?;
        crate::failpoints::hit("tree.after_ref_rebuild")?;
        let mut volume = Self {
            root,
            repo,
            journal,
            pointers: folded.pointers,
            upstreams: folded.upstreams,
            trees: BTreeMap::new(),
            handles: BTreeMap::new(),
            pending_closes: Vec::new(),
            owners: BTreeMap::new(),
            next: folded.next_seq,
            stopped: false,
            copies_stale: false,
            stale_checkouts: BTreeSet::new(),
            compact_threshold: u64::MAX,
            metrics: Metrics::default(),
            waiting_since: BTreeMap::new(),
            pages: BTreeMap::new(),
            changed: Vec::new(),
            _lock: lock,
        };
        volume.rebuild_owners()?;
        volume.build_trees()?;
        for (handle, FoldedOpen { open, calls }) in folded.opens {
            let tree = volume.trees.get_mut(&open.tree).ok_or(Error::Format)?;
            if open.anonymous {
                tree.add_anonymous(open.object);
            }
            volume.handles.insert(
                handle,
                Handle {
                    tree: open.tree,
                    writer: open.writer,
                    object: open.object,
                    // Nothing reads it: this handle closes before serving.
                    bytes: Vec::new(),
                    calls,
                },
            );
            volume.pages_opened(open.tree, open.object);
        }
        let mut page_writes = folded.page_writes;
        let saved = folded.saved_through;
        let page_files: BTreeSet<(TreeId, u64)> =
            page_writes.keys().chain(saved.keys()).cloned().collect();
        for key in page_files {
            let writes = page_writes.remove(&key).unwrap_or_default();
            let saved_through = saved.get(&key).copied().unwrap_or(0);
            volume.pages_restore(key.0, key.1, writes, saved_through);
        }
        let abandoned: Vec<_> = volume.handles.keys().copied().collect();
        for handle in abandoned {
            volume.close(handle)?;
        }
        for tree in volume.trees() {
            volume.checkout(tree)?;
        }
        volume.metrics.queued = volume.pointers.under("refs/queue/").len() as u64;
        match volume.compact() {
            Ok(()) => {}
            Err(e @ CompactError::Before(_)) => eprintln!("tree CompactionFailed: {e}"),
            Err(CompactError::After(e)) => return Err(e.into()),
        }
        volume.compact_threshold = compact_at(volume.journal.len());
        Ok(volume)
    }

    /// Every branch is a tree's, the root among them, and every entry of the
    /// upstream map is a fork with a branch whose upstream is the root (forks are
    /// made only of the root, and only the root's queue holds landings): else
    /// [`Error::Format`].
    fn build_trees(&mut self) -> Result<()> {
        for (reference, head) in self.pointers.under("refs/heads/") {
            let id = parse_tree(reference.strip_prefix("refs/heads/").ok_or(Error::Format)?)?;
            let mut snapshot = self.snapshot_at(head)?;
            self.restore_object_table(id, &mut snapshot)?;
            let mut tree = Tree::default();
            tree.install(snapshot)?;
            self.trees.insert(id, tree);
        }
        if self.trees.contains_key(&TreeId::Root)
            && self.upstreams.iter().all(|(fork, upstream)| {
                !fork.is_root() && upstream.is_root() && self.trees.contains_key(fork)
            })
        {
            Ok(())
        } else {
            Err(Error::Format)
        }
    }

    /// Every tree: the root first, then the forks by number.
    pub fn trees(&self) -> Vec<TreeId> {
        self.trees.keys().copied().collect()
    }

    fn upstream_of(&self, tree: TreeId) -> Option<TreeId> {
        self.upstreams.get(&tree).copied()
    }

    /// Snapshot measurements without exposing them through a workload view.
    pub fn metrics(&self) -> Metrics {
        let mut metrics = self.metrics;
        metrics.stages = Stages::current();
        metrics.journal_bytes = self.journal.len();
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

    pub fn head(&self, tree: TreeId) -> Result<Oid> {
        self.tree(tree)?;
        self.pointers.get(&branch(tree)).ok_or(Error::NotFound)
    }

    /// Fork IDs are numeric and never reused. Forking a fork is not exposed yet.
    /// A fork of the root, at the root's saved head.
    pub fn fork(&mut self) -> Result<TreeId> {
        let upstream = TreeId::Root;
        let (event, _) = self.step(upstream, |v, event| {
            let fork = TreeId::Fork(event.get());
            let at = v.head(upstream)?;
            let mut tree = v.tree(upstream)?.clone();
            for object in std::mem::take(&mut tree.anonymous) {
                tree.drop_object(object);
            }
            let mut draft = Draft::new("agentfs", fork, tree);
            draft.changes.push(RefUpdate {
                refname: branch(fork),
                new: Some(at),
                expected_old: None,
            });
            draft.fork = Some((fork, upstream));
            Ok(draft)
        })?;
        Ok(TreeId::Fork(event.get()))
    }

    fn tree(&self, tree: TreeId) -> Result<&Tree> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.trees.get(&tree).ok_or(Error::NotFound)
    }

    fn snapshot_at(&self, head: Oid) -> Result<referee::Snapshot> {
        let _timer = Timer::start(Which::SnapshotRead);
        let commit = self.repo.read_commit(head)?;
        let is_envelope = std::str::from_utf8(&commit.message).is_ok_and(|message| {
            message
                .lines()
                .any(|line| line == "Agentfs-Format: envelope-1")
        });
        if !is_envelope {
            // The Root commit alone has no envelope, and an empty tree.
            if self.repo.list_tree(commit.tree)?.is_empty() {
                return Ok(referee::Snapshot {
                    files: BTreeMap::new(),
                    names: BTreeMap::new(),
                });
            }
            return Err(Error::Format);
        }
        let entries = self.repo.list_tree(commit.tree)?;
        let metadata = entries
            .iter()
            .find(|e| e.name == b"metadata.json")
            .ok_or(Error::Format)?;
        let namespace = entries
            .iter()
            .find(|e| e.name == b"namespace.json")
            .ok_or(Error::Format)?;
        let by_path: BTreeMap<String, (u64, MergeFileIdentity, Lines)> =
            serde_json::from_slice(&self.repo.read_blob(metadata.oid)?)?;
        let names = serde_json::from_slice(&self.repo.read_blob(namespace.oid)?)?;
        let files = by_path
            .into_values()
            .map(|(object, id, lines)| (id, referee::File { object, lines }))
            .collect();
        Ok(referee::Snapshot { files, names })
    }

    fn snapshot_tree(&self, snapshot: &referee::Snapshot) -> Result<Oid> {
        let _timer = Timer::start(Which::Objects);
        let mut files = Vec::new();
        let mut metadata = BTreeMap::new();
        for (id, file) in &snapshot.files {
            let history = snapshot.names.get(id).ok_or(Error::Format)?;
            let path = history.path()?.ok_or(Error::Format)?;
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
        self.envelope(files, metadata, namespace)
    }

    fn envelope(&self, files: Oid, metadata: Oid, namespace: Oid) -> Result<Oid> {
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
        tree: TreeId,
        snapshot: &referee::Snapshot,
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
            .write_blob(&serde_json::to_vec(&ObjectTable { files })?)?;
        updates.push(RefUpdate {
            expected_old: self.pointers.get(&name),
            refname: name,
            new: Some(oid),
        });
        Ok(())
    }

    fn restore_object_table(&self, tree: TreeId, snapshot: &mut referee::Snapshot) -> Result<()> {
        if let Some(oid) = self.pointers.get(&format!("refs/agentfs/objects/{tree}")) {
            let table: ObjectTable = serde_json::from_slice(&self.repo.read_blob(oid)?)?;
            for (id, file) in &mut snapshot.files {
                file.object = *table.files.get(id).ok_or(Error::Format)?;
            }
        }
        Ok(())
    }

    /// Whether two snapshots show the same: the same files, each with the same line
    /// versions in the same order, and the same name versions. The versions each line
    /// version was written over may differ.
    fn shows_the_same(a: &referee::Snapshot, b: &referee::Snapshot) -> bool {
        a.files.len() == b.files.len()
            && a.names.len() == b.names.len()
            && a.files.iter().all(|(id, file)| {
                b.files
                    .get(id)
                    .is_some_and(|other| other.lines.lines == file.lines.lines)
            })
            && a.names.iter().all(|(id, history)| {
                b.names
                    .get(id)
                    .is_some_and(|other| other.head == history.head)
            })
    }

    /// The commit of every direct save on record, by its event number: one Git
    /// child over everything the pointers reach. An event has one commit, so
    /// no walk from a fork's head is needed to find it. A copy: `owners` is
    /// folded as saves land and is empty-then-rebuilt here at every open.
    fn rebuild_owners(&mut self) -> Result<()> {
        let _timer = Timer::start(Which::EventOwners);
        let mut tips: Vec<Oid> = Vec::new();
        // Landings queue as commits in the root's queue; syncs are blobs.
        for prefix in ["refs/heads/", &queue(TreeId::Root), "refs/collisions/"] {
            tips.extend(self.pointers.under(prefix).into_iter().map(|(_, oid)| oid));
        }
        self.owners = self
            .repo
            .direct_events(&tips)?
            .into_iter()
            .map(|(event, oid)| (EventNumber::new(event), oid))
            .collect();
        Ok(())
    }

    fn prior_decisions(&self, mut head: Oid) -> Result<referee::PriorDecisions> {
        let _timer = Timer::start(Which::PriorDecisions);
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

    fn build_landing(
        &self,
        tree: TreeId,
        reference: &str,
        incoming: Oid,
        event: EventNumber,
    ) -> Result<Draft> {
        let current = self.head(tree)?;
        let t = self.tree(tree)?;
        let incoming_commit = self.repo.read_commit(incoming)?;
        let base_commit = *incoming_commit.parents.first().ok_or(Error::Format)?;
        let (mine, base, theirs) = (
            Self::snapshot_of(t),
            self.snapshot_at(base_commit)?,
            self.snapshot_at(incoming)?,
        );
        let prior = self.prior_decisions(current)?;
        let decision = {
            let _timer = Timer::start(Which::Judge);
            referee::land(&mine, &base, &theirs, &prior)?
        };
        let head = if base_commit == current {
            // Made on the parent's current version: every line replaces the
            // parent's, so nothing collides, and the save itself is the head.
            // The journal's record of this step moves the pointer (finding 45).
            if !decision.collisions.is_empty()
                || !referee::touches(&decision.snapshot, &theirs).is_empty()
            {
                return Err(Error::Format);
            }
            incoming
        } else {
            let content = self.snapshot_tree(&decision.snapshot)?;
            let mut message = format!(
                "Landing\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Landing\nAgentfs-Tree: {tree}\nAgentfs-Event: {event}\nAgentfs-Format: envelope-1\n"
            );
            if !decision.collisions.is_empty() {
                message.push_str(&format!(
                    "Agentfs-Collisions: {}\n",
                    serde_json::to_string(&decision.collisions)?
                ));
            }
            self.repo
                .write_commit(content, &[current, incoming], &message)?
        };
        let mut changes = vec![
            RefUpdate {
                refname: branch(tree),
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
            if self.pointers.get(&name).is_none() {
                changes.push(RefUpdate {
                    refname: name,
                    new: Some(incoming),
                    expected_old: None,
                });
            }
        }
        self.object_table_update(tree, &decision.snapshot, &mut changes)?;
        let enqueued = self.enqueue_moves(tree, head, event, &mut changes)?;
        let mut built = t.clone();
        built.install(decision.snapshot)?;
        let mut draft = Draft::new("agentfs", tree, built);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.taken = 1;
        Ok(draft)
    }

    fn take_landing(&mut self, tree: TreeId, reference: &str, incoming: Oid) -> Result<()> {
        let started = Instant::now();
        self.pages_take(tree, |v, event| {
            v.build_landing(tree, reference, incoming, event)
        })?;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.takes = self.metrics.takes.saturating_add(1);
        self.metrics.take_nanos = self.metrics.take_nanos.saturating_add(nanos);
        self.metrics.max_take_nanos = self.metrics.max_take_nanos.max(nanos);
        self.metrics.landings = self.metrics.landings.saturating_add(1);
        self.metrics.landing_nanos = self.metrics.landing_nanos.saturating_add(nanos);
        Ok(())
    }

    fn plan_sync(&self, tree: TreeId, open: &BTreeSet<queue::Key>) -> Result<SyncPlan> {
        let fork = self.tree(tree)?;
        let fork_head = self.head(tree)?;
        let fork_snapshot = Self::snapshot_of(fork);
        let mut todo = Vec::new();
        let upstream = self.upstream_of(tree).ok_or(Error::Format)?;
        let mut cursor = self.head(upstream)?;
        while !self.repo.is_ancestor(cursor, fork_head)? {
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
            let undecided = referee::undecided(
                &fork_snapshot,
                &mut LandedInto {
                    repo: &self.repo,
                    owners: &self.owners,
                    upstream: oid,
                    reached: BTreeMap::new(),
                },
            )?;
            let last_lines = self.last_lines(&parent, oid, &undecided)?;
            let next = {
                let _timer = Timer::start(Which::SyncMerge);
                referee::sync(&parent, &fork_snapshot, &undecided, &last_lines)?
            };
            let changes = referee::touches(&current, &next);
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

    /// The lines each file held in the parent's last version of it, for the files
    /// the parent at `at` has ended and a fork keeps under an undecided name. The
    /// parent's snapshot has no file to read them from (finding 44).
    fn last_lines(
        &self,
        parent: &referee::Snapshot,
        at: Oid,
        undecided: &referee::Undecided,
    ) -> Result<BTreeMap<MergeFileIdentity, Lines>> {
        let mut wanted = referee::ended_files(parent, undecided)?;
        let mut lines = BTreeMap::new();
        let mut cursor = at;
        while !wanted.is_empty() {
            let Some(previous) = self.repo.read_commit(cursor)?.parents.first().copied() else {
                break;
            };
            let snapshot = self.snapshot_at(previous)?;
            wanted.retain(|id| match snapshot.files.get(id) {
                Some(file) => {
                    lines.insert(*id, file.lines.clone());
                    false
                }
                None => true,
            });
            cursor = previous;
        }
        Ok(lines)
    }

    /// Whether a landing of `tree` waits in its upstream's queue.
    fn pending_from(&self, tree: TreeId) -> Result<bool> {
        let Some(upstream) = self.upstream_of(tree) else {
            return Ok(false);
        };
        for (_, oid) in self.pointers.under(&queue(upstream)) {
            let commit = self.repo.read_commit(oid)?;
            if tree_trailer(&commit.message)? == Some(tree) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn build_sync(
        &self,
        tree: TreeId,
        reference: &str,
        value: Oid,
        plan: SyncPlan,
        event: EventNumber,
    ) -> Result<Draft> {
        let current = self.head(tree)?;
        let mut built = self.tree(tree)?.clone();
        let mut changes = Vec::new();
        if let Some(target) = plan.target {
            let parent = self.snapshot_at(target)?;
            let mut snapshot = plan.snapshot;
            // R6.1: a sync that can fast-forward does. It can when nothing of the fork waits
            // for the upstream and the sync shows what the upstream's commit shows (the
            // model's `World::sync`). The fork then takes that commit's line maps and names,
            // so a version the upstream decided against stays in history only (finding 51).
            let head = if Self::shows_the_same(&parent, &snapshot) && !self.pending_from(tree)? {
                for (id, file) in &mut snapshot.files {
                    if let Some(theirs) = parent.files.get(id) {
                        file.lines = theirs.lines.clone();
                    }
                }
                snapshot.names = parent.names.clone();
                target
            } else {
                let content = self.snapshot_tree(&snapshot)?;
                let message = format!(
                    "Sync\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Sync\nAgentfs-Tree: {tree}\nAgentfs-Event: {event}\nAgentfs-Format: envelope-1\n"
                );
                self.repo
                    .write_commit(content, &[current, target], &message)?
            };
            changes.push(RefUpdate {
                refname: branch(tree),
                new: Some(head),
                expected_old: Some(current),
            });
            self.object_table_update(tree, &snapshot, &mut changes)?;
            built.install(snapshot)?;
        }
        let taken = !plan.remaining;
        if taken {
            changes.push(RefUpdate {
                refname: reference.into(),
                new: None,
                expected_old: Some(value),
            });
        }
        let mut draft = Draft::new("agentfs", tree, built);
        draft.changes = changes;
        draft.taken = u64::from(taken);
        Ok(draft)
    }

    fn take_sync(
        &mut self,
        tree: TreeId,
        reference: &str,
        value: Oid,
        plan: SyncPlan,
    ) -> Result<()> {
        let started = Instant::now();
        self.pages_take(tree, |v, event| {
            v.build_sync(tree, reference, value, plan, event)
        })?;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.takes = self.metrics.takes.saturating_add(1);
        self.metrics.take_nanos = self.metrics.take_nanos.saturating_add(nanos);
        self.metrics.max_take_nanos = self.metrics.max_take_nanos.max(nanos);
        self.metrics.syncs = self.metrics.syncs.saturating_add(1);
        self.metrics.sync_nanos = self.metrics.sync_nanos.saturating_add(nanos);
        Ok(())
    }

    /// Take at most one eligible queue entry. Callers can release the actor
    /// mutex between calls, so opens do not wait behind a waiting entry. A take
    /// that fails stays queued for the next event and this answers `Ok(false)`.
    pub fn progress(&mut self) -> Result<bool> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        match self.progress_one() {
            Err(e) if !self.stopped => {
                eprintln!("tree SaveFailed: {e}");
                Ok(false)
            }
            result => result,
        }
    }

    fn progress_one(&mut self) -> Result<bool> {
        // The root first, then the forks by number: `TreeId`'s order.
        for tree in self.trees() {
            if self.take_next(tree)? == Took::Taken {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One queue step in `tree`: its first entry P1 lets go is taken whole, in
    /// this one call. Failures are not swallowed, unlike [`Volume::progress`].
    pub fn take_next(&mut self, tree: TreeId) -> Result<Took> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.tree(tree)?;
        if self.pointers.under(&queue(tree)).is_empty() {
            return Ok(Took::Empty);
        }
        // A take is a step: pending closes save first, and its plan reads
        // the tree they leave.
        self.run_pending_closes(tree, None)?;
        let entries = self.pointers.under(&queue(tree));
        let open = self.open_keys(tree);
        let mut plans = Vec::new();
        let mut actions = Vec::new();
        let planning = Timer::start(Which::QueuePlan);
        for (_, oid) in &entries {
            if tree.is_root() {
                let commit = self.repo.read_commit(*oid)?;
                let base = *commit.parents.first().ok_or(Error::Format)?;
                let source = tree_trailer(&commit.message)?.ok_or(Error::Format)?;
                let keys = referee::touches(&self.snapshot_at(base)?, &self.snapshot_at(*oid)?);
                plans.push(queue::Plan {
                    source,
                    files: keys,
                });
                actions.push(QueueAction::Landing(*oid));
            } else {
                let sync = self.plan_sync(tree, &open)?;
                plans.push(queue::Plan {
                    source: self.upstream_of(tree).ok_or(Error::Format)?,
                    files: sync.keys.clone(),
                });
                actions.push(QueueAction::Sync(*oid, sync));
            }
        }
        drop(planning);
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
                QueueAction::Landing(oid) => self.take_landing(tree, reference, oid)?,
                QueueAction::Sync(oid, plan) => self.take_sync(tree, reference, oid, plan)?,
            }
            return Ok(Took::Taken);
        }
        Ok(Took::Waiting)
    }

    /// Saves this tree's pending closes, then runs the step `build` makes.
    fn step<F>(&mut self, tree: TreeId, build: F) -> Result<(EventNumber, Saved)>
    where
        F: FnOnce(&Self, EventNumber) -> Result<Draft>,
    {
        self.run_pending_closes(tree, None)?;
        self.run_step(build)
    }

    /// One step, in order. A failure up to and including the journal record
    /// changes nothing and returns its errno. After the record, the commit
    /// point, the step has happened: a failed copy is logged and the call
    /// succeeds (AGENTS principle 5).
    fn run_step<F>(&mut self, build: F) -> Result<(EventNumber, Saved)>
    where
        F: FnOnce(&Self, EventNumber) -> Result<Draft>,
    {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let started = Instant::now();
        let event = EventNumber::new(self.next);
        let recorded = (|| -> Result<Draft> {
            crate::failpoints::hit("tree.before_objects")?;
            let draft = build(self, event)?;
            crate::failpoints::hit("tree.after_objects_durable")?;
            self.pointers.check(&draft.changes)?;
            self.journal_record(draft.entry(), &[])?;
            Ok(draft)
        })();
        let draft = match recorded {
            Ok(draft) => draft,
            Err(e) => {
                eprintln!("tree SaveFailed: {e}");
                return Err(e);
            }
        };
        let saved = draft.saved;
        let is_fork = draft.fork.is_some();
        let changes = draft.changes.clone();
        let tree = draft.tree;
        self.install(draft, event);
        if let Saved::Commit(head) = saved {
            self.owners.insert(event, head);
        }
        if matches!(saved, Saved::Commit(_)) {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.metrics.saves = self.metrics.saves.saturating_add(1);
            self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
            self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
        }
        if let Err(e) = self.copy_out(tree, is_fork, &changes) {
            self.copies_stale = true;
            eprintln!("tree CopyStale: {e}");
        }
        self.compact_if_due();
        Ok((event, saved))
    }

    /// Stage 6: the step is on record, so memory follows. Cannot fail.
    fn install(&mut self, draft: Draft, event: EventNumber) {
        let Draft {
            tree,
            built,
            changes,
            closes,
            opened,
            fork,
            enqueued,
            taken,
            ..
        } = draft;
        if let Some((name, parent)) = fork {
            self.upstreams.insert(name, parent);
        }
        let closed = closes.and_then(|handle| {
            self.pending_closes.retain(|pending| *pending != handle);
            self.handles.remove(&handle)
        });
        if let Some(open) = opened {
            let bytes = built
                .bytes
                .get(&open.object)
                .map_or_else(Vec::new, |bytes| bytes.to_vec());
            self.handles.insert(
                open.handle,
                Handle {
                    tree: open.tree,
                    writer: open.writer,
                    object: open.object,
                    bytes,
                    calls: Vec::new(),
                },
            );
            self.pages_opened(open.tree, open.object);
        }
        self.trees.insert(tree, built);
        self.pointers.apply(&changes);
        self.metrics.queued = self
            .metrics
            .queued
            .saturating_sub(taken)
            .saturating_add(enqueued);
        if let Some(closed) = closed {
            self.pages_saved(closed.tree, closed.object, event.get());
        }
    }

    /// Stage 7: Git's refs and the checkouts, both copies of what is on record.
    fn copy_out(&mut self, tree: TreeId, is_fork: bool, changes: &[RefUpdate]) -> Result<()> {
        // Named first, so that a failure anywhere below redoes this checkout.
        if changes.iter().any(|change| change.refname == branch(tree)) {
            self.stale_checkouts.insert(tree);
        }
        crate::failpoints::hit("tree.after_commit_point")?;
        {
            let _timer = Timer::start(Which::RefCopy);
            if self.copies_stale {
                self.repo.write_config(&upstream_config(&self.upstreams))?;
                self.repo.rebuild_refs(&self.pointers.creates())?;
            } else if !changes.is_empty() {
                if is_fork {
                    self.repo.write_config(&upstream_config(&self.upstreams))?;
                }
                self.repo.ref_transaction(changes)?;
            }
        }
        crate::failpoints::hit("tree.after_ref_copy")?;
        if !self.stale_checkouts.is_empty() {
            crate::failpoints::hit("tree.before_checkout")?;
            for name in &self.stale_checkouts {
                let _timer = Timer::start(Which::Checkout);
                self.checkout(*name)?;
            }
            self.stale_checkouts.clear();
            crate::failpoints::hit("tree.after_checkout")?;
        }
        self.copies_stale = false;
        Ok(())
    }

    /// Writes `t`'s commit and the pointer changes that publish it.
    fn build_save(
        &self,
        tree: TreeId,
        t: Tree,
        writer: &str,
        cause: &str,
        event: EventNumber,
        open: Option<u64>,
    ) -> Result<Draft> {
        let _timer = Timer::start(Which::Objects);
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
            metadata.insert(path, (object, t.identities[object], &*t.lines[object]));
        }
        let blob = self.repo.write_blob(&serde_json::to_vec(&metadata)?)?;
        let namespace = self.repo.write_blob(&serde_json::to_vec(&t.namespace)?)?;
        let content = self.envelope(files, blob, namespace)?;
        let mut message = format!(
            "{cause} by {writer}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: Direct\nAgentfs-Tree: {tree}\nAgentfs-Event: {event}\nAgentfs-Format: envelope-1\n"
        );
        if let Some(open) = open {
            message.push_str(&format!("Agentfs-Open: {open}\n"));
        }
        let current = self.head(tree)?;
        let head = self.repo.write_commit(content, &[current], &message)?;
        let mut changes = vec![RefUpdate {
            refname: branch(tree),
            new: Some(head),
            expected_old: Some(current),
        }];
        self.object_table_update(tree, &Self::snapshot_of(&t), &mut changes)?;
        let enqueued = self.enqueue_moves(tree, head, event, &mut changes)?;
        let mut draft = Draft::new(writer, tree, t);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.saved = Saved::Commit(head);
        Ok(draft)
    }

    // One entry per destination, numbered by the step that causes it: order
    // across restart, independent of commit dates. A tree's head moves to its
    // upstream as a landing and to each tree it is the upstream of as a sync.
    fn enqueue_moves(
        &self,
        tree: TreeId,
        head: Oid,
        event: EventNumber,
        changes: &mut Vec<RefUpdate>,
    ) -> Result<u64> {
        let mut destinations: Vec<(TreeId, Oid)> = Vec::new();
        if let Some(upstream) = self.upstream_of(tree) {
            destinations.push((upstream, head));
        }
        let downstream: Vec<TreeId> = self
            .upstreams
            .iter()
            .filter(|(_, upstream)| **upstream == tree)
            .map(|(fork, _)| *fork)
            .collect();
        if !downstream.is_empty() {
            let sync = self.repo.write_blob(b"sync")?;
            destinations.extend(downstream.into_iter().map(|fork| (fork, sync)));
        }
        for (destination, oid) in &destinations {
            changes.push(RefUpdate {
                refname: format!("{}{event:020}", queue(*destination)),
                new: Some(*oid),
                expected_old: None,
            });
        }
        Ok(destinations.len() as u64)
    }

    fn checkout(&self, tree: TreeId) -> Result<()> {
        let dir = self.root.join("trees").join(tree.to_string());
        fs::create_dir_all(&dir)?;
        // Rebuild from the branch, never from intermediate in-memory state.
        let commit = self.repo.read_commit(self.head(tree)?)?;
        // The trailer tells the Root commit even if a workload used our names.
        let has_envelope = std::str::from_utf8(&commit.message)
            .is_ok_and(|m| m.lines().any(|l| l == "Agentfs-Format: envelope-1"));
        let files = if has_envelope {
            self.repo
                .list_tree(commit.tree)?
                .into_iter()
                .find(|e| e.name == b"files")
                .ok_or(Error::Format)?
                .oid
        } else {
            commit.tree
        };
        self.repo.checkout_tree(files, &dir)?;
        Ok(())
    }
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
    for name in ["HEAD.lock", "config.lock", "packed-refs.lock", "index.lock"] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::journal::Frame;
    use super::*;

    const ROOT: TreeId = TreeId::Root;

    fn frames_of(root: &Path) -> Vec<Frame> {
        Journal::open(&root.join(JOURNAL)).unwrap().1
    }

    #[test]
    fn compaction_keeps_the_fold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = Volume::open(tmp.path()).unwrap();
        // Both kinds of live open with calls and page writes, closed opens with
        // both, a fork and a queue entry.
        let fork = v.fork().unwrap();
        let held = v.write_open(ROOT, "held", "w", true).unwrap();
        v.write(held, 0, b"held-1", false).unwrap();
        let shared = v.write_open(ROOT, "shared", "w", true).unwrap();
        v.write(shared, 0, b"shared", false).unwrap();
        let object = v.open_object(shared).unwrap();
        let other = v.write_open(ROOT, "shared", "w", false).unwrap();
        v.page_writeback(other, 1, b"WXYZ").unwrap();
        v.close(other).unwrap();
        let anonymous = v.anonymous_open(ROOT, "w").unwrap();
        v.write(anonymous, 0, b"unnamed", false).unwrap();
        let closed = v.write_open(ROOT, "closed", "w", true).unwrap();
        v.write(closed, 0, b"closed", false).unwrap();
        let closed_object = v.open_object(closed).unwrap();
        v.record_page_write(
            ROOT,
            closed_object,
            &[pages::Run {
                offset: 0,
                bytes: b"gone".to_vec(),
            }],
        )
        .unwrap();
        v.close(closed).unwrap();
        v.write_open(fork, "sandbox", "w", true).unwrap();

        let mut whole = frames_of(tmp.path());
        let folded = fold(&whole).unwrap();
        assert_eq!(folded.opens.len(), 4);
        assert_eq!(folded.opens[&held].calls.len(), 1);
        assert!(folded.opens[&anonymous].open.anonymous);
        assert_eq!(folded.upstreams[&fork], ROOT);
        assert_eq!(folded.page_writes[&(ROOT, object)][0].1.bytes, b"WXYZ");
        assert!(folded.saved_through.contains_key(&(ROOT, object)));
        assert!(!folded.page_writes.contains_key(&(ROOT, closed_object)));
        assert_eq!(folded.next_seq, v.next);
        assert_eq!(folded.pointers, v.pointers);

        v.compact().unwrap();
        let compacted = frames_of(tmp.path());
        assert!(compacted.len() < whole.len());
        assert_eq!(fold(&compacted).unwrap(), folded);

        // What follows lands on either start and adds up the same.
        v.write(held, 0, b"held-2", false).unwrap();
        v.write(shared, 0, b"!", true).unwrap();
        let again = v.write_open(ROOT, "again", "w", true).unwrap();
        v.close(shared).unwrap();
        v.first_link(ROOT, anonymous, "named", "w").unwrap();
        v.close(again).unwrap();
        while v.progress().unwrap() {}
        let now = frames_of(tmp.path());
        whole.extend_from_slice(&now[compacted.len()..]);
        let folded = fold(&now).unwrap();
        assert_eq!(fold(&whole).unwrap(), folded);
        assert!(!folded.opens[&anonymous].open.anonymous);
        assert_eq!(folded.opens[&held].calls.len(), 2);
        assert_eq!(folded.pointers, v.pointers);
        assert_eq!(folded.next_seq, v.next);
    }

    /// R1.4, R12: a compaction keeps only the page writes memory still holds. A
    /// closed generation's write stays out of the fold, so a recovery close does
    /// not lay it over the saves that came after it.
    #[test]
    fn a_compaction_keeps_a_closed_opens_page_write_out_of_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = Volume::open(tmp.path()).unwrap();
        let first = v.write_open(ROOT, "f", "w", true).unwrap();
        v.page_writeback(first, 0, b"XXXX").unwrap();
        v.close(first).unwrap();
        let second = v.write_open(ROOT, "f", "w", false).unwrap();
        v.write(second, 0, b"YYYY", false).unwrap();
        v.close(second).unwrap();
        // Held open across the compaction, as a long-lived log is.
        let _held = v.write_open(ROOT, "f", "w", false).unwrap();
        v.compact().unwrap();
        drop(v);
        let v = Volume::open(tmp.path()).unwrap();
        assert_eq!(v.read(ROOT, "f").unwrap(), b"YYYY");
    }

    /// An edit of a checkpoint's pointers and upstreams.
    type CheckpointEdit = fn(&mut BTreeMap<String, String>, &mut BTreeMap<String, String>);

    /// A volume with fork 1 compacted to its checkpoint alone; reopened with
    /// `edit` applied to that checkpoint's pointers and upstreams.
    fn reopen_with(edit: CheckpointEdit) -> Result<Volume> {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = Volume::open(tmp.path()).unwrap();
        v.fork().unwrap();
        v.compact().unwrap();
        drop(v);
        let mut frames = frames_of(tmp.path());
        assert_eq!(
            frames.len(),
            1,
            "a compacted volume with no open is its checkpoint"
        );
        let (mut first, _) = frames.remove(0);
        let Entry::Checkpoint {
            pointers,
            upstreams,
            ..
        } = &mut first.entry
        else {
            panic!("the first frame is a checkpoint");
        };
        edit(pointers, upstreams);
        fs::remove_file(tmp.path().join(JOURNAL)).unwrap();
        Journal::create(&tmp.path().join(JOURNAL), &first).unwrap();
        Volume::open(tmp.path())
    }

    #[test]
    fn recovery_refuses_a_journal_that_names_no_tree_as_one() {
        reopen_with(|_, _| {}).unwrap();
        let cases: [(&str, CheckpointEdit); 7] = [
            ("a branch that is no tree", |pointers, _| {
                let root = pointers["refs/heads/root"].clone();
                pointers.insert("refs/heads/main".into(), root);
            }),
            ("no root", |pointers, _| {
                pointers.remove("refs/heads/root");
            }),
            ("an upstream of the root", |_, upstreams| {
                upstreams.insert("root".into(), "fork/1".into());
            }),
            ("a fork's upstream that is no tree", |_, upstreams| {
                upstreams.insert("fork/1".into(), "fork/999".into());
            }),
            ("an upstream map naming no tree", |_, upstreams| {
                upstreams.insert("main".into(), "root".into());
            }),
            ("an upstream map's fork with no branch", |_, upstreams| {
                upstreams.insert("fork/5".into(), "root".into());
            }),
            ("a fork whose upstream is not the root", |_, upstreams| {
                upstreams.insert("fork/1".into(), "fork/1".into());
            }),
        ];
        for (case, edit) in cases {
            assert!(
                matches!(reopen_with(edit), Err(Error::Format)),
                "{case}: not refused as Format"
            );
        }
    }

    #[test]
    fn a_format_2_state_area_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        drop(Volume::open(tmp.path()).unwrap());
        fs::write(tmp.path().join("format"), "agentfs-trees-2\n").unwrap();
        assert!(matches!(Volume::open(tmp.path()), Err(Error::Format)));
    }

    /// A fork's landing in the root's queue is what `pending_from` sees, for
    /// that fork alone, until the root takes it.
    #[test]
    fn pending_from_holds_while_a_forks_landing_is_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = Volume::open(tmp.path()).unwrap();
        let fork = v.fork().unwrap();
        let other = v.fork().unwrap();
        assert!(!v.pending_from(fork).unwrap());
        assert!(!v.pending_from(ROOT).unwrap(), "the root lands nowhere");
        let h = v.write_open(fork, "f", "w", true).unwrap();
        v.write(h, 0, b"x\n", false).unwrap();
        v.close(h).unwrap();
        assert!(v.pending_from(fork).unwrap());
        assert!(
            !v.pending_from(other).unwrap(),
            "another fork's landing is not this fork's"
        );
        while v.take_next(ROOT).unwrap() == Took::Taken {}
        assert!(!v.pending_from(fork).unwrap());
    }

    #[test]
    fn a_compaction_threshold_is_constants() {
        assert_eq!(compact_at(0), 64 << 20);
        assert_eq!(compact_at(1 << 20), 64 << 20);
        assert_eq!(compact_at(100 << 20), 200 << 20);
    }
}
