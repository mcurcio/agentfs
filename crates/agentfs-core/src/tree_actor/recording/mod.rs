//! Recording: a tree's file calls, each journalled before it returns, and the steps that save
//! them: create, close, rename and unlink (docs/recording.md; target git-model §7).

pub(super) mod pages;

use std::collections::BTreeSet;
use std::sync::Arc;

use super::journal::{CallHead, Entry, LiveOpen, RunHead};
use super::stages::{Timer, Which};
use super::tree::{EventNumber, MergeFileIdentity, NameHistory, Versions};
use super::{Draft, Error, Result, Saved, TreeId, Volume, queue};
use crate::paths::RepoPath;

/// Calls name byte positions, never lines. Append finds EOF at save time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Call {
    Write { offset: u64, bytes: Vec<u8> },
    Append(Vec<u8>),
    Truncate(u64),
}

impl Call {
    pub(in crate::tree_actor) fn apply(&self, bytes: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Write {
                offset,
                bytes: data,
            } => {
                let offset = usize::try_from(*offset).map_err(|_| Error::TooLarge)?;
                let end = offset.checked_add(data.len()).ok_or(Error::TooLarge)?;
                // A zero-length pwrite past EOF does not extend a file.
                if !data.is_empty() {
                    bytes.resize(bytes.len().max(end), 0);
                    bytes[offset..end].copy_from_slice(data);
                }
            }
            Self::Append(data) => {
                bytes.len().checked_add(data.len()).ok_or(Error::TooLarge)?;
                bytes.extend_from_slice(data);
            }
            Self::Truncate(size) => {
                let size = usize::try_from(*size).map_err(|_| Error::TooLarge)?;
                bytes.resize(size, 0);
            }
        }
        Ok(())
    }

    /// The call as a journal frame stores it: a JSON head and raw tail bytes.
    pub(in crate::tree_actor) fn to_frame(&self) -> (CallHead, &[u8]) {
        match self {
            Self::Write { offset, bytes } => (
                CallHead::Write {
                    offset: *offset,
                    len: bytes.len() as u64,
                },
                bytes,
            ),
            Self::Append(bytes) => (
                CallHead::Append {
                    len: bytes.len() as u64,
                },
                bytes,
            ),
            Self::Truncate(size) => (CallHead::Truncate(*size), &[]),
        }
    }

    pub(in crate::tree_actor) fn from_frame(head: &CallHead, tail: &[u8]) -> Result<Self> {
        let whole = |len: u64| {
            (len == tail.len() as u64)
                .then(|| tail.to_vec())
                .ok_or(Error::Format)
        };
        match head {
            CallHead::Write { offset, len } => Ok(Self::Write {
                offset: *offset,
                bytes: whole(*len)?,
            }),
            CallHead::Append { len } => Ok(Self::Append(whole(*len)?)),
            CallHead::Truncate(size) => whole(0).map(|_| Self::Truncate(*size)),
        }
    }
}

pub(in crate::tree_actor) struct Handle {
    pub(in crate::tree_actor) tree: TreeId,
    pub(in crate::tree_actor) writer: String,
    pub(in crate::tree_actor) object: u64,
    pub(in crate::tree_actor) bytes: Vec<u8>,
    /// This open's calls, each with its journal number.
    pub(in crate::tree_actor) calls: Vec<(u64, Call)>,
}

impl Volume {
    /// Saved bytes; an independent reader never sees an unfinished save.
    pub fn read(&self, tree: TreeId, path: &str) -> Result<&[u8]> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?)
    }

    /// Stable object IDs for a view's inode table (not merge identities).
    pub fn files(&self, tree: TreeId) -> Result<Vec<(String, u64, usize)>> {
        let tree = self.tree(tree)?;
        Ok(tree
            .names
            .iter()
            .map(|(p, o)| (p.clone(), *o, tree.bytes[o].len()))
            .collect())
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
        writer: &str,
        create: bool,
    ) -> Result<u64> {
        let handle = self.write_open(tree, path, writer, create)?;
        if let Err(error) = self.truncate(handle, 0) {
            if let Err(close) = self.close(handle) {
                eprintln!("tree close after a failed truncate: {close}");
            }
            return Err(error);
        }
        Ok(handle)
    }

    /// Open a private write buffer. Creation is itself a recorded namespace call.
    /// The returned number is persisted before it can be used and never repeats.
    pub fn write_open(
        &mut self,
        tree: TreeId,
        path: &str,
        writer: &str,
        create: bool,
    ) -> Result<u64> {
        name(path)?;
        writer_label(writer)?;
        let Some(object) = self.tree(tree)?.names.get(path).copied() else {
            if !create {
                return Err(Error::NotFound);
            }
            return self
                .step(tree, |v, event| v.build_create(tree, path, writer, event))
                .map(|(event, _)| event.get());
        };
        let handle = self.journal_record(
            Entry::Open {
                tree: tree.to_string(),
                path: path.to_owned(),
                writer: writer.to_owned(),
                object,
            },
            &[],
        )?;
        let bytes = self.tree(tree)?.bytes[&object].to_vec();
        self.handles.insert(
            handle,
            Handle {
                tree,
                writer: writer.to_owned(),
                object,
                bytes,
                calls: Vec::new(),
            },
        );
        self.pages_opened(tree, object);
        Ok(handle)
    }

    /// Create an unnamed open object. Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: TreeId, writer: &str) -> Result<u64> {
        writer_label(writer)?;
        self.tree(tree)?;
        let handle = self.journal_record(
            Entry::AnonymousOpen {
                tree: tree.to_string(),
                writer: writer.to_owned(),
            },
            &[],
        )?;
        self.trees
            .get_mut(&tree)
            .ok_or(Error::NotFound)?
            .add_anonymous(handle);
        self.handles.insert(
            handle,
            Handle {
                tree,
                writer: writer.to_owned(),
                object: handle,
                bytes: Vec::new(),
                calls: Vec::new(),
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
        writer: &str,
    ) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        let t = self.tree(tree)?;
        if t.names.contains_key(path) {
            return Err(Error::Exists);
        }
        if !t.anonymous.contains(&object) {
            return Err(Error::NotFound);
        }
        self.step(tree, |v, event| {
            let mut t = v.tree(tree)?.clone();
            t.anonymous.remove(&object);
            t.namespace.insert(
                t.identities[&object],
                NameHistory::created(event, path.to_owned()),
            );
            t.names.insert(path.to_owned(), object);
            let mut draft = v.build_save(tree, t, writer, "Create", event, None)?;
            draft.linked = Some(object);
            Ok(draft)
        })?;
        Ok(())
    }

    /// Whether this object has never had a name in this tree.
    pub fn is_anonymous(&self, tree: TreeId, object: u64) -> Result<bool> {
        Ok(self.tree(tree)?.anonymous.contains(&object))
    }

    /// An open's stable object number, including before its first link.
    pub fn open_object(&self, handle: u64) -> Result<u64> {
        Ok(self.handles.get(&handle).ok_or(Error::NotFound)?.object)
    }

    pub fn read_open(&self, handle: u64) -> Result<&[u8]> {
        Ok(&self.handles.get(&handle).ok_or(Error::NotFound)?.bytes)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        let call = if append {
            Call::Append(bytes.to_vec())
        } else {
            Call::Write {
                offset,
                bytes: bytes.to_vec(),
            }
        };
        self.call(handle, call)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.call(handle, Call::Truncate(size))
    }

    pub(in crate::tree_actor) fn call(&mut self, handle: u64, call: Call) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        // Check both the writer's view and replay on the head before journaling.
        let mut bytes = h.bytes.clone();
        call.apply(&mut bytes)?;
        let mut replay = self.tree(h.tree)?.bytes[&h.object].to_vec();
        for c in h.calls.iter().map(|(_, c)| c).chain(std::iter::once(&call)) {
            c.apply(&mut replay)?;
        }
        let (head, tail) = call.to_frame();
        let seq = self.journal_record(Entry::Call { handle, call: head }, tail)?;
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.bytes = bytes;
        h.calls.push((seq, call.clone()));
        let (tree, object) = (h.tree, h.object);
        self.pages_note_call(tree, object, &call);
        Ok(())
    }

    /// The journal call for a page write: only `pages` calls it (ledger R1.4).
    /// Returns its journal number.
    pub(in crate::tree_actor) fn record_page_write(
        &mut self,
        tree: TreeId,
        object: u64,
        runs: &[pages::Run],
    ) -> Result<u64> {
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

    /// fsync/FLUSH records durability, not publication. Every call is already durable.
    pub fn sync(&self) -> Result<()> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.journal.sync()
    }

    /// RELEASE publishes calls against this tree's head, never the stale open
    /// base. A failed save changes nothing and answers its errno: the close
    /// stays pending, its handle still counting as open, and saves first at the
    /// next step in its tree and at restart.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let tree = self.handles.get(&handle).ok_or(Error::NotFound)?.tree;
        if !self.pending_closes.contains(&handle) {
            self.pending_closes.push(handle);
        }
        self.run_pending_closes(tree, Some(handle))
    }

    pub fn rename(&mut self, tree: TreeId, from: &str, to: &str, writer: &str) -> Result<()> {
        name(from)?;
        name(to)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(from) {
            return Err(Error::NotFound);
        }
        if from != to {
            self.step(tree, |v, event| {
                v.build_rename(tree, from, to, writer, event)
            })?;
        }
        Ok(())
    }

    pub fn unlink(&mut self, tree: TreeId, path: &str, writer: &str) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(path) {
            return Err(Error::NotFound);
        }
        self.step(tree, |v, event| {
            let mut t = v.tree(tree)?.clone();
            let object = t.names.remove(path).ok_or(Error::NotFound)?;
            t.namespace
                .get_mut(&t.identities[&object])
                .ok_or(Error::Format)?
                .advance(event, None, None);
            v.build_save(tree, t, writer, "Unlink", event, None)
        })?;
        Ok(())
    }

    pub(in crate::tree_actor) fn open_keys(&self, tree: TreeId) -> BTreeSet<queue::Key> {
        let mut keys = BTreeSet::new();
        if let Some(t) = self.trees.get(&tree) {
            for handle in self.handles.values().filter(|h| h.tree == tree) {
                if !t.names.values().any(|object| *object == handle.object) {
                    continue;
                }
                if let Some(id) = t.identities.get(&handle.object) {
                    keys.insert(queue::Key::File(id.0));
                }
                for (path, object) in &t.names {
                    if *object == handle.object {
                        keys.insert(queue::Key::Name(path.clone()));
                    }
                }
            }
        }
        keys
    }

    /// Saves this tree's pending closes in arrival order, answering for `wanted`.
    pub(in crate::tree_actor) fn run_pending_closes(
        &mut self,
        tree: TreeId,
        wanted: Option<u64>,
    ) -> Result<Saved> {
        let due: Vec<u64> = self
            .pending_closes
            .iter()
            .copied()
            .filter(|handle| self.handles.get(handle).is_some_and(|h| h.tree == tree))
            .collect();
        let mut outcome = Saved::Unchanged;
        for handle in due {
            let (_, saved) = self.run_step(|v, event| v.build_close(handle, event))?;
            if wanted == Some(handle) {
                outcome = saved;
            }
        }
        Ok(outcome)
    }

    pub(in crate::tree_actor) fn build_create(
        &self,
        tree: TreeId,
        path: &str,
        writer: &str,
        event: EventNumber,
    ) -> Result<Draft> {
        let object = event.object();
        let mut t = self.tree(tree)?.clone();
        t.names.insert(path.to_owned(), object);
        t.bytes.insert(object, Arc::default());
        t.lines.insert(object, Arc::default());
        t.identities.insert(object, MergeFileIdentity(object));
        t.namespace.insert(
            MergeFileIdentity(object),
            NameHistory::created(event, path.to_owned()),
        );
        t.fresh_names.insert(object);
        let mut draft = self.build_save(tree, t, writer, "Create", event, None)?;
        draft.opened = Some(LiveOpen {
            handle: event.get(),
            tree,
            writer: writer.to_owned(),
            object,
            anonymous: false,
        });
        Ok(draft)
    }

    pub(in crate::tree_actor) fn build_close(
        &self,
        handle: u64,
        event: EventNumber,
    ) -> Result<Draft> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let calls = self.pages_calls_for_close(h);
        let mut t = self.tree(h.tree)?.clone();
        let mut draft = if t.anonymous.remove(&h.object) {
            t.drop_object(h.object);
            Draft::new(&h.writer, h.tree, t)
        } else if !t.names.values().any(|o| *o == h.object) {
            let mut draft = Draft::new(&h.writer, h.tree, t);
            draft.saved = Saved::OrphanDropped;
            draft
        } else if let Some(bytes) = changed_bytes(&t.bytes[&h.object], &calls)? {
            let mut versions = Versions::of(event);
            let mut next_id = || versions.next();
            let lines = if calls.iter().all(|call| matches!(call, Call::Append(_))) {
                let suffix: Vec<u8> = calls
                    .iter()
                    .flat_map(|call| match call {
                        Call::Append(bytes) => bytes.as_slice(),
                        _ => unreachable!(),
                    })
                    .copied()
                    .collect();
                t.lines[&h.object].append(&suffix, &mut next_id)?
            } else {
                let _timer = Timer::start(Which::Align);
                t.lines[&h.object].align(&bytes, &mut next_id)?
            };
            t.lines.insert(h.object, Arc::new(lines));
            t.bytes.insert(h.object, Arc::new(bytes));
            self.build_save(h.tree, t, &h.writer, "Direct", event, Some(handle))?
        } else {
            Draft::new(&h.writer, h.tree, t)
        };
        draft.closes = Some(handle);
        Ok(draft)
    }

    pub(in crate::tree_actor) fn build_rename(
        &self,
        tree: TreeId,
        from: &str,
        to: &str,
        writer: &str,
        event: EventNumber,
    ) -> Result<Draft> {
        let mut t = self.tree(tree)?.clone();
        let object = t.names.remove(from).ok_or(Error::NotFound)?;
        if let Some(replaced) = t.names.get(to).copied()
            && t.fresh_names.contains(&object)
        {
            let source_id = t.identities[&object];
            t.namespace
                .get_mut(&source_id)
                .ok_or(Error::Format)?
                .advance(event, None, None);
            let mut versions = Versions::of(event);
            let lines = t.lines[&replaced].align(&t.bytes[&object], || versions.next())?;
            t.lines.insert(object, Arc::new(lines));
            t.identities.insert(object, t.identities[&replaced]);
            if !t.fresh_names.contains(&replaced) {
                t.fresh_names.remove(&object);
            }
        } else {
            let source_id = t.identities[&object];
            if let Some(replaced) = t.names.get(to).copied() {
                let replaced_id = t.identities[&replaced];
                t.namespace
                    .get_mut(&replaced_id)
                    .ok_or(Error::Format)?
                    .advance(event, None, Some(source_id));
            }
            t.namespace
                .get_mut(&source_id)
                .ok_or(Error::Format)?
                .advance(event, Some(to.to_owned()), None);
            t.fresh_names.remove(&object);
        }
        t.names.insert(to.to_owned(), object);
        self.build_save(tree, t, writer, "Rename", event, None)
    }
}

/// The bytes `calls` leave on `saved`, or none if they leave it as it was: a save
/// that changes nothing commits nothing (finding 36; the owner, 2026-11-07, "1. A").
pub(in crate::tree_actor) fn changed_bytes(
    saved: &[u8],
    calls: &[Call],
) -> Result<Option<Vec<u8>>> {
    let mut bytes = saved.to_vec();
    for call in calls {
        call.apply(&mut bytes)?;
    }
    Ok((bytes != saved).then_some(bytes))
}

pub(in crate::tree_actor) fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.contains('/') {
        return Err(Error::Name);
    }
    Ok(())
}

pub(in crate::tree_actor) fn writer_label(writer: &str) -> Result<()> {
    if writer.is_empty() || writer.len() > 256 || writer.chars().any(char::is_control) {
        return Err(Error::Writer);
    }
    Ok(())
}
