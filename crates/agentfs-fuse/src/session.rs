//! What one FUSE session serves (design §2.1, §17.1).
//!
//! [`crate::fuse::FuseFs`] maps each kernel request onto an
//! [`Operations`]. Two implementations exist:
//!
//! - [`crate::ops::Ops`], every operation on the backing tree, recorded:
//!   what the main mount serves;
//! - [`crate::overlay::Overlay`], a decorator over the same `Ops` that
//!   puts the visibility gate ([`agentfs_core::visibility`]) and the view
//!   owner in front of it: what the sandbox overlay serves.
//!
//! One daemon serving both shares one `Ops`: one inode table, one name
//! ordering, one set of open files and one recorder. `Ops` never knows
//! whether a gate stands in front of it.
//!
//! The trait has no default methods, so adding a kernel operation forces
//! every implementation, the overlay included, to decide what it does.
//! Each method has `Ops`'s inherent method of the same name as its
//! reference behaviour: same arguments, same result, same errors.

use std::os::fd::BorrowedFd;
use std::sync::Arc;

use agentfs_core::record::Caller;
use fuser::FileAttr;

use crate::creds::Supplementary;
use crate::inodes::Kind;
use crate::ops::{DirEntry, Entry, Handle, OpResult, SetAttr, Xattr};

/// Every operation a FUSE session hands its filesystem. Node ids are
/// `Ops`'s inode-table ids; handle ids are `Ops`'s handle and directory
/// snapshot ids.
pub trait Operations: Send + Sync + 'static {
    // ---- names and attributes ----

    /// Looks up `name` in directory `parent`, counting one kernel lookup of
    /// the result.
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry>;
    /// The kernel forgot `n` lookups of `id`.
    fn forget(&self, id: u64, n: u64);
    /// Attributes of `id`, through open handle `fh` when given.
    fn getattr(&self, id: u64, fh: Option<u64>) -> OpResult<FileAttr>;
    fn setattr(&self, id: u64, set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr>;
    fn readlink(&self, id: u64) -> OpResult<Vec<u8>>;
    /// `access(2)` of `id` with `mask` (`R_OK`, `W_OK`, `X_OK`, or `F_OK`
    /// as 0). Called by the kernel only on a session without
    /// `default_permissions`.
    fn access(&self, id: u64, mask: i32) -> OpResult<()>;
    /// The node's current paths in the backing tree, the root as the empty
    /// path; none when the inode table no longer knows it.
    fn paths(&self, id: u64) -> Vec<Vec<u8>>;

    // ---- namespace changes ----

    fn mknod(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        actor: Actor<'_>,
    ) -> OpResult<Entry>;
    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, actor: Actor<'_>) -> OpResult<Entry>;
    fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()>;
    fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()>;
    fn symlink(&self, parent: u64, name: &[u8], target: &[u8], actor: Actor<'_>)
    -> OpResult<Entry>;
    /// `flags` are `renameat2(2)`'s (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`,
    /// `RENAME_WHITEOUT`).
    fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        actor: Actor<'_>,
    ) -> OpResult<()>;
    fn link(&self, id: u64, newparent: u64, newname: &[u8], actor: Actor<'_>) -> OpResult<Entry>;

    // ---- open files ----

    /// Opens `id`; the handle id and the handle (the session registers its
    /// descriptor for passthrough).
    fn open(&self, id: u64, flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)>;
    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)>;
    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>>;
    fn write(&self, fh: u64, offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32>;
    fn flush(&self, fh: u64, actor: Actor<'_>) -> OpResult<()>;
    fn release(&self, fh: u64) -> OpResult<()>;
    fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()>;
    fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        actor: Actor<'_>,
    ) -> OpResult<()>;
    fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64>;
    fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        actor: Actor<'_>,
    ) -> OpResult<u32>;
    /// A handle that can write on `id` shares a passthrough backing id
    /// ([`crate::ops::Ops::mapping_may_write`]).
    fn mapping_may_write(&self, id: u64);

    // ---- directories ----

    /// Opens directory `id`: a snapshot of its entries, as a handle id.
    fn opendir(&self, id: u64) -> OpResult<u64>;
    /// Snapshot `fh`'s entries of directory `id` from `offset` on, each
    /// with the offset of the next.
    fn readdir(&self, id: u64, fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>>;
    fn releasedir(&self, fh: u64);

    // ---- the filesystem and extended attributes ----

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs>;
    fn setxattr(&self, id: u64, name: &[u8], value: &[u8], flags: i32) -> OpResult<()>;
    fn getxattr(&self, id: u64, name: &[u8], size: u32) -> OpResult<Xattr>;
    fn listxattr(&self, id: u64, size: u32) -> OpResult<Xattr>;
    fn removexattr(&self, id: u64, name: &[u8]) -> OpResult<()>;
}

/// What a decorator in front of the shared operations needs of them
/// beyond a session's requests (sandbox overlay slice 3): ids from the
/// operations' own sequences, and a move into the tree from outside it.
/// Implemented by [`crate::ops::Ops`]; [`crate::overlay::Overlay`] is
/// generic over it.
pub trait TreeOperations: Operations {
    /// A node id never handed out before, from the same sequence as every
    /// node id of every session. The file it names becomes known to the
    /// operations only through [`TreeOperations::move_in`].
    fn new_node_id(&self) -> u64;

    /// A handle id never handed out before, from the same sequence as
    /// every file and directory handle id of every session.
    fn new_handle_id(&self) -> u64;

    /// Moves the regular file `name` in directory `from` (outside the
    /// backing tree, on its filesystem) to `newname` in tree directory
    /// `newparent`, as one `renameat2(2)` with `flags` (`0` or
    /// `RENAME_NOREPLACE`; anything else is `EINVAL`), and records the new
    /// path's content ([`agentfs_core::record::Trigger::Rename`], as
    /// `caller`) before it returns.
    ///
    /// Under the names ordering, after resolving `newparent`'s path and
    /// statting `newname`, it asks `check` ([`NameCheck::move_in`]); an
    /// error is the answer and nothing has moved. Then the file is moved,
    /// and the operations take it over as node [`Moved::node`] (the kernel
    /// keeps that id: its dentry moved with the rename) with
    /// [`Moved::lookups`] kernel lookups and its open [`Moved::handles`],
    /// under the ids the kernel knows them by, armed so that later writes
    /// through them are recorded at their flush or release as any tree
    /// file's. Returns the node at its new name.
    ///
    /// Errors: `EINVAL` (bad name or flags), `ENOENT` (`newparent` gone),
    /// `EEXIST` (`RENAME_NOREPLACE` and the name is taken), whatever the
    /// check answers, the rename's own error (nothing moved), and `EIO`
    /// when the record fails (the file has moved; its path is queued for
    /// repair, as [`Operations::rename`] does).
    #[allow(clippy::too_many_arguments)]
    fn move_in(
        &self,
        from: BorrowedFd<'_>,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        moved: Moved,
        check: &dyn NameCheck,
        caller: Caller,
    ) -> OpResult<Entry>;
}

/// A file a decorator hands over to the operations with
/// [`TreeOperations::move_in`]: what the kernel knows it by.
#[derive(Debug)]
pub struct Moved {
    /// Its node id ([`TreeOperations::new_node_id`]).
    pub node: u64,
    /// The kernel's lookups of the node not yet forgotten.
    pub lookups: u64,
    /// Its open handles, by the ids the kernel knows them by
    /// ([`TreeOperations::new_handle_id`]).
    pub handles: Vec<(u64, Arc<Handle>)>,
    /// A writer shared a passthrough backing id on it
    /// ([`Operations::mapping_may_write`]).
    pub mapping_may_write: bool,
}

/// What the operations found at one name while they hold the names
/// ordering: its path in the backing tree and the kind of the entry there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found<'a> {
    /// The path from the backing tree's root (the root is the empty path).
    pub path: &'a [u8],
    /// The kind of the entry at it, not following a symlink; `None` when
    /// the name is free.
    pub kind: Option<Kind>,
}

/// A check a request carries into the operations: called while they hold
/// the names ordering, after they have resolved the names an operation
/// changes and statted them, and before they change the backing tree (the
/// pattern of Linux security modules, whose `security_inode_*` hooks the
/// VFS calls under the directory locks). An error is the operation's
/// answer, and nothing has changed. The operations never know what the
/// check is for.
///
/// A check must not block and must not take a lock that is held while the
/// operations are called (it runs under the names ordering).
pub trait NameCheck: Sync {
    /// A new entry of kind `kind` at `at` (`create`, `mknod`, `mkdir`,
    /// `symlink`).
    fn create(&self, at: Found<'_>, kind: Kind) -> OpResult<()>;
    /// A hard link to an existing file at `at`.
    fn link(&self, at: Found<'_>) -> OpResult<()>;
    /// Removing the entry at `at` (`unlink`, `rmdir`).
    fn remove(&self, at: Found<'_>) -> OpResult<()>;
    /// Renaming `from` to `to`; `flags` as `renameat2(2)`'s.
    fn rename(&self, from: Found<'_>, to: Found<'_>, flags: u32) -> OpResult<()>;
    /// Moving a file from outside the tree to `to`
    /// ([`TreeOperations::move_in`]).
    fn move_in(&self, to: Found<'_>) -> OpResult<()>;
}

/// Who a request acts as (sandbox overlay slice 4): the caller its changes
/// are recorded as, the supplementary groups its effects are checked and
/// done with, and the check it carries into the operations, if any.
///
/// The main mount's requests act as the request's own process
/// ([`Actor::of_process`]); the sandbox overlay's as its view owner with
/// no supplementary groups and its name check
/// ([`crate::overlay::ViewOwner::actor`]).
#[derive(Clone, Copy)]
pub struct Actor<'a> {
    pub caller: Caller,
    pub groups: Supplementary,
    pub check: Option<&'a dyn NameCheck>,
}

impl Actor<'_> {
    /// `caller` as the kernel reported it: its process's supplementary
    /// groups, no check.
    pub const fn of_process(caller: Caller) -> Actor<'static> {
        Actor {
            caller,
            groups: Supplementary::OfProcess,
            check: None,
        }
    }
}

impl std::fmt::Debug for Actor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Actor")
            .field("caller", &self.caller)
            .field("groups", &self.groups)
            .field("check", &self.check.is_some())
            .finish()
    }
}
