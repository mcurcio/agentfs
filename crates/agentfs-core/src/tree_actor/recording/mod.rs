//! Recording: a tree's file calls, each journalled (in the kernel, no fsync) before it returns,
//! and the steps that save them: create, close, rename and unlink, each decided, appended and
//! applied with no git (docs/recording.md, docs/trees.md; target git-model §7).

pub(super) mod pages;

use std::sync::Arc;

use agentfs_contracts::pod::WriterAt;

use super::journal::durable::RecordNo;
use super::journal::{CallHead, Change, Entry, PagePiece, RunHead, SaveOutcome};
use super::tree::EventNumber;
use super::{Error, Result, Saved, TreeActor, TreeId};
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
    /// The length `apply` leaves on bytes of length `len`, with the errors
    /// `apply` returns: a call's errors depend only on lengths, so a check
    /// costs nothing like the bytes.
    pub(in crate::tree_actor) fn len_after(&self, len: usize) -> Result<usize> {
        match self {
            Self::Write {
                offset,
                bytes: data,
            } => {
                let offset = usize::try_from(*offset).map_err(|_| Error::TooLarge)?;
                let end = offset.checked_add(data.len()).ok_or(Error::TooLarge)?;
                Ok(if data.is_empty() { len } else { len.max(end) })
            }
            Self::Append(data) => len.checked_add(data.len()).ok_or(Error::TooLarge),
            Self::Truncate(size) => usize::try_from(*size).map_err(|_| Error::TooLarge),
        }
    }

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
    pub(in crate::tree_actor) by: WriterAt,
    pub(in crate::tree_actor) object: u64,
    pub(in crate::tree_actor) bytes: Vec<u8>,
    /// This open's calls, each with its journal number.
    pub(in crate::tree_actor) calls: Vec<(u64, Call)>,
    /// The saved length the calls were last checked on, and the length they
    /// leave on it (see `call`).
    pub(in crate::tree_actor) replayed: Option<(usize, usize)>,
}

impl TreeActor {
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
        let handle = self.journal_record(
            Entry::Open {
                tree: tree.to_string(),
                path: path.to_owned(),
                by: by.clone(),
                object,
            },
            &[],
        )?;
        let bytes = self.tree(tree)?.bytes[&object].to_vec();
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
        let handle = self.journal_record(
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
                bytes: Vec::new(),
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

    /// Checks the call on the writer's view and on the saved head with every
    /// earlier call, journals it, then applies it to the view. Both checks are
    /// on lengths, which are all `apply`'s errors depend on, and the head's
    /// replay is redone only when the saved length moved: a call costs its own
    /// bytes, not the file's or its open's.
    pub(in crate::tree_actor) fn call(&mut self, handle: u64, call: Call) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        call.len_after(h.bytes.len())?;
        let saved = self.tree(h.tree)?.bytes[&h.object].len();
        let replayed = match h.replayed {
            Some((base, len)) if base == saved => len,
            _ => h
                .calls
                .iter()
                .try_fold(saved, |len, (_, c)| c.len_after(len))?,
        };
        let replayed = call.len_after(replayed)?;
        let (head, tail) = call.to_frame();
        let seq = self.journal_record(Entry::Call { handle, call: head }, tail)?;
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        call.apply(&mut h.bytes)?;
        h.replayed = Some((saved, replayed));
        let (tree, object) = (h.tree, h.object);
        self.pages_note_call(tree, object, &call);
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.calls.push((seq, call));
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
        self.record_step(by.clone(), change.clone(), pointers)?;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        Self::apply_shape(&mut self.stopped, t, &change, seq);
        t.bytes.insert(seq, Arc::default());
        self.handles.insert(
            seq,
            Handle {
                tree,
                by: by.clone(),
                object: seq,
                bytes: Vec::new(),
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
        shape.apply(&change, EventNumber::new(seq))?;
        let pointers = self.publish(tree, seq);
        let started = std::time::Instant::now();
        self.record_step(by.clone(), change, pointers)?;
        self.trees.get_mut(&tree).ok_or(Error::NotFound)?.shape = shape;
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
    ) {
        if let Err(e) = tree.shape.apply(change, EventNumber::new(seq)) {
            eprintln!("tree SaveFailed: a recorded step does not apply: {e}");
            *stopped = true;
        }
    }

    fn count_save(&mut self, started: std::time::Instant) {
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.saves = self.metrics.saves.saturating_add(1);
        self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
        self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
    }

    /// A close: decides its outcome on the saved bytes, appends the save and
    /// applies it. The builder replays the same calls on the built bytes.
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
            match changed_bytes(saved, &calls)? {
                Some(bytes) => {
                    let outcome = SaveOutcome::Saved {
                        len: bytes.len() as u64,
                        crc32: crc32fast::hash(&bytes),
                    };
                    saved_bytes = Some(bytes);
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
                    len: run.bytes.len() as u64,
                })
                .collect(),
            outcome,
        };
        let started = std::time::Instant::now();
        self.record_step(by, change, pointers)?;
        self.pending_closes.retain(|pending| *pending != handle);
        self.handles.remove(&handle);
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        t.shape.save(object, outcome);
        match (outcome, saved_bytes) {
            (SaveOutcome::AnonymousDropped, _) => t.drop_object(object),
            (_, Some(bytes)) => {
                t.bytes.insert(object, Arc::new(bytes.into()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `call` checks on lengths alone: `len_after` must give `apply`'s length
    /// and fail where it fails.
    #[test]
    fn len_after_is_the_length_apply_leaves() {
        let calls = [
            Call::Write {
                offset: 3,
                bytes: b"abc".to_vec(),
            },
            Call::Write {
                offset: 40,
                bytes: Vec::new(),
            },
            Call::Write {
                offset: 0,
                bytes: b"x".to_vec(),
            },
            Call::Append(b"tail".to_vec()),
            Call::Truncate(2),
            Call::Truncate(9),
        ];
        for len in [0, 1, 5, 12] {
            for call in &calls {
                let mut bytes = vec![b'.'; len];
                call.apply(&mut bytes).unwrap();
                assert_eq!(
                    call.len_after(len).unwrap(),
                    bytes.len(),
                    "{call:?} on {len}"
                );
            }
        }
        let too_far = Call::Write {
            offset: u64::MAX,
            bytes: b"a".to_vec(),
        };
        assert!(too_far.len_after(0).is_err());
        assert!(too_far.apply(&mut Vec::new()).is_err());
    }
}
