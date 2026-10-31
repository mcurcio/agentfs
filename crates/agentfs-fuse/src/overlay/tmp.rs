//! The sandbox overlay's tmp layer (design §1.1, §2.4):
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
//!   its node id in decimal.
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

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fuser::{Errno, FileAttr, FileType};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, Timestamps};

use crate::backing::{attr, proc_path};
use crate::creds;
use crate::ops::{DirEntry, Entry, Handle, OpResult, SetAttr, read_at, seek, timespec, write_at};
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
pub struct TmpLayer {
    owner: ViewOwner,
    /// The store directory, and a descriptor of it.
    dir: PathBuf,
    store: OwnedFd,
    /// The daemon is not the owner, so it switches to the owner to create.
    switch: bool,
    tables: Mutex<Tables>,
}

/// The layer's names, nodes and handles.
#[derive(Default)]
struct Tables {
    /// (tree directory node, name) to the node the layer holds there.
    names: HashMap<(u64, Vec<u8>), u64>,
    nodes: HashMap<u64, TmpNode>,
    handles: HashMap<u64, Arc<Handle>>,
}

/// One file of the layer, as the kernel knows it.
struct TmpNode {
    /// Its name, until it is unlinked, replaced or wiped (its store file
    /// is removed then; open handles keep it).
    name: Option<(u64, Vec<u8>)>,
    /// The kernel's lookups not yet forgotten.
    lookups: u64,
    handles: Vec<u64>,
    mapping_may_write: bool,
}

impl Tables {
    /// Forgets node `id` once nothing names, looks up or holds it open.
    fn maybe_drop(&mut self, id: u64) {
        if self
            .nodes
            .get(&id)
            .is_some_and(|n| n.name.is_none() && n.lookups == 0 && n.handles.is_empty())
        {
            self.nodes.remove(&id);
        }
    }

    /// Takes the name `key` from the node holding it, if any; returns that
    /// node. Its store file is the caller's to remove.
    fn unname(&mut self, key: &(u64, Vec<u8>)) -> Option<u64> {
        let id = self.names.remove(key)?;
        if let Some(node) = self.nodes.get_mut(&id) {
            node.name = None;
        }
        Some(id)
    }
}

/// The store file of node `id`.
fn store_name(id: u64) -> String {
    id.to_string()
}

/// `OFlags` for opening a file the way `open(2)` with `flags` asked,
/// never creating, following or leaking it.
fn open_flags(flags: i32) -> OFlags {
    let mut oflags = OFlags::from_bits_retain(flags as u32)
        & !(OFlags::CREATE | OFlags::EXCL | OFlags::NOCTTY | OFlags::NOATIME);
    oflags.remove(OFlags::PATH);
    oflags | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Whether `open(2)` with `flags` can write.
fn writes(flags: i32) -> bool {
    flags & 0o3 != 0 || flags & OFlags::TRUNC.bits() as i32 != 0
}

fn eno(e: rustix::io::Errno) -> Errno {
    Errno::from_i32(e.raw_os_error())
}

/// What a node's attributes are read and changed through.
enum Target {
    /// Its store file, by name.
    Store(u64),
    /// An open handle, for a file with no name left.
    Handle(Arc<Handle>),
}

impl TmpLayer {
    /// Opens the store at `dir`: created (mode 0700) if missing, given to
    /// `owner`, checked to be on the filesystem of the backing tree's root
    /// `tree` ([`TmpLayerError::OtherFilesystem`]), and wiped.
    pub fn open(dir: &Path, tree: BorrowedFd<'_>, owner: ViewOwner) -> Result<Self, TmpLayerError> {
        let io = |source: io::Error| TmpLayerError::Io {
            path: dir.to_path_buf(),
            source,
        };
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(e)),
        }
        let store = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(e.into()))?;
        let st = rustix::fs::fstat(&store).map_err(|e| io(e.into()))?;
        if st.st_uid != owner.uid() || st.st_gid != owner.gid() {
            rustix::fs::fchown(
                &store,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(|e| io(e.into()))?;
        }
        if u32::from(st.st_mode) & 0o7777 != 0o700 {
            rustix::fs::fchmod(&store, Mode::from_raw_mode(0o700)).map_err(|e| io(e.into()))?;
        }
        let tree = rustix::fs::fstat(tree).map_err(|e| io(e.into()))?;
        let (store_dev, tree_dev) = (u64::from(st.st_dev), u64::from(tree.st_dev));
        if store_dev != tree_dev {
            return Err(TmpLayerError::OtherFilesystem {
                path: dir.to_path_buf(),
                store: store_dev,
                tree: tree_dev,
            });
        }
        let layer = Self {
            owner,
            dir: dir.to_path_buf(),
            store,
            switch: rustix::process::geteuid().as_raw() != owner.uid(),
            tables: Mutex::new(Tables::default()),
        };
        layer.wipe().map_err(io)?;
        Ok(layer)
    }

    fn tables(&self) -> MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Removes every file of the store and forgets every name; returns how
    /// many files it removed. Open handles keep working on their (now
    /// unnamed) files.
    pub fn wipe(&self) -> io::Result<usize> {
        let mut tables = self.tables();
        let ids: Vec<u64> = tables.names.values().copied().collect();
        tables.names.clear();
        for id in ids {
            if let Some(node) = tables.nodes.get_mut(&id) {
                node.name = None;
            }
            tables.maybe_drop(id);
        }
        let mut removed = 0;
        let mut first_error = None;
        for entry in std::fs::read_dir(&self.dir)? {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            // Only this layer writes here, but a directory is removed too.
            let result = match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(removed),
        }
    }

    /// Removes node `id`'s store file. A failure leaves it for the wipe
    /// at stop or the next start.
    fn remove_store_file(&self, id: u64) {
        let _ = rustix::fs::unlinkat(&self.store, store_name(id), AtFlags::empty());
    }

    // ---- names ----

    /// The file the layer holds at `name` in tree directory `parent`,
    /// counting one kernel lookup; `None` when it holds none there.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Option<OpResult<Entry>> {
        let mut tables = self.tables();
        let id = *tables.names.get(&(parent, name.to_vec()))?;
        let st = match rustix::fs::statat(&self.store, store_name(id), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(e) => return Some(Err(eno(e))),
        };
        if let Some(node) = tables.nodes.get_mut(&id) {
            node.lookups += 1;
        }
        Some(Ok(Entry {
            id,
            attr: attr(id, &st),
        }))
    }

    /// Whether the layer holds a file at `name` in `parent`.
    pub fn holds(&self, parent: u64, name: &[u8]) -> bool {
        self.tables().names.contains_key(&(parent, name.to_vec()))
    }

    /// The layer's files in tree directory `parent`, for a listing.
    pub fn entries(&self, parent: u64) -> Vec<DirEntry> {
        let mut entries: Vec<DirEntry> = self
            .tables()
            .names
            .iter()
            .filter(|((p, _), _)| *p == parent)
            .map(|((_, name), id)| DirEntry {
                id: *id,
                kind: FileType::RegularFile,
                name: name.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// Creates the store file of node `node` for `name` in `parent`, as
    /// the owner, opened with `oflags`, and names it. Called holding the
    /// tables.
    #[allow(clippy::too_many_arguments)]
    fn make(
        &self,
        tables: &mut Tables,
        parent: u64,
        name: &[u8],
        mode: u32,
        oflags: OFlags,
        node: u64,
        fh: Option<u64>,
    ) -> OpResult<(OwnedFd, Entry)> {
        let key = (parent, name.to_vec());
        if tables.names.contains_key(&key) {
            return Err(Errno::EEXIST);
        }
        let mode = mode & 0o777;
        let file = store_name(node);
        let (made, _) = creds::create_as(
            self.switch,
            self.owner.caller(0),
            creds::Supplementary::None,
            || {
                rustix::fs::openat(
                    &self.store,
                    file.as_str(),
                    oflags | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::from_raw_mode(mode),
                )
            },
        );
        let fd = made.map_err(eno)?;
        let owned = |fd: &OwnedFd| -> OpResult<rustix::fs::Stat> {
            let st = rustix::fs::fstat(fd).map_err(eno)?;
            // Never a file the owner does not own (made as the daemon).
            if st.st_uid != self.owner.uid() {
                return Err(Errno::EACCES);
            }
            if u32::from(st.st_mode) & 0o7777 != mode {
                // The daemon's umask narrowed it.
                rustix::fs::fchmod(fd, Mode::from_raw_mode(mode)).map_err(eno)?;
                return rustix::fs::fstat(fd).map_err(eno);
            }
            Ok(st)
        };
        let st = match owned(&fd) {
            Ok(st) => st,
            Err(e) => {
                self.remove_store_file(node);
                return Err(e);
            }
        };
        tables.names.insert(key.clone(), node);
        tables.nodes.insert(
            node,
            TmpNode {
                name: Some(key),
                lookups: 1,
                handles: fh.into_iter().collect(),
                mapping_may_write: false,
            },
        );
        Ok((
            fd,
            Entry {
                id: node,
                attr: attr(node, &st),
            },
        ))
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
        let mut tables = self.tables();
        let (fd, entry) = self.make(
            &mut tables,
            parent,
            name,
            mode,
            open_flags(flags) & !OFlags::TRUNC,
            node,
            Some(fh),
        )?;
        let handle = Arc::new(Handle::new(node, fd, writes(flags), None, 0));
        tables.handles.insert(fh, handle.clone());
        Ok((entry, fh, handle))
    }

    /// `mknod(2)` of a regular file: [`TmpLayer::create`] without a handle.
    pub fn mknod(&self, parent: u64, name: &[u8], mode: u32, node: u64) -> OpResult<Entry> {
        let mut tables = self.tables();
        let (_, entry) = self.make(&mut tables, parent, name, mode, OFlags::RDONLY, node, None)?;
        Ok(entry)
    }

    /// Removes the layer's file at `name` in `parent`; `None` when it holds
    /// none there.
    pub fn unlink(&self, parent: u64, name: &[u8]) -> Option<OpResult<()>> {
        let mut tables = self.tables();
        let id = tables.unname(&(parent, name.to_vec()))?;
        self.remove_store_file(id);
        tables.maybe_drop(id);
        Some(Ok(()))
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
        let mut tables = self.tables();
        let from = (parent, name.to_vec());
        let id = *tables.names.get(&from)?;
        let noreplace = match RenameFlags::from_bits(flags) {
            Some(f) if f.is_empty() => false,
            Some(f) if f == RenameFlags::NOREPLACE => true,
            _ => return Some(Err(Errno::EINVAL)),
        };
        let to = (newparent, newname.to_vec());
        if from == to {
            return Some(Ok(()));
        }
        if tables.names.contains_key(&to) {
            if noreplace {
                return Some(Err(Errno::EEXIST));
            }
            if let Some(replaced) = tables.unname(&to) {
                self.remove_store_file(replaced);
                tables.maybe_drop(replaced);
            }
        }
        tables.names.remove(&from);
        tables.names.insert(to.clone(), id);
        if let Some(node) = tables.nodes.get_mut(&id) {
            node.name = Some(to);
        }
        Some(Ok(()))
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
        let mut tables = self.tables();
        let key = (parent, name.to_vec());
        let id = *tables.names.get(&key)?;
        let node = tables.nodes.get(&id)?;
        let moved = Moved {
            node: id,
            lookups: node.lookups,
            handles: node
                .handles
                .iter()
                .filter_map(|fh| tables.handles.get(fh).map(|h| (*fh, h.clone())))
                .collect(),
            mapping_may_write: node.mapping_may_write,
        };
        let file = store_name(id);
        let result = into_tree(MoveOut {
            store: self.store.as_fd(),
            name: file.as_bytes(),
            moved,
        });
        if result.is_ok() {
            tables.names.remove(&key);
            if let Some(node) = tables.nodes.remove(&id) {
                for fh in node.handles {
                    tables.handles.remove(&fh);
                }
            }
        }
        Some(result)
    }

    // ---- nodes: `None` when `id` is not the layer's ----

    /// What node `id` (or handle `fh`, when the layer's) is reached
    /// through; `None` when neither is the layer's.
    fn target(&self, id: u64, fh: Option<u64>) -> Option<OpResult<Target>> {
        let tables = self.tables();
        if let Some(h) = fh.and_then(|fh| tables.handles.get(&fh)) {
            return Some(Ok(Target::Handle(h.clone())));
        }
        let node = tables.nodes.get(&id)?;
        if node.name.is_some() {
            return Some(Ok(Target::Store(id)));
        }
        Some(
            node.handles
                .iter()
                .find_map(|fh| tables.handles.get(fh))
                .map(|h| Target::Handle(h.clone()))
                .ok_or(Errno::ENOENT),
        )
    }

    /// A descriptor of `target` (`O_PATH` for a store file).
    fn target_fd(&self, target: &Target) -> OpResult<OwnedFd> {
        match target {
            Target::Store(id) => rustix::fs::openat(
                &self.store,
                store_name(*id),
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(eno),
            Target::Handle(h) => h.fd.try_clone().map_err(|_| Errno::EIO),
        }
    }

    pub fn forget(&self, id: u64, n: u64) -> Option<()> {
        let mut tables = self.tables();
        let node = tables.nodes.get_mut(&id)?;
        node.lookups = node.lookups.saturating_sub(n);
        tables.maybe_drop(id);
        Some(())
    }

    pub fn getattr(&self, id: u64, fh: Option<u64>) -> Option<OpResult<FileAttr>> {
        let target = match self.target(id, fh)? {
            Ok(target) => target,
            Err(e) => return Some(Err(e)),
        };
        let st = match &target {
            Target::Store(id) => {
                rustix::fs::statat(&self.store, store_name(*id), AtFlags::SYMLINK_NOFOLLOW)
            }
            Target::Handle(h) => rustix::fs::fstat(&h.fd),
        };
        Some(st.map(|st| attr(id, &st)).map_err(eno))
    }

    /// Applies `set` (mode, size, times; never ownership, which the
    /// overlay refuses before) to the file.
    pub fn setattr(&self, id: u64, set: &SetAttr) -> Option<OpResult<FileAttr>> {
        let target = match self.target(id, set.fh)? {
            Ok(target) => target,
            Err(e) => return Some(Err(e)),
        };
        Some(self.apply(id, &target, set))
    }

    fn apply(&self, id: u64, target: &Target, set: &SetAttr) -> OpResult<FileAttr> {
        let fd = self.target_fd(target)?;
        // The magic link reaches exactly this inode: never a symlink.
        let at = proc_path(&fd);
        if let Some(mode) = set.mode {
            rustix::fs::chmodat(
                rustix::fs::CWD,
                &at,
                Mode::from_raw_mode(mode & 0o7777),
                AtFlags::empty(),
            )
            .map_err(eno)?;
        }
        if let Some(size) = set.size {
            match target {
                Target::Handle(h) if h.write => rustix::fs::ftruncate(&h.fd, size),
                _ => rustix::fs::open(&at, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
                    .and_then(|w| rustix::fs::ftruncate(&w, size)),
            }
            .map_err(eno)?;
        }
        if set.atime.is_some() || set.mtime.is_some() {
            let times = Timestamps {
                last_access: timespec(set.atime),
                last_modification: timespec(set.mtime),
            };
            rustix::fs::utimensat(rustix::fs::CWD, &at, &times, AtFlags::empty()).map_err(eno)?;
        }
        let st = rustix::fs::fstat(&fd).map_err(eno)?;
        Ok(attr(id, &st))
    }

    /// Opens node `id` with `flags` as handle `fh` (a fresh id).
    pub fn open_node(&self, id: u64, flags: i32, fh: u64) -> Option<OpResult<(u64, Arc<Handle>)>> {
        let mut tables = self.tables();
        let node = tables.nodes.get(&id)?;
        let oflags = open_flags(flags);
        let fd = if node.name.is_some() {
            rustix::fs::openat(&self.store, store_name(id), oflags, Mode::empty()).map_err(eno)
        } else {
            match node.handles.iter().find_map(|fh| tables.handles.get(fh)) {
                Some(h) => {
                    rustix::fs::open(proc_path(&h.fd), oflags & !OFlags::NOFOLLOW, Mode::empty())
                        .map_err(eno)
                }
                None => Err(Errno::ENOENT),
            }
        };
        let fd = match fd {
            Ok(fd) => fd,
            Err(e) => return Some(Err(e)),
        };
        let handle = Arc::new(Handle::new(id, fd, writes(flags), None, 0));
        if let Some(node) = tables.nodes.get_mut(&id) {
            node.handles.push(fh);
        }
        tables.handles.insert(fh, handle.clone());
        Some(Ok((fh, handle)))
    }

    pub fn mapping_may_write(&self, id: u64) -> Option<()> {
        self.tables().nodes.get_mut(&id)?.mapping_may_write = true;
        Some(())
    }

    // ---- handles: `None` when `fh` is not the layer's ----

    fn handle(&self, fh: u64) -> Option<Arc<Handle>> {
        self.tables().handles.get(&fh).cloned()
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Option<OpResult<Vec<u8>>> {
        let h = self.handle(fh)?;
        Some(read_at(&h.fd, offset, size))
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Option<OpResult<u32>> {
        let h = self.handle(fh)?;
        Some(write_at(&h.fd, offset, data))
    }

    /// Nothing to record: `Some(Ok(()))` for the layer's handle.
    pub fn flush(&self, fh: u64) -> Option<OpResult<()>> {
        self.handle(fh).map(|_| Ok(()))
    }

    pub fn release(&self, fh: u64) -> Option<OpResult<()>> {
        let mut tables = self.tables();
        let h = tables.handles.remove(&fh)?;
        if let Some(node) = tables.nodes.get_mut(&h.node) {
            node.handles.retain(|f| *f != fh);
        }
        tables.maybe_drop(h.node);
        Some(Ok(()))
    }

    pub fn fsync(&self, fh: u64, datasync: bool) -> Option<OpResult<()>> {
        let h = self.handle(fh)?;
        let synced = if datasync {
            rustix::fs::fdatasync(&h.fd)
        } else {
            rustix::fs::fsync(&h.fd)
        };
        Some(synced.map_err(eno))
    }

    pub fn fallocate(&self, fh: u64, offset: u64, length: u64, mode: i32) -> Option<OpResult<()>> {
        let h = self.handle(fh)?;
        Some(
            rustix::fs::fallocate(
                &h.fd,
                rustix::fs::FallocateFlags::from_bits_retain(mode as u32),
                offset,
                length,
            )
            .map_err(eno),
        )
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> Option<OpResult<i64>> {
        let h = self.handle(fh)?;
        Some(seek(&h.fd, offset, whence))
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
        let (input, output) = (self.handle(fh_in), self.handle(fh_out));
        let (input, output) = match (input, output) {
            (None, None) => return None,
            (Some(input), Some(output)) => (input, output),
            _ => return Some(Err(Errno::EXDEV)),
        };
        let mut off_in = offset_in;
        let mut off_out = offset_out;
        let len = usize::try_from(len.min(u64::from(u32::MAX))).unwrap_or(usize::MAX);
        Some(
            rustix::fs::copy_file_range(
                &input.fd,
                Some(&mut off_in),
                &output.fd,
                Some(&mut off_out),
                len,
            )
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .map_err(eno),
        )
    }
}

#[cfg(test)]
mod tests;
