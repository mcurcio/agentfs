//! The view host (docs/library/filesystem.md, "The view host"): each view is
//! a FUSE mount of one tree, its directories, files and symbolic links with
//! their recorded modes and owners and the tree's own times, reached only
//! through its client's session (`agentfs_core::session`), so its calls name
//! no tree and no writer. The kernel judges every permission
//! (`default_permissions`). Inode 1 is the root directory;
//! every other entry's inode is its node number + 2. Direct I/O and zero
//! TTLs; no passthrough or writeback-cache
//! negotiation. Shared mappings go through the kernel's page cache all the
//! same, so a read with no lock owner is a page fill and a write with
//! `FUSE_WRITE_CACHE` a page write; `agentfs_core::tree_actor`'s `pages`
//! module holds their rules (R1.4).
//!
//! The tree's untracked area (docs/trees.md, "Rules and the untracked area")
//! is merged into each directory, overlayfs-style: a tracked entry wins its
//! name, and a new name goes where the operator's rules place it. The
//! untracked half is plain passthrough on real files (`untracked`); this
//! module routes each call to one half or the other.

mod untracked;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_contracts::clients::{Access, Binding, FileOwner};
use agentfs_contracts::views::{MountOptions, MountPoint, ViewError, ViewId, Views};
use agentfs_core::session::{SessionGuard, Sessions, TreeSession};
use agentfs_core::tree_actor::{
    Attrs, Error, FileMap, Kind, Meta, Node, Perm, Progress, Recording, RenameMode, Saved, SetTime,
    TreeId,
};
use agentfs_rules::{NewKind, PathFacts, Placement};
use fuser::*;
use untracked::{Untracked, is_untracked};

const TTL: Duration = Duration::ZERO;
/// Files listed per READDIR at most: more than a reply's buffer holds, and the
/// kernel asks again from where a reply stopped.
const READDIR_BATCH: usize = 1024;
type Result<T> = std::result::Result<T, Errno>;

/// What the view host tells its owner, from its own threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    /// The view's mount is gone (unmounted, by `close` or from outside) and
    /// its session joined: every file it had open is released.
    ViewEnded(ViewId),
    /// A take landed and the kernel's pages of the files it changed were
    /// dropped.
    Landed,
}

/// Where each open view is mounted, and its kernel channel.
struct Mounted {
    at: PathBuf,
    tree: TreeId,
    notifier: Notifier,
    /// `close` has asked the kernel to unmount it.
    closing: bool,
}

type OpenViews = Arc<Mutex<BTreeMap<ViewId, Mounted>>>;

fn open_views(views: &OpenViews) -> MutexGuard<'_, BTreeMap<ViewId, Mounted>> {
    views.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every open view's tree and channel, read out of the lock: invalidation can
/// wait on a page fill.
fn notifiers(views: &OpenViews) -> Vec<(TreeId, Notifier)> {
    open_views(views)
        .values()
        .map(|view| (view.tree, view.notifier.clone()))
        .collect()
}

/// The views of one served volume, and its take and metrics workers.
pub struct ViewHost {
    sessions: Sessions,
    /// The state area, canonical: no view may be inside it or hold it.
    state: PathBuf,
    metrics_output: Option<PathBuf>,
    notify: Arc<dyn Fn(HostEvent) + Send + Sync>,
    views: OpenViews,
    next: u64,
    stop: Arc<AtomicBool>,
    takes: JoinHandle<io::Result<()>>,
    metrics: Option<JoinHandle<io::Result<()>>>,
    /// Each view's thread: it joins the view's session, then `ViewEnded`.
    joins: Vec<JoinHandle<()>>,
}

impl ViewHost {
    /// Starts the take worker (takes one at a time, each decided off the
    /// actor's lock, woken by built and failed events: never a timer) and,
    /// with `metrics_output`, the metrics worker. `notify` runs on the host's
    /// threads. The output belongs outside the state area and every view; no
    /// measurement affects a decision.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a metrics output with no filename or inside the
    /// state area; a failed thread spawn.
    pub fn new(
        sessions: Sessions,
        state: &Path,
        metrics_output: Option<&Path>,
        notify: Arc<dyn Fn(HostEvent) + Send + Sync>,
    ) -> io::Result<Self> {
        let state = state.canonicalize()?;
        let metrics_output = metrics_output
            .map(|path| {
                let filename = path.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "metrics output needs a filename",
                    )
                })?;
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let path = parent.canonicalize()?.join(filename);
                if path.starts_with(&state) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "metrics output must be outside state and views",
                    ));
                }
                Ok(path)
            })
            .transpose()?;
        let views = OpenViews::default();
        let stop = Arc::new(AtomicBool::new(false));
        let takes = {
            let recording = Arc::clone(sessions.recording());
            let (views, stop, notify) =
                (Arc::clone(&views), Arc::clone(&stop), Arc::clone(&notify));
            std::thread::Builder::new()
                .name("tree-takes".into())
                .spawn(move || take_worker(&recording, &views, &stop, &*notify))?
        };
        let metrics = metrics_output
            .clone()
            .map(|output| {
                let recording = Arc::clone(sessions.recording());
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name("tree-metrics".into())
                    .spawn(move || metrics_worker(&recording, &output, &stop))
            })
            .transpose()?;
        Ok(Self {
            sessions,
            state,
            metrics_output,
            notify,
            views,
            next: 1,
            stop,
            takes,
            metrics,
            joins: Vec::new(),
        })
    }

    /// After every view ended: stops and joins the workers. Serving ends with
    /// an error when the actor stopped (a failed journal fsync stops it off
    /// any view's thread).
    ///
    /// # Errors
    ///
    /// A worker's error or panic; the actor's [`Error::Stopped`].
    pub fn wait(self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.sessions.recording().wake_workers();
        let takes = self
            .takes
            .join()
            .map_err(|_| io::Error::other("queue worker panicked"))?;
        let metrics = self
            .metrics
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| io::Error::other("metrics worker panicked"))?
            })
            .transpose();
        for join in self.joins {
            join.join()
                .map_err(|_| io::Error::other("a view's thread panicked"))?;
        }
        takes?;
        metrics?;
        drop(
            self.sessions
                .recording()
                .actor()
                .map_err(io::Error::other)?,
        );
        Ok(())
    }

    /// The mount point checks: an existing empty directory, outside the
    /// state area, every open view and the metrics output, holding none of
    /// them.
    fn mount_point(&self, at: &MountPoint) -> Option<PathBuf> {
        let at = at.0.canonicalize().ok()?;
        let empty = std::fs::read_dir(&at).ok()?.next().is_none();
        let apart = |other: &Path| !at.starts_with(other) && !other.starts_with(&at);
        let separate = open_views(&self.views).values().all(|view| apart(&view.at));
        let metrics_apart = self
            .metrics_output
            .as_ref()
            .is_none_or(|output| !output.starts_with(&at));
        (at.is_dir() && empty && apart(&self.state) && separate && metrics_apart).then_some(at)
    }
}

impl Views for ViewHost {
    /// Mounts `binding`'s session's tree at `at`, each entry presented with
    /// its recorded owner, read-only for a read-only grant. Never waits for
    /// history: the tree must be among the actor's saved trees.
    fn open(
        &mut self,
        at: MountPoint,
        binding: Binding,
        options: MountOptions,
    ) -> std::result::Result<ViewId, ViewError> {
        let session = self
            .sessions
            .session(binding.session)
            .ok_or(ViewError::UnknownSession)?;
        let at = self
            .mount_point(&at)
            .ok_or(ViewError::MountFailed(libc::EINVAL))?;
        let saved = self
            .sessions
            .recording()
            .actor()
            .map_err(|e| ViewError::MountFailed(e.errno()))?
            .trees()
            .contains(&session.tree());
        if !saved {
            return Err(ViewError::MountFailed(libc::ENOENT));
        }
        let tree = session.tree();
        let read_only = binding.grant.access() == Access::ReadOnly;
        let area = session
            .untracked_area()
            .map_err(|e| ViewError::MountFailed(e.errno()))?;
        let give = crate::privileges::Privileges::current().can_chown_new();
        let view = View {
            session,
            handles: Arc::new(Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            })),
            untracked: Arc::new(Untracked::new(area, give)),
        };
        let mounted = fuser::spawn_mount(view, &at, &session_config(&options, read_only))
            .map_err(|e| ViewError::MountFailed(e.raw_os_error().unwrap_or(libc::EIO)))?;
        let id = ViewId(self.next);
        self.next += 1;
        open_views(&self.views).insert(
            id,
            Mounted {
                at,
                tree,
                notifier: mounted.notifier(),
                closing: false,
            },
        );
        let (views, notify) = (Arc::clone(&self.views), Arc::clone(&self.notify));
        let join = std::thread::Builder::new()
            .name("tree-view".into())
            .spawn(move || {
                if let Err(e) = mounted.join() {
                    eprintln!("tree ViewFailed: tree={tree}: {e}");
                }
                open_views(&views).remove(&id);
                notify(HostEvent::ViewEnded(id));
            })
            .map_err(|e| ViewError::MountFailed(e.raw_os_error().unwrap_or(libc::EIO)))?;
        self.joins.push(join);
        Ok(id)
    }

    /// A lazy unmount (`fusermount3 -uz`): the mount leaves the namespace at
    /// once and its files close as the kernel releases them. Idempotent.
    fn close(&mut self, view: ViewId) {
        let at = match open_views(&self.views).get_mut(&view) {
            Some(mounted) if !mounted.closing => {
                mounted.closing = true;
                mounted.at.clone()
            }
            _ => return,
        };
        match std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(&at)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("tree UnmountFailed: {}: {status}", at.display()),
            Err(e) => eprintln!("tree UnmountFailed: {}: {e}", at.display()),
        }
    }
}

/// The take worker: takes one at a time, each decided off the actor's lock;
/// after each landed take, the kernel's pages of what it changed dropped,
/// then `Landed`. Woken by built and failed events (docs/trees.md, "Takes"),
/// never a timer. A test's gate, while it lasts, takes in its place.
fn take_worker(
    recording: &Recording,
    views: &OpenViews,
    stop: &AtomicBool,
    notify: &(dyn Fn(HostEvent) + Send + Sync),
) -> io::Result<()> {
    // Once a test's gate ends, takes go on as the worker's: a stop drain
    // releases only with every queue empty.
    #[cfg(feature = "failpoints")]
    if let Some(gate) = agentfs_core::failpoints::take_gate() {
        run_take_gate(gate, recording, views)?;
    }
    let mut epoch = 0;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        while recording.progress().map_err(io::Error::other)? == Progress::Landed {
            let changed = recording.drain_changed().map_err(io::Error::other)?;
            // With the actor unlocked: invalidation can wait on a page fill.
            invalidate_changed(&notifiers(views), &changed);
            notify(HostEvent::Landed);
        }
        epoch = recording.wait_for_work(epoch);
    }
}

/// Each file carries `seq`, one more than the last: a reader that saw `n`
/// knows a file numbered `n + 2` was read after it.
fn metrics_worker(recording: &Recording, output: &Path, stop: &AtomicBool) -> io::Result<()> {
    let (mut seq, mut epoch) = (0_u64, 0);
    loop {
        let last = stop.load(Ordering::Acquire);
        seq += 1;
        let metrics = recording.actor().map_err(io::Error::other)?.metrics();
        let mut written = serde_json::to_value(&metrics)?;
        if let Some(fields) = written.as_object_mut() {
            fields.insert("seq".into(), seq.into());
        }
        let bytes = serde_json::to_vec(&written)?;
        let temporary = output.with_extension(format!("metrics-{}.tmp", std::process::id()));
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, output)?;
        if last {
            return Ok(());
        }
        // At each built or failed event, and every 100 ms between.
        epoch = recording.wait_for_event_or(epoch, Duration::from_millis(100));
    }
}

/// A test's takes in place of the worker's: one at each line (failpoints'
/// `TakeGate`), the actor locked only for that take.
#[cfg(feature = "failpoints")]
fn run_take_gate(
    mut gate: agentfs_core::failpoints::TakeGate,
    actor: &Recording,
    views: &OpenViews,
) -> io::Result<()> {
    while let Some(tree) = gate.next_tree()? {
        if tree == "settle" {
            let answer = match actor.settle() {
                Ok(_) => "settled".to_owned(),
                Err(e) => format!("error {e}"),
            };
            gate.answer(&answer)?;
            continue;
        }
        let took = tree
            .parse::<TreeId>()
            .map_err(|e| e.to_string())
            .and_then(|tree| actor.take_next(tree).map_err(|e| e.to_string()));
        let changed = actor.drain_changed().map_err(io::Error::other)?;
        invalidate_changed(&notifiers(views), &changed);
        let answer = match took {
            Ok(agentfs_core::tree_actor::Took::Taken) => "taken".to_owned(),
            Ok(agentfs_core::tree_actor::Took::Waiting) => "waiting".to_owned(),
            Ok(agentfs_core::tree_actor::Took::Empty) => "empty".to_owned(),
            Err(e) => format!("error {e}"),
        };
        gate.answer(&answer)?;
    }
    Ok(())
}

/// Drops the kernel's pages of files a take changed. A file holds no dirty page
/// then, because takes wait for write-opens. A failure leaves old pages in
/// mappings and stops nothing: fills read the new bytes once they go.
fn invalidate_changed(notifiers: &[(TreeId, Notifier)], changed: &[(TreeId, u64)]) {
    for (tree, object) in changed {
        for (_, notifier) in notifiers.iter().filter(|(name, _)| name == tree) {
            if let Err(e) = notifier.inval_inode(INodeNo(object + 2), 0, 0) {
                eprintln!("tree InvalidateFailed: tree={tree} object={object}: {e}");
            }
        }
    }
}

/// The fuser config for one view: FUSE `allow_other` (`SessionACL::All`) when
/// `options.allow_other`, else the mounting user alone; permission checks by
/// the kernel, no devices, no set-user-ID; read-only for a read-only client.
fn session_config(options: &MountOptions, read_only: bool) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("agentfs-trees".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        if read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    config.acl = if options.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config.n_threads = Some(1);
    config
}

#[derive(Clone)]
struct View {
    session: TreeSession,
    handles: Arc<Mutex<Handles>>,
    /// The tree's untracked half.
    untracked: Arc<Untracked>,
}

struct Handles {
    next: u64,
    entries: BTreeMap<u64, Open>,
}
enum Open {
    /// A read-only open's snapshot: the saved file's map at open, read with
    /// the actor unlocked.
    Read(FileMap),
    Write {
        handle: u64,
        append: bool,
        readable: bool,
        /// `O_SYNC` or `O_DSYNC`: each write is durable before it is answered.
        sync: bool,
    },
    /// An open of an untracked file: its descriptor in the area, read and
    /// written with no lock held.
    Untracked { fd: Arc<OwnedFd>, append: bool },
}

/// A write-open's actor handle; `None` for a read-only open.
fn open_handle(open: &Open) -> Option<u64> {
    match open {
        Open::Write { handle, .. } => Some(*handle),
        Open::Read(_) | Open::Untracked { .. } => None,
    }
}

fn errno(error: Error) -> Errno {
    Errno::from_i32(error.errno())
}

/// The inode of a node: 1 for the root directory, else its number + 2,
/// below every untracked entry's.
fn inode(node: &Node) -> INodeNo {
    let ino = node.number.map_or(1, |number| number + 2);
    debug_assert!(
        ino < untracked::UNTRACKED,
        "a tracked inode reached 1 << 63"
    );
    INodeNo(ino)
}

/// The kind a new entry of `kind` is, as the rules take it.
fn new_kind(kind: Kind) -> NewKind {
    match kind {
        Kind::File => NewKind::File,
        Kind::Dir => NewKind::Dir,
        Kind::Symlink => NewKind::Symlink,
    }
}

/// A kind of the area's, as the rules take it.
fn area_kind(kind: FileType) -> NewKind {
    match kind {
        FileType::Directory => NewKind::Dir,
        FileType::Symlink => NewKind::Symlink,
        FileType::NamedPipe => NewKind::Fifo,
        FileType::Socket => NewKind::Socket,
        _ => NewKind::File,
    }
}

/// Where a name lives in a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Half {
    Tracked,
    Untracked,
}

/// `dir`'s entry `name` as a tree path (`dir` `""` for the root).
fn child_path(dir: &str, name: &OsStr) -> Result<String> {
    let name = name.to_str().ok_or(Errno::EINVAL)?;
    Ok(if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    })
}

/// The directory holding `path`: `""` for a top-level entry.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// A time as the kernel takes it.
fn time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

/// What a create gives the new entry: the mode the kernel gives (its umask
/// applied), owned by the calling process.
fn create_attrs(req: &Request, mode: u32) -> Attrs {
    Attrs {
        perm: Perm::from_mode(mode),
        owner: FileOwner {
            uid: req.uid(),
            gid: req.gid(),
        },
    }
}

/// A chmod, chown or chgrp: the given mode, uid and gid over the entry's
/// current ones. The kernel has judged the caller's authority.
fn changed_attrs(meta: &Meta, mode: Option<u32>, uid: Option<u32>, gid: Option<u32>) -> Attrs {
    let now = meta.attrs();
    Attrs {
        perm: mode.map_or(now.perm, Perm::from_mode),
        owner: FileOwner {
            uid: uid.unwrap_or(now.owner.uid),
            gid: gid.unwrap_or(now.owner.gid),
        },
    }
}

/// A utimensat time as the tree takes it: `EINVAL` before the epoch.
fn set_time(time: TimeOrNow) -> Result<SetTime> {
    match time {
        TimeOrNow::Now => Ok(SetTime::Now),
        TimeOrNow::SpecificTime(at) => {
            let since = at.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
            u64::try_from(since.as_nanos())
                .map(SetTime::At)
                .map_err(|_| Errno::EINVAL)
        }
    }
}

/// What a setattr asks for, besides its inode and handle.
#[derive(Clone, Copy, Default)]
struct AttrChanges {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
}

impl AttrChanges {
    /// A chmod, chown or chgrp: a step that builds a commit.
    fn meta(&self) -> bool {
        self.mode.is_some() || self.uid.is_some() || self.gid.is_some()
    }
}

/// Whether an open's writes must each be durable before they are answered.
fn sync_flags(flags: i32) -> bool {
    flags & (libc::O_SYNC | libc::O_DSYNC) != 0
}

/// A FLUSH, FSYNC or FSYNCDIR answered once its records are durable, from
/// whichever thread the durability wait ends on.
fn answer_empty(reply: ReplyEmpty) -> agentfs_core::tree_actor::Then {
    Box::new(move |result| match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(e)),
    })
}

impl View {
    /// Runs `call` once the lag gate admits new work in this view's tree: at
    /// once on this thread, else on a gate thread, so a held tree holds no
    /// other request (docs/trees.md, "History trails the files").
    fn gated(&self, call: impl FnOnce(&View, Result<()>) + Send + 'static) {
        let view = self.clone();
        self.session.admit_then(Box::new(move |admitted| {
            call(&view, admitted.map_err(errno));
        }));
    }

    fn actor(&self) -> Result<SessionGuard<'_>> {
        self.session.lock().map_err(|_| Errno::EIO)
    }
    fn handles(&self) -> Result<MutexGuard<'_, Handles>> {
        self.handles.lock().map_err(|_| Errno::EIO)
    }
    /// A directory inode's tree path, and whether it is untracked.
    fn dir_of(&self, v: &SessionGuard<'_>, ino: INodeNo) -> Result<(String, bool)> {
        if is_untracked(ino) {
            Ok((self.untracked.path(ino)?, true))
        } else {
            Ok((self.path(v, ino)?, false))
        }
    }
    /// Where `path`, in a directory untracked or not, lives: a tracked entry
    /// wins its name, then an entry of the area; a new name goes where the
    /// rules place it as a `kind`.
    fn half(
        &self,
        v: &SessionGuard<'_>,
        path: &str,
        parent_untracked: bool,
        kind: NewKind,
    ) -> Result<Half> {
        if !parent_untracked && v.node(path).is_ok() {
            return Ok(Half::Tracked);
        }
        if parent_untracked || self.untracked.exists(path)? {
            return Ok(Half::Untracked);
        }
        let facts = PathFacts {
            parent_untracked,
            inside_repository: self.untracked.inside_repository(path)?,
        };
        Ok(match self.session.rules().place(path, kind, facts) {
            Placement::Tracked => Half::Tracked,
            Placement::Untracked(_) => Half::Untracked,
        })
    }
    /// A call that makes the name `name` in `parent` as a `kind`: run by
    /// `make` under the actor's lock when it is untracked (never gated);
    /// `None` when it is tracked, for the caller's gated call.
    fn untracked_new<T>(
        &self,
        parent: INodeNo,
        name: &OsStr,
        kind: NewKind,
        make: impl FnOnce(&SessionGuard<'_>, &str) -> Result<T>,
    ) -> Option<Result<T>> {
        let decided = (|| {
            let v = self.actor()?;
            let (dir, up) = self.dir_of(&v, parent)?;
            let path = child_path(&dir, name)?;
            Ok((self.half(&v, &path, up, kind)?, v, path))
        })();
        match decided {
            Err(e) => Some(Err(e)),
            Ok((Half::Tracked, _, _)) => None,
            Ok((Half::Untracked, v, _)) if !v.writable() => Some(Err(Errno::EROFS)),
            Ok((Half::Untracked, v, path)) => Some(make(&v, &path)),
        }
    }
    /// An untracked open's descriptor and append flag; `None` for any other
    /// handle.
    fn untracked_fd(&self, fh: FileHandle) -> Result<Option<(Arc<OwnedFd>, bool)>> {
        Ok(
            match self.handles()?.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Untracked { fd, append } => Some((Arc::clone(fd), *append)),
                _ => None,
            },
        )
    }
    /// Keeps an untracked open's descriptor: its handle.
    fn keep_untracked(&self, fd: OwnedFd, flags: i32) -> Result<u64> {
        let mut handles = self.handles()?;
        let fh = handles.next;
        handles.next += 1;
        handles.entries.insert(
            fh,
            Open::Untracked {
                fd: Arc::new(fd),
                append: flags & libc::O_APPEND != 0,
            },
        );
        Ok(fh)
    }
    /// The tree path of a named file, link or directory (`""` the root).
    fn path(&self, v: &SessionGuard<'_>, ino: INodeNo) -> Result<String> {
        if ino.0 == 1 {
            return Ok(String::new());
        }
        let number = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
        v.node_at(number)
            .map(|(path, _)| path.to_owned())
            .map_err(errno)
    }
    /// The node of an inode: a named entry, or a file no name reaches (an
    /// anonymous open's, or one unlinked while open).
    fn node(&self, v: &SessionGuard<'_>, ino: INodeNo) -> Result<Node> {
        if ino.0 == 1 {
            return v.node("").map_err(errno);
        }
        let number = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
        match v.node_at(number) {
            Ok((_, node)) => Ok(node),
            Err(Error::NotFound) => v.open_node(number).map_err(errno),
            Err(e) => Err(errno(e)),
        }
    }
    fn attr(&self, v: &SessionGuard<'_>, ino: INodeNo, fh: Option<FileHandle>) -> Result<FileAttr> {
        let node = self.node(v, ino)?;
        // First: the kernel keeps one size per inode, and an unlinked mapped
        // file's fstat has no path.
        let open_size = ino.0.checked_sub(2).and_then(|object| v.inode_size(object));
        let size: u64 = if node.meta.kind == Kind::Dir {
            0
        } else if let Some(size) = open_size {
            size
        } else if let Some(fh) = fh {
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.len(),
                Open::Write { handle, .. } => v.read_open(*handle).map_err(errno)?.len(),
                Open::Untracked { .. } => return Err(Errno::EBADF),
            }
        } else if v.is_anonymous(ino.0 - 2).map_err(errno)? {
            let handles = self.handles()?;
            let handle = handles
                .entries
                .values()
                .find_map(|entry| match entry {
                    Open::Write { handle, .. }
                        if v.open_object(*handle).ok() == Some(ino.0 - 2) =>
                    {
                        Some(*handle)
                    }
                    _ => None,
                })
                .ok_or(Errno::ENOENT)?;
            v.read_open(handle).map_err(errno)?.len()
        } else {
            node.len
        };
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: time(node.times.atime_ns),
            mtime: time(node.times.mtime_ns),
            ctime: time(node.times.ctime_ns),
            crtime: time(node.times.ctime_ns),
            kind: match node.meta.kind {
                Kind::Dir => FileType::Directory,
                Kind::Symlink => FileType::Symlink,
                Kind::File => FileType::RegularFile,
            },
            perm: u16::try_from(node.meta.mode.bits()).unwrap_or(0),
            nlink: node.links,
            uid: node.meta.uid,
            gid: node.meta.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    /// The attributes of the entry at `path`.
    fn attr_at(
        &self,
        v: &SessionGuard<'_>,
        path: &str,
        fh: Option<FileHandle>,
    ) -> Result<FileAttr> {
        let node = v.node(path).map_err(errno)?;
        self.attr(v, inode(&node), fh)
    }
    fn open_file(
        &self,
        v: &mut SessionGuard<'_>,
        path: &str,
        flags: i32,
        create: Option<Attrs>,
    ) -> Result<u64> {
        // The kernel follows a link before it opens; a link reached here was
        // opened with `O_NOFOLLOW`.
        if v.node(path)
            .is_ok_and(|node| node.meta.kind == Kind::Symlink)
        {
            return Err(Errno::ELOOP);
        }
        let mut handles = self.handles()?;
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !write {
            return Err(Errno::EACCES);
        }
        let entry = if write {
            let handle = if flags & libc::O_TRUNC != 0 {
                v.write_open_truncating(path, create)
            } else {
                v.write_open(path, create)
            }
            .map_err(errno)?;
            Open::Write {
                handle,
                append: flags & libc::O_APPEND != 0,
                readable: flags & libc::O_ACCMODE == libc::O_RDWR,
                sync: sync_flags(flags),
            }
        } else {
            if create.is_some() && v.node(path).is_err() {
                let h = v.write_open(path, create).map_err(errno)?;
                v.close(h).map_err(errno)?;
            }
            Open::Read(v.file_map(path).map_err(errno)?)
        };
        let fh = handles.next;
        handles.next += 1;
        handles.entries.insert(fh, entry);
        Ok(fh)
    }
    /// A setattr's changes, in one order: the metadata (one `set_meta`),
    /// the size, then the times (one `set_times`, which builds no commit). A
    /// file no name reaches has no path to record metadata or times at.
    fn set_attrs(
        &self,
        v: &mut SessionGuard<'_>,
        ino: INodeNo,
        changes: AttrChanges,
        fh: Option<FileHandle>,
    ) -> Result<FileAttr> {
        let times = (
            changes.atime.map(set_time).transpose()?,
            changes.mtime.map(set_time).transpose()?,
        );
        if changes.meta() {
            let path = self.path(v, ino).map_err(|_| Errno::EOPNOTSUPP)?;
            let meta = v.node(&path).map_err(errno)?.meta;
            let attrs = changed_attrs(&meta, changes.mode, changes.uid, changes.gid);
            v.set_meta(&path, attrs).map_err(errno)?;
        }
        if let Some(size) = changes.size {
            if let Some(fh) = fh {
                let handles = self.handles()?;
                match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                    Open::Write { handle, .. } => v.truncate(*handle, size).map_err(errno)?,
                    _ => return Err(Errno::EBADF),
                }
            } else {
                let path = self.path(v, ino)?;
                let h = v.write_open(&path, None).map_err(errno)?;
                let result = v.truncate(h, size).map_err(errno);
                v.close(h).map_err(errno)?;
                result?;
            }
        }
        if times.0.is_some() || times.1.is_some() {
            let path = self.path(v, ino).map_err(|_| Errno::EOPNOTSUPP)?;
            v.set_times(&path, times.0, times.1).map_err(errno)?;
        }
        self.attr(v, ino, fh)
    }
    /// A symbolic link at `path` to `target`, owned by `owner`: its attributes.
    fn make_symlink(
        &self,
        v: &mut SessionGuard<'_>,
        path: &str,
        target: &Path,
        owner: FileOwner,
    ) -> Result<FileAttr> {
        let target = target.to_str().ok_or(Errno::EINVAL)?;
        v.symlink(path, target, owner).map_err(errno)?;
        self.attr_at(v, path, None)
    }
    /// A tracked directory lists its tracked children at offsets 1 on, then
    /// the area's entries no tracked child hides at offsets from
    /// [`AREA_OFFSETS`] on, each its index in the area's sorted listing, so
    /// a resume finds its place in either half. An untracked directory
    /// lists the area's entries alone.
    fn listing(&self, ino: INodeNo, offset: u64) -> Result<Vec<(INodeNo, u64, FileType, String)>> {
        let v = self.actor()?;
        let (dir, untracked) = self.dir_of(&v, ino)?;
        // `..` is the parent's inode; the root is its own parent.
        let up = if dir.is_empty() {
            INodeNo(1)
        } else {
            let parent = parent_dir(&dir);
            match v.node(parent) {
                Ok(node) => inode(&node),
                Err(_) => self
                    .untracked
                    .lookup(parent)?
                    .map_or(INodeNo(1), |attr| attr.ino),
            }
        };
        let mut entries: Vec<(INodeNo, u64, FileType, String)> = Vec::new();
        let area_from = if offset >= AREA_OFFSETS {
            offset - AREA_OFFSETS
        } else {
            // `.` and `..` are entries 0 and 1, the entries 2 on: an
            // offset is the number of entries already read.
            let skip = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            for (i, (name, ino)) in [(".", ino), ("..", up)].into_iter().enumerate().skip(skip) {
                entries.push((ino, i as u64 + 1, FileType::Directory, name.to_owned()));
            }
            if !untracked {
                let children = v
                    .children(&dir, skip.saturating_sub(2), READDIR_BATCH)
                    .map_err(errno)?;
                let full = children.len() == READDIR_BATCH;
                let first = skip.max(2);
                for (i, entry) in children.into_iter().enumerate() {
                    let kind = match entry.kind {
                        Kind::Dir => FileType::Directory,
                        Kind::Symlink => FileType::Symlink,
                        Kind::File => FileType::RegularFile,
                    };
                    let next = (first + i + 1) as u64;
                    entries.push((INodeNo(entry.number + 2), next, kind, entry.name));
                }
                if full {
                    return Ok(entries);
                }
            }
            0
        };
        let area_from = usize::try_from(area_from).map_err(|_| Errno::EINVAL)?;
        for (j, (name, kind)) in self
            .untracked
            .entries(&dir)?
            .into_iter()
            .enumerate()
            .skip(area_from)
        {
            let path = child_path(&dir, OsStr::new(&name))?;
            // A tracked entry wins its name.
            if !untracked && v.node(&path).is_ok() {
                continue;
            }
            let Some(ino) = self.untracked.entry_ino(&dir, &name)? else {
                continue;
            };
            entries.push((ino, AREA_OFFSETS + j as u64 + 1, kind, name));
        }
        Ok(entries)
    }
    fn finish(&self, fh: FileHandle) -> Result<()> {
        if self.untracked_fd(fh)?.is_some() {
            // Closing the descriptor is all an untracked release does.
            self.handles()?.entries.remove(&fh.0);
            return Ok(());
        }
        let mut v = self.actor()?;
        let entry = self.handles()?.entries.remove(&fh.0).ok_or(Errno::EBADF)?;
        if let Open::Write { handle, .. } = entry
            && v.close(handle).map_err(errno)? == Saved::OrphanDropped
        {
            eprintln!(
                "tree OrphanDropped: tree={} open={handle}",
                self.session.tree()
            );
        }
        Ok(())
    }
}

impl Filesystem for View {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Keeps RELEASE ahead of a later OPEN, short of thousands outstanding: the
        // kernel queues background requests past this and an OPEN skips that
        // queue. It clamps the value to `max_user_bgreq`.
        config
            .set_max_background(u16::MAX)
            .map_err(|nearest| io::Error::other(format!("max_background: {nearest}")))?;
        for flag in [
            InitFlags::FUSE_ATOMIC_O_TRUNC,
            InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP,
        ] {
            if config.capabilities().contains(flag) {
                config.add_capabilities(flag).map_err(|missing| {
                    io::Error::other(format!("unsupported FUSE capabilities: {missing:?}"))
                })?;
            }
        }
        Ok(())
    }
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            let v = self.actor()?;
            let (dir, up) = self.dir_of(&v, parent)?;
            let path = child_path(&dir, name)?;
            if !up && let Ok(node) = v.node(&path) {
                return self.attr(&v, inode(&node), None);
            }
            self.untracked.lookup(&path)?.ok_or(Errno::ENOENT)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        if is_untracked(ino) {
            let result = fh
                .map(|fh| self.untracked_fd(fh))
                .transpose()
                .and_then(|fd| {
                    let fd = fd.flatten();
                    self.untracked
                        .getattr(ino, fd.as_ref().map(|(fd, _)| fd.as_ref()))
                });
            return match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            };
        }
        match self.actor().and_then(|v| self.attr(&v, ino, fh)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if is_untracked(ino) {
            let changes =
                flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0;
            let result = (|| {
                if changes && !self.actor()?.writable() {
                    return Err(Errno::EROFS);
                }
                let fd = self.untracked.open(&self.untracked.path(ino)?, flags.0)?;
                self.keep_untracked(fd, flags.0)
            })();
            return match result {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
                Err(e) => reply.error(e),
            };
        }
        let open = move |view: &View, admitted: Result<()>| {
            let result = admitted.and_then(|()| {
                let mut v = view.actor()?;
                let path = view.path(&v, ino)?;
                view.open_file(&mut v, &path, flags.0, None)
            });
            match result {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
                Err(e) => reply.error(e),
            }
        };
        // A write-open starts new work, so the lag gate stands before it.
        if flags.0 & libc::O_ACCMODE == libc::O_RDONLY {
            open(self, Ok(()));
        } else {
            self.gated(open);
        }
    }
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let owner = (req.uid(), req.gid());
        let untracked = self.untracked_new(parent, name, NewKind::File, |v, path| {
            let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
            let (fd, attr) = self.untracked.create(path, flags, mode, owner, meta_of)?;
            Ok((self.keep_untracked(fd, flags)?, attr))
        });
        if let Some(result) = untracked {
            return match result {
                Ok((fh, attr)) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::FOPEN_DIRECT_IO,
                ),
                Err(e) => reply.error(e),
            };
        }
        let name = name.to_owned();
        let attrs = create_attrs(req, mode);
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    let path = child_path(&view.path(&v, parent)?, &name)?;
                    if flags & libc::O_EXCL != 0 && v.node(&path).is_ok() {
                        return Err(Errno::EEXIST);
                    }
                    let fh = view.open_file(&mut v, &path, flags, Some(attrs))?;
                    Ok((fh, view.attr_at(&v, &path, Some(FileHandle(fh)))?))
                })()
            });
            match result {
                Ok((fh, attr)) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::FOPEN_DIRECT_IO,
                ),
                Err(e) => reply.error(e),
            }
        });
    }
    fn tmpfile(
        &self,
        req: &Request,
        parent: INodeNo,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let attrs = create_attrs(req, mode);
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    if view.node(&v, parent)?.meta.kind != Kind::Dir {
                        return Err(Errno::ENOTDIR);
                    }
                    let handle = v.anonymous_open(attrs).map_err(errno)?;
                    let object = v.open_object(handle).map_err(errno)?;
                    let mut handles = view.handles()?;
                    let fh = handles.next;
                    handles.next += 1;
                    handles.entries.insert(
                        fh,
                        Open::Write {
                            handle,
                            append: flags & libc::O_APPEND != 0,
                            readable: flags & libc::O_ACCMODE == libc::O_RDWR,
                            sync: sync_flags(flags),
                        },
                    );
                    drop(handles);
                    let ino = INodeNo(object + 2);
                    Ok((fh, view.attr(&v, ino, Some(FileHandle(fh)))?))
                })()
            });
            match result {
                Ok((fh, attr)) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::FOPEN_DIRECT_IO,
                ),
                Err(e) => reply.error(e),
            }
        });
    }
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = (|| {
            if let Some((fd, _)) = self.untracked_fd(fh)? {
                return untracked::read(&fd, offset, size);
            }
            // The map is cloned under the locks and read after them: a read
            // holds no other request (docs/trees.md, "File maps").
            let map = {
                let mut v = self.actor()?;
                // A descriptor read on direct I/O always names a lock owner; a page
                // fill never does.
                if owner.is_none() {
                    let object = ino.0.checked_sub(2).ok_or(Errno::EBADF)?;
                    match v.page_fill(object, offset, size) {
                        Err(Error::NotFound) => {}
                        filled => return filled.map_err(errno),
                    }
                }
                let handles = self.handles()?;
                match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                    Open::Read(map) => map.clone(),
                    Open::Write {
                        handle,
                        readable: true,
                        ..
                    } => v.read_open(*handle).map_err(errno)?.clone(),
                    Open::Write { .. } | Open::Untracked { .. } => return Err(Errno::EBADF),
                }
            };
            let wanted = u64::from(size).min(map.len().saturating_sub(offset));
            let mut bytes = vec![0; usize::try_from(wanted).map_err(|_| Errno::EFBIG)?];
            let read = map
                .read(offset, &mut bytes)
                .map_err(|e| Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)))?;
            bytes.truncate(read);
            Ok(bytes)
        })();
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(e),
        }
    }
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.untracked_fd(fh) {
            Ok(Some((fd, append))) => {
                // A page write lands where the kernel says; a write of an
                // `O_APPEND` open at the file's end.
                let append = append && !write_flags.contains(WriteFlags::FUSE_WRITE_CACHE);
                let _writing = self.untracked.writing();
                return match untracked::write(&fd, offset, data, append) {
                    Ok(()) => reply.written(data.len() as u32),
                    Err(e) => reply.error(e),
                };
            }
            Ok(None) => {}
            Err(e) => return reply.error(e),
        }
        // The guards drop before a durability wait is queued.
        let result = (|| {
            let mut v = self.actor()?;
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Write { handle, sync, .. }
                    if write_flags.contains(WriteFlags::FUSE_WRITE_CACHE) =>
                {
                    v.page_writeback(*handle, offset, data).map_err(errno)?;
                    Ok(sync.then_some(*handle))
                }
                Open::Write {
                    handle,
                    append,
                    sync,
                    ..
                } => {
                    v.write(*handle, offset, data, *append).map_err(errno)?;
                    Ok(sync.then_some(*handle))
                }
                _ => Err(Errno::EBADF),
            }
        })();
        let written = data.len() as u32;
        match result {
            Ok(None) => reply.written(written),
            Ok(Some(handle)) => self.session.durable_then(
                handle,
                Box::new(move |result| match result {
                    Ok(()) => reply.written(written),
                    Err(e) => reply.error(errno(e)),
                }),
            ),
            Err(e) => reply.error(e),
        }
    }
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let changes = AttrChanges {
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
        };
        if is_untracked(ino) {
            let result = (|| {
                if !self.actor()?.writable() {
                    return Err(Errno::EROFS);
                }
                let fd = fh.map(|fh| self.untracked_fd(fh)).transpose()?.flatten();
                self.untracked
                    .setattr(ino, changes, fd.as_ref().map(|(fd, _)| fd.as_ref()))
            })();
            return match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            };
        }
        let setattr = move |view: &View, admitted: Result<()>| {
            let result = admitted.and_then(|()| {
                let mut v = view.actor()?;
                view.set_attrs(&mut v, ino, changes, fh)
            });
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            }
        };
        // A path truncate is a write-open, a truncate and a close, and a
        // chmod or chown a step: new work. A utimensat builds no commit.
        if (size.is_some() && fh.is_none()) || changes.meta() {
            self.gated(setattr);
        } else {
            setattr(self, Ok(()));
        }
    }
    /// close(2) of a write-open: every record so far (the open's, and its
    /// file's metadata steps) made durable before close returns. The fsync
    /// runs off this thread; the reply comes when it ends.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let handle = match self
            .handles()
            .map(|h| h.entries.get(&fh.0).map(open_handle))
        {
            Ok(Some(handle)) => handle,
            Ok(None) => return reply.error(Errno::EBADF),
            Err(e) => return reply.error(e),
        };
        match handle {
            Some(handle) => self.session.durable_then(handle, answer_empty(reply)),
            None => reply.ok(),
        }
    }
    /// Every record so far made durable, as FSYNCDIR does; a write-open's
    /// fsync first checks the open is live.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.untracked_fd(fh) {
            Ok(Some((fd, _))) => {
                let synced = if datasync {
                    rustix::fs::fdatasync(fd.as_ref())
                } else {
                    rustix::fs::fsync(fd.as_ref())
                };
                return match synced {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(untracked::io(e)),
                };
            }
            Ok(None) => {}
            Err(e) => return reply.error(e),
        }
        let handle = match self
            .handles()
            .map(|h| h.entries.get(&fh.0).map(open_handle))
        {
            Ok(Some(handle)) => handle,
            Ok(None) => return reply.error(Errno::EBADF),
            Err(e) => return reply.error(e),
        };
        match handle {
            Some(handle) => self.session.durable_then(handle, answer_empty(reply)),
            None => self.session.durable_all_then(answer_empty(reply)),
        }
    }
    /// The directory's names: every record durable (renames, unlinks and
    /// creates included).
    fn fsyncdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // The area's directory at the same path holds the untracked names.
        let synced = self
            .actor()
            .and_then(|v| self.dir_of(&v, ino))
            .and_then(|(dir, _)| self.untracked.fsync_dir(&dir));
        if let Err(e) = synced {
            return reply.error(e);
        }
        self.session.durable_all_then(answer_empty(reply));
    }
    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.finish(fh) {
            Ok(()) => reply.ok(),
            Err(e) => {
                eprintln!(
                    "tree SaveFailed: tree={} errno={e:?}; preserve state and restart",
                    self.session.tree()
                );
                reply.error(e);
            }
        }
    }
    fn link(&self, _req: &Request, ino: INodeNo, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_owned();
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    // An untracked file links as a named tracked file does not.
                    if is_untracked(ino) {
                        return Err(Errno::EPERM);
                    }
                    let object = ino.0.checked_sub(2).ok_or(Errno::EPERM)?;
                    let mut v = view.actor()?;
                    let path = child_path(&view.path(&v, parent)?, &name)?;
                    // Existing named-object hard links retain fuser's default EPERM.
                    if !v.is_anonymous(object).map_err(errno)? {
                        return Err(Errno::EPERM);
                    }
                    v.first_link(object, &path).map_err(errno)?;
                    view.attr(&v, ino, None)
                })()
            });
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        });
    }
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let untracked = (|| {
            let v = self.actor()?;
            let (dir, up) = self.dir_of(&v, parent)?;
            let path = child_path(&dir, name)?;
            if !up && v.node(&path).is_ok() {
                return Ok(None);
            }
            if !v.writable() {
                return Err(Errno::EROFS);
            }
            self.untracked.unlink(&path).map(Some)
        })();
        match untracked {
            Ok(None) => {}
            Ok(Some(())) => return reply.ok(),
            Err(e) => return reply.error(e),
        }
        let name = name.to_owned();
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    let path = child_path(&view.path(&v, parent)?, &name)?;
                    v.unlink(&path).map_err(errno)
                })()
            });
            match result {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }
    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let untracked = self.untracked_new(parent, link_name, NewKind::Symlink, |v, path| {
            let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
            self.untracked
                .symlink(path, target, (req.uid(), req.gid()), meta_of)
        });
        if let Some(result) = untracked {
            return match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            };
        }
        let (name, target) = (link_name.to_owned(), target.to_owned());
        let owner = FileOwner {
            uid: req.uid(),
            gid: req.gid(),
        };
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                let mut v = view.actor()?;
                let path = child_path(&view.path(&v, parent)?, &name)?;
                view.make_symlink(&mut v, &path, &target, owner)
            });
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        });
    }
    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        if is_untracked(ino) {
            return match self.untracked.readlink(ino) {
                Ok(target) => reply.data(&target),
                Err(e) => reply.error(e),
            };
        }
        let result = (|| {
            let v = self.actor()?;
            let path = self.path(&v, ino)?;
            v.readlink(&path).map_err(errno)
        })();
        match result {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(e),
        }
    }
    /// A regular file's mknod is a create and a close; devices, FIFOs and
    /// sockets are refused.
    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(Errno::EPERM);
        }
        let untracked = self.untracked_new(parent, name, NewKind::File, |v, path| {
            let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
            let flags = libc::O_WRONLY | libc::O_EXCL;
            let (_, attr) =
                self.untracked
                    .create(path, flags, mode, (req.uid(), req.gid()), meta_of)?;
            Ok(attr)
        });
        if let Some(result) = untracked {
            return match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            };
        }
        let name = name.to_owned();
        let attrs = create_attrs(req, mode);
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                let mut v = view.actor()?;
                let path = child_path(&view.path(&v, parent)?, &name)?;
                if v.node(&path).is_ok() {
                    return Err(Errno::EEXIST);
                }
                let h = v.write_open(&path, Some(attrs)).map_err(errno)?;
                v.close(h).map_err(errno)?;
                view.attr_at(&v, &path, None)
            });
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        });
    }
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let untracked = self.untracked_new(parent, name, NewKind::Dir, |v, path| {
            if v.node(path).is_ok() {
                return Err(Errno::EEXIST);
            }
            let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
            self.untracked
                .mkdir(path, mode, (req.uid(), req.gid()), meta_of)
        });
        if let Some(result) = untracked {
            return match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            };
        }
        let name = name.to_owned();
        let attrs = create_attrs(req, mode);
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    let path = child_path(&view.path(&v, parent)?, &name)?;
                    v.mkdir(&path, attrs).map_err(errno)?;
                    view.attr_at(&v, &path, None)
                })()
            });
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        });
    }
    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let untracked = (|| {
            let v = self.actor()?;
            let (dir, up) = self.dir_of(&v, parent)?;
            let path = child_path(&dir, name)?;
            if !up && v.node(&path).is_ok() {
                return Ok(None);
            }
            if !v.writable() {
                return Err(Errno::EROFS);
            }
            self.untracked.rmdir(&path, false).map(Some)
        })();
        match untracked {
            Ok(None) => {}
            Ok(Some(())) => return reply.ok(),
            Err(e) => return reply.error(e),
        }
        let name = name.to_owned();
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    let path = child_path(&view.path(&v, parent)?, &name)?;
                    // Its untracked entries are its children too; its empty
                    // container goes with it.
                    if v.node(&path).is_ok_and(|node| node.meta.kind == Kind::Dir)
                        && !view.untracked.entries(&path)?.is_empty()
                    {
                        return Err(Errno::ENOTEMPTY);
                    }
                    v.rmdir(&path).map_err(errno)?;
                    view.untracked.rmdir(&path, true)
                })()
            });
            match result {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // RENAME_EXCHANGE and RENAME_WHITEOUT are not supported.
        let mode = if flags.is_empty() {
            RenameMode::Replace
        } else if flags == RenameFlags::RENAME_NOREPLACE {
            RenameMode::NoReplace
        } else {
            return reply.error(Errno::EINVAL);
        };
        let untracked = (|| {
            let v = self.actor()?;
            let (from_dir, from_up) = self.dir_of(&v, parent)?;
            let (to_dir, to_up) = self.dir_of(&v, newparent)?;
            let from = child_path(&from_dir, name)?;
            let to = child_path(&to_dir, newname)?;
            let kind = if from_up {
                None
            } else {
                v.node(&from).ok().map(|node| node.meta.kind)
            };
            let to_tracked = !to_up && v.node(&to).is_ok();
            if let Some(kind) = kind {
                // Tracked to a tracked name, or a new name the rules record.
                if !to_tracked && self.half(&v, &to, to_up, new_kind(kind))? == Half::Untracked {
                    return Err(Errno::EXDEV);
                }
                return Ok(None);
            }
            let from_kind = self.untracked.lookup(&from)?.ok_or(Errno::ENOENT)?.kind;
            if to_tracked || self.half(&v, &to, to_up, area_kind(from_kind))? == Half::Tracked {
                return Err(Errno::EXDEV);
            }
            if !v.writable() {
                return Err(Errno::EROFS);
            }
            let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
            self.untracked
                .rename(&from, &to, mode == RenameMode::NoReplace, meta_of)
                .map(Some)
        })();
        match untracked {
            Ok(None) => {}
            Ok(Some(())) => return reply.ok(),
            Err(e) => return reply.error(e),
        }
        let (name, newname) = (name.to_owned(), newname.to_owned());
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    let mut v = view.actor()?;
                    let from = child_path(&view.path(&v, parent)?, &name)?;
                    let to = child_path(&view.path(&v, newparent)?, &newname)?;
                    let dir = v.node(&from).is_ok_and(|node| node.meta.kind == Kind::Dir);
                    v.rename(&from, &to, mode).map_err(errno)?;
                    // A tracked directory's untracked children move with it.
                    if dir && from != to {
                        let meta_of = &mut |dir: &str| v.node(dir).ok().map(|n| n.meta);
                        view.untracked.follow_dir(&from, &to, meta_of)?;
                    }
                    Ok(())
                })()
            });
            match result {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.listing(ino, offset);
        match result {
            Ok(entries) => {
                for (ino, next, kind, name) in entries {
                    if reply.add(ino, next, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }
}

/// The first readdir offset of a tracked directory's untracked entries.
const AREA_OFFSETS: u64 = 1 << 62;

#[cfg(test)]
mod tests {
    use super::*;
    use agentfs_contracts::caps::{ClientCap, Minter};
    use agentfs_contracts::clients::{ClientId, Consistency, Grant, Ordinal, SessionId};
    use agentfs_contracts::name::{ClientLabel, LauncherId, VolumeName};
    use agentfs_contracts::pod::{PodContext, WriterAt};
    use agentfs_contracts::ports::{SessionAuthority, SessionBinder};
    use agentfs_contracts::volume::{VolumeId, VolumeIdentity};

    fn new_actor(dir: &Path) -> Arc<Recording> {
        let identity = VolumeIdentity {
            name: VolumeName::parse("test").unwrap(),
            id: VolumeId(uuid::Uuid::from_u128(1)),
        };
        Recording::create(dir, &identity, &PodContext::default()).unwrap()
    }

    const OWNER: FileOwner = FileOwner {
        uid: 10000,
        gid: 10001,
    };

    /// What a test's create gives a file: mode 0644, owned by [`OWNER`].
    const ATTRS: Attrs = Attrs {
        perm: Perm::from_mode(0o644),
        owner: OWNER,
    };

    /// A session bound to write the root as client `label`.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test is its own composition root"
    )]
    fn writing(sessions: &mut Sessions, label: &str) -> TreeSession {
        let client = ClientId {
            launcher: LauncherId::parse("cli").unwrap(),
            tree: TreeId::Root,
            label: ClientLabel::parse(label).unwrap(),
            n: Ordinal(1),
        };
        let writes_as = WriterAt {
            writer: client.writer(),
            at: PodContext::default(),
        };
        let id = sessions.open();
        let cap = ClientCap::mint(&Minter::new(), client, TreeId::Root, writes_as);
        sessions.bind(id, SessionAuthority::Write(cap));
        sessions.session(id).unwrap()
    }

    fn view(session: TreeSession) -> View {
        let area = session.untracked_area().unwrap();
        View {
            session,
            handles: Arc::new(Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            })),
            untracked: Arc::new(Untracked::new(area, false)),
        }
    }

    fn host(sessions: &Sessions, state: &Path) -> ViewHost {
        ViewHost::new(sessions.clone(), state, None, Arc::new(|_| {})).unwrap()
    }

    fn binding(session: SessionId, tree: TreeId) -> Binding {
        Binding {
            client: ClientId {
                launcher: LauncherId::parse("cli").unwrap(),
                tree,
                label: ClientLabel::parse("c").unwrap(),
                n: Ordinal(1),
            },
            session,
            grant: Grant::live_read_only(tree, Consistency::PerFile),
        }
    }

    #[test]
    fn session_config_allows_other_by_default() {
        let config = session_config(&MountOptions { allow_other: true }, false);
        assert_eq!(config.acl, SessionACL::All);
        assert!(
            config
                .mount_options
                .contains(&MountOption::DefaultPermissions)
        );
        assert!(config.mount_options.contains(&MountOption::RW));
        let config = session_config(&MountOptions { allow_other: false }, true);
        assert_eq!(config.acl, SessionACL::Owner);
        assert!(config.mount_options.contains(&MountOption::RO));
    }

    /// A view is refused before any mount: an unbound session, a mount point
    /// that would expose the state area, and a tree the actor has not saved.
    #[test]
    fn every_view_is_checked_before_its_mount_can_expose_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let mut sessions = Sessions::new(new_actor(&state));
        let mut host = host(&sessions, &state);
        let options = MountOptions { allow_other: true };
        let outside = tmp.path().join("valid");
        std::fs::create_dir(&outside).unwrap();
        let unbound = sessions.open();
        assert_eq!(
            host.open(
                MountPoint(outside.clone()),
                binding(unbound, TreeId::Root),
                options
            ),
            Err(ViewError::UnknownSession)
        );
        let root = sessions.open();
        sessions.bind(root, SessionAuthority::Read(TreeId::Root));
        for invalid in ["state", "state/inside", ".", "missing"] {
            let invalid = tmp.path().join(invalid);
            if !invalid.ends_with("missing") {
                std::fs::create_dir_all(&invalid).unwrap();
            }
            assert_eq!(
                host.open(MountPoint(invalid), binding(root, TreeId::Root), options),
                Err(ViewError::MountFailed(libc::EINVAL))
            );
        }
        let fork = sessions.open();
        sessions.bind(fork, SessionAuthority::Read(TreeId::Fork(9)));
        assert_eq!(
            host.open(MountPoint(outside), binding(fork, TreeId::Fork(9)), options),
            Err(ViewError::MountFailed(libc::ENOENT))
        );
        host.wait().unwrap();
    }

    #[test]
    fn anonymous_inode_attributes_are_available_without_a_file_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let handle = volume.anonymous_open(ATTRS).unwrap();
        let ino = INodeNo(volume.open_object(handle).unwrap() + 2);
        volume.write(handle, 0, b"private", false).unwrap();
        view.handles().unwrap().entries.insert(
            1,
            Open::Write {
                handle,
                append: false,
                readable: true,
                sync: false,
            },
        );
        let attr = view.attr(&volume, ino, None).unwrap();
        assert_eq!(attr.size, 7);
        assert_eq!(attr.nlink, 0);
        assert_eq!((attr.uid, attr.gid), (OWNER.uid, OWNER.gid));
    }

    #[test]
    fn a_readers_snapshot_survives_a_later_save_and_readonly_truncation_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let h = volume.write_open("a", Some(ATTRS)).unwrap();
        volume.write(h, 0, b"old", false).unwrap();
        volume.close(h).unwrap();
        let reader = view
            .open_file(&mut volume, "a", libc::O_RDONLY, None)
            .unwrap();
        let h = volume.write_open("a", None).unwrap();
        volume.truncate(h, 0).unwrap();
        volume.write(h, 0, b"new", false).unwrap();
        volume.close(h).unwrap();
        assert_eq!(volume.read("a").unwrap(), b"new");
        match &view.handles().unwrap().entries[&reader] {
            Open::Read(map) => assert_eq!(map.to_vec().unwrap(), b"old"),
            _ => panic!("read-only open acquired a writer"),
        }
        assert_eq!(
            view.open_file(&mut volume, "a", libc::O_RDONLY | libc::O_TRUNC, None),
            Err(Errno::EACCES)
        );
        assert_eq!(volume.read("a").unwrap(), b"new");
    }

    /// A file's attributes carry its recorded owner, whoever mounted.
    #[test]
    fn attr_presents_the_recorded_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let attrs = Attrs {
            perm: Perm::from_mode(0o640),
            owner: FileOwner {
                uid: 4242,
                gid: 4343,
            },
        };
        let h = volume.write_open("a", Some(attrs)).unwrap();
        volume.close(h).unwrap();
        let attr = view.attr_at(&volume, "a", None).unwrap();
        assert_eq!((attr.uid, attr.gid, attr.perm), (4242, 4343, 0o640));
    }

    /// A chgrp keeps the mode and the owner it does not name.
    #[test]
    fn setattr_with_only_gid_keeps_mode_and_uid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let h = volume.write_open("a", Some(ATTRS)).unwrap();
        volume.close(h).unwrap();
        let ino = inode(&volume.node("a").unwrap());
        let changes = AttrChanges {
            gid: Some(77),
            ..AttrChanges::default()
        };
        let attr = view.set_attrs(&mut volume, ino, changes, None).unwrap();
        assert_eq!((attr.perm, attr.uid, attr.gid), (0o644, OWNER.uid, 77));
        let meta = volume.node("a").unwrap().meta;
        assert_eq!(
            (meta.mode.bits(), meta.uid, meta.gid),
            (0o644, OWNER.uid, 77)
        );
    }

    /// `Now` is the step's time and a specific time is kept to the
    /// nanosecond; a time before the epoch is refused.
    #[test]
    fn utimens_now_and_specific_times_map_to_set_times() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let h = volume.write_open("a", Some(ATTRS)).unwrap();
        volume.close(h).unwrap();
        let ino = inode(&volume.node("a").unwrap());
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let at = UNIX_EPOCH + Duration::new(1_000_000_000, 5);
        let changes = AttrChanges {
            atime: Some(TimeOrNow::Now),
            mtime: Some(TimeOrNow::SpecificTime(at)),
            ..AttrChanges::default()
        };
        let attr = view.set_attrs(&mut volume, ino, changes, None).unwrap();
        assert_eq!(attr.mtime, at);
        let times = volume.node("a").unwrap().times;
        assert_eq!(times.mtime_ns, 1_000_000_000_000_000_005);
        assert!(u128::from(times.atime_ns) >= before, "{times:?}");
        assert_eq!(set_time(TimeOrNow::Now), Ok(SetTime::Now));
        let early = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(set_time(TimeOrNow::SpecificTime(early)), Err(Errno::EINVAL));
    }

    /// A symlink's reply is a link whose size is its target's length, and
    /// its target reads back.
    #[test]
    fn a_symlink_reply_carries_kind_symlink_and_its_target_length() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let mut volume = view.actor().unwrap();
        let attr = view
            .make_symlink(&mut volume, "l", Path::new("dir/target"), OWNER)
            .unwrap();
        assert_eq!(attr.kind, FileType::Symlink);
        assert_eq!(attr.size, 10);
        assert_eq!(
            (attr.uid, attr.gid, attr.perm),
            (OWNER.uid, OWNER.gid, 0o777)
        );
        assert_eq!(volume.readlink("l").unwrap(), b"dir/target");
        assert_eq!(
            view.open_file(&mut volume, "l", libc::O_RDONLY, None),
            Err(Errno::ELOOP)
        );
    }

    /// A tracked entry wins its name: the area's entry under it stays hidden,
    /// and the area's other entries follow the tracked children at their own
    /// offsets, from which a listing resumes.
    #[test]
    fn a_listing_merges_the_area_and_a_tracked_entry_hides_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let view = view(writing(&mut sessions, "client"));
        let area = view.session.untracked_area().unwrap();
        for name in ["hidden", "z-area"] {
            std::fs::write(area.dir().path().join(name), b"area").unwrap();
        }
        let tracked = {
            let mut volume = view.actor().unwrap();
            let h = volume.write_open("hidden", Some(ATTRS)).unwrap();
            volume.close(h).unwrap();
            inode(&volume.node("hidden").unwrap())
        };
        let listing = view.listing(INodeNo(1), 0).unwrap();
        let names: Vec<(&str, bool)> = listing
            .iter()
            .map(|(ino, _, _, name)| (name.as_str(), is_untracked(*ino)))
            .collect();
        assert_eq!(
            names,
            [
                (".", false),
                ("..", false),
                ("hidden", false),
                ("z-area", true)
            ]
        );
        assert_eq!(listing[2].0, tracked);
        let (_, next, kind, _) = &listing[3];
        assert!(*next > AREA_OFFSETS, "{next}");
        assert_eq!(*kind, FileType::RegularFile);
        assert!(view.listing(INodeNo(1), *next).unwrap().is_empty());
        let resumed = view.listing(INodeNo(1), 3).unwrap();
        assert_eq!(resumed.len(), 1, "{resumed:?}");
        assert_eq!(resumed[0].3, "z-area");
        // Looked up, the hidden name is the tracked file.
        let v = view.actor().unwrap();
        assert_eq!(
            view.half(&v, "hidden", false, NewKind::File).unwrap(),
            Half::Tracked
        );
        assert_eq!(
            view.half(&v, "z-area", false, NewKind::File).unwrap(),
            Half::Untracked
        );
    }

    /// New names go where the rules and the built-ins place them.
    #[test]
    fn new_names_go_where_the_rules_place_them() {
        let tmp = tempfile::tempdir().unwrap();
        drop(new_actor(tmp.path()));
        let info = tmp.path().join("history.git/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), "*.log\n").unwrap();
        std::fs::write(info.join("attributes"), "*.db -agentfs\n").unwrap();
        let recording = Recording::open(tmp.path(), &PodContext::default()).unwrap();
        let mut sessions = Sessions::new(recording);
        let view = view(writing(&mut sessions, "client"));
        let area = view.session.untracked_area().unwrap();
        let v = view.actor().unwrap();
        let half = |path: &str, kind| view.half(&v, path, false, kind).unwrap();
        assert_eq!(half("a.log", NewKind::File), Half::Untracked);
        assert_eq!(half("s.db", NewKind::File), Half::Untracked);
        assert_eq!(half(".git", NewKind::Dir), Half::Untracked);
        assert_eq!(half("notes.md", NewKind::File), Half::Tracked);
        assert_eq!(half("repo", NewKind::Dir), Half::Tracked);
        // Inside a nested repository: the area holds `repo/.git`.
        std::fs::create_dir_all(area.dir().path().join("repo/.git")).unwrap();
        assert_eq!(half("repo/x", NewKind::File), Half::Untracked);
        assert_eq!(
            view.half(&v, "x", true, NewKind::File).unwrap(),
            Half::Untracked
        );
    }

    /// A read-only client's view answers `EROFS` to every write, whatever the
    /// kernel lets through.
    #[test]
    fn a_read_only_view_refuses_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sessions = Sessions::new(new_actor(tmp.path()));
        let id = sessions.open();
        sessions.bind(id, SessionAuthority::Read(TreeId::Root));
        let view = view(sessions.session(id).unwrap());
        let mut volume = view.actor().unwrap();
        for flags in [libc::O_WRONLY, libc::O_RDWR, libc::O_RDONLY] {
            assert_eq!(
                view.open_file(&mut volume, "a", flags, Some(ATTRS)),
                Err(Errno::EROFS)
            );
        }
        assert_eq!(
            volume.anonymous_open(ATTRS).map_err(errno),
            Err(Errno::EROFS)
        );
    }
}
