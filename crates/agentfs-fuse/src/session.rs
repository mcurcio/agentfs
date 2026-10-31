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

use std::sync::Arc;

use agentfs_core::record::Caller;
use fuser::FileAttr;

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
    fn setattr(&self, id: u64, set: &SetAttr, caller: Caller) -> OpResult<FileAttr>;
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
        caller: Caller,
    ) -> OpResult<Entry>;
    fn mkdir(&self, parent: u64, name: &[u8], mode: u32, caller: Caller) -> OpResult<Entry>;
    fn unlink(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()>;
    fn rmdir(&self, parent: u64, name: &[u8], caller: Caller) -> OpResult<()>;
    fn symlink(&self, parent: u64, name: &[u8], target: &[u8], caller: Caller) -> OpResult<Entry>;
    /// `flags` are `renameat2(2)`'s (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`,
    /// `RENAME_WHITEOUT`).
    fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        caller: Caller,
    ) -> OpResult<()>;
    fn link(&self, id: u64, newparent: u64, newname: &[u8], caller: Caller) -> OpResult<Entry>;

    // ---- open files ----

    /// Opens `id`; the handle id and the handle (the session registers its
    /// descriptor for passthrough).
    fn open(&self, id: u64, flags: i32, caller: Caller) -> OpResult<(u64, Arc<Handle>)>;
    fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        caller: Caller,
    ) -> OpResult<(Entry, u64, Arc<Handle>)>;
    fn read(&self, fh: u64, offset: u64, size: u32) -> OpResult<Vec<u8>>;
    fn write(&self, fh: u64, offset: u64, data: &[u8], caller: Caller) -> OpResult<u32>;
    fn flush(&self, fh: u64, caller: Caller) -> OpResult<()>;
    fn release(&self, fh: u64) -> OpResult<()>;
    fn fsync(&self, fh: u64, datasync: bool) -> OpResult<()>;
    fn fallocate(
        &self,
        fh: u64,
        offset: u64,
        length: u64,
        mode: i32,
        caller: Caller,
    ) -> OpResult<()>;
    fn lseek(&self, fh: u64, offset: i64, whence: i32) -> OpResult<i64>;
    fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        caller: Caller,
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
