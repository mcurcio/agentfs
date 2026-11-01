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
//! **fsync-class files** (design §8, [`crate::groups`]) are the exception:
//! no boundary records one alone. Each change to a member of a group (a
//! database and its `-wal` and `-journal`) is held against captures and
//! marks the group changed; the whole group is then captured as one record
//! at `fsync` (at most once per `agentfs.coalesce`), by the trailing timer,
//! at the close and release of the group's last handle and at unmount.
//! `-shm` files are never recorded. Class files never use kernel
//! passthrough. While a restore or revert through the mount changes a
//! group, only the operation records it, whole, at its end
//! ([`Ops::capture_operation`]).
//!
//! **The cut** (design §8, §9): no capture holds changes from two entry
//! points. An operation that changes a member takes the group's hold as
//! its writer ([`Group::hold`]) before its system call and marks the group
//! changed inside the hold. When the group holds another entry point's
//! unrecorded changes ([`crate::groups::Foreign`]), the operation lets go
//! of everything, captures the group as that writer with trigger `cut`
//! (waiting first for a running operator operation that defers it), and
//! starts over ([`Ops::again`]).
//!
//! **Permissions** (design §6) are checked by the kernel
//! (`default_permissions`) before a request arrives; the daemon then acts
//! with its own privileges. When it also has `CAP_SETUID` and `CAP_SETGID`
//! (root) it creates each new object as the caller ([`crate::creds`]), so
//! the object is the caller's from the instant it exists; a creation the
//! caller is refused (a device node) is made as the daemon. When it has
//! `CAP_CHOWN` and `CAP_FOWNER` ([`crate::privileges`]) it gives an object
//! it made itself to the caller, keeping the
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

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Instant;

use agentfs_core::record::{Caller, EntryPoint, Origin, Trigger, Writer};
use agentfs_core::volume::{MoveOutcome, Recorded, Recorder, VolumeError};
use fuser::{Errno, FileAttr, FileType, TimeOrNow};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, Stat, Timespec, Timestamps};

use crate::access::{self, Object, Want, Who};
use crate::backing::{self, Backing, attr, join, proc_path};
use crate::boundary::{Boundary, Change, REPAIR_TRIGGER, State};
use crate::creds;
use crate::dirty::{FrozenRef, Stamp, now_ns, same_bytes, timestamp_granularity_ns};
use crate::groups::{Foreign, Group, Groups, Hold};
use crate::inodes::{InodeTable, Kind, ROOT};
use crate::names::is_git_dir_name;
use crate::privileges::Privileges;
use crate::session::{Actor, Found};

pub type OpResult<T> = Result<T, Errno>;

/// The writer of late records whose writer is unknown: agentfs itself.
const NOBODY: Writer = Writer {
    caller: Caller {
        uid: 0,
        gid: 0,
        pid: 0,
    },
    entry: EntryPoint::Agentfs,
};

/// An operation that met a [`Foreign`] group and must start over once the
/// group is cut ([`Ops::hold`], [`Ops::again`]).
#[derive(Default)]
struct Retry(RefCell<Option<(Arc<Group>, Writer)>>);

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
    /// The fsync-class group of the file (design §8): its writes are held
    /// during captures. Found when the file is opened at a path in the
    /// class, and again after any rename on the mount ([`Ops::rename`]), so
    /// a database whose directory moved is captured at its new path.
    group: Mutex<HandleGroup>,
    /// Opened at a path in the class: never kernel passthrough.
    daemon_only: bool,
}

/// A handle's group, as of the mount's rename count `renames`.
#[derive(Debug)]
struct HandleGroup {
    renames: u64,
    group: Option<Arc<Group>>,
}

impl Handle {
    /// A handle on node `node` in fsync-class group `group`, found when
    /// the mount had made `renames` renames.
    pub fn new(
        node: u64,
        fd: OwnedFd,
        write: bool,
        group: Option<Arc<Group>>,
        renames: u64,
    ) -> Self {
        Self {
            node,
            fd,
            write,
            daemon_only: group.is_some(),
            group: Mutex::new(HandleGroup { renames, group }),
        }
    }

    /// Whether reads and writes must go through the daemon, never kernel
    /// passthrough: an fsync-class file, whose writes the daemon must see
    /// to hold them during a capture.
    pub fn daemon_only(&self) -> bool {
        self.daemon_only
    }
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

/// A kernel cache entry [`Ops::cached_at`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached {
    /// A node's attributes and pages.
    Node(u64),
    /// A name in a directory node.
    Entry(u64, Vec<u8>),
}

/// When [`Ops::capture`] captures a group. Every rule but `Operation` and
/// `Unmount` leaves a group alone while a running operator operation
/// changes it ([`Group::deferred`]).
#[derive(Debug, Clone, Copy)]
enum When {
    /// If it changed since its last capture.
    Changed,
    /// The end of the operator operation that changed it.
    Operation,
    /// Unmount: if it changed, whatever else.
    Unmount,
    /// If an `fsync` at this instant is due ([`crate::groups::GroupState::fsync_due`]).
    Fsync(Instant),
    /// If the trailing timer is due by this instant.
    Timer(Instant),
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
    /// Renames made so far: a handle finds its group again after one.
    renames: AtomicU64,
    boundary: Boundary,
    /// fsync-class groups (design §8).
    groups: Groups,
    config: OpsConfig,
    /// The daemon may give new objects to their caller
    /// ([`crate::privileges`]).
    chown_new: bool,
    /// The daemon creates new objects as their caller ([`crate::creds`]).
    create_as_caller: bool,
    /// The daemon's effective uid: the owner of every object it creates
    /// as itself.
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

/// `__FMODE_EXEC` (`FMODE_EXEC`, 0x20, in `include/linux/fs.h`; reserved
/// in `include/uapi/asm-generic/fcntl.h`): the kernel sets it in the open
/// flags of a file it opens to execute, and FUSE passes it on.
pub(crate) const FMODE_EXEC: i32 = 0o40;

/// The kind of entry `name` in `dir`, not following a symlink; `None` when
/// the name is free.
fn kind_at(dir: &impl AsFd, name: &[u8]) -> OpResult<Option<Kind>> {
    match rustix::fs::statat(dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(Kind::from_mode(mode_of(&st)))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(errno(e)),
    }
}

/// A `setattr` time as `utimensat(2)` takes it: omitted, now, or exact.
pub(crate) fn timespec(t: Option<TimeOrNow>) -> Timespec {
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

/// `pread(2)` of up to `size` bytes at `offset`, short only at the end of
/// the file.
pub(crate) fn read_at(fd: &impl AsFd, offset: u64, size: u32) -> OpResult<Vec<u8>> {
    let mut buf = vec![0u8; size as usize];
    let mut done = 0;
    while done < buf.len() {
        let n = rustix::io::pread(fd, &mut buf[done..], offset + done as u64).eno()?;
        if n == 0 {
            break;
        }
        done += n;
    }
    buf.truncate(done);
    Ok(buf)
}

/// `pwrite(2)` of all of `data` at `offset`; how many bytes were written.
pub(crate) fn write_at(fd: &impl AsFd, offset: u64, data: &[u8]) -> OpResult<u32> {
    let mut done = 0;
    loop {
        if done >= data.len() {
            return Ok(u32::try_from(done).unwrap_or(u32::MAX));
        }
        match rustix::io::pwrite(fd, &data[done..], offset + done as u64).eno() {
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => done += n,
            Err(e) => return Err(e),
        }
    }
}

/// `lseek(2)` with the kernel's `whence` (`SEEK_SET` 0 to `SEEK_HOLE` 4).
pub(crate) fn seek(fd: &impl AsFd, offset: i64, whence: i32) -> OpResult<i64> {
    let pos = match whence {
        0 => rustix::fs::SeekFrom::Start(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
        1 => rustix::fs::SeekFrom::Current(offset),
        2 => rustix::fs::SeekFrom::End(offset),
        3 => rustix::fs::SeekFrom::Data(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
        4 => rustix::fs::SeekFrom::Hole(u64::try_from(offset).map_err(|_| Errno::EINVAL)?),
        _ => return Err(Errno::EINVAL),
    };
    let at = rustix::fs::seek(fd, pos).eno()?;
    i64::try_from(at).map_err(|_| Errno::EINVAL)
}

impl Ops {
    /// The filesystem over the backing tree `backing`, recording through
    /// `recorder`.
    pub fn new(backing: Backing, recorder: Recorder, config: OpsConfig) -> io::Result<Self> {
        let st = rustix::fs::fstat(backing.root())?;
        let (dev, ino) = dev_ino(&st);
        let tree = backing.path().to_path_buf();
        let privileges = Privileges::current();
        Ok(Self {
            backing,
            table: Mutex::new(InodeTable::new(dev, ino)),
            handles: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            names: RwLock::new(()),
            renames: AtomicU64::new(0),
            groups: {
                let recorder = recorder.clone();
                Groups::new(Arc::new(move |pid| recorder.in_operation(pid)))
            },
            boundary: Boundary::new(recorder, tree),
            config,
            chown_new: privileges.can_chown_new(),
            create_as_caller: privileges.can_create_as_caller(),
            euid: rustix::process::geteuid().as_raw(),
        })
    }

    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    pub fn boundary(&self) -> &Boundary {
        &self.boundary
    }

    /// [`creds::create_as`] with this mount's setting; an object made as
    /// the daemon when it was to be made as the caller is counted
    /// (`agentfs.fuse.create_as_caller_fallbacks`, design §6).
    fn create_as<T>(
        &self,
        actor: Actor<'_>,
        create: impl FnMut() -> rustix::io::Result<T>,
    ) -> (rustix::io::Result<T>, bool) {
        let (made, as_caller) =
            creds::create_as(self.create_as_caller, actor.caller, actor.groups, create);
        if self.create_as_caller && !as_caller && made.is_ok() {
            self.boundary
                .recorder()
                .telemetry()
                .create_as_caller_fallback();
        }
        (made, as_caller)
    }

    pub fn groups(&self) -> &Groups {
        &self.groups
    }

    /// The fsync-class group of the path `raw`, if it is in the class.
    fn group_at(&self, raw: &[u8]) -> Option<Arc<Group>> {
        self.boundary.group_of(raw).map(|m| self.groups.get(m))
    }

    /// The fsync-class group of node `id`: of the first of its current
    /// paths in the class, or else (no name left) of its open handles.
    fn node_group(&self, id: u64) -> Option<Arc<Group>> {
        if !self.boundary.recorder().has_fsync_class() {
            return None;
        }
        let (cached, paths) = {
            let table = self.table();
            let node = table.get(id)?;
            (node.group.clone(), table.paths(id))
        };
        if paths.is_empty() {
            return cached;
        }
        paths.iter().find_map(|p| self.group_at(p))
    }

    /// The group of handle `h` now. A handle opened in the class finds its
    /// group again after any rename on the mount, from its node's current
    /// paths (design §8): a database whose directory was renamed is
    /// captured at its new path from its next change on. A file with no
    /// name left keeps the group it had.
    fn handle_group(&self, h: &Handle) -> Option<Arc<Group>> {
        if !h.daemon_only {
            return None;
        }
        let renames = self.renames.load(Ordering::Acquire);
        let mut hg = h.group.lock().unwrap_or_else(PoisonError::into_inner);
        if hg.renames == renames {
            return hg.group.clone();
        }
        hg.renames = renames;
        let paths = self.table().paths(h.node);
        if paths.is_empty() {
            return hg.group.clone();
        }
        let now = paths.iter().find_map(|p| self.group_at(p));
        let same = match (&hg.group, &now) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(old) = &hg.group {
                old.released();
            }
            if let Some(new) = &now {
                new.opened();
            }
            if let Some(node) = self.table().get_mut(h.node) {
                node.group = now.clone();
            }
            hg.group = now.clone();
        }
        now
    }

    /// Records `changes` as one record, except changes at fsync-class paths
    /// (design §8): those are their group's, which the operation marked
    /// changed inside its hold ([`Ops::hold`]), to be captured whole later
    /// (by an operator operation's actor, at the operation's end:
    /// [`Ops::capture_operation`]). The caller holds `names` for reading
    /// and no group hold.
    fn record(
        &self,
        trigger: Trigger,
        writer: Writer,
        changes: Vec<Change>,
    ) -> Result<bool, VolumeError> {
        self.record_as(trigger, writer, changes, None)
    }

    /// [`Ops::record`] as the pod `origin`, when given: an inherited repair
    /// (design §14.1).
    fn record_as(
        &self,
        trigger: Trigger,
        writer: Writer,
        changes: Vec<Change>,
        origin: Option<&Origin>,
    ) -> Result<bool, VolumeError> {
        if !self.boundary.recorder().has_fsync_class() {
            return self.boundary.record_as(trigger, writer, changes, origin);
        }
        let changes: Vec<Change> = changes
            .into_iter()
            .filter(|c| self.group_at(&c.path).is_none())
            .collect();
        if changes.is_empty() {
            return Ok(false);
        }
        self.boundary.record_as(trigger, writer, changes, origin)
    }

    /// Takes `group`'s hold for `writer` (design §8, §9), if there is a
    /// group. When the group holds another entry point's unrecorded
    /// changes, notes the cut in `retry` and fails: the operation returns,
    /// letting go of `names` and every hold, and [`Ops::again`] cuts the
    /// group and starts it over (the errno never reaches the kernel).
    fn hold<'g>(
        &self,
        group: Option<&'g Arc<Group>>,
        writer: &Writer,
        retry: &Retry,
    ) -> OpResult<Option<Hold<'g>>> {
        let Some(group) = group else {
            return Ok(None);
        };
        match group.hold(writer) {
            Ok(hold) => Ok(Some(hold)),
            Err(Foreign { pending }) => {
                *retry.0.borrow_mut() = Some((group.clone(), pending));
                Err(Errno::EAGAIN)
            }
        }
    }

    /// Runs `op` until it ends without meeting a [`Foreign`] group: after
    /// each such meeting, with no lock held, captures the group as its
    /// pending writer with trigger `cut`, holding `names` for reading; if
    /// a running operator operation defers it, lets go of `names` and waits
    /// for the operation's end capture ([`Group::wait_undeferred`]). A cut
    /// that fails fails the operation with `EIO`: the change it would have
    /// mixed in is never made.
    fn again<T>(&self, mut op: impl FnMut(&Retry) -> OpResult<T>) -> OpResult<T> {
        loop {
            let retry = Retry::default();
            let result = op(&retry);
            let Some((group, pending)) = retry.0.into_inner() else {
                return result;
            };
            {
                let _names = self.read_names();
                self.capture(&group, Trigger::Cut, Some(pending), When::Changed)
                    .map_err(|_| Errno::EIO)?;
            }
            group.wait_undeferred();
        }
    }

    /// Marks `group` changed by `writer` after the change was made, for a
    /// member found only by walking the tree once it was renamed (a
    /// directory's member, a repair). A group holding another entry
    /// point's changes is cut first; one a running operator operation
    /// defers, or whose cut fails, is left to its pending writer's record.
    /// The caller holds `names` for reading and no hold.
    fn mark_late(&self, group: &Group, writer: &Writer) {
        loop {
            match group.hold(writer) {
                Ok(hold) => {
                    hold.changed();
                    return;
                }
                Err(Foreign { pending }) => {
                    match self.capture(group, Trigger::Cut, Some(pending), When::Changed) {
                        Ok(false) if group.deferred().is_some() => return,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            }
        }
    }

    /// Captures `group` (design §8) if `when` says it is due: freezes its
    /// gate, so no member changes meanwhile, and records every member's
    /// current state (content, or removal) as one all-or-nothing record.
    /// The record's writer is `writer`, else whoever changed the group
    /// last, else [`NOBODY`]. Returns whether
    /// anything was journaled. A member that cannot be read (anything but
    /// a missing file) fails the capture, leaving the group changed. The
    /// caller holds `names` for reading, so no member is renamed meanwhile
    /// (and so a capture holds back renames on the whole mount for the
    /// length of its copy), and no hold on any group.
    fn capture(
        &self,
        group: &Group,
        trigger: Trigger,
        writer: Option<Writer>,
        when: When,
    ) -> Result<bool, VolumeError> {
        let _one = group.capturing();
        let deferred =
            !matches!(when, When::Operation | When::Unmount) && group.deferred().is_some();
        {
            let s = group.state();
            let due = match when {
                // A running operator operation records it at its end.
                _ if deferred => false,
                When::Changed | When::Operation | When::Unmount => s.is_dirty(),
                When::Fsync(now) => s.fsync_due(now, group.coalesce()),
                When::Timer(now) => s.due_at(group.coalesce()).is_some_and(|at| at <= now),
            };
            if !due {
                return Ok(false);
            }
        }
        let frozen = group.gate().freeze();
        let (generation, last) = {
            let s = group.state();
            (s.generation, s.writer.clone())
        };
        let writer = writer.or(last).unwrap_or(NOBODY);
        let mut fds = Vec::new();
        let mut changes = Vec::with_capacity(group.members().len());
        let mut result = Ok(false);
        for member in group.members() {
            match self.member_change(member, &mut fds) {
                Ok(Some(change)) => changes.push(change),
                Ok(None) => {}
                Err(e) => {
                    result = Err(self.capture_failed(trigger, group, member, e));
                    break;
                }
            }
        }
        if result.is_ok() {
            result = self.boundary.record_group(trigger, writer, changes);
        }
        drop(fds);
        drop(frozen);
        group.captured(generation, result.is_ok(), matches!(when, When::Operation));
        result
    }

    /// The current state of fsync-class member `rel` for a capture: like
    /// [`Ops::leaf_change`], except that only a missing file (`ENOENT`,
    /// `ENOTDIR`) is a removal. Any other failure (`EMFILE`, `ENOMEM`) must
    /// not record the database as deleted: it fails the capture.
    fn member_change(&self, rel: &[u8], fds: &mut Vec<OwnedFd>) -> io::Result<Option<Change>> {
        let fd = match self.backing.open_path(rel) {
            Ok(fd) => fd,
            Err(e)
                if [rustix::io::Errno::NOENT, rustix::io::Errno::NOTDIR]
                    .iter()
                    .any(|n| e.raw_os_error() == Some(n.raw_os_error())) =>
            {
                return Ok(Some(Change::removed(rel.to_vec())));
            }
            Err(e) => return Err(e),
        };
        let st = rustix::fs::fstat(&fd)?;
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
                target: rustix::fs::readlinkat(&fd, "", Vec::new())?.into_bytes(),
            },
            // A directory at a member's name is no member.
            Kind::Dir => return Ok(None),
            Kind::Other => State::Removed,
        };
        Ok(Some(Change {
            path: rel.to_vec(),
            state,
        }))
    }

    /// A capture of `group` could not read `member`: raises
    /// `RecordFailed` (design §13) and returns the error.
    fn capture_failed(
        &self,
        trigger: Trigger,
        group: &Group,
        member: &[u8],
        error: io::Error,
    ) -> VolumeError {
        let paths = group
            .members()
            .iter()
            .filter_map(|m| {
                std::str::from_utf8(m)
                    .ok()
                    .and_then(|m| agentfs_core::paths::RepoPath::new(m).ok())
            })
            .collect();
        let error = io::Error::new(
            error.kind(),
            format!(
                "reading {} for a capture: {error}",
                String::from_utf8_lossy(member)
            ),
        );
        self.boundary
            .recorder()
            .events()
            .emit(agentfs_core::events::Event::RecordFailed {
                trigger,
                paths,
                error: error.to_string(),
            });
        VolumeError::Io(error)
    }

    /// Captures every group the trailing timer has due at `now` (design
    /// §8), then forgets groups nothing uses. Returns how many captures
    /// were journaled.
    pub fn capture_due(&self, now: Instant) -> usize {
        let mut n = 0;
        for group in self.groups.due(now) {
            let _names = self.read_names();
            if let Ok(true) = self.capture(&group, Trigger::Interval, None, When::Timer(now)) {
                n += 1;
            }
        }
        self.groups.prune();
        n
    }

    /// Captures every group the running operator operation changed, each
    /// whole as one record of the operation (design §8, §15): called by
    /// the operation before it ends. Returns how many captures were
    /// journaled, or the first failure (the others are still tried).
    pub fn capture_operation(&self) -> Result<usize, VolumeError> {
        let mut n = 0;
        let mut failed = None;
        for (group, actor) in self.groups.in_operation() {
            let _names = self.read_names();
            let writer = Writer::agentfs(actor);
            match self.capture(&group, Trigger::Restore, Some(writer), When::Operation) {
                Ok(true) => n += 1,
                Ok(false) => {}
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        // A group left changed is the timer's once the operation ends.
        self.groups.wake();
        failed.map_or(Ok(n), Err)
    }

    /// Captures every changed group now, whatever its schedule: at unmount.
    /// Returns how many captures were journaled.
    pub fn capture_all(&self) -> usize {
        let mut n = 0;
        for group in self.groups.dirty() {
            let _names = self.read_names();
            if let Ok(true) = self.capture(&group, Trigger::Sweep, None, When::Unmount) {
                n += 1;
            }
        }
        n
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
        let _names = self.read_names();
        let st = self.stat_child(parent, name)?;
        Ok(self.entry(parent, name, &st))
    }

    /// The kind of `name` in `parent`, not following a symlink. Nothing
    /// is counted: no node id is handed out or kept.
    pub fn kind_at(&self, parent: u64, name: &[u8]) -> OpResult<Kind> {
        let _names = self.read_names();
        let st = self.stat_child(parent, name)?;
        Ok(Kind::from_mode(mode_of(&st)))
    }

    /// `name` in `parent`, statted without following a symlink. The
    /// caller holds the names lock.
    fn stat_child(&self, parent: u64, name: &[u8]) -> OpResult<Stat> {
        check_name(name)?;
        let dir = self.open_dir_node(parent)?;
        rustix::fs::statat(&dir, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()
    }

    /// Counts a kernel lookup of `name` in `parent`, now `st`.
    fn entry(&self, parent: u64, name: &[u8], st: &Stat) -> Entry {
        let (dev, ino) = dev_ino(st);
        let id = self
            .table()
            .lookup(parent, name, dev, ino, Kind::from_mode(mode_of(st)));
        self.prune_stale_names(id, parent, name, (dev, ino));
        Entry {
            id,
            attr: attr(id, st),
        }
    }

    /// Drops the names of node `id`, other than `parent/name`, that no
    /// longer lead to its backing inode. Another process changed the
    /// backing tree under them: a second front of the same tree (design
    /// §14) renamed or removed the file, and the filesystem may since have
    /// reused its inode number for the file just looked up. Left alone, a
    /// stale first name would be the node's path: opens would fail and
    /// records would name the wrong path. Only nodes with several names
    /// pay the extra lookups.
    fn prune_stale_names(&self, id: u64, parent: u64, name: &[u8], inode: (u64, u64)) {
        let several = self
            .table()
            .get(id)
            .is_some_and(|node| node.links().len() >= 2);
        if several {
            self.current_names(id, inode, Some((parent, name)));
        }
    }

    /// The paths of node `id` that still lead to its backing inode `inode`
    /// (design §14). Names another front renamed or removed under this
    /// mount, or whose inode number the filesystem has reused, are dropped
    /// from the node and left out. A name that cannot be checked (any
    /// failure but a missing path) is kept. `known`, a name just looked
    /// up, is current without a check.
    fn current_names(
        &self,
        id: u64,
        inode: (u64, u64),
        known: Option<(u64, &[u8])>,
    ) -> Vec<Vec<u8>> {
        let names: Vec<(u64, Vec<u8>, Vec<u8>)> = {
            let table = self.table();
            let Some(node) = table.get(id) else {
                return Vec::new();
            };
            node.links()
                .iter()
                .filter_map(|(p, n)| Some((*p, n.clone(), join(&table.path(*p)?, n))))
                .collect()
        };
        let mut current = Vec::with_capacity(names.len());
        let mut stale = Vec::new();
        for (p, n, path) in names {
            let is_known = known.is_some_and(|(kp, kn)| kp == p && kn == n.as_slice());
            if is_known || self.leads_to(&path, inode) != Some(false) {
                current.push(path);
            } else {
                stale.push((p, n));
            }
        }
        if !stale.is_empty() {
            let mut table = self.table();
            for (p, n) in stale {
                if table.child(p, &n) == Some(id) {
                    table.remove_link(p, &n);
                }
            }
        }
        current
    }

    /// Whether `path` in the backing tree is the inode `inode`: `None` if
    /// that cannot be told (a failure other than a missing path).
    fn leads_to(&self, path: &[u8], inode: (u64, u64)) -> Option<bool> {
        match self.backing.open_path(path) {
            Ok(fd) => rustix::fs::fstat(&fd).ok().map(|st| dev_ino(&st) == inode),
            Err(e) => {
                let missing = [Errno::ENOENT, Errno::ENOTDIR, Errno::ELOOP]
                    .iter()
                    .any(|n| e.raw_os_error() == Some(n.code()));
                if missing { Some(false) } else { None }
            }
        }
    }

    /// The paths to record an open file `fd` (node `id`, now `st`) under
    /// (design §14): its names that still lead to it. A file with none
    /// left that is still in the tree was moved by another front, or its
    /// directory was: it is recorded at the path the kernel knows for the
    /// descriptor, if that leads to it. A file removed from the tree is
    /// recorded nowhere: the front that removed it records the removal.
    fn record_paths(&self, id: u64, fd: &impl AsFd, st: &Stat) -> Vec<Vec<u8>> {
        let inode = dev_ino(st);
        let paths = self.current_names(id, inode, None);
        if !paths.is_empty() || st.st_nlink == 0 {
            return paths;
        }
        match self.backing.rel_of(fd) {
            Some(rel) if self.leads_to(&rel, inode) == Some(true) => vec![rel],
            _ => Vec::new(),
        }
    }

    /// The kernel forgot `n` lookups of `id`. A node whose mapping may
    /// write unseen ([`Ops::mapping_may_write`]) with no handle open is
    /// recorded first if it changed: the kernel forgets a node only once
    /// its inode is evicted, which a live mapping prevents (it pins the
    /// dentry), so every write through it has landed (design §3.1).
    pub fn forget(&self, id: u64, n: u64) {
        let watched = self
            .table()
            .get(id)
            .filter(|node| node.mapping_may_write && node.handles.is_empty())
            .map(|node| node.dirty.last_writer.clone().unwrap_or(NOBODY));
        if let Some(writer) = watched {
            let _names = self.read_names();
            if let Ok(fd) = self.node_fd(id)
                && self
                    .record_node(id, &fd, Trigger::Release, writer.clone())
                    .is_err()
            {
                // The sweep re-derives the content from the tree.
                let paths = self.table().paths(id);
                self.boundary.queue_repair(paths, writer);
            }
        }
        self.table().forget(id, n);
    }

    /// A handle that can write on node `id` was given a passthrough
    /// backing id another handle had open (the kernel allows no other
    /// handle there, design §17.1). A shared mapping it makes holds the
    /// backing file, not the FUSE file, so RELEASE can come while the
    /// mapping still writes: the node stays armed and swept until the
    /// kernel forgets it ([`Ops::forget`]).
    pub fn mapping_may_write(&self, id: u64) {
        if let Some(node) = self.table().get_mut(id) {
            node.mapping_may_write = true;
        }
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

    pub fn setattr(&self, id: u64, set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr> {
        self.again(|retry| self.setattr_once(id, set, actor, retry))
    }

    fn setattr_once(
        &self,
        id: u64,
        set: &SetAttr,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<FileAttr> {
        let writer = actor.writer();
        let names = self.read_names();
        let kind = self.kind_of(id)?;
        let fd = match set.fh.and_then(|fh| self.handle(fh).ok()) {
            Some(h) => NodeFd::Handle(h),
            None => self.node_fd(id)?,
        };
        // A size or mode change to an fsync-class file changes its group:
        // held until the change is made and marked.
        let group = if kind == Kind::File && (set.size.is_some() || set.mode.is_some()) {
            match &fd {
                NodeFd::Handle(h) if h.daemon_only() => self.handle_group(h),
                _ => self.node_group(id),
            }
        } else {
            None
        };
        let hold = self.hold(group.as_ref(), &writer, retry)?;
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
                node.dirty.arm(writer.clone());
            }
        }
        if set.atime.is_some() || set.mtime.is_some() {
            self.utimens(id, &fd, kind, set)?;
        }
        let after = rustix::fs::fstat(&fd).eno()?;
        let record = kind == Kind::File
            && (set.size.is_some() || executable(mode_of(&before)) != executable(mode_of(&after)));
        if let Some(hold) = &hold
            && record
        {
            hold.changed();
        }
        drop(hold);
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
            match self.record(Trigger::Setattr, writer.clone(), changes) {
                Ok(_) => {
                    if let Some(node) = self.table().get_mut(id) {
                        node.dirty
                            .after_record(stamp(&after), now, self.config.granularity_ns);
                        node.dirty.frozen = None;
                    }
                }
                Err(_) => {
                    self.boundary.queue_repair(paths, writer);
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
    /// (design §6); an object made `as_caller` ([`crate::creds`]) is
    /// already the caller's. `fd` is the object itself when it is open; otherwise
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
        as_caller: bool,
    ) -> OpResult<()> {
        if (as_caller || !self.chown_new) && kind == Kind::Symlink {
            return Ok(());
        }
        let owner = if as_caller { caller.uid } else { self.euid };
        let opened;
        let fd = match fd {
            Some(fd) => fd,
            None => {
                opened = self.open_new(dir, name, kind, owner)?;
                &opened
            }
        };
        if self.chown_new && !as_caller {
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
    /// daemon made: of that type, owned by `owner` (the daemon, or the
    /// caller it was made as) and, unless a directory, with a single link. Anything else means the name was
    /// replaced in the backing tree between the creation and now (a symlink
    /// or hard link to a file elsewhere): `ESTALE`, and nothing is changed.
    fn open_new(&self, dir: &OwnedFd, name: &[u8], kind: Kind, owner: u32) -> OpResult<OwnedFd> {
        let fd = rustix::fs::openat(
            dir,
            OsStr::from_bytes(name),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .eno()?;
        let st = rustix::fs::fstat(&fd).eno()?;
        let same_kind = Kind::from_mode(mode_of(&st)) == kind;
        let ours = st.st_uid == owner;
        let single = kind == Kind::Dir || st.st_nlink == 1;
        if !(same_kind && ours && single) {
            return Err(estale());
        }
        Ok(fd)
    }

    /// Asks `actor`'s check, if it carries one, about a new entry of kind
    /// `kind` at `name` in directory `dir` (node `parent`), from what is
    /// there now ([`NameCheck::create`]). The caller holds `names`.
    fn check_create(
        &self,
        actor: Actor<'_>,
        dir: &OwnedFd,
        parent: u64,
        name: &[u8],
        kind: Kind,
    ) -> OpResult<()> {
        let Some(check) = actor.check else {
            return Ok(());
        };
        let path = join(&self.path_of(parent)?, name);
        check.create(
            Found {
                path: &path,
                kind: kind_at(dir, name)?,
            },
            kind,
        )
    }

    /// Undoes the lookup count of an entry the kernel never received.
    fn unlookup(&self, id: u64) {
        self.table().forget(id, 1);
    }

    pub fn mkdir(&self, parent: u64, name: &[u8], mode: u32, actor: Actor<'_>) -> OpResult<Entry> {
        let caller = actor.caller;
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        self.check_create(actor, &dir, parent, name, Kind::Dir)?;
        let (made, as_caller) = self.create_as(actor, || {
            rustix::fs::mkdirat(
                &dir,
                OsStr::from_bytes(name),
                Mode::from_raw_mode(mode & 0o7777),
            )
        });
        made.eno()?;
        self.own_new(&dir, name, None, &pst, caller, mode, Kind::Dir, as_caller)?;
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
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.again(|retry| self.mknod_once(parent, name, mode, rdev, actor, retry))
    }

    fn mknod_once(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<Entry> {
        let caller = actor.caller;
        let writer = actor.writer();
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        let kind = Kind::from_mode(mode);
        self.check_create(actor, &dir, parent, name, kind)?;
        let group = self.group_at(&join(&self.path_of(parent)?, name));
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        // A device node needs CAP_MKNOD, which the caller lacks: refused
        // as the caller, it is made by the daemon.
        let (made, as_caller) = self.create_as(actor, || {
            rustix::fs::mknodat(
                &dir,
                OsStr::from_bytes(name),
                rustix::fs::FileType::from_raw_mode(mode),
                Mode::from_raw_mode(mode & 0o7777),
                u64::from(rdev),
            )
        });
        made.eno()?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
        self.own_new(&dir, name, None, &pst, caller, mode, kind, as_caller)?;
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
                .record(Trigger::Close, writer.clone(), vec![change])
                .is_err()
            {
                self.boundary.queue_repair([path], writer);
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
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.again(|retry| self.symlink_once(parent, name, target, actor, retry))
    }

    fn symlink_once(
        &self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<Entry> {
        let caller = actor.caller;
        let writer = actor.writer();
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        self.check_create(actor, &dir, parent, name, Kind::Symlink)?;
        let group = self.group_at(&join(&self.path_of(parent)?, name));
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        let (made, as_caller) = self.create_as(actor, || {
            rustix::fs::symlinkat(OsStr::from_bytes(target), &dir, OsStr::from_bytes(name))
        });
        made.eno()?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
        self.own_new(
            &dir,
            name,
            None,
            &pst,
            caller,
            0o777,
            Kind::Symlink,
            as_caller,
        )?;
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
            .record(Trigger::Symlink, writer.clone(), vec![change])
            .is_err()
        {
            self.boundary.queue_repair([path], writer);
            self.unlookup(entry.id);
            return Err(Errno::EIO);
        }
        Ok(entry)
    }

    pub fn link(&self, id: u64, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<Entry> {
        self.again(|retry| self.link_once(id, parent, name, actor, retry))
    }

    fn link_once(
        &self,
        id: u64,
        parent: u64,
        name: &[u8],
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<Entry> {
        let writer = actor.writer();
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
        if let Some(check) = actor.check {
            let path = join(&self.path_of(parent)?, name);
            check.link(Found {
                path: &path,
                kind: kind_at(&dir, name)?,
            })?;
        }
        let group = self.group_at(&join(&self.path_of(parent)?, name));
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        rustix::fs::linkat(
            &sdir,
            OsStr::from_bytes(&sname),
            &dir,
            OsStr::from_bytes(name),
            AtFlags::empty(),
        )
        .eno()?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
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
            .record(Trigger::Link, writer.clone(), vec![change])
            .is_err()
        {
            self.boundary.queue_repair([path], writer);
            self.unlookup(entry.id);
            return Err(Errno::EIO);
        }
        Ok(entry)
    }

    // ---- removing and renaming ----

    pub fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.remove(parent, name, false, actor)
    }

    pub fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.remove(parent, name, true, actor)
    }

    fn remove(&self, parent: u64, name: &[u8], dir: bool, actor: Actor<'_>) -> OpResult<()> {
        self.again(|retry| self.remove_once(parent, name, dir, actor, retry))
    }

    fn remove_once(
        &self,
        parent: u64,
        name: &[u8],
        dir: bool,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<()> {
        let writer = actor.writer();
        check_name(name)?;
        let _names = self.read_names();
        let pdir = self.open_dir_node(parent)?;
        let path = join(&self.path_of(parent)?, name);
        let flags = if dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        if let Some(check) = actor.check {
            check.remove(Found {
                path: &path,
                kind: kind_at(&pdir, name)?,
            })?;
        }
        // Removing a member changes its group: held while it happens.
        let group = self.group_at(&path);
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        rustix::fs::unlinkat(&pdir, OsStr::from_bytes(name), flags).eno()?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
        self.table().remove_link(parent, name);
        let trigger = if dir { Trigger::Rmdir } else { Trigger::Unlink };
        if self
            .record(trigger, writer.clone(), vec![Change::removed(path.clone())])
            .is_err()
        {
            self.boundary.queue_repair([path], writer);
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
        actor: Actor<'_>,
    ) -> OpResult<()> {
        self.again(|retry| self.rename_once(parent, name, newparent, newname, flags, actor, retry))
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_once(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<()> {
        let writer = actor.writer();
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
        if let Some(check) = actor.check {
            check.rename(
                Found {
                    path: &src_path,
                    kind: Some(Kind::from_mode(mode_of(&src))),
                },
                Found {
                    path: &dst_path,
                    kind: dst.as_ref().map(|d| Kind::from_mode(mode_of(d))),
                },
                flags.bits(),
            )?;
        }
        // Renaming a member changes its group, at either name: held while
        // it happens. `names` is held for writing, so no capture runs: a
        // hold waits at most for a write in progress through another entry
        // point, which never waits for `names`.
        let src_group = self.group_at(&src_path);
        let dst_group = self
            .group_at(&dst_path)
            .filter(|d| src_group.as_ref().is_none_or(|s| !Arc::ptr_eq(s, d)));
        let src_hold = self.hold(src_group.as_ref(), &writer, retry)?;
        let dst_hold = self.hold(dst_group.as_ref(), &writer, retry)?;
        rustix::fs::renameat_with(
            &sdir,
            OsStr::from_bytes(name),
            &ddir,
            OsStr::from_bytes(newname),
            flags,
        )
        .eno()?;
        for hold in [&src_hold, &dst_hold].into_iter().flatten() {
            hold.changed();
        }
        drop((src_hold, dst_hold));
        // Renaming one name of an inode over another of its names changes
        // nothing (POSIX).
        if dst.as_ref().is_some_and(|d| dev_ino(d) == dev_ino(&src)) {
            return Ok(());
        }
        self.table()
            .rename(parent, name, newparent, newname, exchange);
        // Open fsync-class handles find their group again (design §8).
        self.renames.fetch_add(1, Ordering::AcqRel);
        let is_dir = Kind::from_mode(mode_of(&src)) == Kind::Dir;
        if is_dir && !exchange {
            // One move, O(1) in the paths beneath (design §3.1), journaled
            // before any other boundary can name a path beneath either
            // directory: the names stay locked until it is durable.
            match self
                .boundary
                .record_move(Trigger::Rename, writer.clone(), &src_path, &dst_path)
            {
                Ok(MoveOutcome::Journaled(_) | MoveOutcome::Nothing) => return Ok(()),
                Ok(MoveOutcome::Unsupported) => {}
                Err(_) => {
                    drop(names);
                    self.boundary.queue_repair([src_path, dst_path], writer);
                    return Err(Errno::EIO);
                }
            }
        }
        drop(names);

        let _names = self.read_names();
        let (removed, roots) = if exchange {
            (
                vec![src_path.clone(), dst_path.clone()],
                vec![src_path.clone(), dst_path.clone()],
            )
        } else if is_dir {
            (
                vec![src_path.clone(), dst_path.clone()],
                vec![dst_path.clone()],
            )
        } else {
            (vec![src_path.clone()], vec![dst_path.clone()])
        };
        if self
            .record_tree(Trigger::Rename, writer.clone(), removed, &roots, None)
            .is_err()
        {
            self.boundary.queue_repair([src_path, dst_path], writer);
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
    /// the first carrying the removals. A path in an fsync-class group is
    /// its group's: the group is marked changed by `writer`
    /// ([`Ops::mark_late`]) instead. The caller holds `names` for reading
    /// and no hold.
    fn record_tree(
        &self,
        trigger: Trigger,
        writer: Writer,
        removed: Vec<Vec<u8>>,
        roots: &[Vec<u8>],
        origin: Option<&Origin>,
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
        if self.boundary.recorder().has_fsync_class() {
            for path in removed.iter().chain(&leaves) {
                if let Some(group) = self.group_at(path) {
                    self.mark_late(&group, &writer);
                }
            }
        }
        let mut first: Vec<Change> = removed.into_iter().map(Change::removed).collect();
        let chunk = self.config.chunk.max(1);
        let mut chunks = leaves.chunks(chunk).peekable();
        if chunks.peek().is_none() {
            return self
                .record_as(trigger, writer, first, origin)
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
            self.record_as(trigger, writer.clone(), changes, origin)
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
    pub fn open(&self, id: u64, flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
        self.again(|retry| self.open_once(id, flags, actor, retry))
    }

    fn open_once(
        &self,
        id: u64,
        flags: i32,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<(u64, Arc<Handle>)> {
        let writer = actor.writer();
        let mut oflags = OFlags::from_bits_retain(flags as u32)
            & !(OFlags::CREATE | OFlags::EXCL | OFlags::NOCTTY | OFlags::NOATIME);
        oflags.remove(OFlags::PATH);
        // Not an open flag: `openat2(2)` refuses it (EINVAL), so a file
        // could never be executed through the mount.
        oflags.remove(OFlags::from_bits_retain(FMODE_EXEC as u32));
        let acc = flags & 0o3;
        let truncate = oflags.contains(OFlags::TRUNC);
        let write = acc != 0 || truncate;
        let _names = self.read_names();
        let group = self.node_group(id);
        // Truncating a member changes its group: held, like a write.
        let hold = self.hold(group.as_ref().filter(|_| truncate), &writer, retry)?;
        let fd = self.open_node(id, oflags)?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
        let st = if write {
            Some(rustix::fs::fstat(&fd).eno()?)
        } else {
            None
        };
        Ok(self.add_handle(id, fd, write, truncate, false, st.as_ref(), writer, group))
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
        writer: Writer,
        group: Option<Arc<Group>>,
    ) -> (u64, Arc<Handle>) {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        // The caller holds `names`, so no rename runs since `group` was
        // found.
        let handle = Arc::new(Handle::new(
            id,
            fd,
            write,
            group.clone(),
            self.renames.load(Ordering::Acquire),
        ));
        if let Some(group) = &group {
            group.opened();
        }
        {
            let mut table = self.table();
            if let Some(node) = table.get_mut(id) {
                node.handles.push(fh);
                node.group = group.clone();
                if write || created {
                    node.dirty.arm(writer);
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
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        self.again(|retry| self.create_once(parent, name, mode, flags, actor, retry))
    }

    fn create_once(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        let caller = actor.caller;
        let writer = actor.writer();
        check_name(name)?;
        let _names = self.read_names();
        let dir = self.open_dir_node(parent)?;
        let pst = rustix::fs::fstat(&dir).eno()?;
        let requested = OFlags::from_bits_retain(flags as u32);
        let base = (requested
            & !(OFlags::NOCTTY | OFlags::NOATIME | OFlags::CREATE | OFlags::EXCL))
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC;
        self.check_create(actor, &dir, parent, name, Kind::File)?;
        // Creating or truncating a member changes its group: held.
        let group = self.group_at(&join(&self.path_of(parent)?, name));
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        let (created, as_caller) = self.create_as(actor, || {
            rustix::fs::openat(
                &dir,
                OsStr::from_bytes(name),
                base | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(mode & 0o7777),
            )
        });
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
                // mode bits and the caller's groups.
                let groups = creds::groups_of(actor.caller, actor.groups);
                let who = Who {
                    uid: caller.uid,
                    gid: caller.gid,
                    groups: &groups,
                };
                if !access::permits(Object::from_stat(&st), who, Want::from_open_flags(flags)) {
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
            self.own_new(
                &dir,
                name,
                Some(&fd),
                &pst,
                caller,
                mode,
                Kind::File,
                as_caller,
            )?;
        }
        let st = rustix::fs::fstat(&fd).eno()?;
        let entry = self.entry(parent, name, &st);
        let truncate = !new && requested.contains(OFlags::TRUNC);
        let write = write || truncate;
        if let Some(hold) = &hold
            && (new || truncate)
        {
            hold.changed();
        }
        drop(hold);
        let (fh, handle) =
            self.add_handle(entry.id, fd, write, truncate, new, Some(&st), writer, group);
        Ok((entry, fh, handle))
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        read_at(&self.handle(fh)?.fd, offset, size)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32> {
        self.again(|retry| self.write_once(fh, offset, data, actor, retry))
    }

    fn write_once(
        &self,
        fh: u64,
        offset: u64,
        data: &[u8],
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<u32> {
        let writer = actor.writer();
        let h = self.handle(fh)?;
        if let Some(node) = self.table().get_mut(h.node) {
            node.dirty.arm(writer.clone());
        }
        let group = self.handle_group(&h);
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        let result = write_at(&h.fd, offset, data);
        if let Some(hold) = &hold {
            hold.changed();
        }
        result
    }

    /// `close` of a descriptor: records the inode if it changed since its
    /// last record (design §3.1). `EIO` if recording fails; the inode stays
    /// dirty, so the next boundary records it.
    pub fn flush(&self, fh: u64, actor: Actor<'_>) -> OpResult<()> {
        let writer = actor.writer();
        let h = self.handle(fh)?;
        let _names = self.read_names();
        if let Some(group) = self.handle_group(&h) {
            // An fsync-class file (design §8): its whole group, if it
            // changed and this is the group's last open handle. Closing one
            // member while another stays open (SQLite's `-journal` in
            // DELETE mode, every transaction) leaves the change to the
            // rate-limited fsync rule and the trailing timer.
            if group.state().handles > 1 {
                return Ok(());
            }
            // As whoever changed it: a close through another entry point
            // never claims another's changes.
            return self
                .capture(&group, Trigger::Close, None, When::Changed)
                .map(|_| ())
                .map_err(|_| Errno::EIO);
        }
        self.record_node(h.node, &h.fd, Trigger::Close, writer)?;
        // Recorded, not yet answered (design §18 test 3).
        agentfs_core::failpoints::hit("fuse.after_record").map_err(|_| Errno::EIO)
    }

    /// Records node `id` from `fd` if it changed. Ok(true) if a record was
    /// made.
    fn record_node(
        &self,
        id: u64,
        fd: &impl AsFd,
        trigger: Trigger,
        writer: Writer,
    ) -> OpResult<bool> {
        // fsync-class nodes are recorded by group captures only.
        if !self
            .table()
            .get(id)
            .is_some_and(|n| n.dirty.armed && n.group.is_none())
        {
            return Ok(false);
        }
        let st = rustix::fs::fstat(fd).eno()?;
        let current = stamp(&st);
        let pinned = {
            let table = self.table();
            let Some(node) = table.get(id) else {
                return Ok(false);
            };
            if node.kind != Kind::File || !node.dirty.needs_record(&current) {
                return Ok(false);
            }
            node.dirty
                .racy_only(&current)
                .then(|| node.dirty.frozen.clone())
        };
        // Never a cached name another front has since moved or removed
        // (design §14).
        let paths = self.record_paths(id, fd, &st);
        if let Some(pinned) = pinned {
            // Recording again only because the last record was racy: not
            // if the bytes are the copy that record froze, and that copy
            // is still the last state journaled at the path (design §3.1).
            // The bytes compared were read after `before`, so the proof
            // stands for the stamp as of then, as a record would.
            let before = now_ns();
            let skip = pinned.is_some_and(|pin| self.still_recorded(&pin, &paths, fd, &st));
            self.boundary.recorder().telemetry().racy_recheck(skip);
            if skip {
                if let Some(node) = self.table().get_mut(id) {
                    node.dirty
                        .after_record(current, before, self.config.granularity_ns);
                    // No longer racy: no later boundary compares with the
                    // pinned copy, whose space goes once committed.
                    if !node.dirty.racy {
                        node.dirty.frozen = None;
                    }
                }
                return Ok(false);
            }
        }
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
            // No name left (unlinked here, or removed by another front):
            // nowhere to record it.
            Recorded::default()
        } else {
            self.boundary
                .record(trigger, writer, changes)
                .map_err(|_| Errno::EIO)?
        };
        let racy = now - current.changed_ns() < self.config.granularity_ns;
        // A racy record's copy is pinned for the next boundary to compare
        // with: only one written at the file's one name.
        let frozen = match (racy, paths.as_slice(), recorded.written.as_slice()) {
            (true, [name], [(path, copy)]) => {
                self.boundary
                    .recorder()
                    .open_copy(*copy)
                    .ok()
                    .map(|file| FrozenRef {
                        name: name.clone(),
                        path: path.clone(),
                        copy: *copy,
                        file: Arc::new(file),
                    })
            }
            _ => None,
        };
        if let Some(node) = self.table().get_mut(id) {
            node.dirty
                .after_record(current, now, self.config.granularity_ns);
            node.dirty.frozen = frozen;
        }
        Ok(recorded.seq.is_some())
    }

    /// Whether node's content, open as `fd` with stat `st` and named
    /// `paths` now, is exactly the copy `pin` a racy record froze, and
    /// that copy is still the last state journaled at its path (design
    /// §3.1). Any doubt is a no: the boundary records.
    fn still_recorded(
        &self,
        pin: &FrozenRef,
        paths: &[Vec<u8>],
        fd: &impl AsFd,
        st: &Stat,
    ) -> bool {
        let size = u64::try_from(st.st_size).unwrap_or(u64::MAX);
        if paths != std::slice::from_ref(&pin.name) {
            return false;
        }
        if pin.file.metadata().map(|m| m.len()).ok() != Some(size) {
            return false;
        }
        // The handle may be write-only: read through a new descriptor.
        let Ok(now) = std::fs::File::open(proc_path(fd)) else {
            return false;
        };
        same_bytes(&pin.file, &now, size).unwrap_or(false)
            && self.boundary.recorder().still_last(&pin.path, pin.copy)
    }

    /// The last reference to an open file is gone. If it was the last
    /// handle that could write and the inode changed since its last record
    /// (a write through a mapping, or after the last close), it is recorded
    /// now, late (design §3.1).
    pub fn release(&self, fh: u64) -> OpResult<()> {
        let Some(h) = self.handles().remove(&fh) else {
            return Err(Errno::EBADF);
        };
        let group = self.handle_group(&h);
        let last_in_group = group.as_ref().is_some_and(|g| g.released() == 0);
        let (last_writer, check) = {
            let mut table = self.table();
            let Some(node) = table.get_mut(h.node) else {
                return Ok(());
            };
            node.handles.retain(|f| *f != fh);
            if node.handles.is_empty() {
                node.group = None;
            }
            if h.write {
                node.write_handles = node.write_handles.saturating_sub(1);
            }
            (
                node.dirty.last_writer.clone().unwrap_or(NOBODY),
                // An fsync-class handle never records its node alone.
                group.is_none() && node.write_handles == 0 && node.dirty.armed,
            )
        };
        if let Some(group) = &group
            && last_in_group
        {
            // The group's last handle: capture it if it changed. A failure
            // leaves it changed, for the timer to retry.
            let _names = self.read_names();
            let _ = self.capture(group, Trigger::Release, None, When::Changed);
        }
        let mut result = Ok(());
        if check {
            let _names = self.read_names();
            match self.record_node(h.node, &h.fd, Trigger::Release, last_writer.clone()) {
                Ok(_) => {
                    // Nothing can write it unseen any more but a mapping,
                    // which RELEASE covers unless the mapping holds a
                    // passthrough backing file: then the node stays armed
                    // until FORGET.
                    if let Some(node) = self.table().get_mut(h.node)
                        && node.write_handles == 0
                        && !node.mapping_may_write
                    {
                        node.dirty.armed = false;
                        node.dirty.frozen = None;
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
            rustix::fs::fdatasync(&h.fd).eno()?;
        } else {
            rustix::fs::fsync(&h.fd).eno()?;
        }
        if let Some(group) = self.handle_group(&h) {
            // At most once per `agentfs.coalesce` (design §8). A failed
            // capture does not fail the fsync, which did reach the disk:
            // the group stays changed and the timer retries it.
            let _names = self.read_names();
            let _ = self.capture(&group, Trigger::Fsync, None, When::Fsync(Instant::now()));
        }
        Ok(())
    }

    pub fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        self.again(|retry| self.fallocate_once(fh, offset, length, mode, actor, retry))
    }

    fn fallocate_once(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<()> {
        let writer = actor.writer();
        let h = self.handle(fh)?;
        if let Some(node) = self.table().get_mut(h.node) {
            node.dirty.arm(writer.clone());
        }
        let group = self.handle_group(&h);
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        let result = rustix::fs::fallocate(
            &h.fd,
            rustix::fs::FallocateFlags::from_bits_retain(mode as u32),
            offset,
            length,
        )
        .eno();
        if let Some(hold) = &hold {
            hold.changed();
        }
        result
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64> {
        seek(&self.handle(fh)?.fd, offset, whence)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        actor: Actor<'_>,
    ) -> OpResult<u32> {
        self.again(|retry| {
            self.copy_file_range_once(fh_in, offset_in, fh_out, offset_out, len, actor, retry)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range_once(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        actor: Actor<'_>,
        retry: &Retry,
    ) -> OpResult<u32> {
        let writer = actor.writer();
        let input = self.handle(fh_in)?;
        let output = self.handle(fh_out)?;
        if let Some(node) = self.table().get_mut(output.node) {
            node.dirty.arm(writer.clone());
        }
        let mut off_in = offset_in;
        let mut off_out = offset_out;
        let len = usize::try_from(len.min(u64::from(u32::MAX))).unwrap_or(usize::MAX);
        let group = self.handle_group(&output);
        let hold = self.hold(group.as_ref(), &writer, retry)?;
        let n = rustix::fs::copy_file_range(
            &input.fd,
            Some(&mut off_in),
            &output.fd,
            Some(&mut off_out),
            len,
        )
        .eno();
        if let Some(hold) = &hold {
            hold.changed();
        }
        Ok(u32::try_from(n?).unwrap_or(u32::MAX))
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
                .record_tree(
                    REPAIR_TRIGGER,
                    repair.writer.clone(),
                    removed,
                    roots,
                    repair.origin.as_ref(),
                )
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
        // A node whose mapping may write unseen is swept with no handle
        // open too, from its name.
        let candidates: Vec<(u64, Option<u64>, Writer)> = {
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
                    (node.dirty.armed && (open || node.mapping_may_write) && node.group.is_none())
                        .then(|| {
                            (
                                id,
                                node.handles.first().copied(),
                                node.dirty.last_writer.clone().unwrap_or(NOBODY),
                            )
                        })
                })
                .collect()
        };
        for (id, fh, writer) in candidates {
            let fd = match fh {
                Some(fh) => match self.handle(fh) {
                    Ok(h) => NodeFd::Handle(h),
                    Err(_) => continue,
                },
                None => match self.node_fd(id) {
                    Ok(fd) => fd,
                    Err(_) => continue,
                },
            };
            if let Ok(true) = self.record_node(id, &fd, Trigger::Sweep, writer) {
                report.recorded += 1;
            }
        }
        report
    }

    /// What the kernel may have cached of the path `rel` (relative to the
    /// root), for a change made to the backing tree by someone else (the
    /// history service's restore, design §15): the entry of each component
    /// in its parent, up to the first the table does not know (the kernel
    /// may hold it as a negative entry), and the node at the path, if known
    /// (its attributes and pages).
    pub fn cached_at(&self, rel: &[u8]) -> Vec<Cached> {
        let table = self.table();
        let mut id = ROOT;
        // Every entry on the way: a step may have removed a directory (an
        // emptied one is pruned) or made one.
        let mut targets = Vec::new();
        for name in rel.split(|&b| b == b'/').filter(|n| !n.is_empty()) {
            targets.push(Cached::Entry(id, name.to_vec()));
            match table.child(id, name) {
                Some(child) => id = child,
                None => return targets,
            }
        }
        targets.push(Cached::Node(id));
        targets
    }

    /// Whether the file at `rel` (a path from the mount root) has a handle
    /// open for writing through the mount. A path the mount has never
    /// looked up has no open handle (design §15: a restore through the
    /// mount refuses such files without `--force`).
    pub fn open_for_writing(&self, rel: &[u8]) -> bool {
        let table = self.table();
        let mut id = ROOT;
        for name in rel.split(|&b| b == b'/').filter(|n| !n.is_empty()) {
            match table.child(id, name) {
                Some(child) => id = child,
                None => return false,
            }
        }
        table.get(id).is_some_and(|n| n.write_handles > 0)
    }

    /// How many handles are open (for tests and the shutdown report).
    pub fn open_handles(&self) -> usize {
        self.handles().len()
    }
}

/// What the main mount serves: each method is the inherent method of the
/// same name ([`crate::session`]). `access` always allows: the main mount's
/// kernel checks modes itself (`default_permissions`), so it never asks.
impl crate::session::Operations for Ops {
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        Ops::lookup(self, parent, name)
    }

    fn forget(&self, id: u64, n: u64) {
        Ops::forget(self, id, n);
    }

    fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr> {
        Ops::getattr(self, id, fh)
    }

    fn setattr(&self, id: u64, set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr> {
        Ops::setattr(self, id, set, actor)
    }

    fn readlink(&self, id: u64) -> OpResult<Vec<u8>> {
        Ops::readlink(self, id)
    }

    fn access(&self, _id: u64, _mask: i32) -> OpResult<()> {
        Ok(())
    }

    fn paths(&self, id: u64) -> Vec<Vec<u8>> {
        self.table().paths(id)
    }

    fn mknod(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        Ops::mknod(self, parent, name, mode, rdev, actor)
    }

    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, actor: Actor<'_>) -> OpResult<Entry> {
        Ops::mkdir(self, parent, name, mode, actor)
    }

    fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        Ops::unlink(self, parent, name, actor)
    }

    fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        Ops::rmdir(self, parent, name, actor)
    }

    fn symlink(
        &self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        Ops::symlink(self, parent, name, target, actor)
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
        Ops::rename(self, parent, name, newparent, newname, flags, actor)
    }

    fn link(&self, id: u64, newparent: u64, newname: &[u8], actor: Actor<'_>) -> OpResult<Entry> {
        Ops::link(self, id, newparent, newname, actor)
    }

    fn open(&self, id: u64, flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
        Ops::open(self, id, flags, actor)
    }

    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        Ops::create(self, parent, name, mode, flags, actor)
    }

    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        Ops::read(self, fh, offset, size)
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32> {
        Ops::write(self, fh, offset, data, actor)
    }

    fn flush(&self, fh: u64, actor: Actor<'_>) -> OpResult<()> {
        Ops::flush(self, fh, actor)
    }

    fn release(&self, fh: u64) -> OpResult<()> {
        Ops::release(self, fh)
    }

    fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()> {
        Ops::fsync(self, fh, datasync)
    }

    fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        Ops::fallocate(self, fh, offset, length, mode, actor)
    }

    fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64> {
        Ops::lseek(self, fh, offset, whence)
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
        Ops::copy_file_range(self, fh_in, offset_in, fh_out, offset_out, len, actor)
    }

    fn mapping_may_write(&self, id: u64) {
        Ops::mapping_may_write(self, id);
    }

    fn opendir(&self, id: u64) -> OpResult<u64> {
        Ops::opendir(self, id)
    }

    fn readdir(&self, _id: u64, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        Ops::readdir(self, fh, offset)
    }

    fn releasedir(&self, fh: u64) {
        Ops::releasedir(self, fh);
    }

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        Ops::statfs(self)
    }

    fn setxattr(&self, id: u64, name: &[u8], value: &[u8], flags: i32) -> OpResult<()> {
        Ops::setxattr(self, id, name, value, flags)
    }

    fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr> {
        Ops::getxattr(self, id, name, size)
    }

    fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr> {
        Ops::listxattr(self, id, size)
    }

    fn removexattr(&self, id: u64, name: &[u8]) -> OpResult<()> {
        Ops::removexattr(self, id, name)
    }
}

impl crate::session::TreeOperations for Ops {
    fn new_node_id(&self) -> u64 {
        self.table().new_id()
    }

    fn new_handle_id(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn kind_at(&self, parent: u64, name: &[u8]) -> OpResult<Kind> {
        Ops::kind_at(self, parent, name)
    }

    fn move_in(
        &self,
        from: BorrowedFd<'_>,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        moved: crate::session::Moved,
        check: &dyn crate::session::NameCheck,
        writer: &Writer,
    ) -> OpResult<Entry> {
        // Taken by the attempt that moves it: one that meets a foreign
        // group starts over before.
        let moved = RefCell::new(Some(moved));
        self.again(|retry| {
            self.move_in_once(
                from, name, newparent, newname, flags, &moved, check, writer, retry,
            )
        })
    }
}

impl Ops {
    #[allow(clippy::too_many_arguments)]
    fn move_in_once(
        &self,
        from: BorrowedFd<'_>,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        moved: &RefCell<Option<crate::session::Moved>>,
        check: &dyn crate::session::NameCheck,
        writer: &Writer,
        retry: &Retry,
    ) -> OpResult<Entry> {
        check_name(newname)?;
        let flags = match RenameFlags::from_bits(flags) {
            Some(f) if f.is_empty() || f == RenameFlags::NOREPLACE => f,
            _ => return Err(Errno::EINVAL),
        };
        let names = self.write_names();
        let ddir = self.open_dir_node(newparent)?;
        let dst_path = join(&self.path_of(newparent)?, newname);
        let dst =
            rustix::fs::statat(&ddir, OsStr::from_bytes(newname), AtFlags::SYMLINK_NOFOLLOW).ok();
        let src =
            rustix::fs::statat(from, OsStr::from_bytes(name), AtFlags::SYMLINK_NOFOLLOW).eno()?;
        if Kind::from_mode(mode_of(&src)) != Kind::File {
            return Err(Errno::EINVAL);
        }
        // Decided under the names ordering, on what is there now: an error
        // is the answer, and nothing has moved.
        check.move_in(crate::session::Found {
            path: &dst_path,
            kind: dst.as_ref().map(|st| Kind::from_mode(mode_of(st))),
        })?;
        // Moving a file over a member changes its group: held.
        let group = self.group_at(&dst_path);
        let hold = self.hold(group.as_ref(), writer, retry)?;
        let Some(moved) = moved.borrow_mut().take() else {
            return Err(Errno::EIO);
        };
        rustix::fs::renameat_with(
            from,
            OsStr::from_bytes(name),
            &ddir,
            OsStr::from_bytes(newname),
            flags,
        )
        .eno()?;
        if let Some(hold) = &hold {
            hold.changed();
        }
        drop(hold);
        let st = rustix::fs::statat(&ddir, OsStr::from_bytes(newname), AtFlags::SYMLINK_NOFOLLOW)
            .unwrap_or(src);
        let (dev, ino) = dev_ino(&src);
        // The kernel keeps the node id and handle ids it knows the file by.
        let write = moved.handles.iter().any(|(_, h)| h.write);
        {
            let mut handles = self.handles();
            for (fh, handle) in &moved.handles {
                handles.insert(*fh, handle.clone());
            }
        }
        {
            let mut table = self.table();
            table.adopt(
                moved.node,
                newparent,
                newname,
                dev,
                ino,
                Kind::File,
                moved.lookups,
            );
            if let Some(node) = table.get_mut(moved.node) {
                node.mapping_may_write = moved.mapping_may_write;
                for (fh, handle) in &moved.handles {
                    node.handles.push(*fh);
                    if handle.write {
                        node.write_handles += 1;
                    }
                }
                // As `add_handle` arms a node opened for writing: later
                // writes through the handles are recorded at their flush or
                // release.
                if write {
                    node.dirty.arm(writer.clone());
                    if node.dirty.baseline.is_none() {
                        node.dirty.baseline = Some(stamp(&st));
                    }
                }
            }
            table.maybe_drop(moved.node);
        }
        // Adopted handles find their fsync-class group at the new path.
        self.renames.fetch_add(1, Ordering::AcqRel);
        drop(names);

        let _names = self.read_names();
        if self
            .record_tree(
                Trigger::Rename,
                writer.clone(),
                vec![],
                std::slice::from_ref(&dst_path),
                None,
            )
            .is_err()
        {
            self.boundary.queue_repair([dst_path], writer.clone());
            return Err(Errno::EIO);
        }
        Ok(Entry {
            id: moved.node,
            attr: attr(moved.node, &st),
        })
    }
}

#[cfg(test)]
mod tests;
