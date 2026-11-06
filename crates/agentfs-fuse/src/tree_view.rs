//! Phase 1 flat-file view of the target actor (git-model §3/§7/§18).
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

use agentfs_core::tree_actor::{Error, Saved, Volume};
use fuser::*;

const TTL: Duration = Duration::ZERO;
type Result<T> = std::result::Result<T, Errno>;

/// Views in one process share one locked volume actor.
pub struct Binding {
    pub tree: String,
    pub writer: String,
    pub mountpoint: PathBuf,
}

/// Mount until every view is unmounted. All mountpoints must be empty,
/// outside the private state and separate from one another.
pub fn serve(state: &Path, volume: Volume, bindings: Vec<Binding>) -> io::Result<()> {
    serve_with_metrics(state, volume, bindings, None)
}

/// Serve views with optional process-local measurement snapshots. The output
/// belongs outside the state area and views; no measurement affects decisions.
pub fn serve_with_metrics(
    state: &Path,
    volume: Volume,
    bindings: Vec<Binding>,
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
        volume.head(&binding.tree).map_err(io::Error::other)?;
        if binding.writer.is_empty()
            || binding.writer.chars().any(char::is_control)
            || binding.writer.len() > 256
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid tree writer",
            ));
        }
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
    let actor = Arc::new(Mutex::new(volume));
    let signal = Arc::new(ProgressSignal::default());
    let mut sessions = Vec::new();
    let mut notifiers: Vec<(String, Notifier)> = Vec::new();
    for (binding, at) in bindings.into_iter().zip(points) {
        let tree = binding.tree.clone();
        let view = View {
            actor: actor.clone(),
            signal: Arc::clone(&signal),
            tree: binding.tree,
            writer: binding.writer,
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("agentfs-trees".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::RW,
        ];
        config.acl = SessionACL::Owner;
        config.n_threads = Some(1);
        let session = fuser::spawn_mount(view, at, &config)?;
        notifiers.push((tree, session.notifier()));
        sessions.push(session);
    }
    let progress_actor = Arc::clone(&actor);
    let progress_signal = Arc::clone(&signal);
    let progress_worker = std::thread::spawn(move || -> io::Result<()> {
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
                    let mut volume = progress_actor
                        .lock()
                        .map_err(|_| io::Error::other("tree actor poisoned"))?;
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
                let metrics = actor
                    .lock()
                    .map_err(|_| io::Error::other("tree actor poisoned"))?
                    .metrics();
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
    Ok(())
}

/// Drops the kernel's pages of files a take changed. A file holds no dirty page
/// then, because takes wait for write-opens. A failure leaves old pages in
/// mappings and stops nothing: fills read the new bytes once they go.
fn invalidate_changed(notifiers: &[(String, Notifier)], changed: &[(String, u64)]) {
    for (tree, object) in changed {
        for (_, notifier) in notifiers.iter().filter(|(name, _)| name == tree) {
            if let Err(e) = notifier.inval_inode(INodeNo(object + 2), 0, 0) {
                eprintln!("tree InvalidateFailed: tree={tree} object={object}: {e}");
            }
        }
    }
}

struct View {
    actor: Arc<Mutex<Volume>>,
    signal: Arc<ProgressSignal>,
    tree: String,
    writer: String,
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
    volume: MutexGuard<'a, Volume>,
    signal: &'a ProgressSignal,
    modified: bool,
}

impl Deref for ActorGuard<'_> {
    type Target = Volume;
    fn deref(&self) -> &Volume {
        &self.volume
    }
}

impl DerefMut for ActorGuard<'_> {
    fn deref_mut(&mut self) -> &mut Volume {
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
    },
}

fn errno(error: Error) -> Errno {
    Errno::from_i32(error.errno())
}

impl View {
    fn actor(&self) -> Result<ActorGuard<'_>> {
        let v = self.actor.lock().map_err(|_| Errno::EIO)?;
        if v.is_stopped() {
            return Err(Errno::EIO);
        }
        Ok(ActorGuard {
            volume: v,
            signal: &self.signal,
            modified: false,
        })
    }
    fn handles(&self) -> Result<MutexGuard<'_, Handles>> {
        self.handles.lock().map_err(|_| Errno::EIO)
    }
    fn path(&self, v: &Volume, ino: INodeNo) -> Result<String> {
        v.files(&self.tree)
            .map_err(errno)?
            .into_iter()
            .find(|(_, o, _)| *o + 2 == ino.0)
            .map(|(p, _, _)| p)
            .ok_or(Errno::ENOENT)
    }
    fn attr(&self, v: &Volume, ino: INodeNo, fh: Option<FileHandle>) -> Result<FileAttr> {
        // First: the kernel keeps one size per inode, and an unlinked mapped
        // file's fstat has no path.
        let open_size = ino
            .0
            .checked_sub(2)
            .and_then(|object| v.inode_size(&self.tree, object));
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
        } else if v.is_anonymous(&self.tree, ino.0 - 2).map_err(errno)? {
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
            v.read(&self.tree, &self.path(v, ino)?)
                .map_err(errno)?
                .len()
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
            } else if v.is_anonymous(&self.tree, ino.0 - 2).map_err(errno)? {
                0
            } else {
                1
            },
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn open_file(&self, v: &mut Volume, path: &str, flags: i32, create: bool) -> Result<u64> {
        let mut handles = self.handles()?;
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !write {
            return Err(Errno::EACCES);
        }
        let entry = if write {
            let handle = v
                .write_open(&self.tree, path, &self.writer, create)
                .map_err(errno)?;
            if flags & libc::O_TRUNC != 0 {
                v.truncate(handle, 0).map_err(errno)?;
            }
            Open::Write {
                handle,
                append: flags & libc::O_APPEND != 0,
                readable: flags & libc::O_ACCMODE == libc::O_RDWR,
            }
        } else {
            if create {
                let h = v
                    .write_open(&self.tree, path, &self.writer, true)
                    .map_err(errno)?;
                v.close(h).map_err(errno)?;
            }
            Open::Read(v.read(&self.tree, path).map_err(errno)?.to_vec())
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
            let (_, object, _) = v
                .files(&self.tree)
                .map_err(errno)?
                .into_iter()
                .find(|(p, _, _)| p == name)
                .ok_or(Errno::ENOENT)?;
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
            if flags & libc::O_EXCL != 0 && v.read(&self.tree, path).is_ok() {
                return Err(Errno::EEXIST);
            }
            let fh = self.open_file(&mut v, path, flags, true)?;
            let object = v
                .files(&self.tree)
                .map_err(errno)?
                .into_iter()
                .find(|(p, _, _)| p == path)
                .ok_or(Errno::ENOENT)?
                .1;
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
            let handle = v.anonymous_open(&self.tree, &self.writer).map_err(errno)?;
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
                match v.volume.page_fill(&self.tree, object, offset, size) {
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
        let result = (|| {
            let mut v = self.actor()?;
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Write { handle, .. }
                    if write_flags.contains(WriteFlags::FUSE_WRITE_CACHE) && offset == u64::MAX =>
                {
                    v.page_writeback(*handle, offset, data).map_err(errno)
                }
                Open::Write { handle, append, .. } => {
                    v.write(*handle, offset, data, *append).map_err(errno)
                }
                _ => Err(Errno::EBADF),
            }
        })();
        match result {
            Ok(()) => reply.written(data.len() as u32),
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
                        .write_open(&self.tree, &path, &self.writer, false)
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
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.actor().and_then(|v| v.sync().map_err(errno)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.flush(req, ino, fh, LockOwner(0), reply);
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
            if !v.is_anonymous(&self.tree, object).map_err(errno)? {
                return Err(Errno::EPERM);
            }
            v.first_link(
                &self.tree,
                object,
                name.to_str().ok_or(Errno::EINVAL)?,
                &self.writer,
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
                .unlink(
                    &self.tree,
                    name.to_str().ok_or(Errno::EINVAL)?,
                    &self.writer,
                )
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
                    &self.tree,
                    name.to_str().ok_or(Errno::EINVAL)?,
                    newname.to_str().ok_or(Errno::EINVAL)?,
                    &self.writer,
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
            let mut entries = vec![
                (INodeNo(1), FileType::Directory, ".".to_owned()),
                (INodeNo(1), FileType::Directory, "..".to_owned()),
            ];
            entries.extend(
                v.files(&self.tree)
                    .map_err(errno)?
                    .into_iter()
                    .map(|(p, o, _)| (INodeNo(o + 2), FileType::RegularFile, p)),
            );
            Ok(entries)
        })();
        match result {
            Ok(entries) => {
                for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(ino, (i + 1) as u64, kind, name) {
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

    #[test]
    fn all_bindings_are_checked_before_any_mount_can_expose_state() {
        for invalid in ["state", "state/inside", "."] {
            let tmp = tempfile::tempdir().unwrap();
            let state = tmp.path().join("state");
            let volume = Volume::open(&state).unwrap();
            let outside = tmp.path().join("valid");
            std::fs::create_dir(&outside).unwrap();
            let invalid = tmp.path().join(invalid);
            std::fs::create_dir_all(&invalid).unwrap();
            let bindings = vec![
                Binding {
                    tree: "main".into(),
                    writer: "main".into(),
                    mountpoint: outside,
                },
                Binding {
                    tree: "main".into(),
                    writer: "other".into(),
                    mountpoint: invalid,
                },
            ];
            assert_eq!(
                serve(&state, volume, bindings).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn anonymous_inode_attributes_are_available_without_a_file_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let view = View {
            actor: Arc::new(Mutex::new(Volume::open(tmp.path()).unwrap())),
            signal: Arc::new(ProgressSignal::default()),
            tree: "main".into(),
            writer: "client".into(),
            handles: Mutex::new(Handles {
                next: 2,
                entries: BTreeMap::new(),
            }),
        };
        let mut volume = view.actor().unwrap();
        let handle = volume.anonymous_open("main", "client").unwrap();
        let ino = INodeNo(volume.open_object(handle).unwrap() + 2);
        volume.write(handle, 0, b"private", false).unwrap();
        view.handles().unwrap().entries.insert(
            1,
            Open::Write {
                handle,
                append: false,
                readable: true,
            },
        );
        let attr = view.attr(&volume, ino, None).unwrap();
        assert_eq!(attr.size, 7);
        assert_eq!(attr.nlink, 0);
    }

    #[test]
    fn a_readers_snapshot_survives_a_later_save_and_readonly_truncation_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let view = View {
            actor: Arc::new(Mutex::new(Volume::open(tmp.path()).unwrap())),
            signal: Arc::new(ProgressSignal::default()),
            tree: "main".into(),
            writer: "client".into(),
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let mut volume = view.actor().unwrap();
        let h = volume.write_open("main", "a", "seed", true).unwrap();
        volume.write(h, 0, b"old", false).unwrap();
        volume.close(h).unwrap();
        let reader = view
            .open_file(&mut volume, "a", libc::O_RDONLY, false)
            .unwrap();
        let h = volume.write_open("main", "a", "next", false).unwrap();
        volume.truncate(h, 0).unwrap();
        volume.write(h, 0, b"new", false).unwrap();
        volume.close(h).unwrap();
        assert_eq!(volume.read("main", "a").unwrap(), b"new");
        match &view.handles().unwrap().entries[&reader] {
            Open::Read(bytes) => assert_eq!(bytes, b"old"),
            _ => panic!("read-only open acquired a writer"),
        }
        assert_eq!(
            view.open_file(&mut volume, "a", libc::O_RDONLY | libc::O_TRUNC, false),
            Err(Errno::EACCES)
        );
        assert_eq!(volume.read("main", "a").unwrap(), b"new");
    }
}
