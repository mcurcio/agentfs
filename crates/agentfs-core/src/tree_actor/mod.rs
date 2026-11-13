//! The tree actor (git-model §3, §6, §7, §12 and §18; `docs/trees.md`).
//!
//! One exclusively locked actor owns a repository, private checkouts and a
//! journal. Every call is fsynced before acknowledgement (deliberately stronger
//! and slower than the target's group commit). A step's pointer changes are one
//! journal record, the commit point: objects are durable before it, and Git's
//! refs and the private checkouts are copies made after it, rebuilt from the
//! journal at every open and after any failed copy. Nothing decides from a copy.

mod history;
mod journal;
mod pointers;
mod queue;
mod recording;
mod referee;
pub mod stages;
mod tree;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use git2::Oid;
use serde::Serialize;

use crate::file_lock::{FileLock, Mode};
use crate::tree_git::{RefUpdate, TreeGit};
pub use agentfs_contracts::{NotATree, TreeId};
use history::message::{self, Message};
use history::upstreams::Upstreams;
use history::{SyncStep, check_objects, refs, remove_stale_locks};
use journal::{
    CompactError, Entry, FoldedOpen, Journal, LiveOpen, PointerChange, Record, compact_at, fold,
};
use pointers::Pointers;
use recording::{Handle, pages};
use stages::{Stages, Timer, Which};
use tree::{EventNumber, Tree};

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

impl From<NotATree> for Error {
    /// Stored text that names no tree (a journal record, a ref, a trailer) is damaged state.
    fn from(_: NotATree) -> Self {
        Self::Format
    }
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

enum QueueAction {
    Landing(Oid),
    Sync(Oid, queue::Cut<SyncStep>),
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
    /// A fork this step makes, by number, and its upstream.
    fork: Option<(u64, TreeId)>,
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
                .map(|(fork, upstream)| (TreeId::Fork(fork).to_string(), upstream.to_string())),
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
    upstreams: Upstreams,
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
    repo.write_config(&refs::head(), &BTreeMap::new())?;
    let tree = repo.write_tree(&[])?;
    let head = repo.write_commit(tree, &[], &Message::Root.render()?)?;
    Journal::create(
        &root.join(JOURNAL),
        &Record {
            seq: 0,
            at_ns: now_ns(),
            entry: Entry::Checkpoint {
                next_seq: 1,
                pointers: [(refs::branch(TreeId::Root), head.to_string())]
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
        repo.write_config(&refs::head(), &folded.upstreams.merges())?;
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
        volume.metrics.queued = volume.pointers.under(refs::QUEUES).len() as u64;
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
        for (id, head) in refs::branches(&self.pointers)? {
            let mut snapshot = self.snapshot_at(head)?;
            self.restore_object_table(id, &mut snapshot)?;
            let mut tree = Tree::default();
            tree.install(snapshot)?;
            self.trees.insert(id, tree);
        }
        if !self.trees.contains_key(&TreeId::Root) {
            return Err(Error::Format);
        }
        self.upstreams
            .check_recovered(|fork| self.trees.contains_key(&fork))
    }

    /// Every tree: the root first, then the forks by number.
    pub fn trees(&self) -> Vec<TreeId> {
        self.trees.keys().copied().collect()
    }

    fn upstream_of(&self, tree: TreeId) -> Option<TreeId> {
        self.upstreams.upstream_of(tree)
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
        self.pointers
            .get(&refs::branch(tree))
            .ok_or(Error::NotFound)
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
                refname: refs::branch(fork),
                new: Some(at),
                expected_old: None,
            });
            draft.fork = Some((event.get(), upstream));
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

    fn take_sync(
        &mut self,
        tree: TreeId,
        reference: &str,
        value: Oid,
        cut: queue::Cut<SyncStep>,
    ) -> Result<()> {
        let started = Instant::now();
        self.pages_take(tree, |v, event| {
            v.build_sync(tree, reference, value, cut, event)
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
        if self.pointers.under(&refs::queue(tree)).is_empty() {
            return Ok(Took::Empty);
        }
        // A take is a step: pending closes save first, and its plan reads
        // the tree they leave.
        self.run_pending_closes(tree, None)?;
        let entries = self.pointers.under(&refs::queue(tree));
        let open = match self.trees.get(&tree) {
            Some(t) => queue::held(
                t,
                self.handles
                    .values()
                    .filter(|h| h.tree == tree)
                    .map(|h| h.object),
            ),
            None => BTreeSet::new(),
        };
        let mut plans = Vec::new();
        let mut actions = Vec::new();
        let planning = Timer::start(Which::QueuePlan);
        for (_, oid) in &entries {
            if tree.is_root() {
                let commit = self.repo.read_commit(*oid)?;
                let base = *commit.parents.first().ok_or(Error::Format)?;
                let source = message::tree_of(&commit.message)?.ok_or(Error::Format)?;
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
                    files: sync.keys().clone(),
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
                QueueAction::Sync(oid, cut) => self.take_sync(tree, reference, oid, cut)?,
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
        if let Some((fork, upstream)) = fork {
            self.upstreams.add(fork, upstream);
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
        if changes
            .iter()
            .any(|change| change.refname == refs::branch(tree))
        {
            self.stale_checkouts.insert(tree);
        }
        crate::failpoints::hit("tree.after_commit_point")?;
        {
            let _timer = Timer::start(Which::RefCopy);
            if self.copies_stale {
                self.repo
                    .write_config(&refs::head(), &self.upstreams.merges())?;
                self.repo.rebuild_refs(&self.pointers.creates())?;
            } else if !changes.is_empty() {
                if is_fork {
                    self.repo
                        .write_config(&refs::head(), &self.upstreams.merges())?;
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
        assert_eq!(folded.upstreams.upstream_of(fork), Some(ROOT));
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
