//! The journal's commit point: appending a step's record, and compacting the journal behind it
//! (target git-model §3).

use super::durable::RecordNo;
use super::segment::FramePos;
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
    /// Appends one record, with no fsync: the journal's
    /// [`Durability`](super::durable::Durability) makes it durable when a
    /// caller asks. Returns its number and where its tail is. Only a file
    /// that can no longer be trusted stops the actor.
    pub(in crate::tree_actor) fn journal_record(
        &mut self,
        entry: Entry,
        tail: &[u8],
    ) -> Result<(u64, FramePos)> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        // Saved maps the builder has checked out let go of their frames first.
        self.rebase_built();
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
        let pos = match self.journal.append(&record, tail) {
            Ok(pos) => pos,
            Err(e) => {
                self.stopped = self.journal.unknown();
                return Err(e);
            }
        };
        self.next += 1;
        Ok((seq, pos))
    }

    /// A fuzzy checkpoint (ARIES): the pointers and the live opens, written
    /// as the first frame of a new segment, then every older segment dropped
    /// whose records are built and which holds no live open's call or held
    /// page write. A file held open pins only its own frames, never other
    /// files' saves: they are copied forward once the dead bytes they pin
    /// exceed their own. A closed generation's page write stays out of the
    /// fold (R12). Only with history built through every record: the
    /// checkpoint's pointers are the built ones, and its redo start is its
    /// last record.
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
                page_writes: self.pages_held(),
                objects: snapshot
                    .all_objects()
                    .into_iter()
                    .map(|(tree, objects)| (tree.to_string(), objects))
                    .collect(),
            },
        };
        self.journal.checkpoint(&checkpoint)?;
        self.history.checkpointed(last);
        // The records name built values only: no step past the checkpoint yet.
        self.pointers = snapshot.pointers().to_targets();
        let (handles, pages) = (&self.handles, &self.pages);
        let live = |record: &Record| match &record.entry {
            Entry::Call { handle, .. } => handles.contains_key(handle),
            // A page write that names no tree is kept, never dropped.
            Entry::PageWrite { tree, object, .. } => tree
                .parse()
                .map_or(true, |tree| pages::holds(pages, tree, *object, record.seq.get())),
            _ => false,
        };
        let stats = self
            .journal
            .drop_behind(last, &live)
            .map_err(|e| CompactError::After(std::io::Error::other(e.to_string())))?;
        self.metrics.journal_dropped = self.metrics.journal_dropped.saturating_add(stats.segments);
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
            // The checkpoint stands; the segments not dropped wait for the next.
            Err(e @ CompactError::After(_)) => {
                eprintln!("tree CompactionFailed: {e}");
                self.compact_threshold = compact_at(self.journal.len());
            }
        }
    }
}
