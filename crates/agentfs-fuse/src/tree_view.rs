//! The view host (docs/library/filesystem.md, "The view host"): each view is
//! a flat-file FUSE mount of one tree, reached only through its client's
//! session (`agentfs_core::session`), so its calls name no tree and no
//! writer. Direct I/O and zero TTLs; no passthrough or writeback-cache
//! negotiation. Shared mappings go through the kernel's page cache all the
//! same, so a read with no lock owner is a page fill and a write with
//! `FUSE_WRITE_CACHE` a page write; `agentfs_core::tree_actor`'s `pages`
//! module holds their rules (R1.4).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_contracts::clients::{Access, Binding, FileOwner};
use agentfs_contracts::views::{MountOptions, MountPoint, ViewError, ViewId, Views};
use agentfs_core::session::{SessionGuard, Sessions, TreeSession};
use agentfs_core::tree_actor::{Error, FileMap, Progress, Recording, Saved, TreeId};
use fuser::*;

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
    /// Mounts `binding`'s session's tree at `at`, every file presented as its
    /// grant's `acts_as`, read-only for a read-only grant. Never waits for
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
        let view = View {
            session,
            owner: binding.grant.acts_as(),
            handles: Arc::new(Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            })),
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
/// never a timer.
fn take_worker(
    recording: &Recording,
    views: &OpenViews,
    stop: &AtomicBool,
    notify: &(dyn Fn(HostEvent) + Send + Sync),
) -> io::Result<()> {
    #[cfg(feature = "failpoints")]
    if let Some(gate) = agentfs_core::failpoints::take_gate() {
        return run_take_gate(gate, recording, views);
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
    /// Every file's and directory's owner, until file metadata is recorded.
    owner: FileOwner,
    handles: Arc<Mutex<Handles>>,
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
}

/// A write-open's actor handle; `None` for a read-only open.
fn open_handle(open: &Open) -> Option<u64> {
    match open {
        Open::Write { handle, .. } => Some(*handle),
        Open::Read(_) => None,
    }
}

fn errno(error: Error) -> Errno {
    Errno::from_i32(error.errno())
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
    fn path(&self, v: &SessionGuard<'_>, ino: INodeNo) -> Result<String> {
        let object = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
        v.path_of(object).map(str::to_owned).map_err(errno)
    }
    fn attr(&self, v: &SessionGuard<'_>, ino: INodeNo, fh: Option<FileHandle>) -> Result<FileAttr> {
        // First: the kernel keeps one size per inode, and an unlinked mapped
        // file's fstat has no path.
        let open_size = ino.0.checked_sub(2).and_then(|object| v.inode_size(object));
        let size: u64 = if ino.0 == 1 {
            0
        } else if let Some(size) = open_size {
            size
        } else if let Some(fh) = fh {
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.len(),
                Open::Write { handle, .. } => v.read_open(*handle).map_err(errno)?.len(),
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
            v.file_len(&self.path(v, ino)?).map_err(errno)?
        };
        let dir = ino.0 == 1;
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if dir { 0o700 } else { 0o600 },
            nlink: if dir {
                2
            } else if v.is_anonymous(ino.0 - 2).map_err(errno)? {
                0
            } else {
                1
            },
            uid: self.owner.uid,
            gid: self.owner.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn open_file(
        &self,
        v: &mut SessionGuard<'_>,
        path: &str,
        flags: i32,
        create: bool,
    ) -> Result<u64> {
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
            if create {
                let h = v.write_open(path, true).map_err(errno)?;
                v.close(h).map_err(errno)?;
            }
            Open::Read(v.file_map(path).map_err(errno)?)
        };
        let fh = handles.next;
        handles.next += 1;
        handles.entries.insert(fh, entry);
        Ok(fh)
    }
    fn finish(&self, fh: FileHandle) -> Result<()> {
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
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let name = name.to_str().ok_or(Errno::EINVAL)?;
            let v = self.actor()?;
            let object = v.object_at(name).map_err(errno)?;
            self.attr(&v, INodeNo(object + 2), None)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.actor().and_then(|v| self.attr(&v, ino, fh)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let open = move |view: &View, admitted: Result<()>| {
            let result = admitted.and_then(|()| {
                let mut v = view.actor()?;
                let path = view.path(&v, ino)?;
                view.open_file(&mut v, &path, flags.0, false)
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
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name.to_owned();
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    if parent.0 != 1 {
                        return Err(Errno::ENOTDIR);
                    }
                    let path = name.to_str().ok_or(Errno::EINVAL)?;
                    let mut v = view.actor()?;
                    if flags & libc::O_EXCL != 0 && v.read(path).is_ok() {
                        return Err(Errno::EEXIST);
                    }
                    let fh = view.open_file(&mut v, path, flags, true)?;
                    let object = v.object_at(path).map_err(errno)?;
                    Ok((
                        fh,
                        view.attr(&v, INodeNo(object + 2), Some(FileHandle(fh)))?,
                    ))
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
        _req: &Request,
        parent: INodeNo,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    if parent.0 != 1 {
                        return Err(Errno::ENOTDIR);
                    }
                    let mut v = view.actor()?;
                    let handle = v.anonymous_open().map_err(errno)?;
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
                    _ => return Err(Errno::EBADF),
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
        let setattr = move |view: &View, admitted: Result<()>| {
            let result = admitted.and_then(|()| {
                (|| {
                    if mode.is_some()
                        || uid.is_some()
                        || gid.is_some()
                        || atime.is_some()
                        || mtime.is_some()
                    {
                        return Err(Errno::EOPNOTSUPP);
                    }
                    let mut v = view.actor()?;
                    if let Some(size) = size {
                        if let Some(fh) = fh {
                            let handles = view.handles()?;
                            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                                Open::Write { handle, .. } => {
                                    v.truncate(*handle, size).map_err(errno)?
                                }
                                _ => return Err(Errno::EBADF),
                            }
                        } else {
                            let path = view.path(&v, ino)?;
                            let h = v.write_open(&path, false).map_err(errno)?;
                            let result = v.truncate(h, size).map_err(errno);
                            v.close(h).map_err(errno)?;
                            result?;
                        }
                    }
                    view.attr(&v, ino, fh)
                })()
            });
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            }
        };
        // A path truncate is a write-open, a truncate and a close: new work.
        if size.is_some() && fh.is_none() {
            self.gated(setattr);
        } else {
            setattr(self, Ok(()));
        }
    }
    /// close(2): the open's records made durable before close returns. The
    /// fsync runs off this thread; the reply comes when it ends.
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
    /// A write-open's records made durable; a read-only descriptor's fsync
    /// makes every record durable, as FSYNCDIR does.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
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
            None => self.session.durable_all_then(answer_empty(reply)),
        }
    }
    /// The directory's names: every record durable (renames, unlinks and
    /// creates included).
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
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
                    if parent.0 != 1 {
                        return Err(Errno::ENOTDIR);
                    }
                    let object = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
                    let mut v = view.actor()?;
                    // Existing named-object hard links retain fuser's default EPERM.
                    if !v.is_anonymous(object).map_err(errno)? {
                        return Err(Errno::EPERM);
                    }
                    v.first_link(object, name.to_str().ok_or(Errno::EINVAL)?)
                        .map_err(errno)?;
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
        let name = name.to_owned();
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    if parent.0 != 1 {
                        return Err(Errno::ENOTDIR);
                    }
                    view.actor()?
                        .unlink(name.to_str().ok_or(Errno::EINVAL)?)
                        .map_err(errno)
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
        let (name, newname) = (name.to_owned(), newname.to_owned());
        self.gated(move |view, admitted| {
            let result = admitted.and_then(|()| {
                (|| {
                    if parent.0 != 1 || newparent.0 != 1 {
                        return Err(Errno::ENOTDIR);
                    }
                    if !flags.is_empty() {
                        return Err(Errno::EOPNOTSUPP);
                    }
                    view.actor()?
                        .rename(
                            name.to_str().ok_or(Errno::EINVAL)?,
                            newname.to_str().ok_or(Errno::EINVAL)?,
                        )
                        .map_err(errno)
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
        let result = (|| {
            if ino.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let v = self.actor()?;
            // `.` and `..` are entries 0 and 1, the files 2 on: an offset is
            // the number of entries already read.
            let skip = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            let mut entries: Vec<(INodeNo, FileType, String)> = [".", ".."]
                .into_iter()
                .skip(skip)
                .map(|name| (INodeNo(1), FileType::Directory, name.to_owned()))
                .collect();
            entries.extend(
                v.files_from(skip.saturating_sub(2), READDIR_BATCH)
                    .map_err(errno)?
                    .into_iter()
                    .map(|(p, o)| (INodeNo(o + 2), FileType::RegularFile, p)),
            );
            Ok((skip, entries))
        })();
        match result {
            Ok((skip, entries)) => {
                for (i, (ino, kind, name)) in entries.into_iter().enumerate() {
                    if reply.add(ino, (skip + i + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }
}

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
        View {
            session,
            owner: OWNER,
            handles: Arc::new(Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            })),
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
            grant: Grant::live_read_only(tree, Consistency::PerFile, OWNER),
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
        let handle = volume.anonymous_open().unwrap();
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
        let h = volume.write_open("a", true).unwrap();
        volume.write(h, 0, b"old", false).unwrap();
        volume.close(h).unwrap();
        let reader = view
            .open_file(&mut volume, "a", libc::O_RDONLY, false)
            .unwrap();
        let h = volume.write_open("a", false).unwrap();
        volume.truncate(h, 0).unwrap();
        volume.write(h, 0, b"new", false).unwrap();
        volume.close(h).unwrap();
        assert_eq!(volume.read("a").unwrap(), b"new");
        match &view.handles().unwrap().entries[&reader] {
            Open::Read(map) => assert_eq!(map.to_vec().unwrap(), b"old"),
            _ => panic!("read-only open acquired a writer"),
        }
        assert_eq!(
            view.open_file(&mut volume, "a", libc::O_RDONLY | libc::O_TRUNC, false),
            Err(Errno::EACCES)
        );
        assert_eq!(volume.read("a").unwrap(), b"new");
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
                view.open_file(&mut volume, "a", flags, true),
                Err(Errno::EROFS)
            );
        }
        assert_eq!(volume.anonymous_open().map_err(errno), Err(Errno::EROFS));
    }
}
