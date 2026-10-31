//! Kernel cache invalidations across a daemon's FUSE sessions (design
//! §17.1; sandbox overlay slice 4).
//!
//! The main mount and the sandbox overlay are two FUSE sessions over one
//! [`crate::ops::Ops`], and node ids are the same in both. Each kernel
//! caches names (1 s) per session, and the pages of files open through it,
//! so a change made through one session is an event every other session's
//! kernel must hear: [`Coherent`], a decorator in front of each session's
//! [`Operations`], reports each change the session served, and the
//! [`Invalidator`] sends it to the other sessions' kernels
//! ([`KernelCache`]).
//!
//! - **Events, not timers.** Every change is sent once, as it happens.
//! - **Never from a request thread.** The kernel may wait on a request of
//!   the session it invalidates to finish an invalidation, and two
//!   sessions' requests invalidating each other would wait on each other,
//!   so a FUSE request thread only queues ([`Invalidator::changed`] never
//!   blocks); one worker thread sends.
//! - **Not to the session the change came through**: its kernel already
//!   knows.
//! - A session attached late ([`Invalidator::attach`]) has nothing cached
//!   from before, so changes queued before it attached are not sent to it.
//!
//! [`Invalidator::invalidate`] sends at once to every session, from a
//! thread that is not a FUSE request's: a restore made outside every
//! session ([`crate::mount::MountHooks`]).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use fuser::FileAttr;

use crate::mount::MountKind;
use crate::ops::{Cached, DirEntry, Entry, Handle, OpResult, SetAttr, Xattr};
use crate::session::{Actor, Operations};

/// One session's kernel cache, as the invalidator drops entries from it.
/// Implemented by [`fuser::Notifier`]; tests use a recording fake.
pub trait KernelCache: Send + Sync {
    /// Drops the kernel's cached entry `name` in directory node `parent`.
    /// An entry the kernel does not hold is not an error.
    fn inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()>;
    /// Drops node `id`'s cached attributes and pages. A node the kernel
    /// does not hold is not an error.
    fn inval_inode(&self, id: u64) -> io::Result<()>;
}

impl KernelCache for fuser::Notifier {
    // fuser answers Ok for an entry or node the kernel does not hold
    // (ENOENT).
    fn inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        fuser::Notifier::inval_entry(self, fuser::INodeNo(parent), OsStr::from_bytes(name))
    }

    fn inval_inode(&self, id: u64) -> io::Result<()> {
        fuser::Notifier::inval_inode(self, fuser::INodeNo(id), 0, 0)
    }
}

/// Sends kernel cache invalidations to a daemon's FUSE sessions.
/// `Send + Sync`.
pub struct Invalidator {
    shared: Arc<Shared>,
    /// The queue to the worker; `None` once stopped.
    queue: Mutex<Option<mpsc::Sender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// What the invalidator and its worker share.
struct Shared {
    caches: Mutex<Vec<Attached>>,
    /// The sequence number of the next change queued.
    next: AtomicU64,
}

/// One session's kernel cache, attached.
struct Attached {
    kind: MountKind,
    /// The first change it hears: those queued before it attached are not
    /// sent to it.
    since: u64,
    cache: Arc<dyn KernelCache>,
    /// Its first failure was logged.
    said: Arc<AtomicBool>,
}

/// A change queued for the worker.
struct Job {
    seq: u64,
    from: MountKind,
    cached: Vec<Cached>,
}

/// The kind a log line names.
fn kind_name(kind: MountKind) -> &'static str {
    match kind {
        MountKind::Main => "mount point",
        MountKind::SandboxOverlay => "sandbox overlay",
    }
}

/// Sends `cached` to `cache`; the first error, after trying every entry.
fn send(cache: &dyn KernelCache, cached: &[Cached]) -> io::Result<()> {
    let mut first = Ok(());
    for c in cached {
        let sent = match c {
            Cached::Node(id) => cache.inval_inode(*id),
            Cached::Entry(parent, name) => cache.inval_entry(*parent, name),
        };
        if let Err(e) = sent
            && first.is_ok()
        {
            first = Err(e);
        }
    }
    first
}

impl Shared {
    fn caches(&self) -> MutexGuard<'_, Vec<Attached>> {
        self.caches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The worker's step: `job` to every cache attached before it was
    /// queued, except its own session's. A failing cache is logged once
    /// and never stops the others.
    fn deliver(&self, job: &Job) {
        let targets: Vec<_> = self
            .caches()
            .iter()
            .filter(|a| a.kind != job.from && a.since <= job.seq)
            .map(|a| (a.kind, a.cache.clone(), a.said.clone()))
            .collect();
        for (kind, cache, said) in targets {
            if let Err(e) = send(cache.as_ref(), &job.cached)
                && !said.swap(true, Ordering::Relaxed)
            {
                eprintln!(
                    "agentfs: invalidating the {}'s kernel cache failed: {e} (said once; its \
                     cached names expire within a second, and attributes are never cached)",
                    kind_name(kind)
                );
            }
        }
    }
}

impl Invalidator {
    /// An invalidator with no session attached yet, and its worker thread.
    pub fn start() -> io::Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            caches: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel::<Job>();
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("agentfs-invalidate".into())
                .spawn(move || {
                    for job in rx {
                        shared.deliver(&job);
                    }
                })?
        };
        Ok(Arc::new(Self {
            shared,
            queue: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
        }))
    }

    /// Session `kind`'s kernel cache hears every later change made through
    /// another session. A second attach of the same kind replaces the
    /// first.
    pub fn attach(&self, kind: MountKind, cache: Box<dyn KernelCache>) {
        let mut caches = self.shared.caches();
        caches.retain(|a| a.kind != kind);
        caches.push(Attached {
            kind,
            since: self.shared.next.load(Ordering::SeqCst),
            cache: Arc::from(cache),
            said: Arc::new(AtomicBool::new(false)),
        });
    }

    /// A change served through session `from`: every other attached
    /// session drops `cached`. Queues and returns; never blocks.
    pub fn changed(&self, from: MountKind, cached: Vec<Cached>) {
        if cached.is_empty() {
            return;
        }
        let seq = self.shared.next.fetch_add(1, Ordering::SeqCst);
        let queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = queue.as_ref() {
            // The worker only stops after the sender is gone.
            let _ = tx.send(Job { seq, from, cached });
        }
    }

    /// Every attached session drops `cached` now, sent from the calling
    /// thread (never a FUSE request's); the first error, if any.
    pub fn invalidate(&self, cached: &[Cached]) -> Result<(), String> {
        let targets: Vec<_> = self
            .shared
            .caches()
            .iter()
            .map(|a| (a.kind, a.cache.clone()))
            .collect();
        let mut first = None;
        for (kind, cache) in targets {
            if let Err(e) = send(cache.as_ref(), cached) {
                first.get_or_insert_with(|| format!("the {}: {e}", kind_name(kind)));
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Sends what is queued, then stops the worker. Later calls are no-ops.
    pub fn stop(&self) {
        // Dropping the sender ends the worker's loop once the queue is
        // drained.
        drop(
            self.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

impl Drop for Invalidator {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where [`Coherent`] reports the changes its session served: the mount
/// wires it to [`Invalidator::changed`] with the session's kind.
pub type ChangeSink = Arc<dyn Fn(Vec<Cached>) + Send + Sync>;

/// A session's [`Operations`] that reports every change it served to a
/// [`ChangeSink`], after the operation succeeded:
///
/// | Operation | Reported |
/// |---|---|
/// | `mknod`, `mkdir`, `symlink`, `link`, `create` | `Entry(parent, name)` |
/// | `unlink`, `rmdir` | `Entry(parent, name)` |
/// | `rename` | `Entry(parent, name)`, `Entry(newparent, newname)` |
/// | `setattr` | `Node(id)` |
/// | `flush` and `release` of a handle that can write, `fallocate`, `copy_file_range` (its destination) | `Node(node)` |
/// | every other operation, and any that failed | nothing |
///
/// Everything else is passed on unchanged. It learns a handle's node and
/// whether it can write from the `open` and `create` results it passes
/// back, and forgets it at `release`.
pub struct Coherent<O: Operations> {
    inner: Arc<O>,
    sink: ChangeSink,
    /// Each open handle's node and whether it can write, by handle id.
    handles: Mutex<HashMap<u64, (u64, bool)>>,
}

impl<O: Operations> Coherent<O> {
    pub fn new(inner: Arc<O>, sink: ChangeSink) -> Self {
        Self {
            inner,
            sink,
            handles: Mutex::new(HashMap::new()),
        }
    }

    fn handles(&self) -> MutexGuard<'_, HashMap<u64, (u64, bool)>> {
        self.handles.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reports `cached` when `result` is a success; passes it back.
    fn report<T>(&self, result: OpResult<T>, cached: impl FnOnce() -> Vec<Cached>) -> OpResult<T> {
        if result.is_ok() {
            (self.sink)(cached());
        }
        result
    }

    /// `Entry(parent, name)`.
    fn entry(parent: u64, name: &[u8]) -> Vec<Cached> {
        vec![Cached::Entry(parent, name.to_vec())]
    }

    /// Learns handle `fh` on `node`.
    fn opened(&self, fh: u64, node: u64, handle: &Handle) {
        self.handles().insert(fh, (node, handle.write));
    }

    /// The node of handle `fh`, if it can write.
    fn writer(&self, fh: u64) -> Option<u64> {
        self.handles()
            .get(&fh)
            .and_then(|&(node, write)| write.then_some(node))
    }
}

impl<O: Operations> Operations for Coherent<O> {
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        self.inner.lookup(parent, name)
    }

    fn forget(&self, id: u64, n: u64) {
        self.inner.forget(id, n);
    }

    fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr> {
        self.inner.getattr(id, fh)
    }

    fn setattr(&self, id: u64, set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr> {
        self.report(self.inner.setattr(id, set, actor), || {
            vec![Cached::Node(id)]
        })
    }

    fn readlink(&self, id: u64) -> OpResult<Vec<u8>> {
        self.inner.readlink(id)
    }

    fn access(&self, id: u64, mask: i32) -> OpResult<()> {
        self.inner.access(id, mask)
    }

    fn paths(&self, id: u64) -> Vec<Vec<u8>> {
        self.inner.paths(id)
    }

    fn mknod(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.report(self.inner.mknod(parent, name, mode, rdev, actor), || {
            Self::entry(parent, name)
        })
    }

    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, actor: Actor<'_>) -> OpResult<Entry> {
        self.report(self.inner.mkdir(parent, name, mode, actor), || {
            Self::entry(parent, name)
        })
    }

    fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.report(self.inner.unlink(parent, name, actor), || {
            Self::entry(parent, name)
        })
    }

    fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.report(self.inner.rmdir(parent, name, actor), || {
            Self::entry(parent, name)
        })
    }

    fn symlink(
        &self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.report(self.inner.symlink(parent, name, target, actor), || {
            Self::entry(parent, name)
        })
    }

    fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        let renamed = self
            .inner
            .rename(parent, name, newparent, newname, flags, actor);
        self.report(renamed, || {
            vec![
                Cached::Entry(parent, name.to_vec()),
                Cached::Entry(newparent, newname.to_vec()),
            ]
        })
    }

    fn link(&self, id: u64, newparent: u64, newname: &[u8], actor: Actor<'_>) -> OpResult<Entry> {
        self.report(self.inner.link(id, newparent, newname, actor), || {
            Self::entry(newparent, newname)
        })
    }

    fn open(&self, id: u64, flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
        let opened = self.inner.open(id, flags, actor)?;
        self.opened(opened.0, id, &opened.1);
        Ok(opened)
    }

    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        let created = self.inner.create(parent, name, mode, flags, actor)?;
        self.opened(created.1, created.0.id, &created.2);
        (self.sink)(Self::entry(parent, name));
        Ok(created)
    }

    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        self.inner.read(fh, offset, size)
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32> {
        self.inner.write(fh, offset, data, actor)
    }

    fn flush(&self, fh: u64, actor: Actor<'_>) -> OpResult<()> {
        let flushed = self.inner.flush(fh, actor);
        match self.writer(fh) {
            Some(node) => self.report(flushed, || vec![Cached::Node(node)]),
            None => flushed,
        }
    }

    fn release(&self, fh: u64) -> OpResult<()> {
        let released = self.inner.release(fh);
        let writer = self.writer(fh);
        self.handles().remove(&fh);
        match writer {
            Some(node) => self.report(released, || vec![Cached::Node(node)]),
            None => released,
        }
    }

    fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()> {
        self.inner.fsync(fh, datasync)
    }

    fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        let done = self.inner.fallocate(fh, offset, length, mode, actor);
        match self.handles().get(&fh).map(|&(node, _)| node) {
            Some(node) => self.report(done, || vec![Cached::Node(node)]),
            None => done,
        }
    }

    fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64> {
        self.inner.lseek(fh, offset, whence)
    }

    fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        actor: Actor<'_>,
    ) -> OpResult<u32> {
        let copied = self
            .inner
            .copy_file_range(fh_in, offset_in, fh_out, offset_out, len, actor);
        match self.handles().get(&fh_out).map(|&(node, _)| node) {
            Some(node) => self.report(copied, || vec![Cached::Node(node)]),
            None => copied,
        }
    }

    fn mapping_may_write(&self, id: u64) {
        self.inner.mapping_may_write(id);
    }

    fn opendir(&self, id: u64) -> OpResult<u64> {
        self.inner.opendir(id)
    }

    fn readdir(&self, id: u64, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        self.inner.readdir(id, fh, offset)
    }

    fn releasedir(&self, fh: u64) {
        self.inner.releasedir(fh);
    }

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        self.inner.statfs()
    }

    fn setxattr(&self, id: u64, name: &[u8], value: &[u8], flags: i32) -> OpResult<()> {
        self.inner.setxattr(id, name, value, flags)
    }

    fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr> {
        self.inner.getxattr(id, name, size)
    }

    fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr> {
        self.inner.listxattr(id, size)
    }

    fn removexattr(&self, id: u64, name: &[u8]) -> OpResult<()> {
        self.inner.removexattr(id, name)
    }
}

#[cfg(test)]
mod tests {
    // [`Coherent`] over a stub [`Operations`] that succeeds or fails on
    // demand, with a recording sink; the [`Invalidator`] with recording
    // [`KernelCache`] fakes.

    use std::os::fd::OwnedFd;
    use std::sync::atomic::AtomicBool;
    use std::time::UNIX_EPOCH;

    use agentfs_core::record::Caller;
    use fuser::{Errno, FileType, INodeNo};

    use super::*;

    const C: Caller = Caller {
        uid: 1000,
        gid: 1000,
        pid: 42,
    };
    const A: Actor<'static> = Actor::of_process(C);
    const O_RDONLY: i32 = 0;
    const O_WRONLY: i32 = 1;

    fn attr(id: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(id),
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn handle(id: u64, write: bool) -> Arc<Handle> {
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        Arc::new(Handle::new(id, fd, write, None, 0))
    }

    /// Every operation succeeds, or every one fails with `EIO` once `fail` is
    /// set. `open` gives handle 7 on its node, writable as the flags ask;
    /// `create` gives node 9 with writable handle 8.
    #[derive(Default)]
    struct Stub {
        fail: AtomicBool,
    }

    impl Stub {
        fn ok<T>(&self, value: T) -> OpResult<T> {
            if self.fail.load(Ordering::Relaxed) {
                Err(Errno::EIO)
            } else {
                Ok(value)
            }
        }

        fn entry(&self, id: u64) -> OpResult<Entry> {
            self.ok(Entry { id, attr: attr(id) })
        }
    }

    impl Operations for Stub {
        fn lookup(&self, _parent: u64, _name: &[u8]) -> OpResult<Entry> {
            self.entry(3)
        }
        fn forget(&self, _id: u64, _n: u64) {}
        fn getattr(&self, id: u64, _fh: Option<u64>) -> OpResult<FileAttr> {
            self.ok(attr(id))
        }
        fn setattr(&self, id: u64, _set: &SetAttr, _actor: Actor<'_>) -> OpResult<FileAttr> {
            self.ok(attr(id))
        }
        fn readlink(&self, _id: u64) -> OpResult<Vec<u8>> {
            self.ok(Vec::new())
        }
        fn access(&self, _id: u64, _mask: i32) -> OpResult<()> {
            self.ok(())
        }
        fn paths(&self, _id: u64) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn mknod(&self, _p: u64, _n: &[u8], _m: u32, _r: u32, _a: Actor<'_>) -> OpResult<Entry> {
            self.entry(4)
        }
        fn mkdir(&self, _p: u64, _n: &[u8], _m: u32, _a: Actor<'_>) -> OpResult<Entry> {
            self.entry(4)
        }
        fn unlink(&self, _p: u64, _n: &[u8], _a: Actor<'_>) -> OpResult<()> {
            self.ok(())
        }
        fn rmdir(&self, _p: u64, _n: &[u8], _a: Actor<'_>) -> OpResult<()> {
            self.ok(())
        }
        fn symlink(&self, _p: u64, _n: &[u8], _t: &[u8], _a: Actor<'_>) -> OpResult<Entry> {
            self.entry(4)
        }
        fn rename(
            &self,
            _p: u64,
            _n: &[u8],
            _np: u64,
            _nn: &[u8],
            _f: u32,
            _a: Actor<'_>,
        ) -> OpResult<()> {
            self.ok(())
        }
        fn link(&self, id: u64, _np: u64, _nn: &[u8], _a: Actor<'_>) -> OpResult<Entry> {
            self.entry(id)
        }
        fn open(&self, id: u64, flags: i32, _a: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
            self.ok((7, handle(id, flags & 3 != 0)))
        }
        fn create(
            &self,
            _p: u64,
            _n: &[u8],
            _m: u32,
            _f: i32,
            _a: Actor<'_>,
        ) -> OpResult<(Entry, u64, Arc<Handle>)> {
            self.ok((
                Entry {
                    id: 9,
                    attr: attr(9),
                },
                8,
                handle(9, true),
            ))
        }
        fn read(&self, _fh: u64, _o: u64, _s: u32) -> OpResult<Vec<u8>> {
            self.ok(Vec::new())
        }
        fn write(&self, _fh: u64, _o: u64, data: &[u8], _a: Actor<'_>) -> OpResult<u32> {
            self.ok(u32::try_from(data.len()).unwrap())
        }
        fn flush(&self, _fh: u64, _a: Actor<'_>) -> OpResult<()> {
            self.ok(())
        }
        fn release(&self, _fh: u64) -> OpResult<()> {
            self.ok(())
        }
        fn fsync(&self, _fh: u64, _d: bool) -> OpResult<()> {
            self.ok(())
        }
        fn fallocate(&self, _fh: u64, _o: u64, _l: u64, _m: i32, _a: Actor<'_>) -> OpResult<()> {
            self.ok(())
        }
        fn lseek(&self, _fh: u64, offset: i64, _w: i32) -> OpResult<i64> {
            self.ok(offset)
        }
        fn copy_file_range(
            &self,
            _fi: u64,
            _oi: u64,
            _fo: u64,
            _oo: u64,
            len: u64,
            _a: Actor<'_>,
        ) -> OpResult<u32> {
            self.ok(u32::try_from(len).unwrap())
        }
        fn mapping_may_write(&self, _id: u64) {}
        fn opendir(&self, _id: u64) -> OpResult<u64> {
            self.ok(1)
        }
        fn readdir(&self, _id: u64, _fh: u64, _o: u64) -> OpResult<Vec<(u64, DirEntry)>> {
            self.ok(Vec::new())
        }
        fn releasedir(&self, _fh: u64) {}
        fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
            Err(Errno::ENOSYS)
        }
        fn setxattr(&self, _id: u64, _n: &[u8], _v: &[u8], _f: i32) -> OpResult<()> {
            self.ok(())
        }
        fn getxattr(&self, _id: u64, _n: &[u8], _s: u32) -> OpResult<Xattr> {
            self.ok(Xattr::Size(0))
        }
        fn listxattr(&self, _id: u64, _s: u32) -> OpResult<Xattr> {
            self.ok(Xattr::Size(0))
        }
        fn removexattr(&self, _id: u64, _n: &[u8]) -> OpResult<()> {
            self.ok(())
        }
    }

    /// A [`Coherent`] over a [`Stub`], and what it reported.
    struct Fx {
        stub: Arc<Stub>,
        coherent: Coherent<Stub>,
        reported: Arc<Mutex<Vec<Vec<Cached>>>>,
    }

    impl Fx {
        fn new() -> Self {
            let stub = Arc::new(Stub::default());
            let reported = Arc::new(Mutex::new(Vec::new()));
            let sink: ChangeSink = {
                let reported = reported.clone();
                Arc::new(move |cached| reported.lock().unwrap().push(cached))
            };
            Self {
                coherent: Coherent::new(stub.clone(), sink),
                stub,
                reported,
            }
        }

        /// What was reported since the last take.
        fn take(&self) -> Vec<Vec<Cached>> {
            std::mem::take(&mut *self.reported.lock().unwrap())
        }

        fn fail(&self, fail: bool) {
            self.stub.fail.store(fail, Ordering::Relaxed);
        }
    }

    fn entry(parent: u64, name: &str) -> Cached {
        Cached::Entry(parent, name.as_bytes().to_vec())
    }

    /// Each row of `Coherent`'s table, and nothing for a read or a failure.
    #[test]
    fn coherent_reports_each_change_after_it_succeeded() {
        let fx = Fx::new();
        let c = &fx.coherent;
        c.mknod(1, b"n", 0o100644, 0, A).unwrap();
        c.mkdir(1, b"d", 0o755, A).unwrap();
        c.symlink(1, b"s", b"t", A).unwrap();
        c.link(3, 2, b"l", A).unwrap();
        c.unlink(1, b"u", A).unwrap();
        c.rmdir(1, b"r", A).unwrap();
        c.rename(1, b"a", 2, b"b", 0, A).unwrap();
        c.setattr(5, &SetAttr::default(), A).unwrap();
        assert_eq!(
            fx.take(),
            [
                vec![entry(1, "n")],
                vec![entry(1, "d")],
                vec![entry(1, "s")],
                vec![entry(2, "l")],
                vec![entry(1, "u")],
                vec![entry(1, "r")],
                vec![entry(1, "a"), entry(2, "b")],
                vec![Cached::Node(5)],
            ]
        );

        // A handle that can write: its node, at flush, fallocate, a copy into
        // it and release; one that cannot: nothing.
        let (_, fh, _) = c.create(1, b"c", 0o644, O_WRONLY, A).unwrap();
        assert_eq!(fx.take(), [vec![entry(1, "c")]]);
        let (ro, _) = c.open(6, O_RDONLY, A).unwrap();
        c.write(fh, 0, b"x", A).unwrap();
        c.flush(fh, A).unwrap();
        c.fallocate(fh, 0, 1, 0, A).unwrap();
        c.copy_file_range(ro, 0, fh, 0, 1, A).unwrap();
        c.flush(ro, A).unwrap();
        c.release(ro).unwrap();
        c.release(fh).unwrap();
        assert_eq!(
            fx.take(),
            [
                vec![Cached::Node(9)],
                vec![Cached::Node(9)],
                vec![Cached::Node(9)],
                vec![Cached::Node(9)],
            ]
        );
        // A writable open, learned from the result.
        let (w, _) = c.open(6, O_WRONLY, A).unwrap();
        c.flush(w, A).unwrap();
        assert_eq!(fx.take(), [vec![Cached::Node(6)]]);

        // Reads change nothing.
        c.lookup(1, b"x").unwrap();
        c.getattr(1, None).unwrap();
        c.read(w, 0, 1).unwrap();
        c.opendir(1).unwrap();
        c.getxattr(1, b"user.a", 0).unwrap();
        assert!(fx.take().is_empty());

        // Nothing for an operation that failed.
        fx.fail(true);
        assert!(c.mknod(1, b"n", 0o100644, 0, A).is_err());
        assert!(c.mkdir(1, b"d", 0o755, A).is_err());
        assert!(c.symlink(1, b"s", b"t", A).is_err());
        assert!(c.link(3, 2, b"l", A).is_err());
        assert!(c.unlink(1, b"u", A).is_err());
        assert!(c.rmdir(1, b"r", A).is_err());
        assert!(c.rename(1, b"a", 2, b"b", 0, A).is_err());
        assert!(c.setattr(5, &SetAttr::default(), A).is_err());
        assert!(c.create(1, b"c", 0o644, O_WRONLY, A).is_err());
        assert!(c.flush(w, A).is_err());
        assert!(c.fallocate(w, 0, 1, 0, A).is_err());
        assert!(c.copy_file_range(w, 0, w, 0, 1, A).is_err());
        assert!(c.release(w).is_err());
        assert!(fx.take().is_empty());
    }

    /// A kernel cache that records what it is sent, failing every send when
    /// `fail` is set.
    #[derive(Clone, Default)]
    struct Recording {
        seen: Arc<Mutex<Vec<Cached>>>,
        fail: bool,
    }

    impl Recording {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }

        fn seen(&self) -> Vec<Cached> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl KernelCache for Recording {
        fn inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push(Cached::Entry(parent, name.to_vec()));
            if self.fail {
                Err(io::Error::from_raw_os_error(libc::ENODEV))
            } else {
                Ok(())
            }
        }

        fn inval_inode(&self, id: u64) -> io::Result<()> {
            self.seen.lock().unwrap().push(Cached::Node(id));
            if self.fail {
                Err(io::Error::from_raw_os_error(libc::ENODEV))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_change_reaches_every_other_session_and_never_its_own() {
        let inv = Invalidator::start().unwrap();
        let (main, overlay) = (Recording::default(), Recording::default());
        inv.attach(MountKind::Main, Box::new(main.clone()));
        inv.attach(MountKind::SandboxOverlay, Box::new(overlay.clone()));
        inv.changed(MountKind::Main, vec![entry(1, "a"), Cached::Node(2)]);
        inv.changed(MountKind::SandboxOverlay, vec![Cached::Node(3)]);
        inv.changed(MountKind::Main, Vec::new());
        inv.stop();
        assert_eq!(overlay.seen(), [entry(1, "a"), Cached::Node(2)]);
        assert_eq!(main.seen(), [Cached::Node(3)]);
        // Stopped: later changes go nowhere, and a second stop is a no-op.
        inv.changed(MountKind::Main, vec![Cached::Node(4)]);
        inv.stop();
        assert_eq!(overlay.seen().len(), 2);
    }

    #[test]
    fn invalidate_reaches_every_session_from_the_caller() {
        let inv = Invalidator::start().unwrap();
        let (main, overlay) = (Recording::default(), Recording::default());
        inv.attach(MountKind::Main, Box::new(main.clone()));
        inv.attach(MountKind::SandboxOverlay, Box::new(overlay.clone()));
        inv.invalidate(&[entry(1, "a"), Cached::Node(2)]).unwrap();
        // Sent before it returned: nothing waits on the worker.
        assert_eq!(main.seen(), [entry(1, "a"), Cached::Node(2)]);
        assert_eq!(overlay.seen(), [entry(1, "a"), Cached::Node(2)]);
    }

    #[test]
    fn a_session_attached_later_hears_nothing_queued_before() {
        let inv = Invalidator::start().unwrap();
        let (first, later) = (Recording::default(), Recording::default());
        inv.attach(MountKind::Main, Box::new(first.clone()));
        inv.changed(MountKind::SandboxOverlay, vec![Cached::Node(1)]);
        // Replaces the first cache of its kind.
        inv.attach(MountKind::Main, Box::new(later.clone()));
        inv.changed(MountKind::SandboxOverlay, vec![Cached::Node(2)]);
        inv.stop();
        assert_eq!(later.seen(), [Cached::Node(2)]);
        assert!(!first.seen().contains(&Cached::Node(2)));
    }

    #[test]
    fn stop_sends_everything_queued() {
        let inv = Invalidator::start().unwrap();
        let overlay = Recording::default();
        inv.attach(MountKind::SandboxOverlay, Box::new(overlay.clone()));
        for id in 0..1000 {
            inv.changed(MountKind::Main, vec![Cached::Node(id)]);
        }
        inv.stop();
        let expected: Vec<Cached> = (0..1000).map(Cached::Node).collect();
        assert_eq!(overlay.seen(), expected);
    }

    #[test]
    fn a_failing_cache_never_stops_the_others() {
        let inv = Invalidator::start().unwrap();
        let (main, overlay) = (Recording::default(), Recording::failing());
        inv.attach(MountKind::Main, Box::new(main.clone()));
        inv.attach(MountKind::SandboxOverlay, Box::new(overlay.clone()));
        // The worker goes on after a failure, and every entry is tried.
        inv.changed(MountKind::Main, vec![Cached::Node(1), Cached::Node(2)]);
        inv.changed(MountKind::SandboxOverlay, vec![Cached::Node(3)]);
        inv.changed(MountKind::Main, vec![Cached::Node(4)]);
        // invalidate reaches every cache, and answers the failure.
        let err = inv.invalidate(&[Cached::Node(5)]).unwrap_err();
        assert!(err.contains("sandbox overlay"), "{err}");
        inv.stop();
        // The worker and the caller send concurrently: compare as sets.
        let ids = |r: &Recording| {
            let mut ids: Vec<u64> = r
                .seen()
                .into_iter()
                .map(|c| match c {
                    Cached::Node(id) => id,
                    Cached::Entry(..) => u64::MAX,
                })
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(ids(&main), [3, 5]);
        assert_eq!(ids(&overlay), [1, 2, 4, 5]);
    }
}
