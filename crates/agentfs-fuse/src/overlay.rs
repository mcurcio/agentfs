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
//! | `lookup` | parent not shown: `ENOENT`; owner may not search parent: `EACCES`; a name the tmp layer holds: its file (the upper layer wins, as in overlayfs); `may_show` hidden or the name not a repository path: `ENOENT` (`EIO` on a rules error) without asking the operations; else looked up, and a result not shown for its kind is forgotten again and answered `ENOENT` (`EIO`) |
//! | `getattr`, `readlink`, `access` | node not shown: `ENOENT`; through an open handle: passed on. `access` checks the mask as the owner (`EACCES`) |
//! | `getxattr` | node not shown: `ENOENT`; a name outside `user.*`: `ENODATA`; owner may not read the node: `EACCES` |
//! | `listxattr` | node not shown: `ENOENT`; the `user.*` names only, sized for that list |
//! | `opendir` / `readdir` | not shown: `ENOENT`; owner may not read: `EACCES`; the overlay's own snapshot, with offsets 1 to n and no gaps: `.` and `..`, the tree's entries shown for their kind whose names the tmp layer does not hold, then the tmp layer's files |
//! | `open` | not shown: `ENOENT`; owner lacks the access the flags ask (execute too for an open to execute, `__FMODE_EXEC`): `EACCES` |
//! | `create`, `mknod` of a regular file | parent shown, owner may write and search it; a name the tmp layer holds: `EEXIST` with `O_EXCL` (and for `mknod`), else its file opened as `open` does; `place_new(File)`: tree: passed on; tmp layer: a shown directory there is `EEXIST` (`EISDIR` for `create` without `O_EXCL`), else a new tmp file; refused: `EACCES` (`EIO` on a rules error). A placed name in the tree that holds an entry hidden for its kind (a special file, a nested repository) is refused the same way (`EACCES`), decided by the [`Judge`] under the names ordering on what is there then, so the operations never touch a hidden entry and it never answers `EEXIST`. An existing file opened without `O_EXCL` needs the owner's access to it, which the operations check with no supplementary groups |
//! | `mkdir` | a name the tmp layer holds: `EEXIST`; else as `create`, with `place_new(Directory)` |
//! | `symlink`, `link`, `mknod` of anything else | `EACCES`, whatever exists |
//! | `unlink`, `rmdir` | a tmp file: removed from the layer once the owner may write and search the parent (`EACCES`); name hidden for the kind found: `ENOENT`; owner may not write and search the parent, or the sticky rule refuses: `EACCES`; `rmdir` of a directory holding tmp files: `ENOTEMPTY` |
//! | `rename` | `RENAME_WHITEOUT`: `EINVAL`; source hidden: `ENOENT`; `place_move`: cross-device: `EXDEV`; refused: `EACCES` (`EIO`); an existing target hidden for its kind: `EACCES` (`EIO`), never replaced; tree: passed on, after the owner's checks on both parents. `RENAME_EXCHANGE` needs both ends to place in the tree. A tmp source: `RENAME_EXCHANGE` is `EACCES`; to a skipped name it stays in the layer; to a recordable name it moves into the tree ([`TreeOperations::move_in`], judged by [`Judge`] under the names ordering) |
//! | `setattr` | not shown: `ENOENT`; a uid or gid: `EPERM`; a mode with set-id or sticky bits, or a mode change by an owner who does not own the object: `EPERM`; a size without a handle the owner may not write: `EACCES`; times as for `utimensat(2)` |
//! | `setxattr`, `removexattr` | `ENOTSUP` |
//! | `read`, `write`, `flush`, `release`, `fsync`, `fallocate`, `lseek`, `statfs`, `forget`, `mapping_may_write`, `paths` | passed on: handles were gated when opened |
//! | `copy_file_range` | between two tmp files: in the layer; between a tmp file and a tree file: `EXDEV` (programs fall back to reading and writing); else passed on |
//!
//! A tmp file ([`tmp`]) answers for itself: its attributes (`getattr`,
//! `access`, `setattr` after the same checks, `open` after the owner's
//! access check), `readlink` `EINVAL`, no extended attributes, no paths in
//! the tree, and its handles' reads and writes (a `flush` records
//! nothing).
//!
//! Every operation that the decorator passes on gets
//! [`ViewOwner::actor`] of the request's process: the view owner's uid and
//! gid with no supplementary groups, never the request's own, and the
//! [`Judge`], which the operations ask under the names ordering. Each
//! refused change is counted once ([`RefusalCounter`]): by the overlay when
//! it refuses before calling the operations, by the Judge when it refuses
//! under the names ordering. Reads of hidden names are not counted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use agentfs_core::paths::RepoPath;
use agentfs_core::record::Caller;
use agentfs_core::telemetry::OverlayRefusal;
use agentfs_core::visibility::{
    Denial, ExistingKind, Gate, Hidden, NewKind, Placement, Source, Visibility,
};
use fuser::{Errno, FileAttr, FileType, TimeOrNow};
use rustix::fs::{OFlags, RenameFlags};

use crate::access::{self, Object, Want, Who};
use crate::backing::join;
use crate::creds::Supplementary;
use crate::inodes::{Kind, ROOT};
use crate::ops::{DirEntry, Entry, FMODE_EXEC, Handle, OpResult, SetAttr, Xattr};
use crate::privileges::Privileges;
use crate::session::{Actor, Found, NameCheck, Operations, TreeOperations};

pub mod tmp;

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

    /// Who a request from process `pid` acts as through the operations
    /// (sandbox overlay slice 4): the owner's uid and gid with no
    /// supplementary groups ([`crate::creds::Supplementary::None`]), the
    /// request's pid, and `check`. The only way the overlay makes an
    /// [`Actor`].
    pub fn actor<'a>(&self, pid: u32, check: &'a dyn NameCheck) -> Actor<'a> {
        Actor {
            caller: self.caller(pid),
            groups: Supplementary::None,
            check: Some(check),
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
/// otherwise its errno: tree goes on; cross-device is `EXDEV`; a refusal
/// is `EACCES`, `ENOENT` for a hidden source, `EIO` for a rules error. The
/// overlay routes tmp placements to the layer; one that reaches admit is
/// refused (`EACCES`).
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

/// Why a placement refuses a change, as counted
/// ([`agentfs_core::telemetry::Telemetry::overlay_refused`]); `None` for a
/// placement that lets the change go on (the tree or the tmp layer).
pub fn refusal_reason(placement: Placement) -> Option<OverlayRefusal> {
    match placement {
        Placement::Tree | Placement::TmpLayer => None,
        Placement::CrossDevice => Some(OverlayRefusal::CrossDevice),
        Placement::Refused(Denial::Kind) => Some(OverlayRefusal::Kind),
        Placement::Refused(Denial::NotRecordable) => Some(OverlayRefusal::NotRecordable),
        Placement::Refused(Denial::SourceHidden) => Some(OverlayRefusal::Hidden),
        Placement::Refused(Denial::RulesError) => Some(OverlayRefusal::RulesError),
    }
}

/// Why a change to a hidden name is refused, as counted.
pub fn hidden_reason(hidden: Hidden) -> OverlayRefusal {
    match hidden {
        Hidden::NotRecordable => OverlayRefusal::Hidden,
        Hidden::RulesError => OverlayRefusal::RulesError,
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

/// What an existing entry of the inode table's kind `kind` is to the gate.
fn kind_to_gate(kind: Kind) -> ExistingKind {
    match kind {
        Kind::File => ExistingKind::File,
        Kind::Dir => ExistingKind::Directory,
        Kind::Symlink => ExistingKind::Symlink,
        Kind::Other => ExistingKind::Special,
    }
}

/// A refusal the [`Judge`] answers: its errno, and why it is counted
/// (`None`: not a refusal, such as a name that is gone).
type Refused = (Errno, Option<OverlayRefusal>);

/// `EACCES` (`EIO` on a rules error) unless an existing entry at `path`,
/// of kind `kind`, is shown for its kind.
fn refuse_hidden_kind(gate: &Gate, path: &RepoPath, kind: ExistingKind) -> Result<(), Refused> {
    match gate.show(path, kind) {
        Visibility::Shown => Ok(()),
        Visibility::Hidden(Hidden::NotRecordable) => {
            Err((Errno::EACCES, Some(OverlayRefusal::Hidden)))
        }
        Visibility::Hidden(Hidden::RulesError) => {
            Err((Errno::EIO, Some(OverlayRefusal::RulesError)))
        }
    }
}

/// [`admit`], with the reason a refusal is counted.
fn admitted(placement: Placement) -> Result<(), Refused> {
    admit(placement).map_err(|e| (e, refusal_reason(placement)))
}

/// A path that is not a repository path, where a name is to be made or
/// moved to: refused as a name the rules cannot record.
const NOT_A_REPO_PATH: Refused = (Errno::EACCES, Some(OverlayRefusal::NotRecordable));

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

/// The prefix of the only extended attributes the overlay shows.
const USER_XATTR: &[u8] = b"user.";

/// The `user.*` names of a NUL-terminated name list, in the same form.
fn user_xattrs(list: &[u8]) -> Vec<u8> {
    list.split_inclusive(|b| *b == 0)
        .filter(|name| name.starts_with(USER_XATTR))
        .flatten()
        .copied()
        .collect()
}

/// The access an open with `flags` wants: as [`Want::from_open_flags`],
/// and execute for an open to execute ([`FMODE_EXEC`]).
fn open_want(flags: i32) -> Want {
    let want = Want::from_open_flags(flags);
    Want {
        exec: want.exec || flags & FMODE_EXEC != 0,
        ..want
    }
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const O_EXCL: i32 = OFlags::EXCL.bits() as i32;
const RENAME_EXCHANGE: u32 = RenameFlags::EXCHANGE.bits();

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

/// Counts one change the overlay refused (sandbox overlay slice 4): the
/// mount wires it to [`agentfs_core::telemetry::Telemetry::overlay_refused`];
/// tests record.
pub type RefusalCounter = Arc<dyn Fn(agentfs_core::telemetry::OverlayRefusal) + Send + Sync>;

/// The overlay's [`NameCheck`] (sandbox overlay slice 3): the visibility
/// gate's answer for a name change the operations are about to make, from
/// what they found under the names ordering, as the overlay answers it
/// ([`admit`], [`hidden_errno`]). Pure: no I/O, no lock.
///
/// | Check | Answer |
/// |---|---|
/// | `create` | a path that is not a repository path: `EACCES`; a file: `place_new(File)` must be tree; a directory: `place_new(Directory)` must be tree; anything else: `EACCES`; then an entry found there must be shown for its kind (`EACCES`, `EIO` on a rules error) |
/// | `link` | `EACCES` |
/// | `remove` | the entry found must be shown for its kind (`ENOENT`, `EIO`); none: `ENOENT` |
/// | `rename` | `RENAME_WHITEOUT`: `EINVAL`; `place_move(Tree(from, kind), to)` admitted; with `RENAME_EXCHANGE`, the entry at `to` must exist and `place_move(Tree(to, kind), from)` be admitted too; otherwise an entry found at `to` must be shown for its kind (`EACCES`, `EIO`) |
/// | `move_in` | `to` must place a tmp-layer file in the tree (`EACCES`, `EIO`); an entry found there must be shown for its kind (`EACCES`, `EIO`) |
///
/// It counts each refusal it answers ([`RefusalCounter`]), and only those:
/// what the overlay refused before calling the operations never reaches it.
pub struct Judge {
    gate: Gate,
    refused: RefusalCounter,
}

impl Judge {
    pub fn new(gate: Gate, refused: RefusalCounter) -> Self {
        Self { gate, refused }
    }

    /// The errno of a check, a refusal counted.
    fn answer(&self, checked: Result<(), Refused>) -> OpResult<()> {
        checked.map_err(|(errno, reason)| {
            if let Some(reason) = reason {
                (self.refused)(reason);
            }
            errno
        })
    }

    /// `EACCES` (`EIO`) when the entry found at `at`, if any, is hidden for
    /// its kind.
    fn existing_shown(&self, path: &RepoPath, at: Found<'_>) -> Result<(), Refused> {
        match at.kind {
            Some(kind) => refuse_hidden_kind(&self.gate, path, kind_to_gate(kind)),
            None => Ok(()),
        }
    }

    fn check_create(&self, at: Found<'_>, kind: Kind) -> Result<(), Refused> {
        let path = repo_path(at.path).ok_or(NOT_A_REPO_PATH)?;
        let new = match kind {
            Kind::File => NewKind::File,
            Kind::Dir => NewKind::Directory,
            Kind::Symlink | Kind::Other => {
                return Err((Errno::EACCES, Some(OverlayRefusal::Kind)));
            }
        };
        admitted(self.gate.place_new(&path, new))?;
        self.existing_shown(&path, at)
    }

    fn check_remove(&self, at: Found<'_>) -> Result<(), Refused> {
        let path = repo_path(at.path).ok_or((Errno::ENOENT, Some(OverlayRefusal::Hidden)))?;
        let kind = at.kind.ok_or((Errno::ENOENT, None))?;
        match self.gate.show(&path, kind_to_gate(kind)) {
            Visibility::Shown => Ok(()),
            Visibility::Hidden(h) => Err((hidden_errno(h), Some(hidden_reason(h)))),
        }
    }

    fn check_rename(&self, from: Found<'_>, to: Found<'_>, flags: u32) -> Result<(), Refused> {
        if flags & RenameFlags::WHITEOUT.bits() != 0 {
            return Err((Errno::EINVAL, None));
        }
        let from_path =
            repo_path(from.path).ok_or((Errno::ENOENT, Some(OverlayRefusal::Hidden)))?;
        let from_kind = from.kind.ok_or((Errno::ENOENT, None))?;
        let to_path = repo_path(to.path).ok_or(NOT_A_REPO_PATH)?;
        admitted(
            self.gate
                .place_move(Source::Tree(&from_path, kind_to_gate(from_kind)), &to_path),
        )?;
        if flags & RENAME_EXCHANGE != 0 {
            let to_kind = to.kind.ok_or((Errno::ENOENT, None))?;
            admitted(
                self.gate
                    .place_move(Source::Tree(&to_path, kind_to_gate(to_kind)), &from_path),
            )
        } else {
            self.existing_shown(&to_path, to)
        }
    }

    fn check_move_in(&self, to: Found<'_>) -> Result<(), Refused> {
        let path = repo_path(to.path).ok_or(NOT_A_REPO_PATH)?;
        // Where a tmp-layer file moved to `path` goes: only the tree is
        // admitted (a skipped name is the layer's, never the tree's).
        admitted(self.gate.place_move(Source::TmpLayer(&path), &path))?;
        self.existing_shown(&path, to)
    }
}

impl NameCheck for Judge {
    fn create(&self, at: Found<'_>, kind: Kind) -> OpResult<()> {
        self.answer(self.check_create(at, kind))
    }

    fn link(&self, _at: Found<'_>) -> OpResult<()> {
        self.answer(Err((Errno::EACCES, Some(OverlayRefusal::Kind))))
    }

    fn remove(&self, at: Found<'_>) -> OpResult<()> {
        self.answer(self.check_remove(at))
    }

    fn rename(&self, from: Found<'_>, to: Found<'_>, flags: u32) -> OpResult<()> {
        self.answer(self.check_rename(from, to, flags))
    }

    fn move_in(&self, to: Found<'_>) -> OpResult<()> {
        self.answer(self.check_move_in(to))
    }
}

/// A shown node: one of its paths that is shown, and its attributes.
struct Shown {
    path: Vec<u8>,
    attr: FileAttr,
}

/// Where a new regular file goes.
enum NewFile {
    /// The tree: the operations decide on what is there, under the names
    /// ordering ([`Judge`]).
    Tree,
    Tmp,
}

/// One directory listing the overlay serves: (offset of the next entry,
/// entry).
type Listing = Arc<Vec<(u64, DirEntry)>>;

/// The sandbox overlay: [`Operations`] behind the visibility gate, done as
/// the view owner, with the tmp layer above the tree.
pub struct Overlay<O: TreeOperations> {
    inner: Arc<O>,
    gate: Gate,
    owner: ViewOwner,
    tmp: Arc<tmp::TmpLayer>,
    judge: Judge,
    /// Counts the changes the overlay refuses before calling the
    /// operations (the [`Judge`] counts its own).
    refused: RefusalCounter,
    /// The overlay's own directory snapshots, by handle id.
    dirs: Mutex<HashMap<u64, Listing>>,
}

impl<O: TreeOperations> Overlay<O> {
    pub fn new(
        inner: Arc<O>,
        gate: Gate,
        owner: ViewOwner,
        tmp: Arc<tmp::TmpLayer>,
        refused: RefusalCounter,
    ) -> Self {
        Self {
            inner,
            judge: Judge::new(gate.clone(), refused.clone()),
            gate,
            owner,
            tmp,
            refused,
            dirs: Mutex::new(HashMap::new()),
        }
    }

    /// Who a request from process `pid` acts as through the operations:
    /// the view owner, with no supplementary groups and the [`Judge`].
    fn as_owner(&self, pid: u32) -> Actor<'_> {
        self.owner.actor(pid, &self.judge)
    }

    /// Counts one change refused here, before the operations are called.
    fn count(&self, reason: OverlayRefusal) {
        (self.refused)(reason);
    }

    /// [`admit`], counting a refusal.
    fn admit(&self, placement: Placement) -> OpResult<()> {
        if let Some(reason) = refusal_reason(placement) {
            self.count(reason);
        }
        admit(placement)
    }

    /// The errno of a change to a hidden name, counted.
    fn refuse_hidden_change(&self, hidden: Hidden) -> Errno {
        self.count(hidden_reason(hidden));
        hidden_errno(hidden)
    }

    /// The repository path of a name to make or move to in shown
    /// directory `dir`; a name that is none is refused (`EACCES`) and
    /// counted.
    fn new_path(&self, dir: &Shown, name: &[u8]) -> OpResult<RepoPath> {
        Self::child(dir, name).ok_or_else(|| {
            self.count(OverlayRefusal::NotRecordable);
            Errno::EACCES
        })
    }

    /// The repository path of an existing name to change in shown
    /// directory `dir`; a name that is none is hidden (`ENOENT`), counted.
    fn changed_path(&self, dir: &Shown, name: &[u8]) -> OpResult<RepoPath> {
        Self::child(dir, name).ok_or_else(|| self.refuse_hidden_change(Hidden::NotRecordable))
    }

    fn dirs(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Listing>> {
        self.dirs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The tmp layer's attributes of node `id`; `None` when it is not a
    /// tmp file.
    fn tmp_attr(&self, id: u64) -> Option<OpResult<FileAttr>> {
        self.tmp.getattr(id, None)
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

    /// The existing entry `name` in `parent`, if there is one ([`Self::peek`]).
    fn existing(&self, parent: u64, name: &[u8]) -> OpResult<Option<Entry>> {
        match self.peek(parent, name) {
            Ok(entry) => Ok(Some(entry)),
            Err(e) if e == Errno::ENOENT => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Shown directory `parent`, in which the owner may write and search
    /// (to add a name).
    fn writable_dir(&self, parent: u64) -> OpResult<Shown> {
        let dir = self.shown(parent)?;
        self.require(&dir.attr, WRITE_SEARCH)?;
        Ok(dir)
    }

    /// `name` in shown, writable directory `parent` for a new object of
    /// kind `kind`: the gate places the name in the tree, decided before
    /// the operations are asked anything about the name. What is at the
    /// name is judged under the names ordering ([`Judge`]).
    fn place_new(&self, parent: u64, name: &[u8], kind: NewKind) -> OpResult<()> {
        let dir = self.writable_dir(parent)?;
        let path = self.new_path(&dir, name)?;
        self.admit(self.gate.place_new(&path, kind))
    }

    /// Where a new regular file `name` in `parent` goes, for `create`
    /// (`exclusive` with `O_EXCL`) and `mknod` (always exclusive). The
    /// caller has checked the tmp layer does not hold the name.
    fn place_file(&self, parent: u64, name: &[u8], exclusive: bool) -> OpResult<NewFile> {
        let dir = self.writable_dir(parent)?;
        let path = self.new_path(&dir, name)?;
        match self.gate.place_new(&path, NewKind::File) {
            Placement::TmpLayer => {}
            placement => {
                self.admit(placement)?;
                return Ok(NewFile::Tree);
            }
        }
        // A tmp file shadows whatever the tree hides at its name, and the
        // tree is never asked about a name the gate hides (existence-blind).
        // Only a directory can be shown at a name skipped for a file.
        if self.gate.may_show(&path) == Visibility::Shown
            && let Some(entry) = self.existing(parent, name)?
            && self.gate.show(&path, existing_kind(entry.attr.kind)) == Visibility::Shown
        {
            return Err(if exclusive {
                Errno::EEXIST
            } else {
                Errno::EISDIR
            });
        }
        Ok(NewFile::Tmp)
    }

    /// `unlink` and `rmdir` of a tree entry: the entry.
    fn remove(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        let dir = self.shown(parent)?;
        let path = self.changed_path(&dir, name)?;
        if let Visibility::Hidden(h) = self.gate.may_show(&path) {
            return Err(self.refuse_hidden_change(h));
        }
        let entry = self.peek(parent, name)?;
        if let Visibility::Hidden(h) = self.gate.show(&path, existing_kind(entry.attr.kind)) {
            return Err(self.refuse_hidden_change(h));
        }
        self.require_unlink(&dir.attr, &entry.attr)?;
        Ok(entry)
    }

    /// `rename` of tmp file `name` in `parent`: within the layer to a
    /// skipped name, into the tree to a recordable one.
    #[allow(clippy::too_many_arguments)]
    fn rename_tmp(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        caller: Caller,
    ) -> OpResult<()> {
        if flags & RENAME_EXCHANGE != 0 {
            self.count(OverlayRefusal::Kind);
            return Err(Errno::EACCES);
        }
        let from_dir = self.writable_dir(parent)?;
        let to_dir = self.writable_dir(newparent)?;
        let from = Self::child(&from_dir, name).ok_or(Errno::ENOENT)?;
        let to = self.new_path(&to_dir, newname)?;
        match self.gate.place_move(Source::TmpLayer(&from), &to) {
            Placement::TmpLayer => {
                // As for a create: a shown directory there is not shadowed.
                if self.gate.may_show(&to) == Visibility::Shown
                    && let Some(entry) = self.existing(newparent, newname)?
                    && self.gate.show(&to, existing_kind(entry.attr.kind)) == Visibility::Shown
                {
                    return Err(Errno::EISDIR);
                }
                self.tmp
                    .rename(parent, name, newparent, newname, flags)
                    .unwrap_or(Err(Errno::ENOENT))
            }
            Placement::Tree => {
                if let Some(dst) = self.existing(newparent, newname)? {
                    self.require_unlink(&to_dir.attr, &dst.attr)?;
                }
                let owner = self.owner.caller(caller.pid);
                self.tmp
                    .move_out(parent, name, |m| {
                        self.inner.move_in(
                            m.store,
                            m.name,
                            newparent,
                            newname,
                            flags,
                            m.moved,
                            &self.judge,
                            owner,
                        )
                    })
                    .unwrap_or(Err(Errno::ENOENT))
                    .map(|_| ())
            }
            placement => self.admit(placement),
        }
    }

    /// The checks of `setattr` on an object with attributes `attr`
    /// (`chmod(2)`, `truncate(2)` and `utimensat(2)` as the owner;
    /// ownership never changes).
    fn may_setattr(&self, attr: &FileAttr, set: &SetAttr) -> OpResult<()> {
        let set_id = set.mode.is_some_and(|mode| mode & 0o7000 != 0);
        if set.uid.is_some() || set.gid.is_some() || set_id {
            self.count(OverlayRefusal::Ownership);
            return Err(Errno::EPERM);
        }
        if set.mode.is_some() && !self.owner.owns(attr) {
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

impl<O: TreeOperations> Overlay<O> {
    /// Every extended attribute name of tree node `id`, as the operations
    /// list them (NUL-terminated names), read whole: asked again when the
    /// list grew between asking its size and reading it.
    fn all_xattrs(&self, id: u64) -> OpResult<Vec<u8>> {
        loop {
            let size = match self.inner.listxattr(id, 0)? {
                Xattr::Size(0) => return Ok(Vec::new()),
                Xattr::Size(n) => n,
                Xattr::Data(data) => return Ok(data),
            };
            match self.inner.listxattr(id, size) {
                Ok(Xattr::Data(data)) => return Ok(data),
                Ok(Xattr::Size(_)) => return Err(Errno::EIO),
                Err(e) if e == Errno::ERANGE => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl<O: TreeOperations> Operations for Overlay<O> {
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        let dir = self.shown(parent)?;
        self.require(&dir.attr, SEARCH)?;
        // The upper layer wins, as in overlayfs.
        if let Some(found) = self.tmp.lookup(parent, name) {
            return found;
        }
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
        if self.tmp.forget(id, n).is_none() {
            self.inner.forget(id, n);
        }
    }

    fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr> {
        if let Some(attr) = self.tmp.getattr(id, fh) {
            return attr;
        }
        match fh {
            Some(_) => self.inner.getattr(id, fh),
            None => Ok(self.shown(id)?.attr),
        }
    }

    fn setattr(&self, id: u64, set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr> {
        if let Some(attr) = self.tmp.getattr(id, set.fh) {
            self.may_setattr(&attr?, set)?;
            return self.tmp.setattr(id, set).unwrap_or(Err(Errno::ENOENT));
        }
        // Through an open handle, gated when it was opened; else the node
        // must be shown.
        let attr = match set.fh {
            Some(fh) => self.inner.getattr(id, Some(fh))?,
            None => self.shown(id)?.attr,
        };
        self.may_setattr(&attr, set)?;
        self.inner.setattr(id, set, self.as_owner(actor.caller.pid))
    }

    fn readlink(&self, id: u64) -> OpResult<Vec<u8>> {
        if let Some(attr) = self.tmp_attr(id) {
            attr?;
            return Err(Errno::EINVAL);
        }
        self.shown(id)?;
        self.inner.readlink(id)
    }

    fn access(&self, id: u64, mask: i32) -> OpResult<()> {
        let attr = match self.tmp_attr(id) {
            Some(attr) => attr?,
            None => self.shown(id)?.attr,
        };
        self.require(&attr, Want::from_mask(mask))
    }

    fn paths(&self, id: u64) -> Vec<Vec<u8>> {
        if self.tmp_attr(id).is_some() {
            return Vec::new();
        }
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
        // Only a regular file: devices, FIFOs and sockets are never placed.
        if !matches!(mode & S_IFMT, 0 | S_IFREG) {
            self.count(OverlayRefusal::Kind);
            return Err(Errno::EACCES);
        }
        if self.tmp.holds(parent, name) {
            self.writable_dir(parent)?;
            return Err(Errno::EEXIST);
        }
        if let NewFile::Tmp = self.place_file(parent, name, true)? {
            return self
                .tmp
                .mknod(parent, name, plain_mode(mode), self.inner.new_node_id());
        }
        // A type of 0 is a regular file: said explicitly, so the
        // operations (and the Judge, under the names ordering) see one.
        self.inner.mknod(
            parent,
            name,
            S_IFREG | (plain_mode(mode) & !S_IFMT),
            rdev,
            self.as_owner(actor.caller.pid),
        )
    }

    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, actor: Actor<'_>) -> OpResult<Entry> {
        if self.tmp.holds(parent, name) {
            self.writable_dir(parent)?;
            return Err(Errno::EEXIST);
        }
        self.place_new(parent, name, NewKind::Directory)?;
        self.inner.mkdir(
            parent,
            name,
            plain_mode(mode),
            self.as_owner(actor.caller.pid),
        )
    }

    fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        if self.tmp.holds(parent, name) {
            self.writable_dir(parent)?;
            if let Some(unlinked) = self.tmp.unlink(parent, name) {
                return unlinked;
            }
        }
        self.remove(parent, name)?;
        self.inner
            .unlink(parent, name, self.as_owner(actor.caller.pid))
    }

    fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        let dir = self.remove(parent, name)?;
        // Empty in the tree, but not through the overlay.
        if !self.tmp.entries(dir.id).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        self.inner
            .rmdir(parent, name, self.as_owner(actor.caller.pid))
    }

    /// Never offered: the gate places no symlink ([`NewKind::Symlink`]).
    fn symlink(
        &self,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
        _actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.count(OverlayRefusal::Kind);
        Err(Errno::EACCES)
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
        if flags & RenameFlags::WHITEOUT.bits() != 0 {
            return Err(Errno::EINVAL);
        }
        if self.tmp.holds(parent, name) {
            return self.rename_tmp(parent, name, newparent, newname, flags, actor.caller);
        }
        let from_dir = self.shown(parent)?;
        let to_dir = self.shown(newparent)?;
        let from = self.changed_path(&from_dir, name)?;
        if let Visibility::Hidden(h) = self.gate.may_show(&from) {
            return Err(self.refuse_hidden_change(h));
        }
        let to = self.new_path(&to_dir, newname)?;
        let src = self.peek(parent, name)?;
        self.admit(
            self.gate
                .place_move(Source::Tree(&from, existing_kind(src.attr.kind)), &to),
        )?;
        // The target may be shown now: its name is recordable. Whether
        // what is there is shown for its kind is judged under the names
        // ordering ([`Judge`]): a target hidden for its kind is never
        // replaced.
        let dst = if flags & RENAME_EXCHANGE != 0 {
            let dst = self.peek(newparent, newname)?;
            self.admit(
                self.gate
                    .place_move(Source::Tree(&to, existing_kind(dst.attr.kind)), &from),
            )?;
            Some(dst)
        } else {
            self.existing(newparent, newname)?
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
            self.as_owner(actor.caller.pid),
        )
    }

    /// Never offered: the gate places no hard link, whatever its source
    /// ([`NewKind::Hardlink`]).
    fn link(
        &self,
        _id: u64,
        _newparent: u64,
        _newname: &[u8],
        _actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.count(OverlayRefusal::Kind);
        Err(Errno::EACCES)
    }

    fn open(&self, id: u64, flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
        let want = open_want(flags);
        if let Some(attr) = self.tmp_attr(id) {
            self.require(&attr?, want)?;
            if let Some(opened) = self.tmp.open_node(id, flags, self.inner.new_handle_id()) {
                return opened;
            }
        }
        let node = self.shown(id)?;
        self.require(&node.attr, want)?;
        self.inner.open(id, flags, self.as_owner(actor.caller.pid))
    }

    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        if self.tmp.holds(parent, name) {
            self.writable_dir(parent)?;
            if flags & O_EXCL != 0 {
                return Err(Errno::EEXIST);
            }
            // Opened as `open` opens it, its lookup counted for the reply.
            if let Some(found) = self.tmp.lookup(parent, name) {
                let entry = found?;
                let opened = self
                    .require(&entry.attr, Want::from_open_flags(flags))
                    .and_then(|()| {
                        self.tmp
                            .open_node(entry.id, flags, self.inner.new_handle_id())
                            .unwrap_or(Err(Errno::ENOENT))
                    });
                return match opened {
                    Ok((fh, handle)) => {
                        let attr = self
                            .tmp
                            .getattr(entry.id, Some(fh))
                            .unwrap_or(Ok(entry.attr))?;
                        Ok((Entry { id: entry.id, attr }, fh, handle))
                    }
                    Err(e) => {
                        self.forget(entry.id, 1);
                        Err(e)
                    }
                };
            }
        }
        match self.place_file(parent, name, flags & O_EXCL != 0)? {
            NewFile::Tree => {}
            NewFile::Tmp => {
                return self.tmp.create(
                    parent,
                    name,
                    plain_mode(mode),
                    flags,
                    self.inner.new_node_id(),
                    self.inner.new_handle_id(),
                );
            }
        }
        // Without O_EXCL an existing file is opened: the operations check
        // the owner has the access the flags ask of it (with no
        // supplementary groups), and the Judge that what is there is shown
        // for its kind, under the names ordering.
        self.inner.create(
            parent,
            name,
            plain_mode(mode),
            flags,
            self.as_owner(actor.caller.pid),
        )
    }

    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>> {
        if let Some(read) = self.tmp.read(fh, offset, size) {
            return read;
        }
        self.inner.read(fh, offset, size)
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32> {
        if let Some(written) = self.tmp.write(fh, offset, data) {
            return written;
        }
        self.inner
            .write(fh, offset, data, self.as_owner(actor.caller.pid))
    }

    fn flush(&self, fh: u64, actor: Actor<'_>) -> OpResult<()> {
        if let Some(flushed) = self.tmp.flush(fh) {
            return flushed;
        }
        self.inner.flush(fh, self.as_owner(actor.caller.pid))
    }

    fn release(&self, fh: u64) -> OpResult<()> {
        if let Some(released) = self.tmp.release(fh) {
            return released;
        }
        self.inner.release(fh)
    }

    fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()> {
        if let Some(synced) = self.tmp.fsync(fh, datasync) {
            return synced;
        }
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
        if let Some(done) = self.tmp.fallocate(fh, offset, length, mode) {
            return done;
        }
        self.inner
            .fallocate(fh, offset, length, mode, self.as_owner(actor.caller.pid))
    }

    fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64> {
        if let Some(at) = self.tmp.lseek(fh, offset, whence) {
            return at;
        }
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
        if let Some(copied) = self
            .tmp
            .copy_file_range(fh_in, offset_in, fh_out, offset_out, len)
        {
            return copied;
        }
        self.inner.copy_file_range(
            fh_in,
            offset_in,
            fh_out,
            offset_out,
            len,
            self.as_owner(actor.caller.pid),
        )
    }

    fn mapping_may_write(&self, id: u64) {
        if self.tmp.mapping_may_write(id).is_none() {
            self.inner.mapping_may_write(id);
        }
    }

    fn opendir(&self, id: u64) -> OpResult<u64> {
        let node = self.shown(id)?;
        self.require(&node.attr, READ)?;
        // The tree's snapshot, read whole and released at once.
        let tree_fh = self.inner.opendir(id)?;
        let mut tree = Vec::new();
        let mut offset = 0;
        let read = loop {
            match self.inner.readdir(id, tree_fh, offset) {
                Ok(page) if page.is_empty() => break Ok(()),
                Ok(page) => {
                    offset = page.last().map_or(offset, |(next, _)| *next);
                    tree.extend(page.into_iter().map(|(_, e)| e));
                }
                Err(e) => break Err(e),
            }
        };
        self.inner.releasedir(tree_fh);
        read?;
        let (mut dots, rest): (Vec<DirEntry>, Vec<DirEntry>) = tree
            .into_iter()
            .partition(|e| e.name == b"." || e.name == b"..");
        // `.` first, then `..`.
        dots.sort_by(|a, b| a.name.cmp(&b.name));
        let held = self.tmp.entries(id);
        let shown = |e: &DirEntry| {
            Self::child(&node, &e.name).is_some_and(|path| {
                self.gate.show(&path, existing_kind(e.kind)) == Visibility::Shown
            })
        };
        let listing: Vec<(u64, DirEntry)> = dots
            .into_iter()
            .chain(
                rest.into_iter()
                    .filter(|e| shown(e) && !held.iter().any(|t| t.name == e.name)),
            )
            .chain(held.iter().cloned())
            .enumerate()
            .map(|(i, e)| (i as u64 + 1, e))
            .collect();
        let fh = self.inner.new_handle_id();
        self.dirs().insert(fh, Arc::new(listing));
        Ok(fh)
    }

    /// The overlay's snapshot from `offset` on: each entry's offset is its
    /// 1-based index in the snapshot, so there are no gaps.
    fn readdir(&self, _id: u64, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        let listing = self.dirs().get(&fh).cloned().ok_or(Errno::EBADF)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        Ok(listing.iter().skip(start).cloned().collect())
    }

    fn releasedir(&self, fh: u64) {
        self.dirs().remove(&fh);
    }

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        self.inner.statfs()
    }

    fn setxattr(&self, _id: u64, _name: &[u8], _value: &[u8], _flags: i32) -> OpResult<()> {
        self.count(OverlayRefusal::Xattr);
        Err(Errno::ENOTSUP)
    }

    /// Only `user.*` attributes exist through the overlay, and reading one
    /// needs the owner's read access, as `getxattr(2)` does.
    fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr> {
        if let Some(attr) = self.tmp_attr(id) {
            attr?;
            return Err(Errno::ENODATA);
        }
        let node = self.shown(id)?;
        if !name.starts_with(USER_XATTR) {
            return Err(Errno::ENODATA);
        }
        self.require(&node.attr, READ)?;
        self.inner.getxattr(id, name, size)
    }

    /// The `user.*` names only: no name the daemon's privilege can see
    /// (`trusted.*`, `security.*`) is listed.
    fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr> {
        let names = if let Some(attr) = self.tmp_attr(id) {
            attr?;
            Vec::new()
        } else {
            self.shown(id)?;
            user_xattrs(&self.all_xattrs(id)?)
        };
        let len = u32::try_from(names.len()).map_err(|_| Errno::E2BIG)?;
        if size == 0 {
            Ok(Xattr::Size(len))
        } else if len > size {
            Err(Errno::ERANGE)
        } else {
            Ok(Xattr::Data(names))
        }
    }

    fn removexattr(&self, _id: u64, _name: &[u8]) -> OpResult<()> {
        self.count(OverlayRefusal::Xattr);
        Err(Errno::ENOTSUP)
    }
}

#[cfg(test)]
mod tests;
