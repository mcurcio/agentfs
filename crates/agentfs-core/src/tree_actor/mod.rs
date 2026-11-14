//! The tree actor (git-model §3, §6, §7, §12 and §18; `docs/trees.md`).
//!
//! One exclusively locked actor owns a journal and the saved state the views
//! serve. Every call is appended to the journal before it returns, with no
//! fsync; a FLUSH, an fsync or an FSYNCDIR makes the records durable by group
//! commit, outside the actor's lock ([`Recording`], [`Durability`]). A step's
//! record is the change itself and its commit point: the actor decides,
//! appends and applies it, and runs no git. The builder makes each durable
//! step's commits behind it, and [`History`], the one owner of the Git store,
//! hands out only what is built (docs/trees.md, "History trails the files").

mod call_path;
mod history;
mod journal;
mod pointers;
mod queue;
mod recording;
mod referee;
pub mod stages;
mod takes;
mod tree;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use agentfs_contracts::name::WriterLabel;
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::ports::Found;
use agentfs_contracts::volume::VolumeIdentity;
use git2::Oid;
use serde::Serialize;

use crate::file_lock::{FileLock, Mode};
pub use agentfs_contracts::{NotATree, TreeId};
pub use call_path::{Progress, Recording, Then};
use history::builder::{BuildState, Builder, JournalReader, copy_all};
pub use history::built::{BuildError, BuiltSnapshot, Health, History, Lag, TreeLag, WaitError};
use history::built_log::{self, BuiltLog};
use history::message::Message;
use history::upstreams::Upstreams;
use history::{
    check_objects, clean_packs, objects_of, read_trees, refs, remove_stale_locks, sync_marker,
};
pub use journal::durable::{Durability, DurableError, RecordNo};
use journal::{
    Change, Entry, FoldedOpen, Journal, PointerChange, Record, Replay, Target, compact_at, fold,
};
use pointers::Pointers;
pub use recording::FileMap;
pub use recording::file_map::FileMapReader;
use recording::{Handle, pages};
use stages::Stages;
use tree::{EventNumber, SavedTree};

const FORMAT: &str = "agentfs-trees-9\n";
/// The volume identity file: [`VolumeIdentity`] as JSON, written once by
/// [`TreeActor::create`], read before any write at every open.
const IDENTITY: &str = "volume";
/// The journal's directory of segments (`journal`).
const JOURNAL: &str = "journal";
/// The builder's built log (`history::built_log`).
const BUILT_LOG: &str = "built.log";
const INITIALIZING: &str = "initializing";
/// The state area's lease file, under the state area: an exclusive `flock`
/// held by the serving process and shared with its tree Git children, which
/// outlive a killed holder until their parent-death signal lands.
pub const LOCK: &str = "actor.lock";
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
    #[error("no tree volume here: create it with `agentfs trees init --volume NAME`")]
    NotCreated,
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
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// History could not be built through what a reader waited for.
    #[error(transparent)]
    History(#[from] WaitError),
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
            Self::Name => libc::EINVAL,
            Self::TooLarge => libc::EFBIG,
            Self::Busy => libc::EBUSY,
            Self::Durable(e) => e.errno(),
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

/// What [`TreeActor::take_next`] did: took one entry, found every entry waiting, or
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
    /// The save is on record as this step; the builder makes its commit.
    Commit(RecordNo),
    /// An old handle after unlink/rename-over must never reach a replacement.
    OrphanDropped,
}

/// The builder's progress, as metrics report it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HistoryMetrics {
    /// The built point: the last record whose commits are built.
    pub built_through: u64,
    /// The last record appended.
    pub appended: u64,
    /// The age of the oldest step not yet built, in nanoseconds.
    pub oldest_unbuilt_ns: u64,
    /// The worst tree's lag (the lag gate's estimate), in milliseconds.
    pub lag_ms_max: u64,
    /// `current`, `lagging`, `stalled` or `failed`.
    pub health: String,
}

/// Process-local counters for observing save and queue costs. Durations are
/// measurements only and never participate in ordering or conflict decisions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
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
    /// The journal's segments' bytes now: what a held open file keeps from compaction.
    pub journal_bytes: u64,
    /// Segments compaction has dropped since open.
    pub journal_dropped: u64,
    /// Bytes compaction has copied forward since open: a live open's frames
    /// that pinned more dead bytes than their own.
    pub journal_carried_bytes: u64,
    /// History as built behind the call path.
    pub history: HistoryMetrics,
}

/// A single synchronous actor. Sharing requires a mutex; a second process
/// cannot open this volume. No method exposes its state area to workloads.
pub struct TreeActor {
    /// The volume identity file's content: every commit's `Agentfs-Volume` and
    /// `Agentfs-Volume-Id`.
    identity: VolumeIdentity,
    /// The history service's pod context, carried by its own commits (Root, Sync).
    service: PodContext,
    journal: Journal,
    /// The one owner of the Git store; the builder runs behind it.
    history: History,
    builder: Option<JoinHandle<()>>,
    /// The pointers as the journal's records name them.
    pointers: Pointers<Target>,
    /// Each fork's upstream. The root has none.
    upstreams: Upstreams,
    trees: BTreeMap<TreeId, SavedTree>,
    handles: BTreeMap<u64, Handle>,
    /// Closes that could not save, in arrival order. Their handles count as
    /// open until a later step in their tree saves them, or a restart does.
    pending_closes: Vec<u64>,
    next: u64,
    stopped: bool,
    compact_threshold: u64,
    metrics: Metrics,
    waiting_since: BTreeMap<String, Instant>,
    pages: BTreeMap<(TreeId, u64), pages::ObjectPages>,
    /// Objects takes changed since the view last asked (`drain_changed`).
    changed: Vec<(TreeId, u64)>,
    /// Each tree's last head-moving step since open: a fast-forward or a
    /// sync is refused if a head it read moved ([`takes`]).
    heads: BTreeMap<TreeId, RecordNo>,
    /// Each key's last touching step, kept while its tree's queue holds an
    /// entry ([`takes`]).
    touched: takes::Touched,
    /// The last compaction's redo start, and the oid each step value the
    /// pointers held then was built as: a step whose pointers were read
    /// before that compaction names oids, never a step it covers.
    compacted: (RecordNo, BTreeMap<RecordNo, String>),
    _lock: Arc<FileLock>,
}

/// A take's bytes in chunks of at most `journal::TAKE_CHUNK`, recorded before it.
type Chunks<'a> = &'a mut dyn Iterator<Item = io::Result<Vec<u8>>>;

/// A started volume's history and its saved trees (`TreeActor::start_history`).
struct Started {
    history: History,
    builder: JoinHandle<()>,
    /// The built point it started at.
    built: RecordNo,
    /// The saved trees as of the journal's last record.
    trees: BTreeMap<TreeId, SavedTree>,
}

/// What a state area holds: reads `format` and `volume` only, takes no lock,
/// writes nothing. An absent or empty directory (its lock file at most), or one
/// a cut-short create left under its `initializing` marker, holds `Nothing`.
///
/// # Errors
///
/// A directory or file that could not be read.
pub fn inspect(root: &Path) -> io::Result<Found> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Found::Nothing),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in entries {
        names.push(entry?.file_name());
    }
    if names.iter().all(|name| name == LOCK) || names.iter().any(|name| name == INITIALIZING) {
        return Ok(Found::Nothing);
    }
    match fs::read(root.join("format")) {
        Ok(format) if format == FORMAT.as_bytes() => {}
        Ok(_) => return Ok(Found::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Found::Other),
        Err(e) => return Err(e),
    }
    match fs::read(root.join(IDENTITY)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).map_or(Found::Other, Found::Volume)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Found::Other),
        Err(e) => Err(e),
    }
}

/// The identity of the volume at `root`: [`Error::NotCreated`] where there is
/// none, [`Error::Format`] for another format.
fn found_volume(root: &Path) -> Result<VolumeIdentity> {
    match inspect(root)? {
        Found::Nothing => Err(Error::NotCreated),
        Found::Other => Err(Error::Format),
        Found::Volume(identity) => Ok(identity),
    }
}

/// The state area's exclusive lock: [`Error::Busy`] while another holds it.
fn lock_area(root: &Path) -> Result<Arc<FileLock>> {
    Ok(Arc::new(
        FileLock::try_take(&root.join(LOCK), Mode::Exclusive)?.ok_or(Error::Busy)?,
    ))
}

fn sync_dir(root: &Path) -> io::Result<()> {
    File::open(root)?.sync_all()
}

/// Raises the soft limit on open files to the hard one, as the Go runtime
/// does at start: each built file's map holds its checkout open
/// (`recording::file_map::BaseFile`), one descriptor per non-empty file and
/// tree. A refusal leaves the limit as it was.
fn raise_open_files() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit read and write only `limit`.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) == 0
            && limit.rlim_cur < limit.rlim_max
        {
            limit.rlim_cur = limit.rlim_max;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit);
        }
    }
}

/// Writes `bytes` to `name` whole: a temporary file, fsynced, renamed into place.
fn write_durably(root: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = root.join(format!("{name}.tmp"));
    fs::write(&temporary, bytes)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, root.join(name))?;
    sync_dir(root)
}

// Only a create deletes, and only under its own marker: a cut one holds
// nothing a call returned for.
fn clear_cut_create(root: &Path) -> io::Result<()> {
    for dir in ["history.git", "trees", JOURNAL] {
        if root.join(dir).exists() {
            fs::remove_dir_all(root.join(dir))?;
        }
    }
    for file in [BUILT_LOG, "format", "format.tmp", IDENTITY, "volume.tmp"] {
        match fs::remove_file(root.join(file)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Each pointer the records name, as built: an oid, or a step's value the
/// built log resolved. [`Error::Format`] for a step value it lacks.
fn resolve_pointers(
    pointers: &Pointers<Target>,
    values: &BTreeMap<(RecordNo, String), Oid>,
) -> Result<Pointers> {
    Ok(Pointers::new(
        pointers
            .iter()
            .map(|(name, target)| {
                let oid = match target {
                    Target::Oid(hex) => Oid::from_str(hex).map_err(|_| Error::Format)?,
                    Target::Step(step) => {
                        *values.get(&(*step, name.clone())).ok_or(Error::Format)?
                    }
                };
                Ok((name.clone(), oid))
            })
            .collect::<Result<_>>()?,
    ))
}

impl TreeActor {
    /// Creates a volume in an absent or empty directory (or one a cut-short
    /// create left under its marker): the marker, `volume`, the repository and
    /// its Root commit (writer `agentfs`, `service`'s pod context), the journal,
    /// then `format`; each fsynced, the directory too. Then serves it, as
    /// [`TreeActor::open`] does.
    ///
    /// # Errors
    ///
    /// [`Error::Exists`] for any other directory, [`Error::Busy`] while another
    /// holds the state area, and any failed write.
    pub fn create(root: &Path, identity: &VolumeIdentity, service: &PodContext) -> Result<Self> {
        crate::history::init_libgit2();
        if !root.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)?;
        }
        let root = root.canonicalize()?;
        let lock = lock_area(&root)?;
        if inspect(&root)? != Found::Nothing {
            return Err(Error::Exists);
        }
        // Never grant another user access to a new state area.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let marker = root.join(INITIALIZING);
        if marker.exists() {
            clear_cut_create(&root)?;
        }
        fs::write(&marker, b"")?;
        File::open(&marker)?.sync_all()?;
        sync_dir(&root)?;
        let mut text = serde_json::to_string(identity)?;
        text.push('\n');
        write_durably(&root, IDENTITY, text.as_bytes())?;
        let repo = history::init_store(&root.join("history.git"), Arc::clone(&lock))?;
        repo.write_config(&refs::head(), &BTreeMap::new())?;
        let tree = repo.write_tree(&[])?;
        let head = repo.write_commit(tree, &[], &Message::Root { service }.render(identity)?)?;
        // Every sync queue entry names this blob.
        repo.write_blob(b"sync")?;
        repo.sync_objects()?;
        Journal::create(
            &root.join(JOURNAL),
            &Record {
                seq: RecordNo::ZERO,
                at_ns: now_ns(),
                entry: Entry::Checkpoint {
                    next: RecordNo::new(1),
                    built: RecordNo::ZERO,
                    pointers: [(refs::branch(TreeId::Root), head.to_string())]
                        .into_iter()
                        .collect(),
                    upstreams: BTreeMap::new(),
                    opens: Vec::new(),
                    page_saved: Vec::new(),
                    page_writes: Vec::new(),
                    objects: BTreeMap::new(),
                },
            },
        )?;
        drop(repo);
        crate::failpoints::hit("tree.init_before_format")?;
        write_durably(&root, "format", FORMAT.as_bytes())?;
        fs::remove_file(&marker)?;
        sync_dir(&root)?;
        Self::serve(root, lock, identity.clone(), service.clone())
    }

    /// Recovers a volume and serves it. Reads `format` and `volume` before any
    /// write. Recovery builds history through the journal's last record, then
    /// closes every abandoned write-open exactly once, as its own writer with
    /// its journaled pod context; `service` is carried by the Sync commits this
    /// run writes. It takes no queue entries: the view's worker or a caller
    /// runs [`TreeActor::progress`].
    ///
    /// # Errors
    ///
    /// [`Error::NotCreated`] where no volume was created; [`Error::Format`] for
    /// a state area that is not this format (an older format included), or
    /// whose journal names a tree that is no [`TreeId`], has no root, gives the
    /// root an upstream or a fork an upstream that is no tree;
    /// [`Error::CorruptJournal`] and [`Error::MissingObject`] when the journal
    /// or a pointer's objects are damaged (docs/runbooks/recording-failures.md),
    /// [`Error::History`] when history cannot be built, and any failure of a
    /// recovery close.
    pub fn open(root: &Path, service: &PodContext) -> Result<Self> {
        found_volume(root)?;
        crate::history::init_libgit2();
        let root = root.canonicalize()?;
        let lock = lock_area(&root)?;
        // Again under the lock: a create may have been cut short since.
        let identity = found_volume(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Self::serve(root, lock, identity, service.clone())
    }

    /// The volume identity file's content.
    pub fn identity(&self) -> &VolumeIdentity {
        &self.identity
    }

    /// `agentfs`, with the history service's pod context: the writer of a fork
    /// step, a sync and the Root commit.
    fn agentfs(&self) -> WriterAt {
        WriterAt {
            writer: WriterLabel::agentfs(),
            at: self.service.clone(),
        }
    }

    /// Starts History and the builder at the built point the built log and the
    /// journal's checkpoint name, with every copy (refs, `config`, checkouts)
    /// rebuilt from it. Returns the saved trees as of the journal's last
    /// record: the built trees with every later record replayed.
    #[allow(
        clippy::too_many_arguments,
        reason = "what a start reads, passed through"
    )]
    fn start_history(
        root: &Path,
        lock: &Arc<FileLock>,
        frames: &[journal::Frame],
        folded_built: RecordNo,
        durability: &Arc<Durability>,
        salt: u32,
        identity: &VolumeIdentity,
        service: &PodContext,
    ) -> Result<Started> {
        let git_dir = root.join("history.git");
        remove_stale_locks(&git_dir)?;
        clean_packs(&git_dir)?;
        let repo = history::open_store(&git_dir, Arc::clone(lock))?;
        // Before any git command: an empty config silently loses core.fsync,
        // and an empty HEAD is "not a git repository".
        repo.write_config(&refs::head(), &BTreeMap::new())?;
        let (built_log, entries) = BuiltLog::open(&root.join(BUILT_LOG))?;
        let built = folded_built.max(built_log::built_through(&entries));
        let values = built_log::values(&entries)?;
        let upto = frames
            .iter()
            .position(|(record, _)| record.seq > built)
            .unwrap_or(frames.len());
        let at_built = fold(&frames[..upto])?;
        let pointers = resolve_pointers(&at_built.pointers, &values)?;
        check_objects(&repo, &pointers)?;
        // Object numbers as of the built point: the checkpoint's, moved by
        // the steps built after it, on the trees as the checkpoint has them.
        let objects = if built > folded_built {
            let checkpoint = fold(&frames[..1])?;
            let at_checkpoint = resolve_pointers(&checkpoint.pointers, &values)?;
            let trees = read_trees(&repo, &at_checkpoint, &checkpoint.objects, |_, _, _| {
                Ok(Arc::default())
            })?
            .iter()
            .map(|(tree, t)| (*tree, SavedTree::of(t)))
            .collect();
            let mut replay = Replay::shapes(trees, &checkpoint.opens)?;
            for (record, tail) in &frames[1..upto] {
                replay.apply(record, tail)?;
            }
            replay
                .trees
                .iter()
                .map(|(tree, t)| Ok((*tree, objects_of(&t.shape)?)))
                .collect::<Result<_>>()?
        } else {
            at_built.objects.clone()
        };
        let checked_out = copy_all(&repo, root, &pointers, &at_built.upstreams)?;
        // Each file reads from its checkout as built: a map of no frame.
        let trees = read_trees(&repo, &pointers, &objects, |tree, _, path| {
            let at = history::checkout_path(root, tree, path);
            let len = fs::metadata(&at)?.len();
            if len == 0 {
                let mut map = FileMap::empty();
                map.saved_at(built);
                return Ok(Arc::new(map.into()));
            }
            let base = recording::file_map::BaseFile::open(&at, built, None)?;
            Ok(Arc::new(FileMap::of_base(base, len).into()))
        })?;
        at_built
            .upstreams
            .check_recovered(|fork| trees.contains_key(&fork))?;
        let owners = history::owners(&repo, &pointers)?;
        // The saved state: the built trees with every record after the built
        // point replayed on them, as the actor applied it at the call. Open
        // never waits for the builder, which starts from the same trees.
        let saved = trees
            .iter()
            .map(|(tree, t)| (*tree, SavedTree::of(t)))
            .collect();
        let mut replay = Replay::new(
            saved,
            &at_built.opens,
            at_built.page_writes.clone(),
            at_built.bytes.clone(),
        )?;
        for (record, tail) in &frames[upto..] {
            replay.apply(record, tail)?;
        }
        let heads = values
            .iter()
            .filter(|((_, name), _)| {
                name.starts_with("refs/heads/") || name.starts_with(refs::QUEUES)
            })
            .map(|((step, _), oid)| (*step, *oid))
            .collect();
        let state = BuildState::at(
            trees,
            &pointers,
            at_built.upstreams.clone(),
            &at_built.opens,
            at_built.page_writes.clone(),
            at_built.bytes.clone(),
            &owners,
            heads,
        )?;
        let objects = objects
            .into_iter()
            .map(|(tree, objects)| (tree, Arc::new(objects)))
            .collect();
        let history = History::new(repo, built, pointers.clone(), owners, objects);
        history.checkpointed(folded_built);
        let builder = Builder {
            state,
            reader: JournalReader::open(Arc::clone(durability), salt, built)?,
            history: history.clone(),
            durability: Arc::clone(durability),
            built_log,
            root: root.to_owned(),
            identity: identity.clone(),
            service: service.clone(),
            copied: pointers,
            checked_out,
            stale: false,
            log: entries,
        };
        let owner = crate::failpoints::owner();
        let handle = std::thread::Builder::new()
            .name("tree-builder".into())
            .spawn(move || {
                crate::failpoints::adopt(owner);
                let history = builder.history.clone();
                builder.run();
                history.builder_gone();
            })?;
        Ok(Started {
            history,
            builder: handle,
            built,
            trees: replay.trees,
        })
    }

    /// Recovers the locked state area of a volume of this format.
    fn serve(
        root: PathBuf,
        lock: Arc<FileLock>,
        identity: VolumeIdentity,
        service: PodContext,
    ) -> Result<Self> {
        raise_open_files();
        let (journal, frames) = Journal::open(&root.join(JOURNAL))?;
        let folded = fold(&frames)?;
        let Started {
            history,
            builder,
            built,
            trees,
        } = Self::start_history(
            &root,
            &lock,
            &frames,
            folded.built,
            journal.durability(),
            journal.salt(),
            &identity,
            &service,
        )?;
        drop(frames);
        let mut volume = Self {
            identity,
            service,
            journal,
            history,
            builder: Some(builder),
            // As the records name them: a step not yet built by its number.
            pointers: folded.pointers,
            upstreams: folded.upstreams,
            trees,
            handles: BTreeMap::new(),
            pending_closes: Vec::new(),
            // A record built and then cut off as a torn tail keeps its number.
            next: folded.next_seq.max(built.get().saturating_add(1)),
            stopped: false,
            compact_threshold: compact_at(0),
            metrics: Metrics::default(),
            waiting_since: BTreeMap::new(),
            pages: BTreeMap::new(),
            changed: Vec::new(),
            heads: BTreeMap::new(),
            touched: BTreeMap::new(),
            compacted: (RecordNo::ZERO, BTreeMap::new()),
            _lock: lock,
        };
        for (handle, FoldedOpen { open, calls }) in folded.opens {
            volume.handles.insert(
                handle,
                Handle {
                    tree: open.tree,
                    by: open.by,
                    object: open.object,
                    // Nothing reads it: this handle closes before serving.
                    bytes: FileMap::empty(),
                    calls,
                    replayed: None,
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
        volume.metrics.queued = volume.pointers.under(refs::QUEUES).len() as u64;
        // A journal history is built through compacts at once; otherwise at
        // the first record after the builder catches up. Open never waits.
        if volume.history.built_through() >= volume.durability().appended() {
            if let Err(e) = volume.compact() {
                eprintln!("tree CompactionFailed: {e}");
            }
            volume.compact_threshold = compact_at(volume.journal.len());
        } else {
            volume.compact_threshold = 0;
        }
        Ok(volume)
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
        let mut metrics = self.metrics.clone();
        metrics.stages = Stages::current();
        metrics.journal_bytes = self.journal.len();
        metrics.journal_carried_bytes = self.journal.carried();
        for since in self.waiting_since.values() {
            let nanos = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
            metrics.wait_nanos = metrics.wait_nanos.saturating_add(nanos);
            metrics.max_wait_nanos = metrics.max_wait_nanos.max(nanos);
        }
        let lag = self.history.lag();
        metrics.history = HistoryMetrics {
            built_through: self.history.built_through().get(),
            appended: self.durability().appended().get(),
            oldest_unbuilt_ns: lag
                .oldest_unbuilt
                .map_or(0, |age| u64::try_from(age.as_nanos()).unwrap_or(u64::MAX)),
            lag_ms_max: lag
                .trees
                .first()
                .map_or(0, |worst| (worst.seconds * 1000.0) as u64),
            health: match lag.health {
                Health::Failed(_) => "failed",
                Health::Current => "current",
                Health::Lagging => "lagging",
                Health::Stalled => "stalled",
            }
            .to_owned(),
        };
        metrics
    }

    /// A failed actor must not keep answering reads from stale state: it
    /// stopped, its journal file is unknown, or a journal fsync failed.
    pub fn is_stopped(&self) -> bool {
        self.stopped || self.journal.unknown() || self.journal.durability().has_failed()
    }

    /// The journal's group commit: what [`Recording`] waits on, outside the
    /// actor's lock. Waiting on it with the actor locked holds every call.
    pub fn durability(&self) -> &Arc<Durability> {
        self.journal.durability()
    }

    /// History, the one owner of the Git store.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Waits until every record appended so far is built: what a test, the
    /// CLI or a replay calls before it reads history.
    ///
    /// # Errors
    ///
    /// [`Error::History`] when the builder failed or stopped first.
    pub fn settle(&self) -> Result<RecordNo> {
        Ok(self.history.wait_built(self.durability().appended())?)
    }

    /// The last record a FLUSH of `open` must cover: its open record, its
    /// calls, its object's page writes.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open.
    pub fn durable_through(&self, open: u64) -> Result<RecordNo> {
        let h = self.handles.get(&open).ok_or(Error::NotFound)?;
        let calls = h.calls.last().map_or(open, |(seq, _, _)| *seq);
        let pages = self.pages_last_write(h.tree, h.object).unwrap_or(open);
        Ok(RecordNo::new(open.max(calls).max(pages)))
    }

    /// The tree's head as built, once every record appended so far is built.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such tree, and as [`TreeActor::settle`].
    pub fn head(&self, tree: TreeId) -> Result<Oid> {
        self.tree(tree)?;
        self.settle()?;
        self.history.snapshot().head(tree).ok_or(Error::NotFound)
    }

    /// Fork IDs are numeric and never reused. Forking a fork is not exposed yet.
    /// A fork of the root, at the root's saved head.
    pub fn fork(&mut self) -> Result<TreeId> {
        let upstream = TreeId::Root;
        self.tree(upstream)?;
        self.run_pending_closes(upstream, None)?;
        let seq = self.next;
        let fork = TreeId::Fork(seq);
        let head = self
            .pointers
            .get(&refs::branch(upstream))
            .ok_or(Error::Format)?;
        let pointers = vec![PointerChange {
            name: refs::branch(fork),
            new: Some(head),
            old: None,
        }];
        let by = self.agentfs();
        self.record_step(
            by,
            Change::Fork {
                upstream: upstream.to_string(),
            },
            pointers,
            &mut std::iter::empty(),
        )?;
        self.upstreams.add(seq, upstream);
        let mut tree = self.tree(upstream)?.clone();
        for object in tree.shape.anonymous.clone() {
            tree.drop_object(object);
        }
        self.trees.insert(fork, tree);
        self.after_step();
        Ok(fork)
    }

    fn tree(&self, tree: TreeId) -> Result<&SavedTree> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        self.trees.get(&tree).ok_or(Error::NotFound)
    }

    /// Take at most one eligible queue entry. Callers can release the actor
    /// mutex between calls, so opens do not wait behind a waiting entry. A take
    /// that fails stays queued for the next event and this answers `Ok(false)`.
    pub fn progress(&mut self) -> Result<bool> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        match self.progress_one() {
            Err(e) if !self.is_stopped() => {
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

    /// One queue step in `tree`: its first entry P1 lets go is taken whole,
    /// decided on history built through every record appended before it
    /// ([`takes`]). Failures are not swallowed, unlike [`TreeActor::progress`].
    pub fn take_next(&mut self, tree: TreeId) -> Result<Took> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let history = self.history.clone();
        let mut actor = self;
        takes::take_one(&mut actor, &history, tree)
    }

    /// Whether a landing of `tree` waits in its upstream's queue, once every
    /// record appended so far is built.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::settle`], and a failed read of history.
    #[cfg(test)]
    pub(in crate::tree_actor) fn pending_from(&self, tree: TreeId) -> Result<bool> {
        let Some(upstream) = self.upstream_of(tree) else {
            return Ok(false);
        };
        self.settle()?;
        history::take::pending_from(&self.history.snapshot(), tree, upstream)
    }

    /// A change of pointer `name` to `new`, from its value now.
    fn change(&self, name: String, new: Option<Target>) -> PointerChange {
        PointerChange {
            old: self.pointers.get(&name),
            name,
            new,
        }
    }

    /// A sync entry, numbered by `event`, in each fork of `tree`.
    fn enqueue_syncs(
        &self,
        tree: TreeId,
        event: EventNumber,
        pointers: &mut Vec<PointerChange>,
    ) -> u64 {
        let mut count = 0;
        for fork in self.upstreams.forks_of(tree) {
            pointers.push(PointerChange {
                name: refs::queue_entry(fork, event),
                new: Some(Target::Oid(sync_marker().to_string())),
                old: None,
            });
            count += 1;
        }
        count
    }

    /// The pointer changes a step that saves `tree` publishes: its branch at
    /// this step's commit, and one entry per destination,
    /// numbered by the step (order across restart, independent of commit
    /// dates). A tree's head moves to its upstream as a landing and to each
    /// tree it is the upstream of as a sync.
    fn publish(&mut self, tree: TreeId, seq: u64) -> Vec<PointerChange> {
        let step = Target::Step(RecordNo::new(seq));
        let event = EventNumber::new(seq);
        let mut pointers = vec![self.change(refs::branch(tree), Some(step.clone()))];
        let mut enqueued = 0;
        if let Some(upstream) = self.upstream_of(tree) {
            pointers.push(PointerChange {
                name: refs::queue_entry(upstream, event),
                new: Some(step),
                old: None,
            });
            enqueued += 1;
        }
        enqueued += self.enqueue_syncs(tree, event, &mut pointers);
        self.metrics.queued = self.metrics.queued.saturating_add(enqueued);
        pointers
    }

    /// Appends one step, with `tail` (a take's files): its record is the
    /// change's commit point. A failure changes nothing and returns its errno.
    /// Then the builder is told; the caller applies the change, which cannot
    /// fail, and calls `after_step`.
    fn record_step(
        &mut self,
        by: WriterAt,
        change: Change,
        pointers: Vec<PointerChange>,
        chunks: Chunks<'_>,
    ) -> Result<(u64, Vec<journal::Tail>)> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let (tree, bytes) = self.build_cost(&change);
        let mut pointers = pointers;
        let recorded = (|| -> Result<(u64, Vec<journal::Tail>)> {
            crate::failpoints::hit("tree.before_step")?;
            // A compaction due runs before the step's pointers are checked:
            // values read before it are named as what they were built as.
            self.before_record();
            self.resolve_compacted(&mut pointers)?;
            self.pointers.check_targets(&pointers)?;
            // A take's bytes, then the take: no compaction comes between.
            let mut tails = Vec::new();
            for chunk in chunks {
                let chunk = chunk?;
                let (_, pos) = self.append_record(Entry::Bytes, &chunk)?;
                tails.push(journal::Tail {
                    pos,
                    len: chunk.len() as u64,
                });
            }
            let (seq, _) = self.append_record(
                Entry::Step {
                    by,
                    change,
                    pointers: pointers.clone(),
                },
                &[],
            )?;
            Ok((seq, tails))
        })();
        let (seq, tails) = match recorded {
            Ok(recorded) => recorded,
            Err(e) => {
                eprintln!("tree SaveFailed: {e}");
                return Err(e);
            }
        };
        self.pointers.apply_targets(&pointers);
        for change in &pointers {
            if let Some(moved) = refs::tree_of_branch(&change.name) {
                self.heads.insert(moved, RecordNo::new(seq));
            }
        }
        self.history.appended_step(RecordNo::new(seq), tree, bytes);
        Ok((seq, tails))
    }

    /// Takes each map the builder built that is the same save as the actor's
    /// (the same step): a map rebased onto its checkout lets go of the
    /// journal segments it read. An open with no call of its own since that
    /// save takes it too.
    fn rebase_built(&mut self) {
        for (tree, object, map) in self.history.take_maps() {
            let Some(saved) = self
                .trees
                .get_mut(&tree)
                .and_then(|t| t.bytes.get_mut(&object))
            else {
                continue;
            };
            if map.step().is_none() || saved.step() != map.step() || saved.len() != map.len() {
                continue;
            }
            *saved = map.clone();
            for handle in self.handles.values_mut() {
                if handle.tree == tree
                    && handle.object == object
                    && handle.bytes.step() == map.step()
                {
                    handle.bytes = map.clone();
                }
            }
        }
    }

    /// The tree a step's build works in, and about how many bytes it writes:
    /// the file a save changed and the tree's listings (the lag gate's
    /// estimate; the builder's measured rate corrects it).
    fn build_cost(&self, change: &Change) -> (TreeId, u64) {
        /// About what one file adds to a commit's listings and metadata.
        const LISTED: u64 = 64;
        let (tree, file) = match change {
            Change::Create { tree, .. }
            | Change::Link { tree, .. }
            | Change::Rename { tree, .. }
            | Change::Unlink { tree, .. }
            | Change::Take { tree, .. } => (tree, 0),
            Change::Save { tree, outcome, .. } => match outcome {
                journal::SaveOutcome::Saved { len, .. } => (tree, *len),
                _ => (tree, 0),
            },
            Change::Fork { upstream } => (upstream, 0),
        };
        let tree: TreeId = tree.parse().unwrap_or(TreeId::Root);
        let listed = self.trees.get(&tree).map_or(0, |t| t.names.len() as u64);
        (tree, file.saturating_add(listed.saturating_mul(LISTED)))
    }

    /// After a step is applied: a compaction when one is due.
    fn after_step(&mut self) {
        self.compact_if_due();
    }
}

impl Drop for TreeActor {
    /// Builds every record appended, then stops the builder: a clean shutdown
    /// leaves history built through the last record.
    fn drop(&mut self) {
        let appended = self.journal.durability().appended();
        self.history.stop(appended);
        if let Some(builder) = self.builder.take() {
            let _ = builder.join();
        }
    }
}

/// The unit tests' volumes: one line each, as `tests/support/trees.rs` gives the
/// integration tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    use agentfs_contracts::name::{VolumeName, WriterLabel};
    use agentfs_contracts::pod::{PodContext, WriterAt};
    use agentfs_contracts::volume::{VolumeId, VolumeIdentity};

    use super::TreeActor;

    /// A new volume `test` with a fixed id, served with an empty pod context.
    pub(crate) fn new_actor(dir: &Path) -> TreeActor {
        let identity = VolumeIdentity {
            name: VolumeName::parse("test").expect("a safe name"),
            id: VolumeId(uuid::Uuid::from_u128(1)),
        };
        TreeActor::create(dir, &identity, &PodContext::default()).expect("creates")
    }

    pub(crate) fn reopen(dir: &Path) -> TreeActor {
        TreeActor::open(dir, &PodContext::default()).expect("reopens")
    }

    /// `label` with an empty pod context.
    pub(crate) fn writer(label: &str) -> WriterAt {
        WriterAt {
            writer: WriterLabel::parse(label).expect("a writer"),
            at: PodContext::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::journal::Frame;
    use super::test_support::{new_actor, reopen, writer};
    use super::*;

    const ROOT: TreeId = TreeId::Root;

    fn frames_of(root: &Path) -> Vec<Frame> {
        Journal::open(&root.join(JOURNAL)).unwrap().1
    }

    /// Two folds that add up to the same, but for how their records name
    /// pointer values (a step's, or the oid it was built as): the same names.
    fn same_fold(a: &journal::Folded, b: &journal::Folded) {
        // Carried frames sit elsewhere: compare what they hold.
        type Opens = Vec<(u64, journal::LiveOpen, Vec<(u64, recording::Call, Vec<u8>)>)>;
        let opens = |f: &journal::Folded| -> Opens {
            f.opens
                .iter()
                .map(|(handle, o)| {
                    let calls = o
                        .calls
                        .iter()
                        .map(|(seq, call, pos)| {
                            (*seq, call.clone(), pos.bytes(0, call.carried()).unwrap())
                        })
                        .collect();
                    (*handle, o.open.clone(), calls)
                })
                .collect()
        };
        type Writes = Vec<((TreeId, u64), Vec<(u64, u64, Vec<u8>)>)>;
        let writes = |f: &journal::Folded| -> Writes {
            f.page_writes
                .iter()
                .map(|(key, runs)| {
                    let runs = runs
                        .iter()
                        .map(|(seq, run)| (*seq, run.offset, run.at.bytes(0, run.len).unwrap()))
                        .collect();
                    (*key, runs)
                })
                .collect()
        };
        assert_eq!(opens(a), opens(b));
        assert_eq!(a.upstreams, b.upstreams);
        assert_eq!(a.next_seq, b.next_seq);
        assert_eq!(writes(a), writes(b));
        assert_eq!(a.saved_through, b.saved_through);
        let names = |f: &journal::Folded| -> Vec<String> {
            f.pointers.iter().map(|(n, _)| n.clone()).collect()
        };
        assert_eq!(names(a), names(b));
    }

    #[test]
    fn compaction_keeps_the_fold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        // Both kinds of live open with calls and page writes, closed opens with
        // both, a fork and a queue entry.
        let fork = v.fork().unwrap();
        let held = v.write_open(ROOT, "held", &w, true).unwrap();
        v.write(held, 0, b"held-1", false).unwrap();
        let shared = v.write_open(ROOT, "shared", &w, true).unwrap();
        v.write(shared, 0, b"shared", false).unwrap();
        let object = v.open_object(shared).unwrap();
        let other = v.write_open(ROOT, "shared", &w, false).unwrap();
        v.page_writeback(other, 1, b"WXYZ").unwrap();
        v.close(other).unwrap();
        let anonymous = v.anonymous_open(ROOT, &w).unwrap();
        v.write(anonymous, 0, b"unnamed", false).unwrap();
        let closed = v.write_open(ROOT, "closed", &w, true).unwrap();
        v.write(closed, 0, b"closed", false).unwrap();
        let closed_object = v.open_object(closed).unwrap();
        v.record_page_write(
            ROOT,
            closed_object,
            &[pages::Changed {
                offset: 0,
                bytes: b"gone".to_vec(),
            }],
        )
        .unwrap();
        v.close(closed).unwrap();
        v.write_open(fork, "sandbox", &w, true).unwrap();

        let mut whole = frames_of(tmp.path());
        let folded = fold(&whole).unwrap();
        assert_eq!(folded.opens.len(), 4);
        assert_eq!(folded.opens[&held].calls.len(), 1);
        assert!(folded.opens[&anonymous].open.anonymous);
        assert_eq!(folded.upstreams.upstream_of(fork), Some(ROOT));
        let run = &folded.page_writes[&(ROOT, object)][0].1;
        assert_eq!(run.at.bytes(0, run.len).unwrap(), b"WXYZ");
        assert!(folded.saved_through.contains_key(&(ROOT, object)));
        assert!(!folded.page_writes.contains_key(&(ROOT, closed_object)));
        assert_eq!(folded.next_seq, v.next);
        assert_eq!(folded.pointers, v.pointers);

        v.settle().unwrap();
        v.compact().unwrap();
        let compacted = frames_of(tmp.path());
        assert!(compacted.len() < whole.len());
        same_fold(&fold(&compacted).unwrap(), &folded);

        // What follows lands on either start and adds up the same.
        v.write(held, 0, b"held-2", false).unwrap();
        v.write(shared, 0, b"!", true).unwrap();
        let again = v.write_open(ROOT, "again", &w, true).unwrap();
        v.close(shared).unwrap();
        v.first_link(ROOT, anonymous, "named", &w).unwrap();
        v.close(again).unwrap();
        while v.progress().unwrap() {}
        let now = frames_of(tmp.path());
        whole.extend_from_slice(&now[compacted.len()..]);
        let folded = fold(&now).unwrap();
        same_fold(&fold(&whole).unwrap(), &folded);
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
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        let first = v.write_open(ROOT, "f", &w, true).unwrap();
        v.page_writeback(first, 0, b"XXXX").unwrap();
        v.close(first).unwrap();
        let second = v.write_open(ROOT, "f", &w, false).unwrap();
        v.write(second, 0, b"YYYY", false).unwrap();
        v.close(second).unwrap();
        // Held open across the compaction, as a long-lived log is.
        let _held = v.write_open(ROOT, "f", &w, false).unwrap();
        v.settle().unwrap();
        v.compact().unwrap();
        drop(v);
        let v = reopen(tmp.path());
        assert_eq!(v.read(ROOT, "f").unwrap(), b"YYYY");
    }

    /// An edit of a checkpoint's pointers and upstreams.
    type CheckpointEdit = fn(&mut BTreeMap<String, String>, &mut BTreeMap<String, String>);

    /// A volume with fork 1 compacted to its checkpoint alone; reopened with
    /// `edit` applied to that checkpoint's pointers and upstreams.
    fn reopen_with(edit: CheckpointEdit) -> Result<TreeActor> {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        v.fork().unwrap();
        v.settle().unwrap();
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
        fs::remove_dir_all(tmp.path().join(JOURNAL)).unwrap();
        Journal::create(&tmp.path().join(JOURNAL), &first).unwrap();
        TreeActor::open(tmp.path(), &PodContext::default())
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
    fn an_older_format_is_refused() {
        for older in [
            "agentfs-trees-2\n",
            "agentfs-trees-3\n",
            "agentfs-trees-4\n",
            "agentfs-trees-5\n",
            "agentfs-trees-6\n",
            "agentfs-trees-7\n",
            "agentfs-trees-8\n",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            drop(new_actor(tmp.path()));
            fs::write(tmp.path().join("format"), older).unwrap();
            assert!(matches!(
                TreeActor::open(tmp.path(), &PodContext::default()),
                Err(Error::Format)
            ));
        }
    }

    /// A fork's landing in the root's queue is what `pending_from` sees, for
    /// that fork alone, until the root takes it.
    #[test]
    fn pending_from_holds_while_a_forks_landing_is_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        let fork = v.fork().unwrap();
        let other = v.fork().unwrap();
        assert!(!v.pending_from(fork).unwrap());
        assert!(!v.pending_from(ROOT).unwrap(), "the root lands nowhere");
        let h = v.write_open(fork, "f", &w, true).unwrap();
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

    /// Files the restart test's volume holds at its checkpoint, and after it.
    const RESTART_FILES: (usize, usize) = (500, 100);
    const RESTART_WORKER: &str = "AGENTFS_TREE_RESTART_WORKER";

    /// The restart test's served volume: files, a compaction, more files
    /// made durable but not built, then a kill.
    #[test]
    fn restart_worker() {
        let Some(root) = std::env::var_os(RESTART_WORKER) else {
            return;
        };
        let mut v = new_actor(Path::new(&root));
        let put = |v: &mut TreeActor, n: usize| {
            let h = v
                .write_open(TreeId::Root, &format!("f{n}"), &writer("w"), true)
                .unwrap();
            v.write(h, 0, format!("{n}\n").as_bytes(), false).unwrap();
            v.close(h).unwrap();
        };
        for n in 0..RESTART_FILES.0 {
            put(&mut v, n);
        }
        v.settle().unwrap();
        v.compact().unwrap();
        for n in RESTART_FILES.0..RESTART_FILES.0 + RESTART_FILES.1 {
            put(&mut v, n);
        }
        v.durability().make_all_durable().unwrap();
        println!("LAST {}", v.durability().appended().get());
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        // SAFETY: signals this process; nothing after it runs.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        unreachable!("killed");
    }

    #[test]
    fn restart_replays_from_the_checkpoint() {
        use std::os::unix::process::ExitStatusExt;
        let tmp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tree_actor::tests::restart_worker",
                "--nocapture",
            ])
            .env(RESTART_WORKER, tmp.path())
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGKILL), "{output:?}");
        let last: u64 = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("LAST "))
            .unwrap()
            .parse()
            .unwrap();
        // The killed process's git children share its lease until they die.
        let lease = FileLock::open(&tmp.path().join(LOCK)).unwrap();
        drop(FileLock::wait(lease, Mode::Exclusive).unwrap());
        let frames = frames_of(tmp.path());
        assert!(
            matches!(frames[0].0.entry, Entry::Checkpoint { .. }),
            "the journal starts at the checkpoint"
        );
        let started = Instant::now();
        let v = super::test_support::reopen(tmp.path());
        let reopened = started.elapsed();
        println!(
            "restart_replays_from_the_checkpoint: reopen of {} files ({} after the checkpoint) in {reopened:?}",
            RESTART_FILES.0 + RESTART_FILES.1,
            RESTART_FILES.1
        );
        assert_eq!(v.durability().appended().get(), last);
        assert_eq!(
            v.files(TreeId::Root).unwrap().len(),
            RESTART_FILES.0 + RESTART_FILES.1
        );
        assert_eq!(v.settle().unwrap().get(), last);
        assert_eq!(v.read(TreeId::Root, "f599").unwrap(), b"599\n");
    }
}
