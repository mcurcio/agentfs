//! Flat-file view of the target actor (git-model.md).
//! Direct I/O and zero TTLs; no passthrough or writeback-cache negotiation.
//! Shared mappings go through the kernel's page cache all the same, so a read
//! with no lock owner is a page fill and a write with `FUSE_WRITE_CACHE` a page
//! write; `agentfs_core::tree_actor`'s `pages` module holds their rules (R1.4).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::pod::WriterAt;
use agentfs_contracts::views::MountOptions;
use agentfs_core::tree_actor::{Error, Recording, Saved, TreeActor, TreeId};
use fuser::*;

const TTL: Duration = Duration::ZERO;
/// Files listed per READDIR at most: more than a reply's buffer holds, and the
/// kernel asks again from where a reply stopped.
const READDIR_BATCH: usize = 1024;
type Result<T> = std::result::Result<T, Errno>;

/// Views in one process share one locked volume actor. Each view writes as
/// `by`, and presents every file as `owner`'s.
pub struct Binding {
    pub tree: TreeId,
    pub by: WriterAt,
    pub owner: FileOwner,
    pub mountpoint: PathBuf,
}

/// Mount until every view is unmounted. All mountpoints must be empty,
/// outside the private state and separate from one another.
pub fn serve(
    state: &Path,
    recording: Arc<Recording>,
    bindings: Vec<Binding>,
    options: MountOptions,
) -> io::Result<()> {
    serve_with_metrics(state, recording, bindings, options, None)
}

/// Serve views with optional process-local measurement snapshots. The output
/// belongs outside the state area and views; no measurement affects decisions.
pub fn serve_with_metrics(
    state: &Path,
    actor: Arc<Recording>,
    bindings: Vec<Binding>,
    options: MountOptions,
    metrics_output: Option<&Path>,
) -> io::Result<()> {
    let state = state.canonicalize()?;
    let mut points: Vec<PathBuf> = Vec::new();
    for binding in &bindings {
        let at = binding.mountpoint.canonicalize()?;
        if !at.is_dir()
            || std::fs::read_dir(&at)?.next().is_some()
            || at.starts_with(&state)
            || state.starts_with(&at)
            || points
                .iter()
                .any(|p| at.starts_with(p) || p.starts_with(&at))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trees mounts must be empty directories, separate from state and each other",
            ));
        }
        actor
            .actor()
            .map_err(io::Error::other)?
            .head(binding.tree)
            .map_err(io::Error::other)?;
        points.push(at);
    }
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
            if path.starts_with(&state) || points.iter().any(|at| path.starts_with(at)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "metrics output must be outside state and views",
                ));
            }
            Ok(path)
        })
        .transpose()?;
    let signal = Arc::new(ProgressSignal::default());
    let mut sessions = Vec::new();
    let mut notifiers: Vec<(TreeId, Notifier)> = Vec::new();
    for (binding, at) in bindings.into_iter().zip(points) {
        let tree = binding.tree;
        let view = View {
            actor: actor.clone(),
            signal: Arc::clone(&signal),
            tree: binding.tree,
            by: binding.by,
            owner: binding.owner,
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let session = fuser::spawn_mount(view, at, &session_config(&options))?;
        notifiers.push((tree, session.notifier()));
        sessions.push(session);
    }
    let progress_actor = Arc::clone(&actor);
    let progress_signal = Arc::clone(&signal);
    let progress_worker = std::thread::spawn(move || -> io::Result<()> {
        #[cfg(feature = "failpoints")]
        if let Some(gate) = agentfs_core::failpoints::take_gate() {
            return run_take_gate(gate, &progress_actor, &notifiers);
        }
        loop {
            {
                let mut state = progress_signal
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("queue signal poisoned"))?;
                while !state.pending && !state.stopped {
                    state = progress_signal
                        .changed
                        .wait(state)
                        .map_err(|_| io::Error::other("queue signal poisoned"))?;
                }
                if state.stopped {
                    return Ok(());
                }
                state.pending = false;
            }
            loop {
                let (progressed, changed) = {
                    let mut volume = progress_actor.actor().map_err(io::Error::other)?;
                    let progressed = volume.progress().map_err(io::Error::other)?;
                    (progressed, volume.drain_changed())
                };
                // With the actor unlocked: invalidation can wait on a page fill.
                invalidate_changed(&notifiers, &changed);
                if !progressed {
                    break;
                }
                // Release the actor between takes so new client opens can run.
                std::thread::yield_now();
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let metrics_worker = metrics_output.map(|output| {
        let actor = Arc::clone(&actor);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || -> io::Result<()> {
            loop {
                let last = stop.load(Ordering::Acquire);
                let metrics = actor.actor().map_err(io::Error::other)?.metrics();
                let bytes = serde_json::to_vec(&metrics)?;
                let temporary =
                    output.with_extension(format!("metrics-{}.tmp", std::process::id()));
                std::fs::write(&temporary, bytes)?;
                std::fs::rename(&temporary, &output)?;
                if last {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    });
    let result = sessions.into_iter().try_for_each(|session| session.join());
    {
        let mut state = signal
            .state
            .lock()
            .map_err(|_| io::Error::other("queue signal poisoned"))?;
        state.stopped = true;
        signal.changed.notify_one();
    }
    stop.store(true, Ordering::Release);
    let progress_result = progress_worker
        .join()
        .map_err(|_| io::Error::other("queue worker panicked"))?;
    let metrics_result = metrics_worker
        .map(|worker| {
            worker
                .join()
                .map_err(|_| io::Error::other("metrics worker panicked"))?
        })
        .transpose();
    result?;
    progress_result?;
    metrics_result?;
    // A failed journal fsync stops the actor off this thread: serving ends with it.
    drop(actor.actor().map_err(io::Error::other)?);
    Ok(())
}

/// A test's takes in place of the worker's: one at each line (failpoints'
/// `TakeGate`), the actor locked only for that take.
#[cfg(feature = "failpoints")]
fn run_take_gate(
    mut gate: agentfs_core::failpoints::TakeGate,
    actor: &Recording,
    notifiers: &[(TreeId, Notifier)],
) -> io::Result<()> {
    while let Some(tree) = gate.next_tree()? {
        let (took, changed) = {
            let mut volume = actor.actor().map_err(io::Error::other)?;
            let took = tree
                .parse::<TreeId>()
                .map_err(|e| e.to_string())
                .and_then(|tree| volume.take_next(tree).map_err(|e| e.to_string()));
            (took, volume.drain_changed())
        };
        invalidate_changed(notifiers, &changed);
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
/// the kernel, no devices, no set-user-ID.
fn session_config(options: &MountOptions) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("agentfs-trees".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::RW,
    ];
    config.acl = if options.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config.n_threads = Some(1);
    config
}

struct View {
    actor: Arc<Recording>,
    signal: Arc<ProgressSignal>,
    tree: TreeId,
    by: WriterAt,
    /// Every file's and directory's owner, until file metadata is recorded.
    owner: FileOwner,
    handles: Mutex<Handles>,
}

struct ProgressState {
    pending: bool,
    stopped: bool,
}

struct ProgressSignal {
    state: Mutex<ProgressState>,
    changed: Condvar,
}

impl Default for ProgressSignal {
    fn default() -> Self {
        Self {
            state: Mutex::new(ProgressState {
                pending: true,
                stopped: false,
            }),
            changed: Condvar::new(),
        }
    }
}

struct ActorGuard<'a> {
    volume: MutexGuard<'a, TreeActor>,
    signal: &'a ProgressSignal,
    modified: bool,
}

impl Deref for ActorGuard<'_> {
    type Target = TreeActor;
    fn deref(&self) -> &TreeActor {
        &self.volume
    }
}

impl DerefMut for ActorGuard<'_> {
    fn deref_mut(&mut self) -> &mut TreeActor {
        self.modified = true;
        &mut self.volume
    }
}

impl Drop for ActorGuard<'_> {
    fn drop(&mut self) {
        if self.modified
            && let Ok(mut state) = self.signal.state.lock()
        {
            state.pending = true;
            self.signal.changed.notify_one();
        }
    }
}
struct Handles {
    next: u64,
    entries: BTreeMap<u64, Open>,
}
enum Open {
    Read(Vec<u8>),
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
    fn actor(&self) -> Result<ActorGuard<'_>> {
        let v = self.actor.actor().map_err(|_| Errno::EIO)?;
        Ok(ActorGuard {
            volume: v,
            signal: &self.signal,
            modified: false,
        })
    }
    fn handles(&self) -> Result<MutexGuard<'_, Handles>> {
        self.handles.lock().map_err(|_| Errno::EIO)
    }
    fn path(&self, v: &TreeActor, ino: INodeNo) -> Result<String> {
        let object = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
        v.path_of(self.tree, object)
            .map(str::to_owned)
            .map_err(errno)
    }
    fn attr(&self, v: &TreeActor, ino: INodeNo, fh: Option<FileHandle>) -> Result<FileAttr> {
        // First: the kernel keeps one size per inode, and an unlinked mapped
        // file's fstat has no path.
        let open_size = ino
            .0
            .checked_sub(2)
            .and_then(|object| v.inode_size(self.tree, object));
        let size = if ino.0 == 1 {
            0
        } else if let Some(size) = open_size {
            size as usize
        } else if let Some(fh) = fh {
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.len(),
                Open::Write { handle, .. } => v.read_open(*handle).map_err(errno)?.len(),
            }
        } else if v.is_anonymous(self.tree, ino.0 - 2).map_err(errno)? {
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
            v.read(self.tree, &self.path(v, ino)?).map_err(errno)?.len()
        };
        let dir = ino.0 == 1;
        Ok(FileAttr {
            ino,
            size: size as u64,
            blocks: (size as u64).div_ceil(512),
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
            } else if v.is_anonymous(self.tree, ino.0 - 2).map_err(errno)? {
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
    fn open_file(&self, v: &mut TreeActor, path: &str, flags: i32, create: bool) -> Result<u64> {
        let mut handles = self.handles()?;
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !write {
            return Err(Errno::EACCES);
        }
        let entry = if write {
            let handle = if flags & libc::O_TRUNC != 0 {
                v.write_open_truncating(self.tree, path, &self.by, create)
            } else {
                v.write_open(self.tree, path, &self.by, create)
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
                let h = v
                    .write_open(self.tree, path, &self.by, true)
                    .map_err(errno)?;
                v.close(h).map_err(errno)?;
            }
            Open::Read(v.read(self.tree, path).map_err(errno)?.to_vec())
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
            eprintln!("tree OrphanDropped: tree={} open={handle}", self.tree);
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
            let object = v.object_at(self.tree, name).map_err(errno)?;
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
        let result = (|| {
            let mut v = self.actor()?;
            let path = self.path(&v, ino)?;
            self.open_file(&mut v, &path, flags.0, false)
        })();
        match result {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
            Err(e) => reply.error(e),
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
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let path = name.to_str().ok_or(Errno::EINVAL)?;
            let mut v = self.actor()?;
            if flags & libc::O_EXCL != 0 && v.read(self.tree, path).is_ok() {
                return Err(Errno::EEXIST);
            }
            let fh = self.open_file(&mut v, path, flags, true)?;
            let object = v.object_at(self.tree, path).map_err(errno)?;
            Ok((
                fh,
                self.attr(&v, INodeNo(object + 2), Some(FileHandle(fh)))?,
            ))
        })();
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
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let mut v = self.actor()?;
            let handle = v.anonymous_open(self.tree, &self.by).map_err(errno)?;
            let object = v.open_object(handle).map_err(errno)?;
            let mut handles = self.handles()?;
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
            Ok((fh, self.attr(&v, ino, Some(FileHandle(fh)))?))
        })();
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
            let mut v = self.actor()?;
            // A descriptor read on direct I/O always names a lock owner; a page
            // fill never does. Through `v.volume`: a fill changes nothing a take
            // waits for, so it does not wake the queue worker.
            if owner.is_none() {
                let object = ino.0.checked_sub(2).ok_or(Errno::EBADF)?;
                match v.volume.page_fill(self.tree, object, offset, size) {
                    Err(Error::NotFound) => {}
                    filled => return filled.map_err(errno),
                }
            }
            let handles = self.handles()?;
            let bytes = match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.as_slice(),
                Open::Write {
                    handle,
                    readable: true,
                    ..
                } => v.read_open(*handle).map_err(errno)?,
                _ => return Err(Errno::EBADF),
            };
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            Ok(bytes[start..start.saturating_add(size as usize).min(bytes.len())].to_vec())
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
            Ok(Some(handle)) => self.actor.durable_then(
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
        let result = (|| {
            if mode.is_some()
                || uid.is_some()
                || gid.is_some()
                || atime.is_some()
                || mtime.is_some()
            {
                return Err(Errno::EOPNOTSUPP);
            }
            let mut v = self.actor()?;
            if let Some(size) = size {
                if let Some(fh) = fh {
                    let handles = self.handles()?;
                    match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                        Open::Write { handle, .. } => v.truncate(*handle, size).map_err(errno)?,
                        _ => return Err(Errno::EBADF),
                    }
                } else {
                    let path = self.path(&v, ino)?;
                    let h = v
                        .write_open(self.tree, &path, &self.by, false)
                        .map_err(errno)?;
                    let result = v.truncate(h, size).map_err(errno);
                    v.close(h).map_err(errno)?;
                    result?;
                }
            }
            self.attr(&v, ino, fh)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
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
            Some(handle) => self.actor.durable_then(handle, answer_empty(reply)),
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
            Some(handle) => self.actor.durable_then(handle, answer_empty(reply)),
            None => self.actor.durable_all_then(answer_empty(reply)),
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
        self.actor.durable_all_then(answer_empty(reply));
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
                    self.tree
                );
                reply.error(e);
            }
        }
    }
    fn link(&self, _req: &Request, ino: INodeNo, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let object = ino.0.checked_sub(2).ok_or(Errno::ENOENT)?;
            let mut v = self.actor()?;
            // Existing named-object hard links retain fuser's default EPERM.
            if !v.is_anonymous(self.tree, object).map_err(errno)? {
                return Err(Errno::EPERM);
            }
            v.first_link(
                self.tree,
                object,
                name.to_str().ok_or(Errno::EINVAL)?,
                &self.by,
            )
            .map_err(errno)?;
            self.attr(&v, ino, None)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            self.actor()?
                .unlink(self.tree, name.to_str().ok_or(Errno::EINVAL)?, &self.by)
                .map_err(errno)
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
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
        let result = (|| {
            if parent.0 != 1 || newparent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            if !flags.is_empty() {
                return Err(Errno::EOPNOTSUPP);
            }
            self.actor()?
                .rename(
                    self.tree,
                    name.to_str().ok_or(Errno::EINVAL)?,
                    newname.to_str().ok_or(Errno::EINVAL)?,
                    &self.by,
                )
                .map_err(errno)
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
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
                v.files_from(self.tree, skip.saturating_sub(2), READDIR_BATCH)
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
    use agentfs_contracts::name::{VolumeName, WriterLabel};
    use agentfs_contracts::pod::PodContext;
    use agentfs_contracts::volume::{VolumeId, VolumeIdentity};

    fn new_actor(dir: &Path) -> Arc<Recording> {
        let identity = VolumeIdentity {
            name: VolumeName::parse("test").unwrap(),
            id: VolumeId(uuid::Uuid::from_u128(1)),
        };
        Recording::create(dir, &identity, &PodContext::default()).unwrap()
    }

    fn writer(label: &str) -> WriterAt {
        WriterAt {
            writer: WriterLabel::parse(label).unwrap(),
            at: PodContext::default(),
        }
    }

    const OWNER: FileOwner = FileOwner {
        uid: 10000,
        gid: 10001,
    };

    #[test]
    fn session_config_allows_other_by_default() {
        let config = session_config(&MountOptions { allow_other: true });
        assert_eq!(config.acl, SessionACL::All);
        assert!(
            config
                .mount_options
                .contains(&MountOption::DefaultPermissions)
        );
        let config = session_config(&MountOptions { allow_other: false });
        assert_eq!(config.acl, SessionACL::Owner);
    }

    #[test]
    fn all_bindings_are_checked_before_any_mount_can_expose_state() {
        for invalid in ["state", "state/inside", "."] {
            let tmp = tempfile::tempdir().unwrap();
            let state = tmp.path().join("state");
            let volume = new_actor(&state);
            let outside = tmp.path().join("valid");
            std::fs::create_dir(&outside).unwrap();
            let invalid = tmp.path().join(invalid);
            std::fs::create_dir_all(&invalid).unwrap();
            let bindings = vec![
                Binding {
                    tree: TreeId::Root,
                    by: writer("root-client"),
                    owner: OWNER,
                    mountpoint: outside,
                },
                Binding {
                    tree: TreeId::Root,
                    by: writer("other"),
                    owner: OWNER,
                    mountpoint: invalid,
                },
            ];
            assert_eq!(
                serve(&state, volume, bindings, MountOptions { allow_other: true })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn anonymous_inode_attributes_are_available_without_a_file_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let view = View {
            actor: new_actor(tmp.path()),
            signal: Arc::new(ProgressSignal::default()),
            tree: TreeId::Root,
            by: writer("client"),
            owner: OWNER,
            handles: Mutex::new(Handles {
                next: 2,
                entries: BTreeMap::new(),
            }),
        };
        let mut volume = view.actor().unwrap();
        let handle = volume
            .anonymous_open(TreeId::Root, &writer("client"))
            .unwrap();
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
        let view = View {
            actor: new_actor(tmp.path()),
            signal: Arc::new(ProgressSignal::default()),
            tree: TreeId::Root,
            by: writer("client"),
            owner: OWNER,
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let mut volume = view.actor().unwrap();
        let h = volume
            .write_open(TreeId::Root, "a", &writer("seed"), true)
            .unwrap();
        volume.write(h, 0, b"old", false).unwrap();
        volume.close(h).unwrap();
        let reader = view
            .open_file(&mut volume, "a", libc::O_RDONLY, false)
            .unwrap();
        let h = volume
            .write_open(TreeId::Root, "a", &writer("next"), false)
            .unwrap();
        volume.truncate(h, 0).unwrap();
        volume.write(h, 0, b"new", false).unwrap();
        volume.close(h).unwrap();
        assert_eq!(volume.read(TreeId::Root, "a").unwrap(), b"new");
        match &view.handles().unwrap().entries[&reader] {
            Open::Read(bytes) => assert_eq!(bytes, b"old"),
            _ => panic!("read-only open acquired a writer"),
        }
        assert_eq!(
            view.open_file(&mut volume, "a", libc::O_RDONLY | libc::O_TRUNC, false),
            Err(Errno::EACCES)
        );
        assert_eq!(volume.read(TreeId::Root, "a").unwrap(), b"new");
    }
}
