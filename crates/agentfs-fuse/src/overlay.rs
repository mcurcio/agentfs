//! The sandbox overlay's session: the visibility gate and the view owner in
//! front of the shared operations (design §1.1, §6).
//!
//! [`Overlay`] decorates an [`Operations`] (the daemon's one
//! [`crate::ops::Ops`]). It asks the gate ([`agentfs_core::visibility`])
//! before every name operation, checks every access as the view owner
//! ([`ViewOwner`], [`crate::access`]) and hands the operations the view
//! owner as the caller. The operations never know it is there.
//!
//! What it answers (`P(x)` is node `x`'s path, the root `""`; a node is
//! *shown* when one of its current paths is shown for its kind; the
//! owner's checks are [`crate::access::permits`] on the object's
//! attributes):
//!
//! | Operation | Answer |
//! |---|---|
//! | `lookup` | parent not shown: `ENOENT`; owner may not search parent: `EACCES`; `may_show` hidden or the name not a repository path: `ENOENT` (`EIO` on a rules error) without asking the operations; else looked up, and a result not shown for its kind is forgotten again and answered `ENOENT` (`EIO`) |
//! | `getattr`, `readlink`, `getxattr`, `listxattr`, `access` | node not shown: `ENOENT`; through an open handle: passed on. `access` checks the mask as the owner (`EACCES`) |
//! | `opendir` / `readdir` | not shown: `ENOENT`; owner may not read: `EACCES`; entries not shown for their kind are left out (rules errors too); `.` and `..` stay |
//! | `open` | not shown: `ENOENT`; owner lacks the access the flags ask: `EACCES` |
//! | `create`, `mknod` of a regular file | parent shown, owner may write and search it; `place_new(File)`: tree: passed on; tmp layer: `EACCES` until the tmp layer is built (slice 3); refused: `EACCES` (`EIO` on a rules error). Decided before the operations run, so a hidden name never answers `EEXIST`. An existing file opened without `O_EXCL` needs the owner's access to it |
//! | `mkdir` | as `create`, with `place_new(Directory)` |
//! | `symlink`, `link`, `mknod` of anything else | `EACCES`, whatever exists |
//! | `unlink`, `rmdir` | name hidden for the kind found: `ENOENT`; owner may not write and search the parent, or the sticky rule refuses: `EACCES` |
//! | `rename` | `RENAME_WHITEOUT`: `EINVAL`; source hidden: `ENOENT`; `place_move`: cross-device: `EXDEV`; refused: `EACCES` (`EIO`); tree: passed on, after the owner's checks on both parents. `RENAME_EXCHANGE` needs both ends to place in the tree |
//! | `setattr` | not shown: `ENOENT`; a uid or gid: `EPERM`; a mode with set-id or sticky bits, or a mode change by an owner who does not own the object: `EPERM`; a size without a handle the owner may not write: `EACCES`; times as for `utimensat(2)` |
//! | `setxattr`, `removexattr` | `ENOTSUP` |
//! | `read`, `write`, `flush`, `release`, `fsync`, `fallocate`, `lseek`, `copy_file_range`, `releasedir`, `statfs`, `forget`, `mapping_may_write`, `paths` | passed on: handles were gated when opened |
//!
//! Every operation that the decorator passes on gets
//! [`ViewOwner::caller`] of the request's process, never the request's own
//! uid and gid.

use std::sync::Arc;

use agentfs_core::paths::RepoPath;
use agentfs_core::record::Caller;
use agentfs_core::visibility::{
    Denial, ExistingKind, Gate, Hidden, NewKind, Placement, Source, Visibility,
};
use fuser::{Errno, FileAttr, FileType, TimeOrNow};
use rustix::fs::{OFlags, RenameFlags};

use crate::access::{self, Object, Want, Who};
use crate::backing::join;
use crate::inodes::ROOT;
use crate::ops::{DirEntry, Entry, Handle, OpResult, SetAttr, Xattr};
use crate::privileges::Privileges;
use crate::session::Operations;

/// `--sandbox-owner UID:GID`, parsed: numeric ids, not yet checked
/// against the daemon's privileges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerIds {
    pub uid: u32,
    pub gid: u32,
}

impl std::str::FromStr for OwnerIds {
    type Err = String;

    /// `UID:GID`, both decimal.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = |part: &str| {
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse::<u32>().ok())
                .flatten()
        };
        match s.split_once(':') {
            Some((uid, gid)) => match (id(uid), id(gid)) {
                (Some(uid), Some(gid)) => Ok(Self { uid, gid }),
                _ => Err(format!("{s:?} is not UID:GID (two decimal numeric ids)")),
            },
            None => Err(format!("{s:?} is not UID:GID (two decimal numeric ids)")),
        }
    }
}

impl std::fmt::Display for OwnerIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// Why the daemon cannot act as the view owner.
#[derive(Debug, thiserror::Error)]
pub enum OwnerError {
    #[error(
        "--sandbox-owner {uid}:{gid}: the daemon (uid {euid}) can act only as itself without \
         CAP_SETUID, CAP_SETGID, CAP_CHOWN and CAP_FOWNER (run it as root, or as the owner)"
    )]
    CannotActAs { uid: u32, gid: u32, euid: u32 },
}

/// The one user every operation through the sandbox overlay is checked
/// and done as, whoever the caller is (NFS's `all_squash` with
/// `anonuid`/`anongid`): its uid and gid, and no supplementary groups.
///
/// Built only by [`ViewOwner::new`], which checks the daemon can act as it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewOwner {
    uid: u32,
    gid: u32,
}

impl ViewOwner {
    /// The view owner `ids`, for a daemon with `privileges` and effective
    /// uid `euid`: refused unless the daemon is that uid or can create as
    /// a caller ([`Privileges::can_create_as_caller`]).
    pub fn new(ids: OwnerIds, privileges: &Privileges, euid: u32) -> Result<Self, OwnerError> {
        if ids.uid != euid && !privileges.can_create_as_caller() {
            return Err(OwnerError::CannotActAs {
                uid: ids.uid,
                gid: ids.gid,
                euid,
            });
        }
        Ok(Self {
            uid: ids.uid,
            gid: ids.gid,
        })
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// The caller the operations are handed for a request from process
    /// `pid`: the owner's uid and gid, the request's pid.
    pub fn caller(&self, pid: u32) -> Caller {
        Caller {
            uid: self.uid,
            gid: self.gid,
            pid,
        }
    }

    /// Whether the owner gets `want` on `object` ([`crate::access::permits`]
    /// with no supplementary groups).
    pub fn permits(&self, object: Object, want: Want) -> bool {
        access::permits(object, self.who(), want)
    }

    fn who(&self) -> Who<'static> {
        Who {
            uid: self.uid,
            gid: self.gid,
            groups: &[],
        }
    }

    /// Whether the owner owns `attr`'s object, or is uid 0 (the
    /// owner-or-capable test of `chmod(2)` and `utimensat(2)`).
    fn owns(&self, attr: &FileAttr) -> bool {
        self.uid == 0 || self.uid == attr.uid
    }
}

/// The errno for a hidden name: `ENOENT`, or `EIO` when the rules could not
/// decide.
pub fn hidden_errno(hidden: Hidden) -> Errno {
    match hidden {
        Hidden::NotRecordable => Errno::ENOENT,
        Hidden::RulesError => Errno::EIO,
    }
}

/// Whether a placement lets the operation go on to the operations, and
/// otherwise its errno: tree goes on; tmp layer is `EACCES` until the tmp
/// layer is built (slice 3); cross-device is `EXDEV`; a refusal is `EACCES`, `ENOENT`
/// for a hidden source, `EIO` for a rules error.
pub fn admit(placement: Placement) -> OpResult<()> {
    match placement {
        Placement::Tree => Ok(()),
        Placement::TmpLayer => Err(Errno::EACCES),
        Placement::CrossDevice => Err(Errno::EXDEV),
        Placement::Refused(Denial::SourceHidden) => Err(Errno::ENOENT),
        Placement::Refused(Denial::RulesError) => Err(Errno::EIO),
        Placement::Refused(Denial::Kind | Denial::NotRecordable) => Err(Errno::EACCES),
    }
}

/// What an existing entry of fuser kind `kind` is to the gate.
fn existing_kind(kind: FileType) -> ExistingKind {
    match kind {
        FileType::RegularFile => ExistingKind::File,
        FileType::Directory => ExistingKind::Directory,
        FileType::Symlink => ExistingKind::Symlink,
        FileType::NamedPipe | FileType::CharDevice | FileType::BlockDevice | FileType::Socket => {
            ExistingKind::Special
        }
    }
}

/// The repository path of backing-tree path `raw`, if it is one: bytes
/// that are not UTF-8 or not a [`RepoPath`] count as hidden.
fn repo_path(raw: &[u8]) -> Option<RepoPath> {
    RepoPath::new(std::str::from_utf8(raw).ok()?).ok()
}

/// The stronger of two reasons to hide: a rules error wins, failing closed.
fn worse(a: Hidden, b: Hidden) -> Hidden {
    if a == Hidden::RulesError || b == Hidden::RulesError {
        Hidden::RulesError
    } else {
        Hidden::NotRecordable
    }
}

/// A new object's mode without set-id and sticky bits, which the overlay
/// never sets (as `setattr` refuses them).
fn plain_mode(mode: u32) -> u32 {
    mode & !0o7000
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const O_EXCL: i32 = OFlags::EXCL.bits() as i32;

const READ: Want = Want {
    read: true,
    write: false,
    exec: false,
};
const SEARCH: Want = Want {
    read: false,
    write: false,
    exec: true,
};
const WRITE: Want = Want {
    read: false,
    write: true,
    exec: false,
};
/// Adding or removing a name in a directory.
const WRITE_SEARCH: Want = Want {
    read: false,
    write: true,
    exec: true,
};

/// A shown node: one of its paths that is shown, and its attributes.
struct Shown {
    path: Vec<u8>,
    attr: FileAttr,
}

/// The sandbox overlay: [`Operations`] behind the visibility gate, done as
/// the view owner.
pub struct Overlay<O: Operations> {
    inner: Arc<O>,
    gate: Gate,
    owner: ViewOwner,
}

impl<O: Operations> Overlay<O> {
    pub fn new(inner: Arc<O>, gate: Gate, owner: ViewOwner) -> Self {
        Self { inner, gate, owner }
    }

    /// Node `id` if it is shown: the root, or a node one of whose current
    /// paths is shown for its kind. Every path is first asked
    /// existence-blind ([`Gate::may_show`]), so a node with no possibly
    /// shown path is answered without asking its attributes.
    fn shown(&self, id: u64) -> OpResult<Shown> {
        if id == ROOT {
            let attr = self.inner.getattr(id, None)?;
            return Ok(Shown {
                path: Vec::new(),
                attr,
            });
        }
        let mut hidden = Hidden::NotRecordable;
        let mut candidates = Vec::new();
        for raw in self.inner.paths(id) {
            match repo_path(&raw).map(|path| (self.gate.may_show(&path), path)) {
                Some((Visibility::Shown, path)) => candidates.push((raw, path)),
                Some((Visibility::Hidden(h), _)) => hidden = worse(hidden, h),
                None => {}
            }
        }
        if candidates.is_empty() {
            return Err(hidden_errno(hidden));
        }
        let attr = self.inner.getattr(id, None)?;
        let kind = existing_kind(attr.kind);
        for (raw, path) in candidates {
            match self.gate.show(&path, kind) {
                Visibility::Shown => return Ok(Shown { path: raw, attr }),
                Visibility::Hidden(h) => hidden = worse(hidden, h),
            }
        }
        Err(hidden_errno(hidden))
    }

    /// The repository path of `name` in shown directory `dir`.
    fn child(dir: &Shown, name: &[u8]) -> Option<RepoPath> {
        repo_path(&join(&dir.path, name))
    }

    /// `EACCES` unless the owner gets `want` on `attr`'s object.
    fn require(&self, attr: &FileAttr, want: Want) -> OpResult<()> {
        if self.owner.permits(Object::from_attr(attr), want) {
            Ok(())
        } else {
            Err(Errno::EACCES)
        }
    }

    /// `EACCES` unless the owner may remove or replace `entry` in `dir`:
    /// write and search there, and the sticky rule.
    fn require_unlink(&self, dir: &FileAttr, entry: &FileAttr) -> OpResult<()> {
        self.require(dir, WRITE_SEARCH)?;
        if access::may_unlink_in(Object::from_attr(dir), entry.uid, self.owner.who()) {
            Ok(())
        } else {
            Err(Errno::EACCES)
        }
    }

    /// The existing entry `name` in `parent`, its kernel lookup given back
    /// at once. Only for a name the gate may show.
    fn peek(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        let entry = self.inner.lookup(parent, name)?;
        self.inner.forget(entry.id, 1);
        Ok(entry)
    }

    /// `name` in shown directory `parent` for a new object of kind `kind`:
    /// the owner may write and search the directory and the gate places
    /// the name in the tree. Decided before the operations are asked
    /// anything about the name.
    fn place_new(&self, parent: u64, name: &[u8], kind: NewKind) -> OpResult<()> {
        let dir = self.shown(parent)?;
        self.require(&dir.attr, WRITE_SEARCH)?;
        let path = Self::child(&dir, name).ok_or(Errno::EACCES)?;
        admit(self.gate.place_new(&path, kind))
    }

    /// `unlink` and `rmdir`.
    fn remove(&self, parent: u64, name: &[u8]) -> OpResult<()> {
        let dir = self.shown(parent)?;
        let path = Self::child(&dir, name).ok_or(Errno::ENOENT)?;
        if let Visibility::Hidden(h) = self.gate.may_show(&path) {
            return Err(hidden_errno(h));
        }
        let entry = self.peek(parent, name)?;
        if let Visibility::Hidden(h) = self.gate.show(&path, existing_kind(entry.attr.kind)) {
            return Err(hidden_errno(h));
        }
        self.require_unlink(&dir.attr, &entry.attr)
    }

    /// The checks of `setattr` on an object with attributes `attr`
    /// (`chmod(2)`, `truncate(2)` and `utimensat(2)` as the owner;
    /// ownership never changes).
    fn may_setattr(&self, attr: &FileAttr, set: &SetAttr) -> OpResult<()> {
        if set.uid.is_some() || set.gid.is_some() {
            return Err(Errno::EPERM);
        }
        if let Some(mode) = set.mode
            && (mode & 0o7000 != 0 || !self.owner.owns(attr))
        {
            return Err(Errno::EPERM);
        }
        if set.size.is_some() && set.fh.is_none() {
            self.require(attr, WRITE)?;
        }
        let explicit = |t: Option<TimeOrNow>| matches!(t, Some(TimeOrNow::SpecificTime(_)));
        let now = |t: Option<TimeOrNow>| matches!(t, Some(TimeOrNow::Now));
        if explicit(set.atime) || explicit(set.mtime) {
            if !self.owner.owns(attr) {
                return Err(Errno::EPERM);
            }
        } else if (now(set.atime) || now(set.mtime)) && !self.owner.owns(attr) {
            self.require(attr, WRITE)?;
        }
        Ok(())
    }
}

impl<O: Operations> Operations for Overlay<O> {
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        let dir = self.shown(parent)?;
        self.require(&dir.attr, SEARCH)?;
        let path = Self::child(&dir, name).ok_or(Errno::ENOENT)?;
        if let Visibility::Hidden(h) = self.gate.may_show(&path) {
            return Err(hidden_errno(h));
        }
        let entry = self.inner.lookup(parent, name)?;
        if let Visibility::Hidden(h) = self.gate.show(&path, existing_kind(entry.attr.kind)) {
            self.inner.forget(entry.id, 1);
            return Err(hidden_errno(h));
        }
        Ok(entry)
    }

    fn forget(&self, id: u64, n: u64) {
        self.inner.forget(id, n);
    }

    fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr> {
        match fh {
            Some(_) => self.inner.getattr(id, fh),
            None => Ok(self.shown(id)?.attr),
        }
    }

    fn setattr(&self, id: u64, set: &SetAttr, caller: Caller) -> OpResult<FileAttr> {
        // Through an open handle, gated when it was opened; else the node
        // must be shown.
        let attr = match set.fh {
            Some(fh) => self.inner.getattr(id, Some(fh))?,
            None => self.shown(id)?.attr,
        };
        self.may_setattr(&attr, set)?;
        self.inner.setattr(id, set, self.owner.caller(caller.pid))
    }

    fn readlink(&self, id: u64) -> OpResult<Vec<u8>> {
        self.shown(id)?;
        self.inner.readlink(id)
    }

    fn access(&self, id: u64, mask: i32) -> OpResult<()> {
        let node = self.shown(id)?;
        self.require(&node.attr, Want::from_mask(mask))
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
        caller: Caller,
    ) -> OpResult<Entry> {
        // Only a regular file: devices, FIFOs and sockets are never placed.
        let kind = match mode & S_IFMT {
            0 | S_IFREG => NewKind::File,
            _ => return Err(Errno::EACCES),
        };
        self.place_new(parent, name, kind)?;
        self.inner.mknod(
            parent,
            name,
            plain_mode(mode),
            rdev,
            self.owner.caller(caller.pid),
        )
    }

    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, caller: Caller) -> OpResult<Entry> {
        self.place_new(parent, name, NewKind::Directory)?;
        self.inner
            .mkdir(parent, name, plain_mode(mode), self.owner.caller(caller.pid))
    }

    fn unlink(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.remove(parent, name)?;
        self.inner
            .unlink(parent, name, self.owner.caller(caller.pid))
    }

    fn rmdir(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.remove(parent, name)?;
        self.inner.rmdir(parent, name, self.owner.caller(caller.pid))
    }

    /// Never offered: the gate places no symlink ([`NewKind::Symlink`]).
    fn symlink(
        &self,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
        _caller: Caller,
    ) -> OpResult<Entry> {
        Err(Errno::EACCES)
    }

    fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        caller: Caller,
    ) -> OpResult<()> {
        if flags & RenameFlags::WHITEOUT.bits() != 0 {
            return Err(Errno::EINVAL);
        }
        let from_dir = self.shown(parent)?;
        let to_dir = self.shown(newparent)?;
        let from = Self::child(&from_dir, name).ok_or(Errno::ENOENT)?;
        if let Visibility::Hidden(h) = self.gate.may_show(&from) {
            return Err(hidden_errno(h));
        }
        let to = Self::child(&to_dir, newname).ok_or(Errno::EACCES)?;
        let src = self.peek(parent, name)?;
        admit(
            self.gate
                .place_move(Source::Tree(&from, existing_kind(src.attr.kind)), &to),
        )?;
        // The target may be shown now: its name is recordable.
        let dst = if flags & RenameFlags::EXCHANGE.bits() != 0 {
            let dst = self.peek(newparent, newname)?;
            admit(
                self.gate
                    .place_move(Source::Tree(&to, existing_kind(dst.attr.kind)), &from),
            )?;
            Some(dst)
        } else {
            self.peek(newparent, newname).ok()
        };
        self.require_unlink(&from_dir.attr, &src.attr)?;
        match &dst {
            Some(dst) => self.require_unlink(&to_dir.attr, &dst.attr)?,
            None => self.require(&to_dir.attr, WRITE_SEARCH)?,
        }
        self.inner.rename(
            parent,
            name,
            newparent,
            newname,
            flags,
            self.owner.caller(caller.pid),
        )
    }

    /// Never offered: the gate places no hard link, whatever its source
    /// ([`NewKind::Hardlink`]).
    fn link(&self, _id: u64, _newparent: u64, _newname: &[u8], _caller: Caller) -> OpResult<Entry> {
        Err(Errno::EACCES)
    }

    fn open(&self, id: u64, flags: i32, caller: Caller) -> OpResult<(u64, Arc<Handle>)> {
        let node = self.shown(id)?;
        self.require(&node.attr, Want::from_open_flags(flags))?;
        self.inner.open(id, flags, self.owner.caller(caller.pid))
    }

    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        caller: Caller,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        self.place_new(parent, name, NewKind::File)?;
        // Without O_EXCL an existing file is opened: the owner needs the
        // access the flags ask of it.
        if flags & O_EXCL == 0
            && let Ok(existing) = self.peek(parent, name)
            && existing.attr.kind == FileType::RegularFile
        {
            self.require(&existing.attr, Want::from_open_flags(flags))?;
        }
        self.inner.create(
            parent,
            name,
            plain_mode(mode),
            flags,
            self.owner.caller(caller.pid),
        )
    }

    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        self.inner.read(fh, offset, size)
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8], caller: Caller) -> OpResult<u32> {
        self.inner
            .write(fh, offset, data, self.owner.caller(caller.pid))
    }

    fn flush(&self, fh: u64, caller: Caller) -> OpResult<()> {
        self.inner.flush(fh, self.owner.caller(caller.pid))
    }

    fn release(&self, fh: u64) -> OpResult<()> {
        self.inner.release(fh)
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
        caller: Caller,
    ) -> OpResult<()> {
        self.inner
            .fallocate(fh, offset, length, mode, self.owner.caller(caller.pid))
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
        caller: Caller,
    ) -> OpResult<u32> {
        self.inner.copy_file_range(
            fh_in,
            offset_in,
            fh_out,
            offset_out,
            len,
            self.owner.caller(caller.pid),
        )
    }

    fn mapping_may_write(&self, id: u64) {
        self.inner.mapping_may_write(id);
    }

    fn opendir(&self, id: u64) -> OpResult<u64> {
        let node = self.shown(id)?;
        self.require(&node.attr, READ)?;
        self.inner.opendir(id)
    }

    fn readdir(&self, id: u64, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        let dir = self.shown(id)?;
        let entries = self.inner.readdir(id, fh, offset)?;
        Ok(entries
            .into_iter()
            .filter(|(_, e)| {
                e.name == b"." || e.name == b".." || {
                    Self::child(&dir, &e.name).is_some_and(|path| {
                        self.gate.show(&path, existing_kind(e.kind)) == Visibility::Shown
                    })
                }
            })
            .collect())
    }

    fn releasedir(&self, fh: u64) {
        self.inner.releasedir(fh);
    }

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        self.inner.statfs()
    }

    fn setxattr(&self, _id: u64, _name: &[u8], _value: &[u8], _flags: i32) -> OpResult<()> {
        Err(Errno::ENOTSUP)
    }

    fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr> {
        self.shown(id)?;
        self.inner.getxattr(id, name, size)
    }

    fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr> {
        self.shown(id)?;
        self.inner.listxattr(id, size)
    }

    fn removexattr(&self, _id: u64, _name: &[u8]) -> OpResult<()> {
        Err(Errno::ENOTSUP)
    }
}

#[cfg(test)]
mod tests;
