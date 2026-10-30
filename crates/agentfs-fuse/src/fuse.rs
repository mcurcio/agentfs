//! The kernel adapter: [`fuser::Filesystem`] over [`Ops`] (design §2.1,
//! §17.1).
//!
//! Each request is handed to [`Ops`] with its caller, and the result mapped
//! to the reply. What this layer decides itself:
//!
//! - **INIT.** It asks for `FUSE_ATOMIC_O_TRUNC` (a shell `>` is one open
//!   with `O_TRUNC`, recorded once at close) and `FUSE_PARALLEL_DIROPS`.
//!   fuser's own defaults (async reads, big writes, max pages) never include
//!   the writeback cache, POSIX or BSD locks (so locks stay in the kernel,
//!   which is right for one mount) or POSIX ACLs (only mode bits are
//!   enforced).
//! - **Passthrough.** With `--passthrough auto` (the default) or `on`, when
//!   the kernel offers `FUSE_PASSTHROUGH` (Linux 6.9+) and the daemon may
//!   register backing files (`CAP_SYS_ADMIN` in the initial user namespace;
//!   [`crate::privileges`]), regular files opened read-only are opened with
//!   a backing file, so their reads and mmap go straight to the backing
//!   inode without the daemon. A handle that can write never is ([`way`],
//!   design §17.1): a mapping of a passthrough file holds the backing file,
//!   not the FUSE file, so RELEASE can come while a shared writable mapping
//!   is live, and writes through it would go unrecorded. One backing id is
//!   kept per inode while any handle on it is open. The kernel refuses to
//!   mix the two modes on one inode (`ETXTBSY`, the open failing with
//!   `EIO`), so an inode with a backing id open keeps it, for a writer too
//!   (whose node is then watched until FORGET, [`Ops::mapping_may_write`]),
//!   and one with daemon handles open gets no new one. If registering fails
//!   for an inode (a stacked backing filesystem such as overlayfs,
//!   `EMFILE`), only that inode goes through the daemon, while it has a
//!   handle open. `on` instead fails the mount when the kernel or
//!   privileges do not allow it, and a later registration failure fails
//!   that open with `EIO` and stops the mount.
//! - **Caching.** Entries are cached for 1 s, attributes not at all: the
//!   daemon does not see every change (a writer sharing a backing id, or
//!   another front on the same backing tree), so only the backing inode
//!   knows the size.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use agentfs_core::record::Caller;
use fuser::{
    AccessFlags, BackingId, BsdFileFlags, CopyFileRangeFlags, Errno, FileHandle, Filesystem,
    FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLseek,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use agentfs_core::service::Stop;

use crate::ops::{Handle, Ops, SetAttr, Xattr};
use crate::privileges::Privileges;

const ENTRY_TTL: Duration = Duration::from_secs(1);
const ATTR_TTL: Duration = Duration::ZERO;

/// The `--passthrough` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passthrough {
    /// Use it when the kernel and privileges allow, else go through the
    /// daemon.
    Auto,
    /// Require it: the mount fails without it.
    On,
    /// Never use it.
    Off,
}

impl std::str::FromStr for Passthrough {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            other => Err(format!("{other:?} is not one of auto, on, off")),
        }
    }
}

/// Whether passthrough is in use, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassthroughStatus {
    Active,
    Off(String),
    /// `--passthrough on` and registering a backing file failed: the mount
    /// is stopping.
    Failed(String),
}

impl std::fmt::Display for PassthroughStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("on"),
            Self::Off(reason) => write!(f, "off ({reason})"),
            Self::Failed(reason) => write!(f, "failed ({reason})"),
        }
    }
}

/// What INIT should do about passthrough: whether to request it, and the
/// resulting status, or why the mount must fail (`--passthrough on`).
/// `refusal` is why the daemon's privileges do not allow it, if they do
/// not ([`Privileges::passthrough_refusal`]).
pub fn decide_passthrough(
    mode: Passthrough,
    kernel_offers: bool,
    refusal: Option<&str>,
) -> Result<(bool, PassthroughStatus), String> {
    let off = |reason: &str| Ok((false, PassthroughStatus::Off(reason.to_owned())));
    let reason = match (mode, kernel_offers, refusal) {
        (Passthrough::Off, _, _) => return off("disabled by --passthrough off"),
        (_, false, _) => "the kernel does not offer FUSE passthrough (needs Linux 6.9 or later)",
        (_, true, Some(refusal)) => refusal,
        (_, true, None) => return Ok((true, PassthroughStatus::Active)),
    };
    if mode == Passthrough::On {
        Err(format!("--passthrough on: {reason}"))
    } else {
        off(reason)
    }
}

/// Registration failures logged before the rest are only counted.
const REFUSALS_LOGGED: u64 = 10;

#[derive(Default)]
struct BackingIds {
    /// Open handle to (node, backing id): the strong references.
    by_handle: HashMap<u64, (u64, Arc<BackingId>)>,
    by_node: HashMap<u64, Weak<BackingId>>,
    /// Handles opened through the daemon while passthrough is active, to
    /// their node, and how many each node has. Such a node gets no backing
    /// id while any of them is open (the kernel refuses to mix the two on
    /// one inode, with `ETXTBSY`), and a node with a backing id open keeps
    /// it for new handles.
    daemon_handles: HashMap<u64, u64>,
    daemon_nodes: HashMap<u64, usize>,
    /// Registrations that failed.
    refused: u64,
}

impl BackingIds {
    fn track_daemon(&mut self, fh: u64, node: u64) {
        self.daemon_handles.insert(fh, node);
        *self.daemon_nodes.entry(node).or_default() += 1;
    }
}

/// Which way one open of a regular file goes while passthrough is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// Through the daemon; later opens of the inode follow it while this
    /// handle is open.
    Daemon,
    /// Passthrough with the inode's backing id already open.
    Shared,
    /// Passthrough with a newly registered backing id.
    Register,
}

/// The passthrough rule (design §17.1), for a handle that is an
/// fsync-class file (`daemon_only`), can write (`write`: opened for
/// writing or truncating), on an inode that has a backing id open
/// (`backing_open`) or daemon handles open (`daemon_open`).
///
/// A handle that can write never gets passthrough: a mapping of a
/// passthrough file holds the backing file, not the FUSE file
/// (`backing_file_mmap`), so RELEASE can come while a shared writable
/// mapping is live, and what is written through it afterwards would never
/// be recorded. A read-only handle cannot make a writable shared mapping.
/// The one exception is the kernel's: an inode with a backing id open
/// fails any open without one (`EIO`), so a writer there shares the id,
/// and its node is watched until the kernel forgets it instead
/// ([`Ops::mapping_may_write`]). An fsync-class file always goes through
/// the daemon (design §8).
fn way(daemon_only: bool, write: bool, backing_open: bool, daemon_open: bool) -> Way {
    if daemon_only {
        Way::Daemon
    } else if backing_open {
        Way::Shared
    } else if write || daemon_open {
        Way::Daemon
    } else {
        Way::Register
    }
}

/// How to open one handle.
enum Route {
    Passthrough(Arc<BackingId>),
    Daemon,
    /// `--passthrough on` cannot be honoured: fail the open.
    Fatal,
}

/// The filesystem fuser runs.
pub struct FuseFs {
    ops: Arc<Ops>,
    mode: Passthrough,
    status: Arc<Mutex<PassthroughStatus>>,
    /// Set when `--passthrough on` fails after INIT: the mount stops.
    failed: Arc<Stop>,
    ids: Mutex<BackingIds>,
}

impl FuseFs {
    pub fn new(
        ops: Arc<Ops>,
        mode: Passthrough,
        status: Arc<Mutex<PassthroughStatus>>,
        failed: Arc<Stop>,
    ) -> Self {
        Self {
            ops,
            mode,
            status,
            failed,
            ids: Mutex::new(BackingIds::default()),
        }
    }

    fn status(&self) -> std::sync::MutexGuard<'_, PassthroughStatus> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ids(&self) -> std::sync::MutexGuard<'_, BackingIds> {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How to open handle `fh`: by [`way`] when passthrough is active and
    /// `handle` is a regular file (`register` makes a new backing id), else
    /// through the daemon. A failed registration sends only this node
    /// through the daemon while it has a handle open; under
    /// `--passthrough on` it fails the open and stops the mount.
    fn route(
        &self,
        fh: u64,
        handle: &Handle,
        register: impl FnOnce(&Handle) -> io::Result<BackingId>,
    ) -> Route {
        if *self.status() != PassthroughStatus::Active {
            return Route::Daemon;
        }
        let regular = rustix::fs::fstat(&handle.fd)
            .is_ok_and(|st| u32::from(st.st_mode) & 0o170000 == 0o100000);
        if !regular {
            return Route::Daemon;
        }
        let mut ids = self.ids();
        let shared = ids.by_node.get(&handle.node).and_then(Weak::upgrade);
        let daemon_open = ids.daemon_nodes.get(&handle.node).is_some_and(|n| *n > 0);
        match (
            way(
                handle.daemon_only(),
                handle.write,
                shared.is_some(),
                daemon_open,
            ),
            shared,
        ) {
            (Way::Shared, Some(id)) => {
                if handle.write {
                    // Its mapping would not hold the FUSE file: watched
                    // until FORGET instead.
                    self.ops.mapping_may_write(handle.node);
                }
                ids.by_handle.insert(fh, (handle.node, id.clone()));
                return Route::Passthrough(id);
            }
            (Way::Register, _) => {}
            _ => {
                ids.track_daemon(fh, handle.node);
                return Route::Daemon;
            }
        }
        match register(handle) {
            Ok(id) => {
                let id = Arc::new(id);
                ids.by_node.insert(handle.node, Arc::downgrade(&id));
                ids.by_handle.insert(fh, (handle.node, id.clone()));
                Route::Passthrough(id)
            }
            Err(e) => {
                ids.refused += 1;
                if self.mode == Passthrough::On {
                    let reason = format!("registering a backing file failed: {e}");
                    eprintln!(
                        "agentfs: --passthrough on: {reason}; failing the open and stopping \
                         the mount"
                    );
                    *self.status() = PassthroughStatus::Failed(reason);
                    self.failed.stop();
                    return Route::Fatal;
                }
                if ids.refused <= REFUSALS_LOGGED {
                    eprintln!(
                        "agentfs: passthrough refused for inode {}: registering a backing file \
                         failed: {e}; its reads and writes go through the daemon while it is \
                         open{}",
                        handle.node,
                        if ids.refused == REFUSALS_LOGGED {
                            " (further refusals are not logged)"
                        } else {
                            ""
                        }
                    );
                }
                ids.track_daemon(fh, handle.node);
                Route::Daemon
            }
        }
    }

    fn forget_backing(&self, fh: u64) {
        let mut ids = self.ids();
        if let Some((node, id)) = ids.by_handle.remove(&fh) {
            // The last handle on the node closes its backing id.
            drop(id);
            if ids
                .by_node
                .get(&node)
                .is_some_and(|w| w.strong_count() == 0)
            {
                ids.by_node.remove(&node);
            }
        }
        if let Some(node) = ids.daemon_handles.remove(&fh)
            && let Some(n) = ids.daemon_nodes.get_mut(&node)
        {
            *n -= 1;
            if *n == 0 {
                ids.daemon_nodes.remove(&node);
            }
        }
    }
}

fn caller(req: &Request) -> Caller {
    Caller {
        uid: req.uid(),
        gid: req.gid(),
        pid: req.pid(),
    }
}

fn bytes(name: &OsStr) -> &[u8] {
    name.as_bytes()
}

impl Filesystem for FuseFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let caps = config.capabilities();
        for flag in [
            InitFlags::FUSE_ATOMIC_O_TRUNC,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ] {
            if caps.contains(flag) {
                let _ = config.add_capabilities(flag);
            }
        }
        let privileges = Privileges::current();
        match decide_passthrough(
            self.mode,
            caps.contains(InitFlags::FUSE_PASSTHROUGH),
            privileges.passthrough_refusal(),
        ) {
            Ok((true, status)) => {
                let requested = config.set_max_stack_depth(1).map_err(|_| ()).and_then(|_| {
                    config
                        .add_capabilities(InitFlags::FUSE_PASSTHROUGH)
                        .map_err(|_| ())
                });
                *self.status() = match requested {
                    Ok(()) => status,
                    Err(()) => PassthroughStatus::Off("the kernel refused it at INIT".into()),
                };
                if self.mode == Passthrough::On && *self.status() != PassthroughStatus::Active {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "--passthrough on: the kernel refused it at INIT",
                    ));
                }
            }
            Ok((false, status)) => *self.status() = status,
            Err(reason) => {
                *self.status() = PassthroughStatus::Off(reason.clone());
                return Err(io::Error::new(io::ErrorKind::Unsupported, reason));
            }
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.ops.lookup(parent.0, bytes(name)) {
            Ok(e) => reply.entry_with_ttls(&ATTR_TTL, &ENTRY_TTL, &e.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.ops.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.ops.getattr(ino.0, fh.map(|f| f.0)) {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        req: &Request,
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
        let set = SetAttr {
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
            fh: fh.map(|f| f.0),
        };
        match self.ops.setattr(ino.0, &set, caller(req)) {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.ops.readlink(ino.0) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(e),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        match self
            .ops
            .mknod(parent.0, bytes(name), mode, rdev, caller(req))
        {
            Ok(e) => reply.entry_with_ttls(&ATTR_TTL, &ENTRY_TTL, &e.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
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
        match self.ops.mkdir(parent.0, bytes(name), mode, caller(req)) {
            Ok(e) => reply.entry_with_ttls(&ATTR_TTL, &ENTRY_TTL, &e.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.ops.unlink(parent.0, bytes(name), caller(req)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.ops.rmdir(parent.0, bytes(name), caller(req)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match self.ops.symlink(
            parent.0,
            bytes(link_name),
            target.as_os_str().as_bytes(),
            caller(req),
        ) {
            Ok(e) => reply.entry_with_ttls(&ATTR_TTL, &ENTRY_TTL, &e.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self.ops.rename(
            parent.0,
            bytes(name),
            newparent.0,
            bytes(newname),
            flags.bits(),
            caller(req),
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self
            .ops
            .link(ino.0, newparent.0, bytes(newname), caller(req))
        {
            Ok(e) => reply.entry_with_ttls(&ATTR_TTL, &ENTRY_TTL, &e.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.ops.open(ino.0, flags.0, caller(req)) {
            Ok((fh, handle)) => match self.route(fh, &handle, |h| reply.open_backing(&h.fd)) {
                Route::Passthrough(id) => {
                    reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &id);
                }
                Route::Daemon => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Route::Fatal => {
                    let _ = self.ops.release(fh);
                    reply.error(Errno::EIO);
                }
            },
            Err(e) => reply.error(e),
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
        match self
            .ops
            .create(parent.0, bytes(name), mode, flags, caller(req))
        {
            Ok((entry, fh, handle)) => {
                match self.route(fh, &handle, |h| reply.open_backing(&h.fd)) {
                    Route::Passthrough(id) => reply.created_passthrough(
                        &ATTR_TTL,
                        &entry.attr,
                        Generation(0),
                        FileHandle(fh),
                        FopenFlags::empty(),
                        &id,
                    ),
                    Route::Daemon => reply.created(
                        &ATTR_TTL,
                        &entry.attr,
                        Generation(0),
                        FileHandle(fh),
                        FopenFlags::empty(),
                    ),
                    Route::Fatal => {
                        let _ = self.ops.release(fh);
                        self.ops.forget(entry.id, 1);
                        reply.error(Errno::EIO);
                    }
                }
            }
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.ops.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.ops.write(fh.0, offset, data, caller(req)) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.ops.flush(fh.0, caller(req)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.ops.release(fh.0);
        self.forget_backing(fh.0);
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.ops.fsync(fh.0, datasync) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.ops.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        match self.ops.readdir(fh.0, offset) {
            Ok(entries) => {
                for (next, entry) in entries {
                    if reply.add(
                        INodeNo(entry.id),
                        next,
                        entry.kind,
                        OsStr::from_bytes(&entry.name),
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.ops.releasedir(fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.ops.statfs() {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                u32::try_from(s.f_bsize).unwrap_or(4096),
                u32::try_from(s.f_namemax).unwrap_or(255),
                u32::try_from(s.f_frsize).unwrap_or(4096),
            ),
            Err(e) => reply.error(e),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        match self.ops.setxattr(ino.0, bytes(name), value, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.ops.getxattr(ino.0, bytes(name), size) {
            Ok(Xattr::Size(n)) => reply.size(n),
            Ok(Xattr::Data(d)) => reply.data(&d),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.ops.listxattr(ino.0, size) {
            Ok(Xattr::Size(n)) => reply.size(n),
            Ok(Xattr::Data(d)) => reply.data(&d),
            Err(e) => reply.error(e),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.ops.removexattr(ino.0, bytes(name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Not called with default_permissions; the kernel checks modes.
        reply.ok();
    }

    fn fallocate(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        match self.ops.fallocate(fh.0, offset, length, mode, caller(req)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        match self.ops.lseek(fh.0, offset, whence) {
            Ok(at) => reply.offset(at),
            Err(e) => reply.error(e),
        }
    }

    fn copy_file_range(
        &self,
        req: &Request,
        _ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        _ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        match self
            .ops
            .copy_file_range(fh_in.0, offset_in, fh_out.0, offset_out, len, caller(req))
        {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::fd::OwnedFd;

    use agentfs_core::events::MemorySink;
    use agentfs_core::identity::Context;
    use agentfs_core::telemetry::Telemetry;
    use agentfs_core::volume::{self, StateDir};

    use super::*;
    use crate::backing::Backing;
    use crate::ops::OpsConfig;

    const NO_PRIVILEGE: &str = "the daemon lacks CAP_SYS_ADMIN";

    #[test]
    fn passthrough_decisions() {
        use Passthrough::*;
        assert_eq!(
            decide_passthrough(Auto, true, None),
            Ok((true, PassthroughStatus::Active))
        );
        assert_eq!(
            decide_passthrough(On, true, None),
            Ok((true, PassthroughStatus::Active))
        );
        let (req, status) = decide_passthrough(Auto, false, None).unwrap();
        assert!(!req);
        assert!(status.to_string().contains("6.9"), "{status}");
        let (req, status) = decide_passthrough(Auto, true, Some(NO_PRIVILEGE)).unwrap();
        assert!(!req);
        assert!(status.to_string().contains("CAP_SYS_ADMIN"), "{status}");
        assert!(
            decide_passthrough(On, false, None)
                .unwrap_err()
                .contains("6.9")
        );
        assert!(
            decide_passthrough(On, true, Some(NO_PRIVILEGE))
                .unwrap_err()
                .contains("CAP_SYS_ADMIN")
        );
        let (req, status) = decide_passthrough(Off, true, Some(NO_PRIVILEGE)).unwrap();
        assert!(!req);
        assert_eq!(status.to_string(), "off (disabled by --passthrough off)");
        assert_eq!(PassthroughStatus::Active.to_string(), "on");
    }

    #[test]
    fn passthrough_parses() {
        assert_eq!("auto".parse(), Ok(Passthrough::Auto));
        assert_eq!("on".parse(), Ok(Passthrough::On));
        assert_eq!("off".parse(), Ok(Passthrough::Off));
        assert!("yes".parse::<Passthrough>().is_err());
    }

    struct Fx {
        _dir: tempfile::TempDir,
        fs: FuseFs,
        failed: Arc<Stop>,
        file: std::path::PathBuf,
    }

    fn fx(mode: Passthrough) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir_all(&backing).unwrap();
        let file = backing.join("f");
        std::fs::write(&file, "x").unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        let (recorder, _committer, _) = volume::open(
            &state,
            &backing,
            Context {
                volume: "vol".into(),
                replica: "pod-0".into(),
                pod_uid: "u".into(),
                ..Context::default()
            },
            Arc::new(MemorySink::new()),
            &Telemetry::noop(),
        )
        .unwrap();
        let ops = Ops::new(
            Backing::open(&backing).unwrap(),
            recorder,
            OpsConfig::default(),
        )
        .unwrap();
        let status = Arc::new(Mutex::new(PassthroughStatus::Active));
        let failed = Arc::new(Stop::default());
        Fx {
            _dir: dir,
            fs: FuseFs::new(Arc::new(ops), mode, status, failed.clone()),
            failed,
            file,
        }
    }

    fn handle(fx: &Fx, node: u64) -> Handle {
        let fd: OwnedFd = std::fs::File::open(&fx.file).unwrap().into();
        Handle::new(node, fd, false, None, 0)
    }

    fn refuse(_: &Handle) -> io::Result<BackingId> {
        Err(io::Error::from_raw_os_error(1))
    }

    #[test]
    fn a_handle_that_can_write_never_gets_passthrough_of_its_own() {
        // (daemon_only, write, backing_open, daemon_open) -> way
        let table = [
            ((false, false, false, false), Way::Register),
            ((false, false, false, true), Way::Daemon),
            ((false, false, true, false), Way::Shared),
            // A writer: through the daemon, unless the kernel forbids it
            // (a backing id already open on the inode).
            ((false, true, false, false), Way::Daemon),
            ((false, true, false, true), Way::Daemon),
            ((false, true, true, false), Way::Shared),
            // An fsync-class file: always the daemon.
            ((true, false, false, false), Way::Daemon),
            ((true, true, false, false), Way::Daemon),
            ((true, false, true, false), Way::Daemon),
        ];
        for ((daemon_only, write, backing_open, daemon_open), want) in table {
            assert_eq!(
                way(daemon_only, write, backing_open, daemon_open),
                want,
                "daemon_only {daemon_only}, write {write}, backing_open {backing_open}, \
                 daemon_open {daemon_open}"
            );
        }
    }

    #[test]
    fn a_writer_goes_through_the_daemon_and_readers_follow_it() {
        let fx = fx(Passthrough::On);
        let fd: OwnedFd = std::fs::OpenOptions::new()
            .write(true)
            .open(&fx.file)
            .unwrap()
            .into();
        let writer = Handle::new(7, fd, true, None, 0);
        let tried = Cell::new(false);
        let route = fx.fs.route(1, &writer, |_| {
            tried.set(true);
            refuse(&writer)
        });
        assert!(matches!(route, Route::Daemon));
        assert!(!tried.get(), "a writer registered a backing file");
        // Not a failure, even under `on`.
        assert!(!fx.failed.is_stopped());
        assert_eq!(*fx.fs.status(), PassthroughStatus::Active);
        // A reader of the same inode follows it while it is open.
        let reader = handle(&fx, 7);
        let route = fx.fs.route(2, &reader, |_| {
            tried.set(true);
            refuse(&reader)
        });
        assert!(matches!(route, Route::Daemon));
        assert!(!tried.get());
        // Once the writer is closed, a reader tries passthrough again.
        fx.fs.forget_backing(1);
        fx.fs.forget_backing(2);
        let _ = fx.fs.route(3, &reader, |_| {
            tried.set(true);
            refuse(&reader)
        });
        assert!(tried.get());
    }

    #[test]
    fn a_refused_registration_falls_back_for_that_inode_only() {
        let fx = fx(Passthrough::Auto);
        let h = handle(&fx, 7);
        assert!(matches!(fx.fs.route(1, &h, refuse), Route::Daemon));
        // Passthrough stays on for the mount.
        assert_eq!(*fx.fs.status(), PassthroughStatus::Active);
        // While a daemon handle is open on node 7, no registration is even
        // tried there (the kernel would refuse mixing the two).
        let tried = Cell::new(false);
        let route = fx.fs.route(2, &h, |_| {
            tried.set(true);
            refuse(&h)
        });
        assert!(matches!(route, Route::Daemon));
        assert!(!tried.get());
        // Another node still tries.
        let other = handle(&fx, 8);
        fx.fs.route(3, &other, |_| {
            tried.set(true);
            refuse(&other)
        });
        assert!(tried.get());
        // Once node 7's daemon handles are closed, it tries again.
        fx.fs.forget_backing(1);
        fx.fs.forget_backing(2);
        tried.set(false);
        fx.fs.route(4, &h, |_| {
            tried.set(true);
            refuse(&h)
        });
        assert!(tried.get());
        assert!(!fx.failed.is_stopped());
    }

    #[test]
    fn a_refused_registration_under_on_stops_the_mount() {
        let fx = fx(Passthrough::On);
        let h = handle(&fx, 7);
        assert!(matches!(fx.fs.route(1, &h, refuse), Route::Fatal));
        assert!(fx.failed.is_stopped());
        assert!(
            matches!(&*fx.fs.status(), PassthroughStatus::Failed(r) if r.contains("registering")),
            "{}",
            fx.fs.status()
        );
    }
}
