//! Recording: a tree's file calls, each journalled (in the kernel, no fsync) before it returns,
//! and the steps that save them: create, close, mkdir, rmdir, symlink, rename, unlink, chmod,
//! chown and times, each decided, appended and applied with no git (docs/recording.md,
//! docs/trees.md; target git-model §7). A file's bytes are a [`FileMap`] over journal frames and
//! the checkout as built, never bytes in memory.

pub(super) mod file_map;
pub(super) mod pages;

use std::collections::BTreeSet;
use std::ops::Range;

use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::name::WriterLabel;
use agentfs_contracts::pod::WriterAt;

pub(in crate::tree_actor) use super::journal::CallHead as Call;
use super::journal::durable::RecordNo;
use super::journal::segment::FramePos;
use super::journal::{
    Change, Entry, PagePiece, RunHead, SaveOutcome, TAKE_CHUNK, Tail, TakeRecord, chunk_map,
};
use super::meta::{Attrs, Kind, Meta, Perm, SetTime, Times};
use super::queue::Key;
use super::tree::{DirState, EventNumber, SavedTree};
use super::{Error, Result, Saved, TreeActor, TreeId};
use crate::paths::RepoPath;
pub use file_map::FileMap;

/// Calls name byte positions, never lines; their bytes are in their frame's
/// tail. Append finds EOF at save time.
impl Call {
    /// The bytes the call carries in its frame.
    pub(in crate::tree_actor) fn carried(&self) -> u64 {
        match self {
            Self::Write { len, .. } | Self::Append { len } => *len,
            Self::Truncate(_) => 0,
        }
    }

    /// The length the call leaves on a file of length `len`, with the errors
    /// [`FileMap::apply`] returns: a call's errors depend only on lengths, so
    /// a check costs nothing like the bytes.
    pub(in crate::tree_actor) fn len_after(&self, len: u64) -> Result<u64> {
        match *self {
            Self::Write { offset, len: n } => {
                let end = offset.checked_add(n).ok_or(Error::TooLarge)?;
                Ok(if n == 0 { len } else { len.max(end) })
            }
            Self::Append { len: n } => len.checked_add(n).ok_or(Error::TooLarge),
            Self::Truncate(size) => Ok(size),
        }
    }

    /// The file range the call changes on a file of length `len`: a write's,
    /// an append's at replay, a truncate's from its size up.
    pub(in crate::tree_actor) fn touched(&self, len: u64) -> Option<Range<u64>> {
        match *self {
            Self::Write { len: 0, .. } | Self::Append { len: 0 } => None,
            Self::Write { offset, len: n } => Some(offset..offset.saturating_add(n)),
            Self::Append { len: n } => Some(len..len.saturating_add(n)),
            Self::Truncate(size) => Some(size..u64::MAX),
        }
    }

    /// Whether a frame's tail of `tail` bytes is this call's.
    pub(in crate::tree_actor) fn check_tail(&self, tail: u64) -> Result<()> {
        if self.carried() == tail {
            Ok(())
        } else {
            Err(Error::Format)
        }
    }
}

/// One entry of a tree as a view presents it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Its node number: a file's or link's object, a directory's number;
    /// `None` for the root directory. A view's inode is the number + 2.
    pub number: Option<u64>,
    pub meta: Meta,
    pub times: Times,
    /// Bytes: a file's length, a link's target's; 0 for a directory.
    pub len: u64,
    /// 1 for a named file or link, 0 for an anonymous one, 2 plus its child
    /// directories for a directory.
    pub links: u32,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub number: u64,
    pub kind: Kind,
}

/// Whether a rename may replace an entry at its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    /// `RENAME_NOREPLACE`: [`Error::Exists`] when the destination exists.
    NoReplace,
}

/// The longest link target, in bytes (Linux's `PATH_MAX` less its NUL).
const MAX_TARGET: usize = 4095;

/// A directory's node.
fn dir_node(path: &str, state: &DirState) -> Node {
    Node {
        number: (!path.is_empty()).then_some(state.number),
        meta: state.meta,
        times: state.times,
        len: 0,
        links: 2 + state.subdirs,
    }
}

pub(in crate::tree_actor) struct Handle {
    pub(in crate::tree_actor) tree: TreeId,
    pub(in crate::tree_actor) by: WriterAt,
    pub(in crate::tree_actor) object: u64,
    /// The open's view: the saved map at open (a clone, never a copy of
    /// bytes), its own calls and the file's page writes.
    pub(in crate::tree_actor) bytes: FileMap,
    /// This open's calls, each with its journal number and where its bytes are.
    pub(in crate::tree_actor) calls: Vec<(u64, Call, FramePos)>,
    /// The saved length the calls were last checked on, and the length they
    /// leave on it (see `call`).
    pub(in crate::tree_actor) replayed: Option<(u64, u64)>,
}

impl TreeActor {
    /// Saved bytes, whole, in memory: the library's read. An independent
    /// reader never sees an unfinished save. A view reads through
    /// [`TreeActor::file_map`] instead.
    pub fn read(&self, tree: TreeId, path: &str) -> Result<Vec<u8>> {
        Ok(self.file_map(tree, path)?.to_vec()?)
    }

    /// The saved file as a map: a clone of the actor's, O(1), which a reader
    /// reads with the actor unlocked and keeps across later saves and builds.
    pub fn file_map(&self, tree: TreeId, path: &str) -> Result<FileMap> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?.clone())
    }

    /// The saved length of `path`.
    pub fn file_len(&self, tree: TreeId, path: &str) -> Result<u64> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?.len())
    }

    /// The entry at `path` (`""` the root directory) as a view presents it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry.
    pub fn node(&self, tree: TreeId, path: &str) -> Result<Node> {
        let t = self.tree(tree)?;
        if let Some(state) = t.dirs.get(path) {
            return Ok(dir_node(path, state));
        }
        let object = *t.names.get(path).ok_or(Error::NotFound)?;
        Self::object_node(t, object)
    }

    /// The named file, link or directory numbered `number`, with its path
    /// (the first in path order, for a file).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for anything else: no such number, an anonymous
    /// or orphaned file ([`TreeActor::open_node`]).
    pub fn node_at(&self, tree: TreeId, number: u64) -> Result<(&str, Node)> {
        let t = self.tree(tree)?;
        if let Some(path) = t.dirs.path_of(number) {
            let state = t.dirs.get(path).ok_or(Error::Format)?;
            return Ok((path, dir_node(path, state)));
        }
        let path = t.names.of(number).next().ok_or(Error::NotFound)?;
        Ok((path, Self::object_node(t, number)?))
    }

    /// The file or link object `object` as a view presents it, named or
    /// not: an anonymous open's file, or one unlinked while open (no link).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an object the tree no longer has.
    pub fn open_node(&self, tree: TreeId, object: u64) -> Result<Node> {
        Self::object_node(self.tree(tree)?, object)
    }

    fn object_node(t: &SavedTree, object: u64) -> Result<Node> {
        let meta = *t.meta.get(&object).ok_or(Error::NotFound)?;
        Ok(Node {
            number: Some(object),
            meta,
            times: t.times.get(&object).copied().unwrap_or_default(),
            len: t.bytes.get(&object).map_or(0, FileMap::len),
            links: u32::try_from(t.names.of(object).count()).unwrap_or(u32::MAX),
        })
    }

    /// At most `count` entries of directory `dir` (`""` the root) in name
    /// order, from the `skip`th: one batch of a directory listing.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry, [`Error::NotADirectory`] for a
    /// file or link.
    pub fn children(
        &self,
        tree: TreeId,
        dir: &str,
        skip: usize,
        count: usize,
    ) -> Result<Vec<DirEntry>> {
        let t = self.tree(tree)?;
        match t.kind_at(dir) {
            Some(Kind::Dir) => {}
            Some(_) => return Err(Error::NotADirectory),
            None => return Err(Error::NotFound),
        }
        Ok(t.children(dir, skip, count)
            .into_iter()
            .map(|(name, number, kind)| DirEntry { name, number, kind })
            .collect())
    }

    /// A directory: a step. Its number is its record's, which it returns.
    /// In a set-group-ID directory it takes the directory's group and keeps
    /// the set-group-ID bit.
    ///
    /// # Errors
    ///
    /// [`Error::Exists`], [`Error::NotFound`] or [`Error::NotADirectory`] as
    /// `mkdir(2)` answers; [`Error::Name`], [`Error::NameTooLong`]; a failed
    /// record.
    pub fn mkdir(&mut self, tree: TreeId, path: &str, attrs: Attrs, by: &WriterAt) -> Result<u64> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::Dir)?;
        }
        let meta = self.tree(tree)?.new_meta(path, Kind::Dir, attrs);
        self.run_pending_closes(tree, None)?;
        let change = Change::Mkdir {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, &[]))
    }

    /// An empty directory removed: a step.
    ///
    /// # Errors
    ///
    /// [`Error::NotEmpty`], [`Error::NotADirectory`] or [`Error::NotFound`]
    /// as `rmdir(2)` answers; a failed record.
    pub fn rmdir(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()> {
        name(path)?;
        self.run_pending_closes(tree, None)?;
        let change = Change::Rmdir {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// A symbolic link to `target`, owned by `owner`, mode `0777`: a step.
    /// Its object is its record's, which it returns; its bytes are the
    /// target, recorded in the step's frame.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an empty target, [`Error::NameTooLong`] for
    /// one over 4095 bytes, [`Error::Invalid`] for one holding NUL; as
    /// [`TreeActor::mkdir`] for the path.
    pub fn symlink(
        &mut self,
        tree: TreeId,
        path: &str,
        target: &str,
        owner: FileOwner,
        by: &WriterAt,
    ) -> Result<u64> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::Symlink)?;
        }
        if target.is_empty() {
            return Err(Error::NotFound);
        }
        if target.len() > MAX_TARGET {
            return Err(Error::NameTooLong);
        }
        if target.contains('\0') {
            return Err(Error::Invalid);
        }
        let attrs = Attrs {
            perm: Perm::from_mode(0o777),
            owner,
        };
        let meta = self.tree(tree)?.new_meta(path, Kind::Symlink, attrs);
        self.run_pending_closes(tree, None)?;
        let change = Change::Symlink {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, target.as_bytes()))
    }

    /// A link's target.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an entry that is no link, [`Error::NotFound`]
    /// for none.
    pub fn readlink(&self, tree: TreeId, path: &str) -> Result<Vec<u8>> {
        if self.tree(tree)?.kind_at(path).ok_or(Error::NotFound)? != Kind::Symlink {
            return Err(Error::Invalid);
        }
        self.read(tree, path)
    }

    /// A chmod or chown of the entry at `path` (`""` the root directory):
    /// its kind kept, its mode and owner `attrs`'. A step, unless they are
    /// its metadata already. A link's mode stays `0777`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry; a failed record.
    pub fn set_meta(
        &mut self,
        tree: TreeId,
        path: &str,
        attrs: Attrs,
        by: &WriterAt,
    ) -> Result<()> {
        if !path.is_empty() {
            name(path)?;
        }
        let kind = self.tree(tree)?.kind_at(path).ok_or(Error::NotFound)?;
        let meta = Meta::new(kind, attrs);
        if self.node(tree, path)?.meta == meta {
            return Ok(());
        }
        self.run_pending_closes(tree, None)?;
        let change = Change::SetMeta {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// An explicit set of the entry's access and modification times
    /// (`utimensat(2)`; `""` the root directory): a step that builds no
    /// commit, since times are each tree's own. Its ctime moves too.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry; a failed record.
    pub fn set_times(
        &mut self,
        tree: TreeId,
        path: &str,
        atime: Option<SetTime>,
        mtime: Option<SetTime>,
        by: &WriterAt,
    ) -> Result<()> {
        if !path.is_empty() {
            name(path)?;
        }
        let change = Change::SetTimes {
            tree: tree.to_string(),
            path: path.to_owned(),
            atime,
            mtime,
        };
        self.named_step(tree, by, change, (false, &[])).map(drop)
    }

    /// Stable object IDs for a view's inode table (not merge identities).
    pub fn files(&self, tree: TreeId) -> Result<Vec<(String, u64, u64)>> {
        let tree = self.tree(tree)?;
        Ok(tree
            .names
            .iter()
            .map(|(p, o)| (p.clone(), *o, tree.bytes.get(o).map_or(0, FileMap::len)))
            .collect())
    }

    /// The object `path` names: one lookup, as a view's LOOKUP needs.
    pub fn object_at(&self, tree: TreeId, path: &str) -> Result<u64> {
        self.tree(tree)?
            .names
            .get(path)
            .copied()
            .ok_or(Error::NotFound)
    }

    /// The first path, in path order, naming `object`: one lookup, as a view
    /// needs for an inode.
    pub fn path_of(&self, tree: TreeId, object: u64) -> Result<&str> {
        self.tree(tree)?
            .names
            .of(object)
            .next()
            .ok_or(Error::NotFound)
    }

    /// `open(O_TRUNC)`: a write-open and its truncate to zero, recorded in that
    /// order. When the truncate fails the kernel gets an error and sends no
    /// RELEASE, so the open closes here, or it would hold its file's takes until a
    /// restart (R9.10). A close that fails too stays pending, as any failed close
    /// does.
    pub fn write_open_truncating(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: Option<Attrs>,
    ) -> Result<u64> {
        let handle = self.write_open(tree, path, by, create)?;
        if let Err(error) = self.truncate(handle, 0) {
            if let Err(close) = self.close(handle) {
                eprintln!("tree close after a failed truncate: {close}");
            }
            return Err(error);
        }
        Ok(handle)
    }

    /// Open a private write buffer. With `create`, a missing file is made
    /// with those attributes (in a set-group-ID directory, its group): a
    /// recorded step. The returned number is persisted before it can be used
    /// and never repeats.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for a directory, [`Error::Invalid`] for a
    /// link, [`Error::NotFound`] for a missing file without `create`, and as
    /// [`TreeActor::mkdir`] for a create.
    pub fn write_open(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: Option<Attrs>,
    ) -> Result<u64> {
        name(path)?;
        if create.is_some() && self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let t = self.tree(tree)?;
        match t.kind_at(path) {
            Some(Kind::Dir) => return Err(Error::IsADirectory),
            Some(Kind::Symlink) => return Err(Error::Invalid),
            Some(Kind::File) | None => {}
        }
        let Some(object) = t.names.get(path).copied() else {
            let Some(attrs) = create else {
                return Err(Error::NotFound);
            };
            return self.create_step(tree, path, by, attrs);
        };
        let (handle, _) = self.journal_record(
            Entry::Open {
                tree: tree.to_string(),
                path: path.to_owned(),
                by: by.clone(),
                object,
            },
            &[],
        )?;
        let bytes = self
            .tree(tree)?
            .bytes
            .get(&object)
            .cloned()
            .unwrap_or_default();
        self.handles.insert(
            handle,
            Handle {
                tree,
                by: by.clone(),
                object,
                bytes,
                calls: Vec::new(),
                replayed: None,
            },
        );
        self.pages_opened(tree, object);
        Ok(handle)
    }

    /// Create an unnamed open object (`O_TMPFILE`) with these attributes.
    /// Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: TreeId, by: &WriterAt, attrs: Attrs) -> Result<u64> {
        self.tree(tree)?;
        let meta = Meta::new(Kind::File, attrs);
        let (handle, _) = self.journal_record(
            Entry::AnonymousOpen {
                tree: tree.to_string(),
                by: by.clone(),
                meta,
            },
            &[],
        )?;
        let at_ns = self.last_at_ns;
        self.trees
            .get_mut(&tree)
            .ok_or(Error::NotFound)?
            .add_anonymous(handle, meta, at_ns);
        self.handles.insert(
            handle,
            Handle {
                tree,
                by: by.clone(),
                object: handle,
                bytes: FileMap::empty(),
                calls: Vec::new(),
                replayed: None,
            },
        );
        self.pages_opened(tree, handle);
        Ok(handle)
    }

    /// Link an anonymous object for the first time, preserving its identity.
    pub fn first_link(
        &mut self,
        tree: TreeId,
        object: u64,
        path: &str,
        by: &WriterAt,
    ) -> Result<()> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let change = Change::Link {
            tree: tree.to_string(),
            object,
            path: path.to_owned(),
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// Whether this object has never had a name in this tree.
    pub fn is_anonymous(&self, tree: TreeId, object: u64) -> Result<bool> {
        Ok(self.tree(tree)?.anonymous.contains(&object))
    }

    /// The tree and writer of an open: what a session checks before it
    /// reaches the handle (`crate::session`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open.
    pub fn open_by(&self, handle: u64) -> Result<(TreeId, &WriterLabel)> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        Ok((h.tree, &h.by.writer))
    }

    /// Every open and pending close of `writer` in `tree` closes as a save,
    /// in handle order, after the tree's closes already pending (git-model
    /// §6): what an ended client's opens become. Answers each of its saves,
    /// in that order. A failed save stops here: it and the rest stay pending,
    /// saved at the tree's next step or at restart. The session's alone
    /// (clippy.toml).
    ///
    /// # Errors
    ///
    /// A failed save, as [`TreeActor::close`].
    pub fn close_writer(&mut self, tree: TreeId, writer: &WriterLabel) -> Result<Vec<Saved>> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let mine: Vec<u64> = self
            .handles
            .iter()
            .filter(|(_, h)| h.tree == tree && h.by.writer == *writer)
            .map(|(handle, _)| *handle)
            .collect();
        for handle in &mine {
            if !self.pending_closes.contains(handle) {
                self.pending_closes.push(*handle);
            }
        }
        let saved = self.save_pending(tree)?;
        Ok(mine
            .iter()
            .filter_map(|handle| {
                saved
                    .iter()
                    .find(|(h, _)| h == handle)
                    .map(|(_, outcome)| *outcome)
            })
            .collect())
    }

    /// An open's stable object number, including before its first link.
    pub fn open_object(&self, handle: u64) -> Result<u64> {
        Ok(self.handles.get(&handle).ok_or(Error::NotFound)?.object)
    }

    /// A write-open's view: its saved map at open, its own calls and the
    /// file's page writes. A view clones it under the lock and reads it after.
    pub fn read_open(&self, handle: u64) -> Result<&FileMap> {
        Ok(&self.handles.get(&handle).ok_or(Error::NotFound)?.bytes)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        let len = bytes.len() as u64;
        let call = if append {
            Call::Append { len }
        } else {
            Call::Write { offset, len }
        };
        self.call(handle, call, bytes)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.call(handle, Call::Truncate(size), &[])
    }

    /// Checks the call on the writer's view and on the saved head with every
    /// earlier call, journals it with its bytes `tail`, then applies it to the
    /// view as one piece of its frame. Both checks are on lengths, which are
    /// all `apply`'s errors depend on, and the head's replay is redone only
    /// when the saved length moved: a call costs its own bytes, not the
    /// file's or its open's.
    pub(in crate::tree_actor) fn call(
        &mut self,
        handle: u64,
        call: Call,
        tail: &[u8],
    ) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        call.len_after(h.bytes.len())?;
        let saved = self
            .tree(h.tree)?
            .bytes
            .get(&h.object)
            .map_or(0, FileMap::len);
        let replayed = match h.replayed {
            Some((base, len)) if base == saved => len,
            _ => h
                .calls
                .iter()
                .try_fold(saved, |len, (_, c, _)| c.len_after(len))?,
        };
        let replayed = call.len_after(replayed)?;
        let (seq, pos) = self.journal_record(
            Entry::Call {
                handle,
                call: call.clone(),
            },
            tail,
        )?;
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.bytes.apply(&call, &pos)?;
        h.replayed = Some((saved, replayed));
        let (tree, object) = (h.tree, h.object);
        self.pages_note_call(tree, object, &call, &pos);
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.calls.push((seq, call, pos));
        Ok(())
    }

    /// The journal call for a page write: only `pages` calls it (ledger R1.4).
    /// Returns its journal number and where its runs' bytes are, in order.
    pub(in crate::tree_actor) fn record_page_write(
        &mut self,
        tree: TreeId,
        object: u64,
        runs: &[pages::Changed],
    ) -> Result<(u64, FramePos)> {
        let heads = runs
            .iter()
            .map(|run| RunHead {
                offset: run.offset,
                len: run.bytes.len() as u64,
            })
            .collect();
        let tail = runs.iter().fold(Vec::new(), |mut tail, run| {
            tail.extend_from_slice(&run.bytes);
            tail
        });
        self.journal_record(
            Entry::PageWrite {
                tree: tree.to_string(),
                object,
                runs: heads,
            },
            &tail,
        )
    }

    /// RELEASE publishes calls against this tree's head, never the stale open
    /// base. A failed save changes nothing and answers its errno: the close
    /// stays pending, its handle still counting as open, and saves first at the
    /// next step in its tree and at restart.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let tree = self.handles.get(&handle).ok_or(Error::NotFound)?.tree;
        if !self.pending_closes.contains(&handle) {
            self.pending_closes.push(handle);
        }
        self.run_pending_closes(tree, Some(handle))
    }

    /// `rename(2)`: a step. A directory moves with every entry under it,
    /// each file keeping its object and identity; an open of one follows
    /// its object. A move onto itself changes nothing.
    ///
    /// # Errors
    ///
    /// As POSIX: [`Error::NotFound`], [`Error::NotADirectory`] (a directory
    /// onto a file), [`Error::IsADirectory`] (a file onto a directory),
    /// [`Error::NotEmpty`] (onto a directory with entries),
    /// [`Error::Invalid`] (a directory into its own subtree), and
    /// [`Error::Exists`] for [`RenameMode::NoReplace`] onto an entry.
    pub fn rename(
        &mut self,
        tree: TreeId,
        from: &str,
        to: &str,
        mode: RenameMode,
        by: &WriterAt,
    ) -> Result<()> {
        name(from)?;
        name(to)?;
        let t = self.tree(tree)?;
        if let (Some(kind), None) = (t.kind_at(from), t.kind_at(to)) {
            self.recorded_name(to, kind)?;
        }
        let change = Change::Rename {
            tree: tree.to_string(),
            from: from.to_owned(),
            to: to.to_owned(),
        };
        let t = self.tree(tree)?;
        t.check(&change)?;
        if mode == RenameMode::NoReplace && t.kind_at(to).is_some() {
            return Err(Error::Exists);
        }
        if from != to {
            self.run_pending_closes(tree, None)?;
            self.named_step(tree, by, change, (true, &[]))?;
        }
        Ok(())
    }

    /// `unlink(2)`: a step.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for a directory, [`Error::NotFound`] for no
    /// such entry; a failed record.
    pub fn unlink(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()> {
        name(path)?;
        let change = Change::Unlink {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// One step: afterwards `path` holds exactly the `len` bytes of `src`,
    /// with `attrs`'s mode and owner: the atomic save a program makes by
    /// renaming a finished file over `path`.
    ///
    /// An existing file keeps its object and identity, and its unchanged
    /// lines keep theirs (aligned against its saved lines as a close's save
    /// is); a missing file is created as a create step would create it.
    ///
    /// The bytes go to the journal in chunk records just before the step; a
    /// crash, or an error from `src`, before the step's record leaves `path`
    /// as it was, and chunk records not followed by their step never reach a
    /// later step.
    ///
    /// Its commit is a direct save's (no `Agentfs-Open` trailer) and is
    /// queued for the upstream as every save is. Opens of the file keep
    /// their own copies and save over it at their close, as with any save.
    ///
    /// Same bytes and same metadata: [`Saved::Unchanged`] and no commit.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] (a directory at `path`), [`Error::Invalid`] (a
    /// link at `path`), [`Error::NotFound`] (no parent directory),
    /// [`Error::Untracked`] (a missing path the rules place in the untracked
    /// area), [`Error::Io`] from `src` (`UnexpectedEof` when it ends before
    /// `len`), and a failed record.
    pub fn replace_file(
        &mut self,
        tree: TreeId,
        path: &str,
        attrs: Attrs,
        src: &mut dyn std::io::Read,
        len: u64,
        by: &WriterAt,
    ) -> Result<Saved> {
        name(path)?;
        let t = self.tree(tree)?;
        let existing = t
            .names
            .get(path)
            .copied()
            .filter(|_| t.kind_at(path) == Some(Kind::File));
        if t.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let t = self.tree(tree)?;
        let meta = match existing {
            Some(_) => Meta::new(Kind::File, attrs),
            None => t.new_meta(path, Kind::File, attrs),
        };
        let change = Change::Replace {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
            len,
            chunks: TakeRecord::chunks_for(len),
        };
        t.check(&change)?;
        self.run_pending_closes(tree, None)?;
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let started = std::time::Instant::now();
        // The chunks, then the step: no compaction comes between.
        self.before_record();
        let mut chunks = Vec::new();
        let mut done = 0;
        while done < len {
            let n = (len - done).min(TAKE_CHUNK);
            let mut buf = vec![0; usize::try_from(n).map_err(|_| Error::TooLarge)?];
            src.read_exact(&mut buf)?;
            let (_, pos) = self.append_record(Entry::Bytes, &buf)?;
            chunks.push(Tail { pos, len: n });
            done += n;
        }
        let seq = self.next;
        let map = chunk_map(&chunks, len, RecordNo::new(seq))?;
        if let Some(object) = existing {
            let t = self.tree(tree)?;
            let saved = t.bytes.get(&object).ok_or(Error::NotFound)?;
            let whole = 0..len;
            if t.meta.get(&object) == Some(&meta)
                && map.same_as(saved, std::slice::from_ref(&whole))?
            {
                return Ok(Saved::Unchanged);
            }
        }
        crate::failpoints::hit("tree.replace_before_step")?;
        let pointers = self.publish(tree, seq);
        let (recorded, _, _) = self.record_with(
            by.clone(),
            change.clone(),
            pointers,
            (&mut std::iter::empty(), &[]),
            Vec::new(),
            false,
        )?;
        if recorded != seq {
            return Err(Error::Format);
        }
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        let object = existing.unwrap_or(seq);
        t.bytes.insert(object, map);
        self.touch(tree, seq, keys);
        if existing.is_some() {
            // The view drops the file's pages, as after a take.
            self.changed.push((tree, object));
        }
        self.pages_saved(tree, object, seq);
        self.count_save(started);
        self.after_step();
        Ok(Saved::Commit(RecordNo::new(seq)))
    }

    /// The guard on every call that makes a name: a name the rules place in
    /// the untracked area never enters a tree ([`Error::Untracked`]), judged
    /// before anything is journaled (docs/trees.md, "Rules and the untracked
    /// area"). Rules decide only for new names: callers ask only when no
    /// entry is at `path`. A name that is not git-safe (a `.git` component
    /// among them) is refused first, as [`Error::Name`].
    fn recorded_name(&self, path: &str, kind: Kind) -> Result<()> {
        let kind = match kind {
            Kind::File => agentfs_rules::NewKind::File,
            Kind::Dir => agentfs_rules::NewKind::Dir,
            Kind::Symlink => agentfs_rules::NewKind::Symlink,
        };
        match self
            .rules
            .place(path, kind, agentfs_rules::PathFacts::default())
        {
            agentfs_rules::Placement::Tracked => Ok(()),
            agentfs_rules::Placement::Untracked(reason) => Err(Error::Untracked {
                path: path.to_owned(),
                reason,
            }),
        }
    }

    /// Saves this tree's pending closes in arrival order, answering for `wanted`.
    pub(in crate::tree_actor) fn run_pending_closes(
        &mut self,
        tree: TreeId,
        wanted: Option<u64>,
    ) -> Result<Saved> {
        Ok(self
            .save_pending(tree)?
            .into_iter()
            .find(|(handle, _)| wanted == Some(*handle))
            .map_or(Saved::Unchanged, |(_, saved)| saved))
    }

    /// Saves this tree's pending closes in arrival order; answers each.
    fn save_pending(&mut self, tree: TreeId) -> Result<Vec<(u64, Saved)>> {
        let due: Vec<u64> = self
            .pending_closes
            .iter()
            .copied()
            .filter(|handle| self.handles.get(handle).is_some_and(|h| h.tree == tree))
            .collect();
        let mut saved = Vec::with_capacity(due.len());
        for handle in due {
            saved.push((handle, self.close_step(handle)?));
        }
        Ok(saved)
    }

    /// A create: the step that names a new object and opens it.
    fn create_step(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        attrs: Attrs,
    ) -> Result<u64> {
        let meta = self.tree(tree)?.new_meta(path, Kind::File, attrs);
        let change = Change::Create {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        let seq = self.next;
        let pointers = self.publish(tree, seq);
        let started = std::time::Instant::now();
        self.record_step(
            by.clone(),
            change.clone(),
            pointers,
            &mut std::iter::empty(),
        )?;
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        let mut empty = FileMap::empty();
        empty.saved_at(RecordNo::new(seq));
        t.bytes.insert(seq, empty);
        self.touch(tree, seq, keys);
        self.handles.insert(
            seq,
            Handle {
                tree,
                by: by.clone(),
                object: seq,
                bytes: FileMap::empty(),
                calls: Vec::new(),
                replayed: None,
            },
        );
        self.pages_opened(tree, seq);
        self.count_save(started);
        self.after_step();
        Ok(seq)
    }

    /// A step that changes names, directories, metadata or times, checked
    /// before its record and applied after it; with `publish`, its commit's
    /// pointer changes (none for times), and `tail` its record's own bytes (a
    /// link's target, whose saved map reads them there). Its number.
    fn named_step(
        &mut self,
        tree: TreeId,
        by: &WriterAt,
        change: Change,
        (publish, tail): (bool, &[u8]),
    ) -> Result<u64> {
        self.tree(tree)?.check(&change)?;
        let seq = self.next;
        let pointers = if publish {
            self.publish(tree, seq)
        } else {
            Vec::new()
        };
        let started = std::time::Instant::now();
        let (recorded, _, pos) = self.record(
            by.clone(),
            change.clone(),
            pointers,
            (&mut std::iter::empty(), tail),
            Vec::new(),
        )?;
        if recorded != seq {
            return Err(Error::Format);
        }
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        if let Change::Symlink { .. } = change {
            let mut map = FileMap::empty();
            map.apply(
                &Call::Write {
                    offset: 0,
                    len: tail.len() as u64,
                },
                &pos,
            )?;
            map.saved_at(RecordNo::new(seq));
            t.bytes.insert(seq, map);
        }
        self.touch(tree, seq, keys);
        if publish {
            self.count_save(started);
        }
        self.after_step();
        Ok(seq)
    }

    /// Applies a recorded step's shape transition, which was checked before
    /// its record: a failure now is a bug, and stops the actor.
    fn apply_shape(
        stopped: &mut bool,
        tree: &mut SavedTree,
        change: &Change,
        seq: u64,
        at_ns: u64,
    ) -> BTreeSet<Key> {
        tree.shape
            .apply(change, EventNumber::new(seq), at_ns)
            .unwrap_or_else(|e| {
                eprintln!("tree SaveFailed: a recorded step does not apply: {e}");
                *stopped = true;
                BTreeSet::new()
            })
    }

    fn count_save(&mut self, started: std::time::Instant) {
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.saves = self.metrics.saves.saturating_add(1);
        self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
        self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
    }

    /// A close: decides its outcome on the saved map, appends the save and
    /// applies it. The builder replays the same calls on the built map.
    fn close_step(&mut self, handle: u64) -> Result<Saved> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let (tree, object, by) = (h.tree, h.object, h.by.clone());
        let t = self.tree(tree)?;
        let mut pieces = Vec::new();
        let mut saved_bytes = None;
        let outcome = if t.anonymous.contains(&object) {
            SaveOutcome::AnonymousDropped
        } else if !t.is_named(object) {
            SaveOutcome::OrphanDropped
        } else {
            pieces = self.pages_close_pieces(h);
            let calls = pages::close_calls(&h.calls, &pieces);
            let saved = t.bytes.get(&object).ok_or(Error::NotFound)?;
            match changed(saved, &calls)? {
                Some(map) => {
                    let outcome = SaveOutcome::Saved {
                        len: map.len(),
                        pieces: map.digest(&own_keys(&calls)),
                    };
                    saved_bytes = Some(map);
                    outcome
                }
                None => SaveOutcome::Unchanged,
            }
        };
        let seq = self.next;
        let pointers = if saved_bytes.is_some() {
            self.publish(tree, seq)
        } else {
            Vec::new()
        };
        let change = Change::Save {
            tree: tree.to_string(),
            handle,
            pages: pieces
                .iter()
                .map(|(record, run)| PagePiece {
                    record: RecordNo::new(*record),
                    offset: run.offset,
                    len: run.len,
                })
                .collect(),
            outcome,
        };
        let started = std::time::Instant::now();
        // Every close lets its open's held keys go, a changed file's or not.
        let keys = self.tree(tree)?.shape.keys(object);
        self.record_step(by, change, pointers, &mut std::iter::empty())?;
        self.touch(tree, seq, keys);
        self.pending_closes.retain(|pending| *pending != handle);
        self.handles.remove(&handle);
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        t.shape.save(object, outcome, at_ns);
        match (outcome, saved_bytes) {
            (SaveOutcome::AnonymousDropped, _) => t.drop_object(object),
            (_, Some(mut map)) => {
                map.saved_at(RecordNo::new(seq));
                t.bytes.insert(object, map);
            }
            _ => {}
        }
        self.pages_saved(tree, object, seq);
        let saved = match outcome {
            SaveOutcome::Saved { .. } => {
                self.count_save(started);
                Saved::Commit(RecordNo::new(seq))
            }
            SaveOutcome::OrphanDropped => Saved::OrphanDropped,
            SaveOutcome::Unchanged | SaveOutcome::AnonymousDropped => Saved::Unchanged,
        };
        self.after_step();
        Ok(saved)
    }
}

/// The map `calls` leave on `saved`, or none if they leave it as it was: a save
/// that changes nothing commits nothing (finding 36; the owner, 2026-11-07, "1. A").
/// "As it was" compares only the bytes the calls touched, never the whole file.
pub(in crate::tree_actor) fn changed(
    saved: &FileMap,
    calls: &[(Call, FramePos)],
) -> Result<Option<FileMap>> {
    let mut map = saved.clone();
    let mut ranges = Vec::new();
    for (call, pos) in calls {
        ranges.extend(call.touched(map.len()));
        map.apply(call, pos)?;
    }
    if calls.is_empty() || (map.len() == saved.len() && map.same_as(saved, &ranges)?) {
        return Ok(None);
    }
    Ok(Some(map))
}

/// The frames a close applied, as [`FileMap::digest`] names them.
pub(in crate::tree_actor) fn own_keys(calls: &[(Call, FramePos)]) -> BTreeSet<(u64, u64)> {
    calls.iter().map(|(_, pos)| pos.key()).collect()
}

/// A path a tree may name: a git-safe relative path ([`RepoPath`]'s rules,
/// `/` between components), each component at most 255 bytes.
pub(in crate::tree_actor) fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.split('/').any(|component| component.len() > 255) {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::journal::segment::Scratch;

    /// `call` checks on lengths alone: `len_after` must give `apply`'s length
    /// and fail where it fails.
    #[test]
    fn len_after_is_the_length_apply_leaves() {
        let calls = [
            Call::Write { offset: 3, len: 3 },
            Call::Write { offset: 40, len: 0 },
            Call::Write { offset: 0, len: 1 },
            Call::Append { len: 4 },
            Call::Truncate(2),
            Call::Truncate(9),
        ];
        let mut frames = Scratch::new();
        for len in [0, 1, 5, 12] {
            for call in &calls {
                let mut map = FileMap::empty();
                map.apply(
                    &Call::Append { len },
                    &frames.add(&vec![b'.'; len as usize]),
                )
                .unwrap();
                let pos = frames.add(&vec![b'x'; call.carried() as usize]);
                map.apply(call, &pos).unwrap();
                assert_eq!(call.len_after(len).unwrap(), map.len(), "{call:?} on {len}");
            }
        }
        let too_far = Call::Write {
            offset: u64::MAX,
            len: 1,
        };
        assert!(too_far.len_after(0).is_err());
        assert!(FileMap::empty().apply(&too_far, &frames.add(b"a")).is_err());
    }

    #[test]
    fn a_close_that_rewrites_the_same_bytes_changes_nothing() {
        let mut frames = Scratch::new();
        let mut saved = FileMap::empty();
        saved
            .apply(&Call::Append { len: 6 }, &frames.add(b"abcdef"))
            .unwrap();
        let same = [
            (Call::Truncate(0), frames.add(b"")),
            (Call::Write { offset: 0, len: 6 }, frames.add(b"abcdef")),
        ];
        assert!(changed(&saved, &same).unwrap().is_none());
        let other = [(Call::Write { offset: 2, len: 1 }, frames.add(b"C"))];
        let map = changed(&saved, &other).unwrap().unwrap();
        assert_eq!(map.to_vec().unwrap(), b"abCdef");
        assert!(changed(&saved, &[]).unwrap().is_none());
    }
}
