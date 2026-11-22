//! The journal's commit point: appending a step's record, and what the actor
//! does under its lock at a built point so history's compactor can let the
//! journal go behind it: the switch record, and its positions moved to
//! carried frames (target git-model §3; docs/trees.md, "Recording and
//! durability").

use super::durable::RecordNo;
use super::segment::FramePos;
use super::{Entry, PointerChange, Record, Switched};
use crate::tree_actor::stages::{Hold, section};
use crate::tree_actor::{Error, Result, TreeActor, now_ns};

impl TreeActor {
    /// Appends one [`Entry::Switch`] for every pointer whose step History
    /// has built and whose built value its snapshot holds, row chains
    /// included, then applies it: append first, apply second, as a step
    /// does. Returns the record, or `None` when there was nothing to switch,
    /// the append failed or the actor has stopped. A failed append changes
    /// no pointer and is tried again at the next built point.
    ///
    /// Called under the lock in the two places the actor acts on a built
    /// point: in `record_with`, right after a step's record, and in
    /// `on_built`. Each caller passes the record returned to
    /// `History::want_through`. O(1) when the built point has not moved
    /// since the last full pass; otherwise O(pointers at a step) and one
    /// append. No fsync, no journal read, no unlink.
    ///
    /// "Whose built value its snapshot holds": a fork's branch is set by its
    /// fork step f to its upstream's value `Step(s)`, s < f, and History
    /// holds it only once f is built. Until then it stays a step, and is
    /// never given another pointer's oid.
    ///
    /// A step keeps its own time: the switch record never moves
    /// `last_at_ns`, which the step just recorded applies its shape with, as
    /// recovery and the builder do from the step's own record.
    pub(in crate::tree_actor) fn switch_built(&mut self) -> Option<RecordNo> {
        if self.is_stopped() {
            return None;
        }
        let snapshot = self.history.snapshot();
        let built = snapshot.at();
        if self.switched_at == Some(built) {
            return None;
        }
        let pointers: Vec<Switched> = self
            .at_steps
            .iter()
            .filter(|(_, step)| **step <= built)
            .filter_map(|(name, step)| {
                Some(Switched {
                    name: name.clone(),
                    step: *step,
                    oid: snapshot.pointers().get(name)?.to_string(),
                })
            })
            .collect();
        if pointers.is_empty() {
            self.switched_at = Some(built);
            return None;
        }
        let changes: Vec<PointerChange> = pointers.iter().map(Switched::change).collect();
        let step_at_ns = self.last_at_ns;
        let appended = self.append_record(Entry::Switch { pointers }, &[]);
        self.last_at_ns = step_at_ns;
        match appended {
            Ok((seq, _)) => {
                self.apply_pointers(&changes);
                self.switched_at = Some(built);
                Some(RecordNo::new(seq))
            }
            Err(e) => {
                eprintln!("tree CompactionFailed: the switch record: {e}");
                None
            }
        }
    }

    /// At a built event, under the lock, only when the compactor carried or
    /// let go of frames since the last time (`Relocations::generation`
    /// moved): moves the actor's positions (live opens' views, held page
    /// writes, saved maps) to the carried copies, so the unlinked segments'
    /// space returns. The one walk compaction leaves under the lock, timed
    /// as `Hold::Compaction` (design rule 1: named and measured).
    pub(in crate::tree_actor) fn relocate_if_moved(&mut self) {
        let moved = self.journal.durability().relocations().clone();
        let generation = moved.generation();
        if generation == self.relocated {
            return;
        }
        let _kind = section(Hold::Compaction);
        for handle in self.handles.values_mut() {
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
        self.relocated = generation;
    }

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
    /// one step, appended back to back.
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
        self.last_at_ns = record.at_ns;
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
    /// let go of their frames.
    pub(in crate::tree_actor) fn before_record(&mut self) {
        self.rebase_built();
    }
}
