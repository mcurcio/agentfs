//! Every filesystem operation, without the kernel (design §2.1, §3.1, §6).
//!
//! [`Ops`] works on the backing tree ([`crate::backing`]) and keeps the
//! inode table ([`crate::inodes`]). Each operation returns `Result<T,
//! Errno>`; the fuser adapter ([`crate::fuse`]) only maps that to a reply,
//! so all behaviour here is unit-tested against a real directory and a real
//! state area, without `/dev/fuse`.
//!
//! **Change boundaries** (design §3.1) record synchronously, before the
//! operation returns:
//!
//! | Operation | Recorded |
//! |---|---|
//! | `flush` of a changed inode | content and executable bit, at every current path (`close`) |
//! | `rename` | source removed, destination state (a directory: every file under it) |
//! | `unlink`, `rmdir` | removal |
//! | `link` | the new path's content |
//! | `symlink` | the link and its target |
//! | `setattr` of size, or of the executable bit of a file | content and mode |
//! | `mknod` of a regular file | its (empty) content |
//! | `release` of the last handle, if changed since the last record | content (late) |
//!
//! `create` marks the new inode so its first `flush` records it even when
//! empty; an `open` with `O_TRUNC` likewise. `mkdir`, `chown`, `utimens`,
//! `chmod` of other bits and extended attributes pass through unrecorded.
//!
//! **Permissions** (design §6) are checked by the kernel
//! (`default_permissions`) before a request arrives; the daemon then acts
//! with its own privileges. When it has `CAP_CHOWN` and `CAP_FOWNER` (root;
//! [`crate::privileges`]) it gives new objects to the caller, keeping the
//! group the backing filesystem chose under a setgid directory, then sets
//! the exact mode the caller asked for. Both are done on a descriptor of
//! the new object, opened without following and checked to be the object
//! just made, never by name: the backing tree may change under the daemon
//! (design §6 "Confinement"). `create` never takes over an existing file.
//!
//! **Locking.** The inode table and the handle map are each behind a mutex
//! that is never held across a system call on the tree or a record.
//! `names` orders path resolution against renames: rename holds it for
//! writing while it renames and updates the table; every other operation
//! holds it for reading while it resolves paths, changes the tree and
//! records, so a record is never made against paths a concurrent rename has
//! already changed.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use agentfs_core::record::{Caller, Trigger};
use agentfs_core::volume::Recorder;
use fuser::{Errno, FileAttr, FileType, TimeOrNow};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, Stat, Timespec, Timestamps};

use crate::backing::{self, Backing, attr, join, proc_path};
use crate::boundary::{Boundary, Change, REPAIR_TRIGGER, State};
use crate::dirty::{Stamp, now_ns, timestamp_granularity_ns};
use crate::inodes::{InodeTable, Kind, ROOT};
use crate::names::is_git_dir_name;
use crate::privileges::Privileges;

pub type OpResult<T> = Result<T, Errno>;

/// The caller of late records whose writer is unknown.
const NOBODY: Caller = Caller {
    uid: 0,
    gid: 0,
    pid: 0,
};

/// A directory rename records at most this many files per record; a larger
/// tree is recorded as several consecutive records (each holds one open
/// descriptor per file while it is made).
pub const RENAME_CHUNK: usize = 4096;

const S_ISGID: u32 = 0o2000;

/// Tunables, injectable for tests.
#[derive(Debug, Clone)]
pub struct OpsConfig {
    /// Timestamp granularity for the racy-stamp rule ([`crate::dirty`]).
    pub granularity_ns: i128,
    /// Files per record when a directory is renamed.
    pub chunk: usize,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            granularity_ns: timestamp_granularity_ns(),
            chunk: RENAME_CHUNK,
        }
    }
}

/// An open file.
#[derive(Debug)]
pub struct Handle {
    pub node: u64,
    pub fd: OwnedFd,
    pub write: bool,
}

/// A node as a lookup-style reply describes it.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub id: u64,
    pub attr: FileAttr,
}

/// One directory entry, as `readdir` lists it.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub id: u64,
    pub kind: FileType,
    pub name: Vec<u8>,
}

/// Attribute changes, as `setattr` carries them.
#[derive(Debug, Clone, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeOrNow>,
    pub mtime: Option<TimeOrNow>,
    pub fh: Option<u64>,
}

/// An extended-attribute reply: the size a value or list needs, or the
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Xattr {
    Size(u32),
    Data(Vec<u8>),
}

/// What one sweep did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// Open, changed files recorded.
    pub recorded: usize,
    /// Queued paths repaired.
    pub repaired: usize,
    /// Queued paths still failing.
    pub still_queued: usize,
}

/// A descriptor for a node: freshly opened by path, or an open handle's
/// (for a file with no name left).
enum NodeFd {
    Owned(OwnedFd),
    Handle(Arc<Handle>),
}

impl AsFd for NodeFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Owned(fd) => fd.as_fd(),
            Self::Handle(h) => h.fd.as_fd(),
        }
    }
}

/// The filesystem.
pub struct Ops {
    backing: Backing,
    table: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    dirs: Mutex<HashMap<u64, Arc<Vec<DirEntry>>>>,
    next_fh: AtomicU64,
    names: RwLock<()>,
    boundary: Boundary,
    config: OpsConfig,
    /// The daemon may give new objects to their caller
    /// ([`crate::privileges`]).
    chown_new: bool,
    /// The daemon's effective uid: the owner of every object it creates.
    euid: u32,
}

/// Converts a rustix result.
trait Eno<T> {
    fn eno(self) -> OpResult<T>;
}

impl<T> Eno<T> for rustix::io::Result<T> {
    fn eno(self) -> OpResult<T> {
        self.map_err(|e| Errno::from_i32(e.raw_os_error()))
    }
}

fn errno(e: rustix::io::Errno) -> Errno {
    Errno::from_i32(e.raw_os_error())
}

fn estale() -> Errno {
    errno(rustix::io::Errno::STALE)
}

fn stamp(st: &Stat) -> Stamp {
    Stamp::from_stat(st)
}

fn mode_of(st: &Stat) -> u32 {
    u32::from(st.st_mode)
}

fn dev_ino(st: &Stat) -> (u64, u64) {
    (u64::from(st.st_dev), u64::from(st.st_ino))
}

fn executable(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// A single name from the kernel: never empty, `.`, `..` or with a `/`.
fn check_name(name: &[u8]) -> OpResult<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// Whether `caller` may open the file `st` with access mode `acc`
/// (`O_RDONLY`, `O_WRONLY`, `O_RDWR`), and `truncate`, by its mode bits.
/// Root (uid 0) always may.
fn may_open(st: &Stat, caller: Caller, acc: i32, truncate: bool) -> bool {
    if caller.uid == 0 {
        return true;
    }
    let mode = mode_of(st);
    let bits = if st.st_uid == caller.uid {
        (mode >> 6) & 0o7
    } else if st.st_gid == caller.gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    let read = acc != 1;
    let write = acc != 0 || truncate;
    (!read || bits & 0o4 != 0) && (!write || bits & 0o2 != 0)
}

fn timespec(t: Option<TimeOrNow>) -> Timespec {
    match t {
        None => Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        Some(TimeOrNow::Now) => Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_NOW,
        },
        Some(TimeOrNow::SpecificTime(t)) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Timespec {
                tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: d.subsec_nanos().into(),
            },
            Err(e) => {
                let d = e.duration();
                let mut secs = -i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
                let mut nanos = i64::from(d.subsec_nanos());
                if nanos > 0 {
                    secs -= 1;
                    nanos = 1_000_000_000 - nanos;
                }
                Timespec {
                    tv_sec: secs,
                    tv_nsec: nanos,
                }
            }
        },
    }
}

impl Ops {
    /// The filesystem over the backing tree `backing`, recording through
    /// `recorder`.
    pub fn new(backing: Backing, recorder: Recorder, config: OpsConfig) -> io::Result<Self> {
        let st = rustix::fs::fstat(backing.root())?;
        let (dev, ino) = dev_ino(&st);
        let tree = backing.path().to_path_buf();
        Ok(Self {
            backing,
            table: Mutex::new(InodeTable::new(dev, ino)),
            handles: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            names: RwLock::new(()),
            boundary: Boundary::new(recorder, tree),
            config,
            chown_new: Privileges::current().can_chown_new(),
            euid: rustix::process::geteuid().as_raw(),
        })
    }

    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    pub fn boundary(&self) -> &Boundary {
        &self.boundary
    }

    fn table(&self) -> MutexGuard<'_, InodeTable> {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn handles(&self) -> MutexGuard<'_, HashMap<u64, Arc<Handle>>> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_names(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        self.names
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_names(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.names
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn handle(&self, fh: u64) -> OpResult<Arc<Handle>> {
        self.handles().get(&fh).cloned().ok_or(Errno::EBADF)
    }

    /// Opens node `id` by its current path with `flags`, checking that the
    /// path still leads to the node's backing inode (`ESTALE` if it does
    /// not).
    fn open_node(&self, id: u64, flags: OFlags) -> OpResult<OwnedFd> {
        let (path, dev, ino) = {
            let table = self.table();
            let node = table.get(id).ok_or(Errno::ENOENT)?;
            (table.path(id).ok_or(Errno::ENOENT)?, node.dev, node.ino)
        };
        let fd = self.backing.open_at(&path, flags, Mode::empty())?;
        let st = rustix::fs::fstat(&fd).eno()?;
        if dev_ino(&st) != (dev, ino) {
            return Err(estale());
        }
        Ok(fd)
    }

    fn open_dir_node(&self, id: u64) -> OpResult<OwnedFd> {
        self.open_node(id, OFlags::PATH | OFlags::DIRECTORY)
    }

    /// A descriptor for node `id`: by path, or through an open handle when
    /// the node has no name left (an unlinked open file).
    fn node_fd(&self, id: u64) -> OpResult<NodeFd> {
        match self.open_node(id, OFlags::PATH) {
            Ok(fd) => Ok(NodeFd::Owned(fd)),
            Err(e) => {
                let fh = self
                    .table()
                    .get(id)
                    .and_then(|n| n.handles.first().copied());
                match fh.and_then(|fh| self.handles().get(&fh).cloned()) {
                    Some(h) => Ok(NodeFd::Handle(h)),
                    None => Err(e),
                }
            }
        }
    }

    fn path_of(&self, id: u64) -> OpResult<Vec<u8>> {
        self.table().path(id).ok_or(Errno::ENOENT)
    }

    fn kind_of(&self, id: u64) -> OpResult<Kind> {
        Ok(self.table().get(id).ok_or(Errno::ENOENT)?.kind)
    }

    // ---- lookups and attributes ----

    pub fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let st =
            rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        Ok(self.entry(parent, name, &st))
    }

    /// Counts a kernel lookup of `name` in `parent`, now `st`.
    fn entry(&self, parent: u64, name: &[u8], st: &Stat) -> Entry {
        let (dev, ino) = dev_ino(st);
        let id = self
            .table()
            .lookup(parent, name, dev, ino, Kind::from_mode(mode_of(st)));
        Entry {
            id,
            attr: attr(id, st),
        }
    }

    pub fn forget(&self, id: u64, n: u64) {
        self.table().forget(id, n);
    }

    pub fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr> {
        let st = match fh.and_then(|fh| self.handle(fh).ok()) {
            Some(h) => rustix::fs::fstat(&h.fd).eno()?,
            None => {
                // Resolving the path must not race a rename of an ancestor.
                let _names = self.read_names();
                rustix::fs::fstat(self.node_fd(id)?).eno()?
            }
        };
        Ok(attr(id, &st))
    }

    pub fn setattr(&self, id: u64, set: &SetAttr, caller: Caller) -> OpResult<FileAttr> {
        let names = self.read_names();
        let kind = self.kind_of(id)?;
        let fd = match set.fh.and_then(|fh| self.handle(fh).ok()) {
            Some(h) => NodeFd::Handle(h),
            None => self.node_fd(id)?,
        };
        let before = rustix::fs::fstat(&fd).eno()?;
        if set.uid.is_some() || set.gid.is_some() {
            rustix::fs::chownat(
                &fd,
                "",
                set.uid.map(rustix::fs::Uid::from_raw),
                set.gid.map(rustix::fs::Gid::from_raw),
                AtFlags::EMPTY_PATH,
            )
            .eno()?;
        }
        if let Some(mode) = set.mode {
            if kind == Kind::Symlink {
                return Err(errno(rustix::io::Errno::OPNOTSUPP));
            }
            rustix::fs::chmodat(
                rustix::fs::CWD,
                proc_path(&fd),
                Mode::from_raw_mode(mode & 0o7777),
                AtFlags::empty(),
            )
            .eno()?;
        }
        if let Some(size) = set.size {
            if kind != Kind::File {
                return Err(Errno::EINVAL);
            }
            match &fd {
                NodeFd::Handle(h) if h.write => rustix::fs::ftruncate(&h.fd, size).eno()?,
                _ => {
                    let w = rustix::fs::open(
                        proc_path(&fd),
                        OFlags::WRONLY | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .eno()?;
                    rustix::fs::ftruncate(&w, size).eno()?;
                }
            }
            if let Some(node) = self.table().get_mut(id) {
                node.dirty.arm(caller);
            }
        }
        if set.atime.is_some() || set.mtime.is_some() {
            self.utimens(id, &fd, kind, set)?;
        }
        let after = rustix::fs::fstat(&fd).eno()?;
        let record = kind == Kind::File
            && (set.size.is_some() || executable(mode_of(&before)) != executable(mode_of(&after)));
        if record {
            let paths = self.table().paths(id);
            let changes = paths
                .iter()
                .map(|p| Change {
                    path: p.clone(),
                    state: State::File {
                        backing: proc_path(&fd),
                        executable: executable(mode_of(&after)),
                    },
                })
                .collect();
            let now = now_ns();
            match self.boundary.record(Trigger::Setattr, caller, changes) {
                Ok(_) => {
                    if let Some(node) = self.table().get_mut(id) {
                        node.dirty
                            .after_record(stamp(&after), now, self.config.granularity_ns);
                    }
                }
                Err(_) => {
                    self.boundary.queue_repair(paths, caller);
                    return Err(Errno::EIO);
                }
            }
        }
        drop(names);
        Ok(attr(id, &after))
    }

    fn utimens(&self, id: u64, fd: &NodeFd, kind: Kind, set: &SetAttr) -> OpResult<()> {
        let times = Timestamps {
            last_access: timespec(set.atime),
            last_modification: timespec(set.mtime),
        };
        let link = self
            .table()
            .get(id)
            .and_then(|n| n.links().first().cloned());
        match link {
            // By name in its parent, so a symlink's own times are set.
            Some((parent, name)) => {
                let dir = self.open_dir_node(parent)?;
                rustix::fs::utimensat(
                    &dir,
                    OsStr::from_bytes(&name),
                    &times,
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .eno()
            }
            None if kind != Kind::Symlink => {
                rustix::fs::utimensat(rustix::fs::CWD, proc_path(fd), &times, AtFlags::empty())
                    .eno()
            }
            None => Err(Errno::ENOENT),
        }
    }

    pub fn readlink(&self, id: u64) -> OpResult<Vec<u8>> {
        let _names = self.read_names();
        let fd = self.node_fd(id)?;
        let target = rustix::fs::readlinkat(&fd, "", Vec::new()).eno()?;
        Ok(target.into_bytes())
    }

    // ---- creating ----

    /// Gives a new object to its caller and sets the mode it asked for
    /// (design §6). `fd` is the object itself when it is open; otherwise
    /// `name` in `dir` is opened first ([`Ops::open_new`]), so neither the
    /// owner nor the mode is ever changed by name: a name swapped for a
    /// symlink in the backing tree meanwhile is never followed.
    #[allow(clippy::too_many_arguments)]
    fn own_new(
        &self,
        dir: &OwnedFd,
        name: &[u8],
        fd: Option<&OwnedFd>,
        parent: &Stat,
        caller: Caller,
        mode: u32,
        kind: Kind,
    ) -> OpResult<()> {
        if !self.chown_new && kind == Kind::Symlink {
            return Ok(());
        }
        let opened;
        let fd = match fd {
            Some(fd) => fd,
            None => {
                opened = self.open_new(dir, name, kind)?;
                &opened
            }
        };
        if self.chown_new {
            let uid = Some(rustix::fs::Uid::from_raw(caller.uid));
            // Under a setgid directory the filesystem already chose the group.
            let gid =
                (mode_of(parent) & S_ISGID == 0).then(|| rustix::fs::Gid::from_raw(caller.gid));
            // On the descriptor itself: an `O_PATH` symlink is changed, not
            // followed.
            rustix::fs::chownat(fd, "", uid, gid, AtFlags::EMPTY_PATH).eno()?;
        }
        if kind == Kind::Symlink {
            return Ok(());
        }
        let current = rustix::fs::fstat(fd).eno()?;
        let mut want = mode & 0o7777;
        if kind == Kind::Dir {
            want |= mode_of(&current) & S_ISGID;
        }
        if mode_of(&current) & 0o7777 != want {
            // Never a symlink here ([`Ops::open_new`] checked the type), so
            // the magic link reaches exactly this inode.
            rustix::fs::chmodat(
                rustix::fs::CWD,
                proc_path(fd),
                Mode::from_raw_mode(want),
                AtFlags::empty(),
            )
            .eno()?;
        }
        Ok(())
    }

    /// Opens `name` in `dir`, just created as a `kind`, without following
    /// it (`O_PATH | O_NOFOLLOW`), and checks it is still the object this
    /// daemon made: of that type, owned by the daemon and, unless a
    /// directory, with a single link. Anything else means the name was
    /// replaced in the backing tree between the creation and now (a symlink
    /// or hard link to a file elsewhere): `ESTALE`, and nothing is changed.
    fn open_new(&self, dir: &OwnedFd, name: &[u8], kind: Kind) -> OpResult<OwnedFd> {
        let fd = rustix::fs::openat(
            dir,
            OsStr::from_bytes(name),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .eno()?;
        let st = rustix::fs::fstat(&fd).eno()?;
        let same_kind = Kind::from_mode(mode_of(&st)) == kind;
        let ours = st.st_uid == self.euid;
        let single = kind == Kind::Dir || st.st_nlink == 1;
        if !(same_kind && ours && single) {
            return Err(estale());
        }
        Ok(fd)
    }

    /// Undoes the lookup count of an entry the kernel never received.
    fn unlookup(&self, id: u64) {
        self.table().forget(id, 1);
    }

    pub fn mkdir(&self, parent: u64, name: &[u8], mode: u32, caller: Caller) -> OpResult<Entry> {
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        rustix::fs::mkdirat(
            &dir,
            OsStr::from_bytes(name),
            Mode::from_raw_mode(mode & 0o7777),
        )
        .eno()?;
        self.own_new(&dir, name, None, &pst, caller, mode, Kind::Dir)?;
        let st =
            rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        Ok(self.entry(parent, name, &st))
    }

    pub fn mknod(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        caller: Caller,
    ) -> OpResult<Entry> {
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        let kind = Kind::from_mode(mode);
        rustix::fs::mknodat(
            &dir,
            OsStr::from_bytes(name),
            rustix::fs::FileType::from_raw_mode(mode),
            Mode::from_raw_mode(mode & 0o7777),
            u64::from(rdev),
        )
        .eno()?;
        self.own_new(&dir, name, None, &pst, caller, mode, kind)?;
        let st =
            rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        let entry = self.entry(parent, name, &st);
        if kind == Kind::File {
            // No handle, so no FLUSH will follow: record the new (empty)
            // file now.
            let path = join(&self.path_of(parent)?, name);
            let fd = self.backing.open_path(&path)?;
            let change = Change {
                path: path.clone(),
                state: State::File {
                    backing: proc_path(&fd),
                    executable: executable(mode_of(&st)),
                },
            };
            if self
                .boundary
                .record(Trigger::Close, caller, vec![change])
                .is_err()
            {
                self.boundary.queue_repair([path], caller);
                self.unlookup(entry.id);
                return Err(Errno::EIO);
            }
        }
        Ok(entry)
    }

    pub fn symlink(
        &self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        caller: Caller,
    ) -> OpResult<Entry> {
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        rustix::fs::symlinkat(OsStr::from_bytes(target), &dir, OsStr::from_bytes(name)).eno()?;
        self.own_new(&dir, name, None, &pst, caller, 0o777, Kind::Symlink)?;
        let st =
            rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        let entry = self.entry(parent, name, &st);
        let path = join(&self.path_of(parent)?, name);
        let change = Change {
            path: path.clone(),
            state: State::Symlink {
                target: target.to_vec(),
            },
        };
        if self
            .boundary
            .record(Trigger::Symlink, caller, vec![change])
            .is_err()
        {
            self.boundary.queue_repair([path], caller);
            self.unlookup(entry.id);
            return Err(Errno::EIO);
        }
        Ok(entry)
    }

    pub fn link(&self, id: u64, parent: u64, name: &[u8], caller: Caller) -> OpResult<Entry> {
        check_name(name)?;
        let _names = self.read_names();
        let (sparent, sname) = self
            .table()
            .get(id)
            .and_then(|n| n.links().first().cloned())
            .ok_or(Errno::ENOENT)?;
        let sdir = self.open_dir_node(sparent)?;
        let sst = rustix::fs::statat(&sdir, OsStr::from_bytes(&sname), AtFlags::SYMLINK_NOFOLLOW)
            .eno()?;
        let expected = self.table().get(id).map(|n| (n.dev, n.ino));
        if Some(dev_ino(&sst)) != expected {
            return Err(estale());
        }
        let dir = self.open_dir_node(parent)?;
        rustix::fs::linkat(
            &sdir,
            OsStr::from_bytes(&sname),
            &dir,
            OsStr::from_bytes(name),
            AtFlags::empty(),
        )
        .eno()?;
        let st =
            rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        let entry = self.entry(parent, name, &st);
        let path = join(&self.path_of(parent)?, name);
        let fd = self.backing.open_path(&path)?;
        let state = match Kind::from_mode(mode_of(&st)) {
            Kind::File => State::File {
                backing: proc_path(&fd),
                executable: executable(mode_of(&st)),
            },
            Kind::Symlink => State::Symlink {
                target: rustix::fs::readlinkat(&fd, "", Vec::new())
                    .eno()?
                    .into_bytes(),
            },
            _ => return Ok(entry),
        };
        let change = Change {
            path: path.clone(),
            state,
        };
        if self
            .boundary
            .record(Trigger::Link, caller, vec![change])
            .is_err()
        {
            self.boundary.queue_repair([path], caller);
            self.unlookup(entry.id);
            return Err(Errno::EIO);
        }
        Ok(entry)
    }

    // ---- removing and renaming ----

    pub fn unlink(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.remove(parent, name, false, caller)
    }

    pub fn rmdir(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.remove(parent, name, true, caller)
    }

    fn remove(&self, parent: u64, name: &[u8], dir: bool, caller: Caller) -> OpResult<()> {
        check_name(name)?;
        let _names = self.read_names();
        let pdir = self.open_dir_node(parent)?;
        let path = join(&self.path_of(parent)?, name);
        let flags = if dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(&pdir, OsStr::from_bytes(name), flags).eno()?;
        self.table().remove_link(parent, name);
        let trigger = if dir { Trigger::Rmdir } else { Trigger::Unlink };
        if self
            .boundary
            .record(trigger, caller, vec![Change::removed(path.clone())])
            .is_err()
        {
            self.boundary.queue_repair([path], caller);
            return Err(Errno::EIO);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        caller: Caller,
    ) -> OpResult<()> {
        check_name(name)?;
        check_name(newname)?;
        let flags = RenameFlags::from_bits(flags).ok_or(Errno::EINVAL)?;
        if flags.contains(RenameFlags::WHITEOUT) {
            return Err(Errno::EINVAL);
        }
        let exchange = flags.contains(RenameFlags::EXCHANGE);
        let names = self.write_names();
        let sdir = self.open_dir_node(parent)?;
        let ddir = self.open_dir_node(newparent)?;
        let src =
            rustix::fs::statat(&sdir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        let dst =
            rustix::fs::statat(&ddir, OsStr::from_bytes(newname), AtFlags::SYMLINK_NOFOLLOW).ok();
        let src_path = join(&self.path_of(parent)?, name);
        let dst_path = join(&self.path_of(newparent)?, newname);
        rustix::fs::renameat_with(
            &sdir,
            OsStr::from_bytes(name),
            &ddir,
            OsStr::from_bytes(newname),
            flags,
        )
        .eno()?;
        // Renaming one name of an inode over another of its names changes
        // nothing (POSIX).
        if dst.as_ref().is_some_and(|d| dev_ino(d) == dev_ino(&src)) {
            return Ok(());
        }
        self.table()
            .rename(parent, name, newparent, newname, exchange);
        drop(names);

        let _names = self.read_names();
        let (removed, roots) = if exchange {
            (
                vec![src_path.clone(), dst_path.clone()],
                vec![src_path.clone(), dst_path.clone()],
            )
        } else if Kind::from_mode(mode_of(&src)) == Kind::Dir {
            (
                vec![src_path.clone(), dst_path.clone()],
                vec![dst_path.clone()],
            )
        } else {
            (vec![src_path.clone()], vec![dst_path.clone()])
        };
        if self
            .record_tree(Trigger::Rename, caller, removed, &roots)
            .is_err()
        {
            self.boundary.queue_repair([src_path, dst_path], caller);
            return Err(Errno::EIO);
        }
        Ok(())
    }

    // ---- deriving state from the tree ----

    /// Records `removed` (as removals) followed by the current state of
    /// each of `roots`, from the backing tree: a file's content, a
    /// symlink's target, every file and symlink under a directory (never
    /// inside a `.git` directory), or a removal if the path is gone. Large
    /// trees are recorded in several records of [`OpsConfig::chunk`] files,
    /// the first carrying the removals.
    fn record_tree(
        &self,
        trigger: Trigger,
        caller: Caller,
        removed: Vec<Vec<u8>>,
        roots: &[Vec<u8>],
    ) -> Result<(), ()> {
        let mut leaves = Vec::new();
        for root in roots {
            match self.backing.open_path(root) {
                Ok(fd) => {
                    let st = rustix::fs::fstat(&fd).map_err(|_| ())?;
                    if Kind::from_mode(mode_of(&st)) == Kind::Dir {
                        self.list_tree(root, &mut leaves).map_err(|_| ())?;
                    } else {
                        leaves.push(root.clone());
                    }
                }
                Err(_) => leaves.push(root.clone()),
            }
        }
        let mut first: Vec<Change> = removed.into_iter().map(Change::removed).collect();
        let chunk = self.config.chunk.max(1);
        let mut chunks = leaves.chunks(chunk).peekable();
        if chunks.peek().is_none() {
            return self
                .boundary
                .record(trigger, caller, first)
                .map(|_| ())
                .map_err(|_| ());
        }
        for leaves in chunks {
            let mut fds = Vec::with_capacity(leaves.len());
            let mut changes = std::mem::take(&mut first);
            for leaf in leaves {
                if let Some(change) = self.leaf_change(leaf, &mut fds) {
                    changes.push(change);
                }
            }
            self.boundary
                .record(trigger, caller, changes)
                .map_err(|_| ())?;
            drop(fds);
        }
        Ok(())
    }

    /// The current state of one path that is not a directory. `fds` keeps
    /// the descriptor a file's content is read from.
    fn leaf_change(&self, rel: &[u8], fds: &mut Vec<OwnedFd>) -> Option<Change> {
        let fd = match self.backing.open_path(rel) {
            Ok(fd) => fd,
            Err(_) => return Some(Change::removed(rel.to_vec())),
        };
        let st = rustix::fs::fstat(&fd).ok()?;
        let mode = mode_of(&st);
        let state = match Kind::from_mode(mode) {
            Kind::File => {
                let backing = proc_path(&fd);
                fds.push(fd);
                State::File {
                    backing,
                    executable: executable(mode),
                }
            }
            Kind::Symlink => State::Symlink {
                target: rustix::fs::readlinkat(&fd, "", Vec::new())
                    .ok()?
                    .into_bytes(),
            },
            // A directory raced into place: its own rename records it.
            Kind::Dir => return None,
            Kind::Other => State::Removed,
        };
        Some(Change {
            path: rel.to_vec(),
            state,
        })
    }

    /// Every file and symlink under directory `rel`, never descending into
    /// a `.git` directory or following a symlink.
    fn list_tree(&self, rel: &[u8], out: &mut Vec<Vec<u8>>) -> io::Result<()> {
        let mut stack = vec![rel.to_vec()];
        while let Some(dir) = stack.pop() {
            let fd =
                match self
                    .backing
                    .open_at(&dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
                {
                    Ok(fd) => fd,
                    // Gone or replaced meanwhile: whoever did that records it.
                    Err(_) => continue,
                };
            let mut entries = rustix::fs::Dir::read_from(&fd)?;
            while let Some(entry) = entries.read() {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let path = join(&dir, name);
                let kind = match entry.file_type() {
                    rustix::fs::FileType::Unknown => {
                        match rustix::fs::statat(&fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                        {
                            Ok(st) => rustix::fs::FileType::from_raw_mode(u32::from(st.st_mode)),
                            Err(_) => continue,
                        }
                    }
                    kind => kind,
                };
                match kind {
                    rustix::fs::FileType::Directory => {
                        if !is_git_dir_name(name) {
                            stack.push(path);
                        }
                    }
                    rustix::fs::FileType::RegularFile | rustix::fs::FileType::Symlink => {
                        out.push(path);
                    }
                    _ => {}
                }
            }
        }
        out.sort();
        Ok(())
    }

    // ---- open files ----

    /// Opens node `id`. Returns the handle id and the handle (the fuser
    /// layer registers its descriptor for passthrough).
    pub fn open(&self, id: u64, flags: i32, caller: Caller) -> OpResult<(u64, Arc<Handle>)> {
        let mut oflags = OFlags::from_bits_retain(flags as u32)
            & !(OFlags::CREATE | OFlags::EXCL | OFlags::NOCTTY | OFlags::NOATIME);
        oflags.remove(OFlags::PATH);
        let acc = flags & 0o3;
        let truncate = oflags.contains(OFlags::TRUNC);
        let write = acc != 0 || truncate;
        let _names = self.read_names();
        let fd = self.open_node(id, oflags)?;
        let st = if write {
            Some(rustix::fs::fstat(&fd).eno()?)
        } else {
            None
        };
        Ok(self.add_handle(id, fd, write, truncate, false, st.as_ref(), caller))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_handle(
        &self,
        id: u64,
        fd: OwnedFd,
        write: bool,
        truncate: bool,
        created: bool,
        st: Option<&Stat>,
        caller: Caller,
    ) -> (u64, Arc<Handle>) {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let handle = Arc::new(Handle {
            node: id,
            fd,
            write,
        });
        {
            let mut table = self.table();
            if let Some(node) = table.get_mut(id) {
                node.handles.push(fh);
                if write || created {
                    node.dirty.arm(caller);
                    if truncate || created {
                        node.dirty.force = true;
                    } else if node.dirty.baseline.is_none() {
                        node.dirty.baseline = st.map(stamp);
                    }
                }
                if write {
                    node.write_handles += 1;
                }
            }
        }
        self.handles().insert(fh, handle.clone());
        (fh, handle)
    }

    /// Creates and opens `name` in `parent`. The backing file is always
    /// created with `O_EXCL`: if the name exists by then (made in the
    /// backing tree directly, after the kernel saw no entry), an exclusive
    /// create fails with `EEXIST`, and any other opens the existing file as
    /// `open` would, without taking it over (its owner and mode are left
    /// alone, and it is not marked new).
    pub fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        caller: Caller,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        let requested = OFlags::from_bits_retain(flags as u32);
        let base = (requested
            & !(OFlags::NOCTTY | OFlags::NOATIME | OFlags::CREATE | OFlags::EXCL))
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC;
        let created = rustix::fs::openat(
            &dir,
            OsStr::from_bytes(name),
            base | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(mode & 0o7777),
        );
        let write = flags & 0o3 != 0;
        let (fd, new) = match created {
            Ok(fd) => (fd, true),
            Err(rustix::io::Errno::EXIST) if !requested.contains(OFlags::EXCL) => {
                // Only a regular file is reopened: never a FIFO (which
                // would block) or a device.
                let path = rustix::fs::openat(
                    &dir,
                    OsStr::from_bytes(name),
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .eno()?;
                let st = rustix::fs::fstat(&path).eno()?;
                if Kind::from_mode(mode_of(&st)) != Kind::File {
                    return Err(Errno::EEXIST);
                }
                // The kernel checked permission to create, not to open
                // this file: check that as the kernel would, from the
                // mode bits (the caller's supplementary groups are not
                // known, so only its primary group counts).
                if !may_open(&st, caller, flags & 0o3, requested.contains(OFlags::TRUNC)) {
                    return Err(Errno::EACCES);
                }
                let fd =
                    rustix::fs::open(proc_path(&path), base & !OFlags::NOFOLLOW, Mode::empty())
                        .eno()?;
                (fd, false)
            }
            Err(e) => return Err(errno(e)),
        };
        if new {
            self.own_new(&dir, name, Some(&fd), &pst, caller, mode, Kind::File)?;
        }
        let st = rustix::fs::fstat(&fd).eno()?;
        let entry = self.entry(parent, name, &st);
        let truncate = !new && requested.contains(OFlags::TRUNC);
        let write = write || truncate;
        let (fh, handle) = self.add_handle(entry.id, fd, write, truncate, new, Some(&st), caller);
        Ok((entry, fh, handle))
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        let h = self.handle(fh)?;
        let mut buf = vec![0u8; size as usize];
        let mut done = 0;
        while done < buf.len() {
            let n = rustix::io::pread(&h.fd, &mut buf[done..], offset + done as u64).eno()?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8], caller: Caller) -> OpResult<u32> {
        let h = self.handle(fh)?;
        if let Some(node) = self.table().get_mut(h.node) {
            node.dirty.arm(caller);
        }
        let mut done = 0;
        while done < data.len() {
            let n = rustix::io::pwrite(&h.fd, &data[done..], offset + done as u64).eno()?;
            if n == 0 {
                return Err(Errno::EIO);
            }
            done += n;
        }
        Ok(u32::try_from(done).unwrap_or(u32::MAX))
    }

    /// `close` of a descriptor: records the inode if it changed since its
    /// last record (design §3.1). `EIO` if recording fails; the inode stays
    /// dirty, so the next boundary records it.
    pub fn flush(&self, fh: u64, caller: Caller) -> OpResult<()> {
        let h = self.handle(fh)?;
        let _names = self.read_names();
        self.record_node(h.node, &h.fd, Trigger::Close, caller)
            .map(|_| ())
    }

    /// Records node `id` from `fd` if it changed. Ok(true) if a record was
    /// made.
    fn record_node(
        &self,
        id: u64,
        fd: &impl AsFd,
        trigger: Trigger,
        caller: Caller,
    ) -> OpResult<bool> {
        if !self.table().get(id).is_some_and(|n| n.dirty.armed) {
            return Ok(false);
        }
        let st = rustix::fs::fstat(fd).eno()?;
        let current = stamp(&st);
        let paths = {
            let table = self.table();
            let Some(node) = table.get(id) else {
                return Ok(false);
            };
            if node.kind != Kind::File || !node.dirty.needs_record(&current) {
                return Ok(false);
            }
            table.paths(id)
        };
        let now = now_ns();
        let changes: Vec<Change> = paths
            .iter()
            .map(|p| Change {
                path: p.clone(),
                state: State::File {
                    backing: proc_path(fd),
                    executable: executable(mode_of(&st)),
                },
            })
            .collect();
        let recorded = if changes.is_empty() {
            // No name left (unlinked): nowhere to record it.
            false
        } else {
            self.boundary
                .record(trigger, caller, changes)
                .map_err(|_| Errno::EIO)?
        };
        if let Some(node) = self.table().get_mut(id) {
            node.dirty
                .after_record(current, now, self.config.granularity_ns);
        }
        Ok(recorded)
    }

    /// The last reference to an open file is gone. If it was the last
    /// handle that could write and the inode changed since its last record
    /// (a write through a mapping, or after the last close), it is recorded
    /// now, late (design §3.1).
    pub fn release(&self, fh: u64) -> OpResult<()> {
        let Some(h) = self.handles().remove(&fh) else {
            return Err(Errno::EBADF);
        };
        let (last_writer, check) = {
            let mut table = self.table();
            let Some(node) = table.get_mut(h.node) else {
                return Ok(());
            };
            node.handles.retain(|f| *f != fh);
            if h.write {
                node.write_handles = node.write_handles.saturating_sub(1);
            }
            (
                node.dirty.last_writer.unwrap_or(NOBODY),
                node.write_handles == 0 && node.dirty.armed,
            )
        };
        let mut result = Ok(());
        if check {
            let _names = self.read_names();
            match self.record_node(h.node, &h.fd, Trigger::Release, last_writer) {
                Ok(_) => {
                    if let Some(node) = self.table().get_mut(h.node)
                        && node.write_handles == 0
                    {
                        node.dirty.armed = false;
                    }
                }
                Err(e) => {
                    // No handle is left to record from: the sweep re-derives
                    // the content from the tree.
                    let paths = self.table().paths(h.node);
                    self.boundary.queue_repair(paths, last_writer);
                    result = Err(e);
                }
            }
        }
        self.table().maybe_drop(h.node);
        result
    }

    pub fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()> {
        let h = self.handle(fh)?;
        if datasync {
            rustix::fs::fdatasync(&h.fd).eno()
        } else {
            rustix::fs::fsync(&h.fd).eno()
        }
    }

    pub fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        caller: Caller,
    ) -> OpResult<()> {
        let h = self.handle(fh)?;
        if let Some(node) = self.table().get_mut(h.node) {
            node.dirty.arm(caller);
        }
        rustix::fs::fallocate(
            &h.fd,
            rustix::fs::FallocateFlags::from_bits_retain(mode as u32),
            offset,
            length,
        )
        .eno()
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64> {
        let h = self.handle(fh)?;
        let pos = match whence {
            0 => rustix::fs::SeekFrom::Start(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
            1 => rustix::fs::SeekFrom::Current(offset),
            2 => rustix::fs::SeekFrom::End(offset),
            3 => rustix::fs::SeekFrom::Data(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
            4 => rustix::fs::SeekFrom::Hole(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
            _ => return Err(Errno::EINVAL),
        };
        let at = rustix::fs::seek(&h.fd, pos).eno()?;
        i64::try_from(at).map_err(|_| Errno::EINVAL)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        caller: Caller,
    ) -> OpResult<u32> {
        let input = self.handle(fh_in)?;
        let output = self.handle(fh_out)?;
        if let Some(node) = self.table().get_mut(output.node) {
            node.dirty.arm(caller);
        }
        let mut off_in = offset_in;
        let mut off_out = offset_out;
        let len = usize::try_from(len.min(u64::from(u32::MAX))).unwrap_or(usize::MAX);
        let n = rustix::fs::copy_file_range(
            &input.fd,
            Some(&mut off_in),
            &output.fd,
            Some(&mut off_out),
            len,
        )
        .eno()?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    // ---- directories ----

    /// Opens directory `id`, taking a snapshot of its entries; `readdir`
    /// pages through the snapshot.
    pub fn opendir(&self, id: u64) -> OpResult<u64> {
        let fd = {
            let _names = self.read_names();
            self.open_node(id, OFlags::RDONLY | OFlags::DIRECTORY)?
        };
        let dir_st = rustix::fs::fstat(&fd).eno()?;
        let mut entries = Vec::new();
        let mut reader = rustix::fs::Dir::read_from(&fd).eno()?;
        let parent = {
            let table = self.table();
            table
                .get(id)
                .and_then(|n| n.links().first().map(|(p, _)| *p))
                .unwrap_or(ROOT)
        };
        while let Some(entry) = reader.read() {
            let entry = entry.eno()?;
            let name = entry.file_name().to_bytes().to_vec();
            let kind = match backing::file_type_of(entry.file_type()) {
                Some(kind) => kind,
                None => match rustix::fs::statat(&fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                {
                    Ok(st) => backing::file_type(mode_of(&st)),
                    Err(_) => continue,
                },
            };
            let entry_id = match name.as_slice() {
                b"." => id,
                b".." => parent,
                _ => self.table().id_for(u64::from(dir_st.st_dev), entry.ino()),
            };
            entries.push(DirEntry {
                id: entry_id,
                kind,
                name,
            });
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(fh, Arc::new(entries));
        Ok(fh)
    }

    /// The snapshot's entries from `offset` on; an entry's offset for the
    /// next call is its index plus one.
    pub fn readdir(&self, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        let entries = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&fh)
            .cloned()
            .ok_or(Errno::EBADF)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        Ok(entries
            .iter()
            .enumerate()
            .skip(start)
            .map(|(i, e)| (i as u64 + 1, e.clone()))
            .collect())
    }

    pub fn releasedir(&self, fh: u64) {
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&fh);
    }

    pub fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        rustix::fs::fstatvfs(self.backing.root()).eno()
    }

    // ---- extended attributes ----

    /// The `/proc/self/fd` path to reach node `id`'s extended attributes:
    /// regular files and directories only, so following the magic link can
    /// never follow a symlink.
    fn xattr_target(&self, id: u64) -> OpResult<(NodeFd, PathBuf)> {
        match self.kind_of(id)? {
            Kind::File | Kind::Dir => {}
            _ => return Err(errno(rustix::io::Errno::OPNOTSUPP)),
        }
        let fd = {
            let _names = self.read_names();
            self.node_fd(id)?
        };
        let path = proc_path(&fd);
        Ok((fd, path))
    }

    pub fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr> {
        let (_fd, path) = self.xattr_target(id)?;
        let name = OsStr::from_bytes(name);
        if size == 0 {
            let n = rustix::fs::getxattr(&path, name, &mut [0u8; 0][..]).eno()?;
            return Ok(Xattr::Size(u32::try_from(n).unwrap_or(u32::MAX)));
        }
        let mut buf = vec![0u8; size as usize];
        let n = rustix::fs::getxattr(&path, name, &mut buf[..]).eno()?;
        buf.truncate(n);
        Ok(Xattr::Data(buf))
    }

    pub fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr> {
        let (_fd, path) = self.xattr_target(id)?;
        if size == 0 {
            let n = rustix::fs::listxattr(&path, &mut [0u8; 0][..]).eno()?;
            return Ok(Xattr::Size(u32::try_from(n).unwrap_or(u32::MAX)));
        }
        let mut buf = vec![0u8; size as usize];
        let n = rustix::fs::listxattr(&path, &mut buf[..]).eno()?;
        buf.truncate(n);
        Ok(Xattr::Data(buf))
    }

    pub fn setxattr(&self, id: u64, name: &[u8], value: &[u8], flags: i32) -> OpResult<()> {
        let (_fd, path) = self.xattr_target(id)?;
        rustix::fs::setxattr(
            &path,
            OsStr::from_bytes(name),
            value,
            rustix::fs::XattrFlags::from_bits_retain(flags as u32),
        )
        .eno()
    }

    pub fn removexattr(&self, id: u64, name: &[u8]) -> OpResult<()> {
        let (_fd, path) = self.xattr_target(id)?;
        rustix::fs::removexattr(&path, OsStr::from_bytes(name)).eno()
    }

    // ---- backstops ----

    /// One sweep (design §3.1, §3.5): retries the repair queue, then
    /// records every inode open for writing that changed since its last
    /// record. With `all`, every armed inode with any open handle is
    /// checked: the final sweep at unmount.
    pub fn sweep(&self, all: bool) -> SweepReport {
        let _names = self.read_names();
        let mut report = SweepReport::default();
        let mut done = Vec::new();
        for repair in self.boundary.repairs() {
            let path = repair.path.clone();
            let removed = vec![path.clone()];
            let roots = std::slice::from_ref(&path);
            if self
                .record_tree(REPAIR_TRIGGER, repair.caller, removed, roots)
                .is_ok()
            {
                done.push(repair);
            }
        }
        if !done.is_empty() {
            report.repaired = done.len();
            self.boundary.repaired(&done);
        }
        report.still_queued = self.boundary.pending_repairs();
        let candidates: Vec<(u64, u64, Caller)> = {
            let table = self.table();
            table
                .ids()
                .into_iter()
                .filter_map(|id| {
                    let node = table.get(id)?;
                    let open = if all {
                        !node.handles.is_empty()
                    } else {
                        node.write_handles > 0
                    };
                    (node.dirty.armed && open).then(|| {
                        (
                            id,
                            node.handles[0],
                            node.dirty.last_writer.unwrap_or(NOBODY),
                        )
                    })
                })
                .collect()
        };
        for (id, fh, caller) in candidates {
            let Ok(h) = self.handle(fh) else { continue };
            if let Ok(true) = self.record_node(id, &h.fd, Trigger::Sweep, caller) {
                report.recorded += 1;
            }
        }
        report
    }

    /// How many handles are open (for tests and the shutdown report).
    pub fn open_handles(&self) -> usize {
        self.handles().len()
    }
}

#[cfg(test)]
mod tests;
