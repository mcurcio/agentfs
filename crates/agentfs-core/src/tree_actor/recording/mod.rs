//! Recording: a tree's file calls, each journalled (in the kernel, no fsync) before it returns,
//! and the steps that save them: create, close, rename and unlink, each decided, appended and
//! applied with no git (docs/recording.md, docs/trees.md; target git-model §7). A file's bytes
//! are a [`FileMap`] over journal frames and the checkout as built, never bytes in memory.

pub(super) mod file_map;
pub(super) mod pages;

use std::collections::BTreeSet;
use std::ops::Range;

use agentfs_contracts::pod::WriterAt;

pub(in crate::tree_actor) use super::journal::CallHead as Call;
use super::journal::durable::RecordNo;
use super::journal::segment::FramePos;
use super::journal::{Change, Entry, PagePiece, RunHead, SaveOutcome};
use super::queue::Key;
use super::tree::EventNumber;
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

    /// At most `count` of the paths [`TreeActor::files`] lists, with their
    /// objects, from the `skip`th: one batch of a directory listing, without
    /// copying the rest.
    pub fn files_from(
        &self,
        tree: TreeId,
        skip: usize,
        count: usize,
    ) -> Result<Vec<(String, u64)>> {
        Ok(self
            .tree(tree)?
            .names
            .iter()
            .skip(skip)
            .take(count)
            .map(|(path, object)| (path.clone(), *object))
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
        by: &WriterAt,
        create: bool,
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

    /// Open a private write buffer. Creation is itself a recorded namespace call.
    /// The returned number is persisted before it can be used and never repeats.
    pub fn write_open(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: bool,
    ) -> Result<u64> {
        name(path)?;
        let Some(object) = self.tree(tree)?.names.get(path).copied() else {
            if !create {
                return Err(Error::NotFound);
            }
            return self.create_step(tree, path, by);
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

    /// Create an unnamed open object. Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: TreeId, by: &WriterAt) -> Result<u64> {
        self.tree(tree)?;
        let (handle, _) = self.journal_record(
            Entry::AnonymousOpen {
                tree: tree.to_string(),
                by: by.clone(),
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
        let t = self.tree(tree)?;
        if t.names.contains_key(path) {
            return Err(Error::Exists);
        }
        if !t.anonymous.contains(&object) {
            return Err(Error::NotFound);
        }
        self.run_pending_closes(tree, None)?;
        let change = Change::Link {
            tree: tree.to_string(),
            object,
            path: path.to_owned(),
        };
        self.named_step(tree, by, change)
    }

    /// Whether this object has never had a name in this tree.
    pub fn is_anonymous(&self, tree: TreeId, object: u64) -> Result<bool> {
        Ok(self.tree(tree)?.anonymous.contains(&object))
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

    pub fn rename(&mut self, tree: TreeId, from: &str, to: &str, by: &WriterAt) -> Result<()> {
        name(from)?;
        name(to)?;
        if !self.tree(tree)?.names.contains_key(from) {
            return Err(Error::NotFound);
        }
        if from != to {
            self.run_pending_closes(tree, None)?;
            let change = Change::Rename {
                tree: tree.to_string(),
                from: from.to_owned(),
                to: to.to_owned(),
            };
            self.named_step(tree, by, change)?;
        }
        Ok(())
    }

    pub fn unlink(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()> {
        name(path)?;
        if !self.tree(tree)?.names.contains_key(path) {
            return Err(Error::NotFound);
        }
        self.run_pending_closes(tree, None)?;
        let change = Change::Unlink {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        self.named_step(tree, by, change)
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
            let saved = self.close_step(handle)?;
            if wanted == Some(handle) {
                outcome = saved;
            }
        }
        Ok(outcome)
    }

    /// A create: the step that names a new object and opens it.
    fn create_step(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<u64> {
        self.run_pending_closes(tree, None)?;
        let seq = self.next;
        let change = Change::Create {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        let pointers = self.publish(tree, seq);
        let started = std::time::Instant::now();
        self.record_step(by.clone(), change.clone(), pointers, &[])?;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq);
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

    /// A link, rename or unlink: a step that changes names, checked by
    /// applying it to a copy of the shape first.
    fn named_step(&mut self, tree: TreeId, by: &WriterAt, change: Change) -> Result<()> {
        let seq = self.next;
        let mut shape = self.tree(tree)?.shape.clone();
        let keys = shape.apply(&change, EventNumber::new(seq))?;
        let pointers = self.publish(tree, seq);
        let started = std::time::Instant::now();
        self.record_step(by.clone(), change, pointers, &[])?;
        self.trees.get_mut(&tree).ok_or(Error::NotFound)?.shape = shape;
        self.touch(tree, seq, keys);
        self.count_save(started);
        self.after_step();
        Ok(())
    }

    /// Applies a recorded step's shape transition, which was checked before
    /// its record: a failure now is a bug, and stops the actor.
    fn apply_shape(
        stopped: &mut bool,
        tree: &mut super::tree::SavedTree,
        change: &Change,
        seq: u64,
    ) -> BTreeSet<Key> {
        tree.shape
            .apply(change, EventNumber::new(seq))
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
        self.record_step(by, change, pointers, &[])?;
        self.touch(tree, seq, keys);
        self.pending_closes.retain(|pending| *pending != handle);
        self.handles.remove(&handle);
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        t.shape.save(object, outcome);
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

pub(in crate::tree_actor) fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.contains('/') {
        return Err(Error::Name);
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
