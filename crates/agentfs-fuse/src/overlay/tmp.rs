//! The sandbox overlay's tmp layer (design §1.1, sandbox overlay slice 3):
//! the files a sandbox creates through the overlay under names the rules
//! skip ([`agentfs_core::visibility::Placement::TmpLayer`]).
//!
//! The layer is the overlay's upper layer, as an overlayfs upper directory
//! is, for regular files only:
//!
//! - **Private.** Its files live in a store directory of the state area
//!   ([`agentfs_core::volume::StateDir::tmp_layer`]), never in the backing
//!   tree, so the main mount, history and restores never see them. Their
//!   names live only in memory: the store holds one file per name, named by
//!   a counter.
//! - **One filesystem.** The store is on the backing tree's filesystem
//!   ([`TmpLayer::open`] refuses another), so a rename into the tree is one
//!   atomic `renameat2(2)` ([`TmpLayer::move_out`],
//!   [`crate::session::TreeOperations::move_in`]).
//! - **Upper wins.** A name the layer holds shadows the tree's entry of the
//!   same name in the overlay: a lookup asks the layer first. Its names are
//!   names the rules skip for a file, so they never collide with a name the
//!   overlay shows as a file.
//! - **Owned by the view owner** from the instant each file exists.
//! - **Discarded** at start ([`TmpLayer::open`]) and at stop
//!   ([`TmpLayer::wipe`]); a crash leaves the store for the next start to
//!   wipe.
//!
//! Node and handle ids come from the shared operations' sequences
//! ([`crate::session::TreeOperations::new_node_id`],
//! [`crate::session::TreeOperations::new_handle_id`]), handed in by the
//! overlay, so a file moved into the tree keeps the node id and handle ids
//! the kernel knows.
//!
//! Methods that take a node or handle id answer `None` when it is not the
//! layer's (any more): the overlay then asks the tree. One mutex guards the
//! layer's tables; it is never held across a read or write, and never
//! taken while the names ordering is held (it is taken before it, in
//! [`TmpLayer::move_out`]).

use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuser::FileAttr;

use crate::ops::{DirEntry, Entry, Handle, OpResult, SetAttr};
use crate::overlay::ViewOwner;
use crate::session::Moved;

/// Why the tmp layer cannot be opened.
#[derive(Debug, thiserror::Error)]
pub enum TmpLayerError {
    #[error("the sandbox overlay's tmp layer {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    /// The store is not on the backing tree's filesystem (device `store`
    /// against `tree`): a file could not move into the tree atomically.
    #[error(
        "the sandbox overlay's tmp layer {} is on another filesystem than the backing tree \
         (device {store}, the tree's {tree}): a save by temp file and rename could not move \
         the file into the tree atomically; keep the state area and the backing tree on one \
         filesystem",
        path.display()
    )]
    OtherFilesystem {
        path: PathBuf,
        store: u64,
        tree: u64,
    },
}

/// A tmp file handed over to the tree by [`TmpLayer::move_out`]: the store
/// directory, the file's name in it, and what the kernel knows it by.
#[derive(Debug)]
pub struct MoveOut<'a> {
    pub store: BorrowedFd<'a>,
    pub name: &'a [u8],
    pub moved: Moved,
}

/// The sandbox overlay's tmp layer. `Send + Sync`; shared by the overlay and
/// the mount (which wipes it at stop).
#[expect(dead_code, reason = "sandbox overlay slice 3 implements the tmp layer")]
pub struct TmpLayer {
    owner: ViewOwner,
}

impl TmpLayer {
    /// Opens the store at `dir`: created (mode 0700) if missing, given to
    /// `owner`, checked to be on the filesystem of the backing tree's root
    /// `tree` ([`TmpLayerError::OtherFilesystem`]), and wiped.
    pub fn open(dir: &Path, tree: BorrowedFd<'_>, owner: ViewOwner) -> Result<Self, TmpLayerError> {
        let _ = (dir, tree, owner);
        todo!("sandbox overlay slice 3")
    }

    /// Removes every file of the store and forgets every name; returns how
    /// many files it removed. Open handles keep working on their (now
    /// unnamed) files.
    pub fn wipe(&self) -> io::Result<usize> {
        todo!("sandbox overlay slice 3")
    }

    // ---- names ----

    /// The file the layer holds at `name` in tree directory `parent`,
    /// counting one kernel lookup; `None` when it holds none there.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Option<OpResult<Entry>> {
        let _ = (parent, name);
        todo!("sandbox overlay slice 3")
    }

    /// Whether the layer holds a file at `name` in `parent`.
    pub fn holds(&self, parent: u64, name: &[u8]) -> bool {
        let _ = (parent, name);
        todo!("sandbox overlay slice 3")
    }

    /// The layer's files in tree directory `parent`, for a listing.
    pub fn entries(&self, parent: u64) -> Vec<DirEntry> {
        let _ = parent;
        todo!("sandbox overlay slice 3")
    }

    /// Creates and opens a file at the free name `name` in `parent`, mode
    /// `mode` (without set-id and sticky bits), as node `node` with handle
    /// `fh` (fresh ids from the shared sequences), opened with `flags`.
    /// `EEXIST` if the layer already holds the name.
    pub fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: i32,
        node: u64,
        fh: u64,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        let _ = (parent, name, mode, flags, node, fh);
        todo!("sandbox overlay slice 3")
    }

    /// `mknod(2)` of a regular file: [`TmpLayer::create`] without a handle.
    pub fn mknod(&self, parent: u64, name: &[u8], mode: u32, node: u64) -> OpResult<Entry> {
        let _ = (parent, name, mode, node);
        todo!("sandbox overlay slice 3")
    }

    /// Removes the layer's file at `name` in `parent`; `None` when it holds
    /// none there.
    pub fn unlink(&self, parent: u64, name: &[u8]) -> Option<OpResult<()>> {
        let _ = (parent, name);
        todo!("sandbox overlay slice 3")
    }

    /// Renames the layer's file at `name` in `parent` to `newname` in
    /// `newparent`, inside the layer (both names skipped by the rules),
    /// replacing the layer's file there unless `flags` has
    /// `RENAME_NOREPLACE` (`EEXIST`); any other flag is `EINVAL`. `None`
    /// when the layer holds no file at the source.
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Option<OpResult<()>> {
        let _ = (parent, name, newparent, newname, flags);
        todo!("sandbox overlay slice 3")
    }

    /// Hands the layer's file at `name` in `parent` to `into_tree`, holding
    /// the layer's lock: if `into_tree` succeeds, the name, the node and its
    /// handles leave the layer (the tree has them now); if it fails, nothing
    /// changes. `None` when the layer holds no file there.
    pub fn move_out(
        &self,
        parent: u64,
        name: &[u8],
        into_tree: impl FnOnce(MoveOut<'_>) -> OpResult<Entry>,
    ) -> Option<OpResult<Entry>> {
        let _ = (parent, name, into_tree);
        todo!("sandbox overlay slice 3")
    }

    // ---- nodes: `None` when `id` is not the layer's ----

    pub fn forget(&self, id: u64, n: u64) -> Option<()> {
        let _ = (id, n);
        todo!("sandbox overlay slice 3")
    }

    pub fn getattr(&self, id: u64, fh: Option<u64>) -> Option<OpResult<FileAttr>> {
        let _ = (id, fh);
        todo!("sandbox overlay slice 3")
    }

    /// Applies `set` (mode, size, times; never ownership, which the
    /// overlay refuses before) to the file.
    pub fn setattr(&self, id: u64, set: &SetAttr) -> Option<OpResult<FileAttr>> {
        let _ = (id, set);
        todo!("sandbox overlay slice 3")
    }

    /// Opens node `id` with `flags` as handle `fh` (a fresh id).
    pub fn open_node(&self, id: u64, flags: i32, fh: u64) -> Option<OpResult<(u64, Arc<Handle>)>> {
        let _ = (id, flags, fh);
        todo!("sandbox overlay slice 3")
    }

    pub fn mapping_may_write(&self, id: u64) -> Option<()> {
        let _ = id;
        todo!("sandbox overlay slice 3")
    }

    // ---- handles: `None` when `fh` is not the layer's ----

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Option<OpResult<Vec<u8>>> {
        let _ = (fh, offset, size);
        todo!("sandbox overlay slice 3")
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Option<OpResult<u32>> {
        let _ = (fh, offset, data);
        todo!("sandbox overlay slice 3")
    }

    /// Nothing to record: `Some(Ok(()))` for the layer's handle.
    pub fn flush(&self, fh: u64) -> Option<OpResult<()>> {
        let _ = fh;
        todo!("sandbox overlay slice 3")
    }

    pub fn release(&self, fh: u64) -> Option<OpResult<()>> {
        let _ = fh;
        todo!("sandbox overlay slice 3")
    }

    pub fn fsync(&self, fh: u64, datasync: bool) -> Option<OpResult<()>> {
        let _ = (fh, datasync);
        todo!("sandbox overlay slice 3")
    }

    pub fn fallocate(&self, fh: u64, offset: u64, length: u64, mode: i32) -> Option<OpResult<()>> {
        let _ = (fh, offset, length, mode);
        todo!("sandbox overlay slice 3")
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> Option<OpResult<i64>> {
        let _ = (fh, offset, whence);
        todo!("sandbox overlay slice 3")
    }

    /// Copies between two of the layer's handles; `Some(Err(EXDEV))` when
    /// only one is the layer's (programs fall back to reading and
    /// writing); `None` when neither is.
    pub fn copy_file_range(
        &self,
        fh_in: u64,
        offset_in: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
    ) -> Option<OpResult<u32>> {
        let _ = (fh_in, offset_in, fh_out, offset_out, len);
        todo!("sandbox overlay slice 3")
    }
}
