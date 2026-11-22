//! The journal's commit point: appending a step's record, and what the actor
//! does under its lock at a built event so history's compactor can let the
//! journal go behind it (target git-model §3; docs/trees.md, "Recording and
//! durability").

use super::durable::RecordNo;
use super::segment::FramePos;
use super::{Entry, Record, Target};
use crate::tree_actor::stages::{Hold, section};
use crate::tree_actor::{Error, Result, TreeActor, now_ns};

impl TreeActor {
    /// At a built event, under the lock: every pointer whose value is a step
    /// at or below History's built point becomes the oid that step built,
    /// row chains included, and each switch is told to History before the
    /// next record ([`History::switched`], C3, C4): no record from then on
    /// names that step, so a checkpoint past it lets the step head go.
    /// O(pointers).
    ///
    /// [`History::switched`]: crate::tree_actor::History::switched
    pub(in crate::tree_actor) fn switch_built(&mut self) {
        let snapshot = self.history.snapshot();
        // EXPERIMENT (erratum 2026-11-22): only steps a published checkpoint
        // keeps the head of, whose oid a fold from it knows.
        let built = snapshot.at().min(self.history.checkpointed().built);

        let at = RecordNo::new(self.next);
        let switched: Vec<(String, RecordNo, String)> = self
            .pointers
            .iter()
            .filter_map(|(name, target)| match target {
                Target::Step(step) if *step <= built => {
                    let oid = snapshot.pointers().get(name)?;
                    Some((name.clone(), *step, oid.to_string()))
                }
                _ => None,
            })
            .collect();
        for (name, step, oid) in switched {
            self.pointers.set(&name, Some(Target::Oid(oid)));
            self.history.switched(&name, step, at);
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
