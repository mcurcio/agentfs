//! The journal's commit point: appending a step's record, and compacting the journal behind it
//! (target git-model §3).

use super::durable::RecordNo;
use super::segment::FramePos;
use super::{CompactError, Entry, OpenRecord, PointerChange, Record, Target};
use crate::tree_actor::history::refs;
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
        self.before_record();
        self.append_record(entry, tail)
    }

    /// [`TreeActor::journal_record`] with nothing run before it: records of
    /// one step that no compaction may come between.
    pub(in crate::tree_actor) fn append_record(
        &mut self,
        entry: Entry,
        tail: &[u8],
    ) -> Result<(u64, FramePos)> {
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

    /// What runs before any record: saved maps the builder has checked out
    /// let go of their frames, and a compaction due since history trailed
    /// runs at the first call after it caught up.
    pub(in crate::tree_actor) fn before_record(&mut self) {
        self.rebase_built();
        self.compact_if_due();
    }

    /// Names each step value `changes` read before the last compaction as
    /// the oid it was built as: no record after a checkpoint names a step
    /// the checkpoint covers (restart builds from the checkpoint and knows
    /// no such step). [`Error::Format`] for one the compaction did not hold.
    pub(in crate::tree_actor) fn resolve_compacted(
        &self,
        changes: &mut [PointerChange],
    ) -> Result<()> {
        let (through, built) = &self.compacted;
        for change in changes.iter_mut() {
            for target in [&mut change.new, &mut change.old].into_iter().flatten() {
                if let Target::Step(step) = target
                    && *step <= *through
                {
                    let oid = built.get(step).ok_or(Error::Format)?;
                    *target = Target::Oid(oid.clone());
                }
            }
        }
        Ok(())
    }

    /// A fuzzy checkpoint (ARIES): the pointers and the live opens, written
    /// as the first frame of a new segment, then every older segment dropped
    /// whose records the builder has read and which holds no live open's call
    /// or held page write. A file held open pins only its own frames, never
    /// other files' saves: they are copied forward once the dead bytes they
    /// pin exceed their own. A closed generation's page write stays out of
    /// the fold (R12). Only with every step built: the checkpoint's pointers
    /// are the built ones, and its redo start is its last record. Records
    /// after the last step (calls, opens, page writes) build nothing the
    /// checkpoint does not hold, so a file under constant writes never holds
    /// compaction back.
    pub(in crate::tree_actor) fn compact(&mut self) -> std::result::Result<(), CompactError> {
        let _timer = Timer::start(Which::Compaction);
        if !self.history.steps_built() {
            return Err(CompactError::Before(std::io::Error::other(
                "history is not built through the journal's last step",
            )));
        }
        let snapshot = self.history.snapshot();
        let last = RecordNo::new(self.next.saturating_sub(1));
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
        // A step's value here is its head; a row chain's (the step's event
        // commit) is never read again: each row step names its chains' old
        // values after any compaction (`TreeActor::record`).
        let built: std::collections::BTreeMap<RecordNo, String> = self
            .pointers
            .iter()
            .filter(|(name, _)| !name.starts_with(refs::ROWS))
            .filter_map(|(name, target)| match target {
                Target::Step(step) => Some((*step, snapshot.pointers().get(name)?.to_string())),
                Target::Oid(_) => None,
            })
            .collect();
        self.compacted = (last, built);
        self.pointers = snapshot.pointers().to_targets();
        let (handles, pages) = (&self.handles, &self.pages);
        let live = |record: &Record| match &record.entry {
            Entry::Call { handle, .. } => handles.contains_key(handle),
            // A page write that names no tree is kept, never dropped.
            Entry::PageWrite { tree, object, .. } => tree.parse().map_or(true, |tree| {
                pages::holds(pages, tree, *object, record.seq.get())
            }),
            _ => false,
        };
        // The builder reads the records after its built point from the
        // linked segments: those stay until it has.
        let stats = self.journal.drop_behind(snapshot.at(), &live);
        // Whatever was carried before a failure has a copy: move to it.
        self.relocate_frames();
        let stats = stats.map_err(|e| CompactError::After(std::io::Error::other(e.to_string())))?;
        self.metrics.journal_dropped = self.metrics.journal_dropped.saturating_add(stats.segments);
        Ok(())
    }

    /// Moves every position the actor holds (live opens' calls and views,
    /// held page writes, saved maps) to where compaction carried its frame,
    /// letting go of the dropped segments. The builder moves its own at its
    /// next batch; a reader's map keeps its segment until the reader lets go.
    fn relocate_frames(&mut self) {
        let moved = self.journal.relocations().clone();
        for handle in self.handles.values_mut() {
            for (_, _, pos) in &mut handle.calls {
                pos.relocate(&moved);
            }
            handle.bytes.relocate(&moved);
        }
        crate::tree_actor::recording::pages::relocate(&mut self.pages, &moved);
        for tree in self.trees.values_mut() {
            for map in tree.bytes.values_mut() {
                if map.on_frames() {
                    map.relocate(&moved);
                }
            }
        }
    }

    pub(in crate::tree_actor) fn compact_if_due(&mut self) {
        let length = self.journal.len();
        if length < self.compact_threshold {
            return;
        }
        // A checkpoint's pointers are the built ones: due, it waits for the
        // first record after the builder has built every step and read past
        // the oldest segment (else it could drop nothing). Calls after the
        // builder's last batch never hold it back: they are live, and only
        // the segment they share with dead records waits for the next step.
        // The call path never waits on the builder; once the journal is twice
        // its due length with steps unbuilt, the lag gate holds new work until
        // the builder catches up.
        let steps_built = self.history.steps_built();
        self.history
            .compaction_overdue(!steps_built && length >= self.compact_threshold.saturating_mul(2));
        let read_past = self
            .journal
            .oldest_through()
            .is_none_or(|oldest| self.history.built_through() >= oldest);
        if !(steps_built && read_past) {
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
