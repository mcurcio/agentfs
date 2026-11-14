//! The journal's commit point: appending a step's record, and compacting the journal behind it
//! (target git-model §3).

use super::durable::RecordNo;
use super::{CompactError, Entry, OpenRecord, Record};
use crate::tree_actor::recording::pages;
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::{Error, Result, TreeActor, now_ns};

/// When a journal this long after its last compaction compacts again.
/// Constants, not a setting: restart memory is at most this plus live calls.
pub(in crate::tree_actor) fn compact_at(len_after_last: u64) -> u64 {
    (64_u64 << 20).max(len_after_last.saturating_mul(2))
}

impl TreeActor {
    /// Appends one record that moves no pointer, or the step `run_step` built,
    /// with no fsync: the journal's [`Durability`](super::durable::Durability)
    /// makes it durable when a caller asks. Only a file that can no longer be
    /// trusted stops the actor.
    pub(in crate::tree_actor) fn journal_record(
        &mut self,
        entry: Entry,
        tail: &[u8],
    ) -> Result<u64> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        // A compaction due since history trailed runs at the first call after
        // it caught up, before that call's record.
        self.compact_if_due();
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let seq = self.next;
        let record = Record {
            seq: RecordNo::new(seq),
            at_ns: now_ns(),
            entry,
        };
        if let Err(e) = self.journal.append(&record, tail) {
            self.stopped = self.journal.unknown();
            return Err(e);
        }
        self.next += 1;
        Ok(seq)
    }

    /// A fuzzy checkpoint (ARIES): the pointers, the live opens, and unchanged
    /// the live opens' calls and the page writes memory still holds. A file held
    /// open pins only its own calls, never other files' saves, and a closed
    /// generation's page write stays out of the fold (R12). Only with history
    /// built through every record: the checkpoint's pointers are the built
    /// ones, and its redo start is its last record.
    pub(in crate::tree_actor) fn compact(&mut self) -> std::result::Result<(), CompactError> {
        let _timer = Timer::start(Which::Compaction);
        let snapshot = self.history.snapshot();
        let last = RecordNo::new(self.next.saturating_sub(1));
        if snapshot.at() < last {
            return Err(CompactError::Before(std::io::Error::other(
                "history is not built through the journal",
            )));
        }
        let opens = self
            .handles
            .iter()
            .map(|(handle, h)| OpenRecord {
                handle: *handle,
                tree: h.tree.to_string(),
                by: h.by.clone(),
                object: h.object,
                anonymous: self
                    .trees
                    .get(&h.tree)
                    .is_some_and(|t| t.anonymous.contains(&h.object)),
            })
            .collect();
        let checkpoint = Record {
            seq: RecordNo::ZERO,
            at_ns: now_ns(),
            entry: Entry::Checkpoint {
                next: RecordNo::new(self.next),
                built: last,
                pointers: snapshot.pointers().to_hex(),
                upstreams: self.upstreams.to_text(),
                opens,
                page_saved: self.pages_saved_through(),
                objects: snapshot
                    .all_objects()
                    .into_iter()
                    .map(|(tree, objects)| (tree.to_string(), objects))
                    .collect(),
            },
        };
        let (handles, pages) = (&self.handles, &self.pages);
        // The fold parsed every page write's tree; one that names none stops the
        // compaction with the journal untouched, never a page write dropped.
        let unparsed = std::cell::Cell::new(false);
        let retained = self
            .journal
            .retained(|record| match &record.entry {
                Entry::Call { handle, .. } => handles.contains_key(handle),
                Entry::PageWrite { tree, object, .. } => match tree.parse() {
                    Ok(tree) => pages::holds(pages, tree, *object, record.seq.get()),
                    Err(_) => {
                        unparsed.set(true);
                        true
                    }
                },
                _ => false,
            })
            .map_err(CompactError::Before)?;
        if unparsed.get() {
            return Err(CompactError::Before(std::io::Error::other(
                "a journalled page write names no tree",
            )));
        }
        self.journal.compact(&checkpoint, &retained)?;
        self.history.checkpointed(last);
        // The records name built values only: no step past the checkpoint yet.
        self.pointers = snapshot.pointers().to_targets();
        Ok(())
    }

    pub(in crate::tree_actor) fn compact_if_due(&mut self) {
        if self.journal.len() < self.compact_threshold {
            return;
        }
        // A checkpoint's pointers are the built ones: due, it waits for the
        // first call after the builder has caught up. The call path never
        // waits on the builder, so the journal grows while history trails.
        if self.history.built_through() < self.durability().appended() {
            return;
        }
        match self.compact() {
            Ok(()) => self.compact_threshold = compact_at(self.journal.len()),
            // Untouched: the next step retries.
            Err(e @ CompactError::Before(_)) => eprintln!("tree CompactionFailed: {e}"),
            // The open file may not be the named one.
            Err(e @ CompactError::After(_)) => {
                eprintln!("tree CompactionFailed: {e}");
                self.stopped = true;
            }
        }
    }
}
