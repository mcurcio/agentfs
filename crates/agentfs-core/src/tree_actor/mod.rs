//! Tree actor for phase 1 of target git-model §3, §6, §7, §12 and §18.
//!
//! One exclusively locked actor owns a repository, private checkouts and a
//! journal. Every call is fsynced before acknowledgement (deliberately stronger
//! and slower than the target's group commit). A step's pointer changes are one
//! journal record, the commit point: objects are durable before it, and Git's
//! refs and the private checkouts are copies made after it, rebuilt from the
//! journal at every open and after any failed copy. Nothing decides from a copy.
//! See docs/target-architecture/phase-1.md.

mod journal;
mod pages;
mod pointers;
pub mod queue;
mod referee;
mod sync;

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
use crate::paths::RepoPath;
use crate::tree_git::{RefUpdate, TreeEntry, TreeGit};
use crate::tree_reconcile::LineMap;
use journal::{
    CallHead, CompactError, Entry, Frame, Journal, OpenRecord, PointerChange, Record, RunHead,
};
use pointers::Pointers;

const FORMAT: &str = "agentfs-trees-2\n";
const JOURNAL: &str = "calls.wal";
const INITIALIZING: &str = "initializing";
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
    #[error("{name} names lost object {oid}; operator recovery required")]
    MissingObject { name: String, oid: Oid },
    #[error("pointer {name} is not at the value a step expected")]
    PointerMoved { name: String },
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

/// When a journal this long after its last compaction compacts again.
/// Constants, not a setting: restart memory is at most this plus live calls.
fn compact_at(len_after_last: u64) -> u64 {
    (64_u64 << 20).max(len_after_last.saturating_mul(2))
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn branch(tree: &str) -> String {
    format!("refs/heads/{tree}")
}

/// Calls name byte positions, never lines. Append finds EOF at save time.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// The call as a journal frame stores it: a JSON head and raw tail bytes.
    fn to_frame(&self) -> (CallHead, &[u8]) {
        match self {
            Self::Write { offset, bytes } => (
                CallHead::Write {
                    offset: *offset,
                    len: bytes.len() as u64,
                },
                bytes,
            ),
            Self::Append(bytes) => (
                CallHead::Append {
                    len: bytes.len() as u64,
                },
                bytes,
            ),
            Self::Truncate(size) => (CallHead::Truncate(*size), &[]),
        }
    }

    fn from_frame(head: &CallHead, tail: &[u8]) -> Result<Self> {
        let whole = |len: u64| {
            (len == tail.len() as u64)
                .then(|| tail.to_vec())
                .ok_or(Error::Format)
        };
        match head {
            CallHead::Write { offset, len } => Ok(Self::Write {
                offset: *offset,
                bytes: whole(*len)?,
            }),
            CallHead::Append { len } => Ok(Self::Append(whole(*len)?)),
            CallHead::Truncate(size) => whole(0).map(|_| Self::Truncate(*size)),
        }
    }
}

/// The saved state of one tree. A step builds on a copy and installs it after
/// its commit point, so file bytes and line maps sit behind `Arc`.
#[derive(Clone, Default)]
struct Tree {
    names: BTreeMap<String, u64>,
    anonymous: BTreeSet<u64>,
    bytes: BTreeMap<u64, Arc<Vec<u8>>>,
    lines: BTreeMap<u64, Arc<LineMap>>,
    identities: BTreeMap<u64, MergeFileIdentity>,
    namespace: BTreeMap<MergeFileIdentity, NameHistory>,
    fresh_names: BTreeSet<u64>,
}

impl Tree {
    /// Adds an object that has no name and no saved state.
    fn add_anonymous(&mut self, object: u64) {
        self.anonymous.insert(object);
        self.bytes.insert(object, Arc::default());
        self.lines.insert(object, Arc::default());
        self.identities.insert(object, MergeFileIdentity(object));
        self.fresh_names.insert(object);
    }

    fn drop_object(&mut self, object: u64) {
        self.bytes.remove(&object);
        self.lines.remove(&object);
        self.identities.remove(&object);
        self.fresh_names.remove(&object);
    }

    /// Replaces the names and files with `snapshot`'s.
    fn install(&mut self, snapshot: referee::Snapshot) -> Result<()> {
        self.names.clear();
        self.fresh_names.clear();
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
                self.fresh_names.insert(file.object);
            }
            self.names.insert(path, file.object);
            self.bytes.insert(file.object, Arc::new(file.lines.bytes()));
            self.lines.insert(file.object, Arc::new(file.lines));
            self.identities.insert(file.object, id);
        }
        self.namespace = snapshot.names;
        Ok(())
    }
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
    files: BTreeMap<MergeFileIdentity, u64>,
}

enum QueueAction {
    Landing(Oid),
    Sync(Oid, SyncPlan),
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
}

/// A live open and its calls, as the journal leaves them.
#[derive(Debug, PartialEq, Eq)]
struct FoldedOpen {
    open: OpenRecord,
    calls: Vec<(u64, Call)>,
}

/// What a journal's frames add up to (git-model §12).
#[derive(Debug, PartialEq, Eq)]
struct Folded {
    pointers: Pointers,
    upstreams: BTreeMap<String, String>,
    next_seq: u64,
    opens: BTreeMap<u64, FoldedOpen>,
    /// A file's page writes while a write-open of it lives.
    page_writes: BTreeMap<(String, u64), Vec<(u64, pages::Run)>>,
    /// The last step that closed a write-open of a file, while one lives.
    saved_through: BTreeMap<(String, u64), u64>,
}

impl Folded {
    fn add_open(&mut self, open: OpenRecord) {
        self.opens.insert(
            open.handle,
            FoldedOpen {
                open,
                calls: Vec::new(),
            },
        );
    }
}

fn parse_oid(hex: &str) -> Result<Oid> {
    Oid::from_str(hex).map_err(|_| Error::Format)
}

fn parse_change(change: &PointerChange) -> Result<RefUpdate> {
    Ok(RefUpdate {
        refname: change.name.clone(),
        new: change.new.as_deref().map(parse_oid).transpose()?,
        expected_old: change.old.as_deref().map(parse_oid).transpose()?,
    })
}

fn runs_from_frame(heads: &[RunHead], tail: &[u8]) -> Result<Vec<pages::Run>> {
    let mut rest = tail;
    let mut runs = Vec::with_capacity(heads.len());
    for head in heads {
        let len = usize::try_from(head.len).map_err(|_| Error::Format)?;
        if rest.len() < len {
            return Err(Error::Format);
        }
        let (bytes, after) = rest.split_at(len);
        runs.push(pages::Run {
            offset: head.offset,
            bytes: bytes.to_vec(),
        });
        rest = after;
    }
    if rest.is_empty() {
        Ok(runs)
    } else {
        Err(Error::Format)
    }
}

/// Adds a journal's frames up. Pure. Frames that disagree with each other
/// cannot come from a good journal: [`Error::Format`], or
/// [`Error::PointerMoved`] for a pointer.
fn fold(frames: &[Frame]) -> Result<Folded> {
    let mut frames = frames.iter();
    let Some((
        Record {
            entry:
                Entry::Checkpoint {
                    next_seq,
                    pointers,
                    upstreams,
                    opens,
                    page_saved,
                },
            ..
        },
        _,
    )) = frames.next()
    else {
        return Err(Error::Format);
    };
    let mut folded = Folded {
        pointers: Pointers::new(
            pointers
                .iter()
                .map(|(name, hex)| Ok((name.clone(), parse_oid(hex)?)))
                .collect::<Result<_>>()?,
        ),
        upstreams: upstreams.clone(),
        next_seq: *next_seq,
        opens: BTreeMap::new(),
        page_writes: BTreeMap::new(),
        saved_through: page_saved
            .iter()
            .map(|(tree, object, seq)| ((tree.clone(), *object), *seq))
            .collect(),
    };
    for open in opens {
        folded.add_open(open.clone());
    }
    for (record, tail) in frames {
        let seq = record.seq;
        folded.next_seq = folded.next_seq.max(seq.saturating_add(1));
        match &record.entry {
            Entry::Checkpoint { .. } => return Err(Error::Format),
            Entry::Open {
                tree,
                writer,
                object,
                ..
            } => folded.add_open(OpenRecord {
                handle: seq,
                tree: tree.clone(),
                writer: writer.clone(),
                object: *object,
                anonymous: false,
            }),
            Entry::AnonymousOpen { tree, writer } => folded.add_open(OpenRecord {
                handle: seq,
                tree: tree.clone(),
                writer: writer.clone(),
                object: seq,
                anonymous: true,
            }),
            Entry::Call { handle, call } => folded
                .opens
                .get_mut(handle)
                .ok_or(Error::Format)?
                .calls
                .push((seq, Call::from_frame(call, tail)?)),
            Entry::PageWrite { tree, object, runs } => folded
                .page_writes
                .entry((tree.clone(), *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Step {
                closes,
                opened,
                linked,
                fork,
                pointers,
                ..
            } => {
                let changes = pointers
                    .iter()
                    .map(parse_change)
                    .collect::<Result<Vec<_>>>()?;
                folded.pointers.check(&changes)?;
                folded.pointers.apply(&changes);
                if let Some((name, parent)) = fork {
                    folded.upstreams.insert(name.clone(), parent.clone());
                }
                if let Some(open) = opened {
                    folded.add_open(open.clone());
                }
                if let Some(object) = linked {
                    folded
                        .opens
                        .values_mut()
                        .filter(|o| o.open.object == *object)
                        .for_each(|o| o.open.anonymous = false);
                }
                if let Some(handle) = closes {
                    let open = folded.opens.remove(handle).ok_or(Error::Format)?.open;
                    folded
                        .saved_through
                        .insert((open.tree.clone(), open.object), seq);
                }
            }
        }
    }
    // Page state lives from a file's write-open to the close of its last one.
    let live: BTreeSet<(String, u64)> = folded
        .opens
        .values()
        .map(|o| (o.open.tree.clone(), o.open.object))
        .collect();
    folded.page_writes.retain(|key, _| live.contains(key));
    folded.saved_through.retain(|key, _| live.contains(key));
    Ok(folded)
}

/// A step built on a copy of its tree and not yet on record.
struct Draft {
    by: String,
    tree: String,
    built: Tree,
    changes: Vec<RefUpdate>,
    closes: Option<u64>,
    opened: Option<OpenRecord>,
    linked: Option<u64>,
    fork: Option<(String, String)>,
    saved: Saved,
    enqueued: u64,
    taken: u64,
}

impl Draft {
    fn new(by: &str, tree: &str, built: Tree) -> Self {
        Self {
            by: by.to_owned(),
            tree: tree.to_owned(),
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
            opened: self.opened.clone(),
            linked: self.linked,
            fork: self.fork.clone(),
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
    upstreams: BTreeMap<String, String>,
    trees: BTreeMap<String, Tree>,
    handles: BTreeMap<u64, Handle>,
    /// Closes that could not save, in arrival order. Their handles count as
    /// open until a later step in their tree saves them, or a restart does.
    pending_closes: Vec<u64>,
    saved_through: BTreeMap<(String, u64), u64>,
    next: u64,
    stopped: bool,
    /// A copy failed: the next step rebuilds refs whole and redoes checkouts.
    copies_stale: bool,
    stale_checkouts: BTreeSet<String>,
    compact_threshold: u64,
    metrics: Metrics,
    waiting_since: BTreeMap<String, Instant>,
    #[allow(dead_code)] // agentfs-eh5.3 reads it.
    pages: BTreeMap<(String, u64), pages::ObjectPages>,
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
        "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: main\nAgentfs-Event: 0\n",
    )?;
    Journal::create(
        &root.join(JOURNAL),
        &Record {
            seq: 0,
            at_ns: now_ns(),
            entry: Entry::Checkpoint {
                next_seq: 1,
                pointers: [(branch("main"), head.to_string())].into_iter().collect(),
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
    /// [`Error::Format`] for a state area that is not this format,
    /// [`Error::CorruptJournal`] and [`Error::MissingObject`] when the journal
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
        repo.write_config(&folded.upstreams)?;
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
            saved_through: folded.saved_through,
            next: folded.next_seq,
            stopped: false,
            copies_stale: false,
            stale_checkouts: BTreeSet::new(),
            compact_threshold: u64::MAX,
            metrics: Metrics::default(),
            waiting_since: BTreeMap::new(),
            pages: BTreeMap::new(),
            _lock: lock,
        };
        volume.build_trees()?;
        for (handle, FoldedOpen { open, calls }) in folded.opens {
            let tree = volume.trees.get_mut(&open.tree).ok_or(Error::Format)?;
            if open.anonymous {
                tree.add_anonymous(open.object);
            }
            volume.handles.insert(
                handle,
                Handle {
                    tree: open.tree.clone(),
                    writer: open.writer,
                    object: open.object,
                    // Nothing reads it: this handle closes before serving.
                    bytes: Vec::new(),
                    calls,
                },
            );
            volume.pages_opened(&open.tree, open.object);
        }
        let mut page_writes = folded.page_writes;
        let page_files: BTreeSet<(String, u64)> = page_writes
            .keys()
            .chain(volume.saved_through.keys())
            .cloned()
            .collect();
        for key in page_files {
            let writes = page_writes.remove(&key).unwrap_or_default();
            let saved_through = volume.saved_through.get(&key).copied().unwrap_or(0);
            volume.pages_restore(&key.0, key.1, writes, saved_through);
        }
        let abandoned: Vec<_> = volume.handles.keys().copied().collect();
        for handle in abandoned {
            volume.close(handle)?;
        }
        for name in volume.tree_names() {
            volume.checkout(&name)?;
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

    fn build_trees(&mut self) -> Result<()> {
        for (reference, head) in self.pointers.under("refs/heads/") {
            let name = reference
                .strip_prefix("refs/heads/")
                .ok_or(Error::Format)?
                .to_owned();
            let mut snapshot = self.snapshot_at(head)?;
            self.restore_object_table(&name, &mut snapshot)?;
            let mut tree = Tree::default();
            tree.install(snapshot)?;
            self.trees.insert(name, tree);
        }
        if self.trees.contains_key("main") {
            Ok(())
        } else {
            Err(Error::Format)
        }
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
        self.tree(tree)?;
        self.pointers.get(&branch(tree)).ok_or(Error::NotFound)
    }

    /// Fork IDs are numeric and never reused. Forking a fork is not exposed yet.
    pub fn fork(&mut self) -> Result<String> {
        let (seq, _) = self.step("main", |v, seq| {
            let name = format!("fork/{seq}");
            let at = v.head("main")?;
            let mut tree = v.tree("main")?.clone();
            for object in std::mem::take(&mut tree.anonymous) {
                tree.drop_object(object);
            }
            let mut draft = Draft::new("agentfs", &name, tree);
            draft.changes.push(RefUpdate {
                refname: branch(&name),
                new: Some(at),
                expected_old: None,
            });
            draft.fork = Some((name, "main".to_owned()));
            Ok(draft)
        })?;
        Ok(format!("fork/{seq}"))
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
        let Some(object) = self.tree(tree)?.names.get(path).copied() else {
            if !create {
                return Err(Error::NotFound);
            }
            return self
                .step(tree, |v, seq| v.build_create(tree, path, writer, seq))
                .map(|(seq, _)| seq);
        };
        let handle = self.journal_record(
            Entry::Open {
                tree: tree.to_owned(),
                path: path.to_owned(),
                writer: writer.to_owned(),
                object,
            },
            &[],
        )?;
        let bytes = self.tree(tree)?.bytes[&object].to_vec();
        self.handles.insert(
            handle,
            Handle {
                tree: tree.to_owned(),
                writer: writer.to_owned(),
                object,
                bytes,
                calls: Vec::new(),
            },
        );
        self.pages_opened(tree, object);
        Ok(handle)
    }

    /// Create an unnamed open object. Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: &str, writer: &str) -> Result<u64> {
        writer_label(writer)?;
        self.tree(tree)?;
        let handle = self.journal_record(
            Entry::AnonymousOpen {
                tree: tree.to_owned(),
                writer: writer.to_owned(),
            },
            &[],
        )?;
        self.trees
            .get_mut(tree)
            .ok_or(Error::NotFound)?
            .add_anonymous(handle);
        self.handles.insert(
            handle,
            Handle {
                tree: tree.to_owned(),
                writer: writer.to_owned(),
                object: handle,
                bytes: Vec::new(),
                calls: Vec::new(),
            },
        );
        self.pages_opened(tree, handle);
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
        self.step(tree, |v, seq| {
            let mut t = v.tree(tree)?.clone();
            t.anonymous.remove(&object);
            t.namespace.insert(
                t.identities[&object],
                NameHistory::created(seq, path.to_owned()),
            );
            t.names.insert(path.to_owned(), object);
            let mut draft = v.build_save(tree, t, writer, "Create", seq, None)?;
            draft.linked = Some(object);
            Ok(draft)
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
        let mut replay = self.tree(&h.tree)?.bytes[&h.object].to_vec();
        for c in h.calls.iter().map(|(_, c)| c).chain(std::iter::once(&call)) {
            c.apply(&mut replay)?;
        }
        let (head, tail) = call.to_frame();
        let seq = self.journal_record(Entry::Call { handle, call: head }, tail)?;
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.bytes = bytes;
        h.calls.push((seq, call.clone()));
        let (tree, object) = (h.tree.clone(), h.object);
        self.pages_note_call(&tree, object, &call);
        Ok(())
    }

    /// The journal call for a page write: only `pages` calls it (ledger R1.4).
    /// Returns its journal number.
    #[allow(dead_code)] // agentfs-eh5.3 calls it.
    fn record_page_write(&mut self, tree: &str, object: u64, runs: &[pages::Run]) -> Result<u64> {
        let heads = runs
            .iter()
            .map(|run| RunHead {
                offset: run.offset,
                len: run.bytes.len() as u64,
            })
            .collect();
        let tail = runs.iter().fold(Vec::new(), |mut tail, run| {
            tail.extend_from_slice(&run.bytes);
            tail
        });
        self.journal_record(
            Entry::PageWrite {
                tree: tree.to_owned(),
                object,
                runs: heads,
            },
            &tail,
        )
    }

    /// fsync/FLUSH records durability, not publication. Every call is already durable.
    pub fn sync(&self) -> Result<()> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.journal.sync()
    }

    /// RELEASE publishes calls against this tree's head, never the stale open
    /// base. A failed save changes nothing and answers its errno: the close
    /// stays pending, its handle still counting as open, and saves first at the
    /// next step in its tree and at restart.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let tree = self
            .handles
            .get(&handle)
            .ok_or(Error::NotFound)?
            .tree
            .clone();
        if !self.pending_closes.contains(&handle) {
            self.pending_closes.push(handle);
        }
        self.run_pending_closes(&tree, Some(handle))
    }

    pub fn rename(&mut self, tree: &str, from: &str, to: &str, writer: &str) -> Result<()> {
        name(from)?;
        name(to)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(from) {
            return Err(Error::NotFound);
        }
        if from != to {
            self.step(tree, |v, seq| v.build_rename(tree, from, to, writer, seq))?;
        }
        Ok(())
    }

    pub fn unlink(&mut self, tree: &str, path: &str, writer: &str) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(path) {
            return Err(Error::NotFound);
        }
        self.step(tree, |v, seq| {
            let mut t = v.tree(tree)?.clone();
            let object = t.names.remove(path).ok_or(Error::NotFound)?;
            t.namespace
                .get_mut(&t.identities[&object])
                .ok_or(Error::Format)?
                .advance(seq, None, None);
            v.build_save(tree, t, writer, "Unlink", seq, None)
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
                        lines: (*tree.lines[object]).clone(),
                    },
                )
            })
            .collect();
        referee::Snapshot {
            files,
            names: tree.namespace.clone(),
        }
    }

    fn snapshot_at(&self, head: Oid) -> Result<referee::Snapshot> {
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
        tree: &str,
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

    fn restore_object_table(&self, tree: &str, snapshot: &mut referee::Snapshot) -> Result<()> {
        if let Some(oid) = self.pointers.get(&format!("refs/agentfs/objects/{tree}")) {
            let table: ObjectTable = serde_json::from_slice(&self.repo.read_blob(oid)?)?;
            for (id, file) in &mut snapshot.files {
                file.object = *table.files.get(id).ok_or(Error::Format)?;
            }
        }
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

    fn build_landing(&self, tree: &str, reference: &str, incoming: Oid, seq: u64) -> Result<Draft> {
        let current = self.head(tree)?;
        let t = self.tree(tree)?;
        let incoming_commit = self.repo.read_commit(incoming)?;
        let base = *incoming_commit.parents.first().ok_or(Error::Format)?;
        let decision = referee::judge(
            &Self::snapshot_of(t),
            &self.snapshot_at(base)?,
            &self.snapshot_at(incoming)?,
            &self.prior_decisions(current)?,
        )?;
        let content = self.snapshot_tree(&decision.snapshot)?;
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
        let enqueued = self.enqueue_moves(tree, head, seq, &mut changes)?;
        let mut built = t.clone();
        built.install(decision.snapshot)?;
        let mut draft = Draft::new("agentfs", tree, built);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.taken = 1;
        Ok(draft)
    }

    fn take_landing(&mut self, tree: &str, reference: &str, incoming: Oid) -> Result<()> {
        let started = Instant::now();
        self.step(tree, |v, seq| {
            v.build_landing(tree, reference, incoming, seq)
        })?;
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
        let fork_head = self.head(tree)?;
        let fork_snapshot = Self::snapshot_of(fork);
        let mut todo = Vec::new();
        let mut cursor = self.head("main")?;
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
            let (lines, names) = self.undecided(&fork_snapshot, fork_head, oid)?;
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
        for (_, oid) in self.pointers.under("refs/queue/main/") {
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

    fn build_sync(
        &self,
        tree: &str,
        reference: &str,
        value: Oid,
        plan: SyncPlan,
        seq: u64,
    ) -> Result<Draft> {
        let current = self.head(tree)?;
        let mut built = self.tree(tree)?.clone();
        let mut changes = Vec::new();
        if let Some(target) = plan.target {
            let parent = self.snapshot_at(target)?;
            let head = if Self::changed_keys(&parent, &plan.snapshot).is_empty()
                && !self.pending_from(tree)?
                && self.repo.is_ancestor(current, target)?
            {
                target
            } else {
                let content = self.snapshot_tree(&plan.snapshot)?;
                let message = format!(
                    "Sync\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Sync\nAgentfs-Tree: {tree}\nAgentfs-Event: {seq}\nAgentfs-Format: envelope-1\n"
                );
                self.repo
                    .write_commit(content, &[current, target], &message)?
            };
            changes.push(RefUpdate {
                refname: branch(tree),
                new: Some(head),
                expected_old: Some(current),
            });
            self.object_table_update(tree, &plan.snapshot, &mut changes)?;
            built.install(plan.snapshot)?;
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

    fn take_sync(&mut self, tree: &str, reference: &str, value: Oid, plan: SyncPlan) -> Result<()> {
        let started = Instant::now();
        self.step(tree, |v, seq| {
            v.build_sync(tree, reference, value, plan, seq)
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
        let mut trees = self.tree_names();
        trees.sort_by_key(|name| name != "main");
        for tree in trees {
            if self
                .pointers
                .under(&format!("refs/queue/{tree}/"))
                .is_empty()
            {
                continue;
            }
            // A take is a step: pending closes save first, and its plan reads
            // the tree they leave.
            self.run_pending_closes(&tree, None)?;
            let entries = self.pointers.under(&format!("refs/queue/{tree}/"));
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

    /// Appends one record that moves no pointer, or the step `run_step` built.
    /// Only a file that can no longer be trusted stops the actor.
    fn journal_record(&mut self, entry: Entry, tail: &[u8]) -> Result<u64> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let seq = self.next;
        let record = Record {
            seq,
            at_ns: now_ns(),
            entry,
        };
        if let Err(e) = self.journal.append(&record, tail) {
            self.stopped = self.journal.unknown();
            return Err(e);
        }
        self.next += 1;
        Ok(seq)
    }

    /// Saves this tree's pending closes, then runs the step `build` makes.
    fn step<F>(&mut self, tree: &str, build: F) -> Result<(u64, Saved)>
    where
        F: FnOnce(&Self, u64) -> Result<Draft>,
    {
        self.run_pending_closes(tree, None)?;
        self.run_step(build)
    }

    /// Saves this tree's pending closes in arrival order, answering for `wanted`.
    fn run_pending_closes(&mut self, tree: &str, wanted: Option<u64>) -> Result<Saved> {
        let due: Vec<u64> = self
            .pending_closes
            .iter()
            .copied()
            .filter(|handle| self.handles.get(handle).is_some_and(|h| h.tree == tree))
            .collect();
        let mut outcome = Saved::Unchanged;
        for handle in due {
            let (_, saved) = self.run_step(|v, seq| v.build_close(handle, seq))?;
            if wanted == Some(handle) {
                outcome = saved;
            }
        }
        Ok(outcome)
    }

    /// One step, in order. A failure up to and including the journal record
    /// changes nothing and returns its errno. After the record, the commit
    /// point, the step has happened: a failed copy is logged and the call
    /// succeeds (AGENTS principle 5).
    fn run_step<F>(&mut self, build: F) -> Result<(u64, Saved)>
    where
        F: FnOnce(&Self, u64) -> Result<Draft>,
    {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let started = Instant::now();
        let seq = self.next;
        let recorded = (|| -> Result<Draft> {
            crate::failpoints::hit("tree.before_objects")?;
            let draft = build(self, seq)?;
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
        let tree = draft.tree.clone();
        self.install(draft, seq);
        if matches!(saved, Saved::Commit(_)) {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.metrics.saves = self.metrics.saves.saturating_add(1);
            self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
            self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
        }
        if let Err(e) = self.copy_out(&tree, is_fork, &changes) {
            self.copies_stale = true;
            eprintln!("tree CopyStale: {e}");
        }
        self.compact_if_due();
        Ok((seq, saved))
    }

    /// Stage 6: the step is on record, so memory follows. Cannot fail.
    fn install(&mut self, draft: Draft, seq: u64) {
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
                    tree: open.tree.clone(),
                    writer: open.writer,
                    object: open.object,
                    bytes,
                    calls: Vec::new(),
                },
            );
            self.pages_opened(&open.tree, open.object);
        }
        self.trees.insert(tree, built);
        self.pointers.apply(&changes);
        self.metrics.queued = self
            .metrics
            .queued
            .saturating_sub(taken)
            .saturating_add(enqueued);
        if let Some(closed) = closed {
            let key = (closed.tree, closed.object);
            if self
                .handles
                .values()
                .any(|h| h.tree == key.0 && h.object == key.1)
            {
                self.saved_through.insert(key.clone(), seq);
            } else {
                self.saved_through.remove(&key);
            }
            self.pages_saved(&key.0, key.1, seq);
        }
    }

    /// Stage 7: Git's refs and the checkouts, both copies of what is on record.
    fn copy_out(&mut self, tree: &str, is_fork: bool, changes: &[RefUpdate]) -> Result<()> {
        // Named first, so that a failure anywhere below redoes this checkout.
        if changes.iter().any(|change| change.refname == branch(tree)) {
            self.stale_checkouts.insert(tree.to_owned());
        }
        crate::failpoints::hit("tree.after_commit_point")?;
        if self.copies_stale {
            self.repo.write_config(&self.upstreams)?;
            self.repo.rebuild_refs(&self.pointers.creates())?;
        } else if !changes.is_empty() {
            if is_fork {
                self.repo.write_config(&self.upstreams)?;
            }
            self.repo.ref_transaction(changes)?;
        }
        crate::failpoints::hit("tree.after_ref_copy")?;
        if !self.stale_checkouts.is_empty() {
            crate::failpoints::hit("tree.before_checkout")?;
            for name in &self.stale_checkouts {
                self.checkout(name)?;
            }
            self.stale_checkouts.clear();
            crate::failpoints::hit("tree.after_checkout")?;
        }
        self.copies_stale = false;
        Ok(())
    }

    /// A fuzzy checkpoint (ARIES): the pointers, the live opens, and unchanged
    /// the calls and page writes of files with a live write-open. A file held
    /// open pins only its own calls, never other files' saves.
    fn compact(&mut self) -> std::result::Result<(), CompactError> {
        let opens = self
            .handles
            .iter()
            .map(|(handle, h)| OpenRecord {
                handle: *handle,
                tree: h.tree.clone(),
                writer: h.writer.clone(),
                object: h.object,
                anonymous: self
                    .trees
                    .get(&h.tree)
                    .is_some_and(|t| t.anonymous.contains(&h.object)),
            })
            .collect();
        let checkpoint = Record {
            seq: 0,
            at_ns: now_ns(),
            entry: Entry::Checkpoint {
                next_seq: self.next,
                pointers: self.pointers.to_hex(),
                upstreams: self.upstreams.clone(),
                opens,
                page_saved: self
                    .saved_through
                    .iter()
                    .map(|((tree, object), seq)| (tree.clone(), *object, *seq))
                    .collect(),
            },
        };
        let handles = &self.handles;
        let retained = self
            .journal
            .retained(|record| match &record.entry {
                Entry::Call { handle, .. } => handles.contains_key(handle),
                Entry::PageWrite { tree, object, .. } => handles
                    .values()
                    .any(|h| h.tree == *tree && h.object == *object),
                _ => false,
            })
            .map_err(CompactError::Before)?;
        self.journal.compact(&checkpoint, &retained)
    }

    fn compact_if_due(&mut self) {
        if self.journal.len() < self.compact_threshold {
            return;
        }
        match self.compact() {
            Ok(()) => self.compact_threshold = compact_at(self.journal.len()),
            // Untouched: the next step retries.
            Err(e @ CompactError::Before(_)) => eprintln!("tree CompactionFailed: {e}"),
            // The open file may not be the named one.
            Err(e @ CompactError::After(_)) => {
                eprintln!("tree CompactionFailed: {e}");
                self.stopped = true;
            }
        }
    }

    fn build_create(&self, tree: &str, path: &str, writer: &str, seq: u64) -> Result<Draft> {
        let mut t = self.tree(tree)?.clone();
        t.names.insert(path.to_owned(), seq);
        t.bytes.insert(seq, Arc::default());
        t.lines.insert(seq, Arc::default());
        t.identities.insert(seq, MergeFileIdentity(seq));
        t.namespace.insert(
            MergeFileIdentity(seq),
            NameHistory::created(seq, path.to_owned()),
        );
        t.fresh_names.insert(seq);
        let mut draft = self.build_save(tree, t, writer, "Create", seq, None)?;
        draft.opened = Some(OpenRecord {
            handle: seq,
            tree: tree.to_owned(),
            writer: writer.to_owned(),
            object: seq,
            anonymous: false,
        });
        Ok(draft)
    }

    fn build_close(&self, handle: u64, seq: u64) -> Result<Draft> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let calls = self.pages_calls_for_close(h);
        let mut t = self.tree(&h.tree)?.clone();
        let mut draft = if t.anonymous.remove(&h.object) {
            t.drop_object(h.object);
            Draft::new(&h.writer, &h.tree, t)
        } else if !t.names.values().any(|o| *o == h.object) {
            let mut draft = Draft::new(&h.writer, &h.tree, t);
            draft.saved = Saved::OrphanDropped;
            draft
        } else if calls.is_empty() {
            Draft::new(&h.writer, &h.tree, t)
        } else {
            let mut bytes = t.bytes[&h.object].to_vec();
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
                t.lines[&h.object].append(&suffix, &mut next_id)?
            } else {
                t.lines[&h.object].align(&bytes, &mut next_id)?
            };
            t.lines.insert(h.object, Arc::new(lines));
            t.bytes.insert(h.object, Arc::new(bytes));
            self.build_save(&h.tree, t, &h.writer, "Direct", seq, Some(handle))?
        };
        draft.closes = Some(handle);
        Ok(draft)
    }

    fn build_rename(
        &self,
        tree: &str,
        from: &str,
        to: &str,
        writer: &str,
        seq: u64,
    ) -> Result<Draft> {
        let mut t = self.tree(tree)?.clone();
        let object = t.names.remove(from).ok_or(Error::NotFound)?;
        if let Some(replaced) = t.names.get(to).copied()
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
            t.lines.insert(object, Arc::new(lines));
            t.identities.insert(object, t.identities[&replaced]);
            if !t.fresh_names.contains(&replaced) {
                t.fresh_names.remove(&object);
            }
        } else {
            let source_id = t.identities[&object];
            if let Some(replaced) = t.names.get(to).copied() {
                let replaced_id = t.identities[&replaced];
                t.namespace
                    .get_mut(&replaced_id)
                    .ok_or(Error::Format)?
                    .advance(seq, None, Some(source_id));
            }
            t.namespace
                .get_mut(&source_id)
                .ok_or(Error::Format)?
                .advance(seq, Some(to.to_owned()), None);
            t.fresh_names.remove(&object);
        }
        t.names.insert(to.to_owned(), object);
        self.build_save(tree, t, writer, "Rename", seq, None)
    }

    /// Writes `t`'s commit and the pointer changes that publish it.
    fn build_save(
        &self,
        name: &str,
        t: Tree,
        writer: &str,
        cause: &str,
        seq: u64,
        open: Option<u64>,
    ) -> Result<Draft> {
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
        let tree = self.envelope(files, blob, namespace)?;
        let mut message = format!(
            "{cause} by {writer}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: Direct\nAgentfs-Tree: {name}\nAgentfs-Event: {seq}\nAgentfs-Format: envelope-1\n"
        );
        if let Some(open) = open {
            message.push_str(&format!("Agentfs-Open: {open}\n"));
        }
        let current = self.head(name)?;
        let head = self.repo.write_commit(tree, &[current], &message)?;
        let mut changes = vec![RefUpdate {
            refname: branch(name),
            new: Some(head),
            expected_old: Some(current),
        }];
        self.object_table_update(name, &Self::snapshot_of(&t), &mut changes)?;
        let enqueued = self.enqueue_moves(name, head, seq, &mut changes)?;
        let mut draft = Draft::new(writer, name, t);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.saved = Saved::Commit(head);
        Ok(draft)
    }

    // One entry per destination, numbered by the step that causes it: order
    // across restart, independent of commit dates.
    fn enqueue_moves(
        &self,
        tree: &str,
        head: Oid,
        seq: u64,
        changes: &mut Vec<RefUpdate>,
    ) -> Result<u64> {
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
        for (destination, oid) in &destinations {
            changes.push(RefUpdate {
                refname: format!("refs/queue/{destination}/{seq:020}"),
                new: Some(*oid),
                expected_old: None,
            });
        }
        Ok(destinations.len() as u64)
    }

    fn checkout(&self, name: &str) -> Result<()> {
        let dir = self.root.join("trees").join(name);
        fs::create_dir_all(&dir)?;
        // Rebuild from the branch, never from intermediate in-memory state.
        let commit = self.repo.read_commit(self.head(name)?)?;
        // The trailer tells the Root commit even if a workload used our names.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let held = v.write_open("main", "held", "w", true).unwrap();
        v.write(held, 0, b"held-1", false).unwrap();
        let shared = v.write_open("main", "shared", "w", true).unwrap();
        v.write(shared, 0, b"shared", false).unwrap();
        let object = v.open_object(shared).unwrap();
        let other = v.write_open("main", "shared", "w", false).unwrap();
        v.record_page_write(
            "main",
            object,
            &[pages::Run {
                offset: 1,
                bytes: b"page".to_vec(),
            }],
        )
        .unwrap();
        v.close(other).unwrap();
        let anonymous = v.anonymous_open("main", "w").unwrap();
        v.write(anonymous, 0, b"unnamed", false).unwrap();
        let closed = v.write_open("main", "closed", "w", true).unwrap();
        v.write(closed, 0, b"closed", false).unwrap();
        let closed_object = v.open_object(closed).unwrap();
        v.record_page_write(
            "main",
            closed_object,
            &[pages::Run {
                offset: 0,
                bytes: b"gone".to_vec(),
            }],
        )
        .unwrap();
        v.close(closed).unwrap();
        v.write_open(&fork, "sandbox", "w", true).unwrap();

        let mut whole = frames_of(tmp.path());
        let folded = fold(&whole).unwrap();
        assert_eq!(folded.opens.len(), 4);
        assert_eq!(folded.opens[&held].calls.len(), 1);
        assert!(folded.opens[&anonymous].open.anonymous);
        assert_eq!(folded.upstreams[&fork], "main");
        assert_eq!(
            folded.page_writes[&("main".to_owned(), object)][0].1.bytes,
            b"page"
        );
        assert!(
            folded
                .saved_through
                .contains_key(&("main".to_owned(), object))
        );
        assert!(
            !folded
                .page_writes
                .contains_key(&("main".to_owned(), closed_object))
        );
        assert_eq!(folded.next_seq, v.next);
        assert_eq!(folded.pointers, v.pointers);

        v.compact().unwrap();
        let compacted = frames_of(tmp.path());
        assert!(compacted.len() < whole.len());
        assert_eq!(fold(&compacted).unwrap(), folded);

        // What follows lands on either start and adds up the same.
        v.write(held, 0, b"held-2", false).unwrap();
        v.write(shared, 0, b"!", true).unwrap();
        let again = v.write_open("main", "again", "w", true).unwrap();
        v.close(shared).unwrap();
        v.first_link("main", anonymous, "named", "w").unwrap();
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

    #[test]
    fn a_compaction_threshold_is_constants() {
        assert_eq!(compact_at(0), 64 << 20);
        assert_eq!(compact_at(1 << 20), 64 << 20);
        assert_eq!(compact_at(100 << 20), 200 << 20);
    }
}
