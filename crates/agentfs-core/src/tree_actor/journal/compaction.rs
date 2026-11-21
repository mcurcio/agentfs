//! History's port onto the journal for compaction: the checkpoint in a file
//! of its own beside the segments, written from history's state at its built
//! point, then the segments it covers let go
//! (docs/target-architecture/future/latency-upkeep.md; target git-model §3).
//!
//! ```text
//! journal/checkpoint       the newest published checkpoint: one frame,
//!                          salted and checksummed as a segment's are
//! journal/checkpoint.tmp   a checkpoint being written; open ignores it
//! journal/carry-<B:020>    live frames copied out of segments a checkpoint
//!                          at B lets go, flag `carried`, record numbers kept
//! ```
//!
//! Interfaces only: nothing calls this module until the compaction slice
//! lands, and every body is `todo!()`. The call path never imports it.
//!
//! Invariants the slice keeps (the design page's C1 to C9):
//! - **C1.** [`JournalCompaction::publish`] runs only after every record at
//!   or below the checkpoint's `built` is durable ([`Durable`] is the proof).
//! - **C2, C2a.** [`JournalCompaction::unlink_covered`] unlinks a segment
//!   only once a [`Published`] checkpoint covers every record in it, only
//!   once the actor has rolled past it (a later segment is in the segment
//!   table), and only once every live frame in it has a durable copy that
//!   the published checkpoint names.
//! - **C5, C7.** A torn tail never removes the checkpoint, and the journal
//!   always opens: the salt, the redo start `built + 1` and "no record after
//!   `built`" come from the checkpoint file; a missing or wholly torn segment
//!   after `built` is a torn tail.
//! - **C9.** No method holds the lock of Durability's state, of the segment
//!   table or of the relocations across I/O or a wait.

#![expect(
    dead_code,
    reason = "interfaces of the compaction slice (latency-upkeep.md, slice 2), wired by it"
)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::durable::{Durability, DurableError, RecordNo};
use super::fold::Folded;
use super::{DropStats, OpenRecord, TreeLocals};

/// The file name of the newest published checkpoint, in `journal/`.
pub(in crate::tree_actor) const CHECKPOINT: &str = "checkpoint";

/// The file name a checkpoint is written to before it is renamed into place.
pub(in crate::tree_actor) const CHECKPOINT_TMP: &str = "checkpoint.tmp";

/// What every record at or below `built` adds up to (C8). Each field's one
/// source at `built` (B):
///
/// | Field | Source at B |
/// |---|---|
/// | `salt` | the journal's, unchanged across compaction |
/// | `built` | History's built point, read in one hold with its pointers, objects and trees |
/// | `pointers` | History's built pointers at B, every value an oid |
/// | `objects` | History's built object numbers at B |
/// | `locals` | History's built trees at B: each tree's shape's locals |
/// | `upstreams` | the recovery fold over the records at or below B (`Folded::upstreams`) |
/// | `opens` | the fold: each live write-open with `by`, `anonymous` and `meta` (`Folded::opens`; the builder's `BuildOpen` has no `by`, so it is not the source) |
/// | `page_saved` | the fold's last saving step per file with a live write-open (`Folded::saved_through`) |
/// | `page_writes` | the page writes recovery holds (`TreeActor::pages_restore` keeps every one the fold gives): the record numbers in `Folded::page_writes`, sorted, each once |
/// | `chunks` | the fold's chunk records no take or replace step has taken yet (`Folded::bytes`) |
/// | `step_heads` | History's step heads at B ([`History::at_built`]): for every pointer, row chains included (today's exception for them rests on compaction switching the actor's pointers in its own hold, which no longer happens), the latest step at or below B that set it and the oid it built, kept until History's built point passes the record at which the actor switched that pointer to an oid (C3, C4) |
/// | `carried` | the carry files this checkpoint names live frames in |
///
/// `next` is `built + 1`: records after B carry their own numbers. Today's
/// `Entry::Checkpoint` takes `locals` from the actor's shapes, which equal
/// history's only when every step is built; at B they are history's.
///
/// [`History::at_built`]: crate::tree_actor::History::at_built
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct Checkpoint {
    pub salt: u32,
    pub built: RecordNo,
    pub pointers: BTreeMap<String, String>,
    pub objects: BTreeMap<String, BTreeMap<u64, u64>>,
    pub locals: BTreeMap<String, TreeLocals>,
    pub upstreams: BTreeMap<String, String>,
    pub opens: Vec<OpenRecord>,
    /// Tree, object, saved_through.
    pub page_saved: Vec<(String, u64, u64)>,
    pub page_writes: Vec<RecordNo>,
    pub chunks: Vec<RecordNo>,
    pub step_heads: BTreeMap<RecordNo, String>,
    /// Each carry file by the `built` of the compaction that wrote it.
    pub carried: Vec<RecordNo>,
}

impl Checkpoint {
    /// The first record after it: `built + 1`.
    pub(in crate::tree_actor) fn next(&self) -> RecordNo {
        todo!()
    }
}

/// Every record at or below `through` is durable: what
/// [`JournalCompaction::publish`] takes as proof of C1. Only
/// [`JournalCompaction::wait_durable`] constructs one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Durable {
    through: RecordNo,
}

/// A checkpoint at `built` renamed into place with its directory synced:
/// what [`JournalCompaction::unlink_covered`] takes as proof of C2. Only
/// [`JournalCompaction::publish`] constructs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Published {
    built: RecordNo,
    carried: Vec<RecordNo>,
}

impl Published {
    /// The redo start's record before: every record at or below it is covered.
    pub(in crate::tree_actor) fn built(&self) -> RecordNo {
        self.built
    }
}

/// Live frames copied out of the segments a checkpoint at `built` lets go,
/// durable: the checkpoint names their carry files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Carried {
    pub built: RecordNo,
    /// Every carry file the checkpoint names: kept ones and the new one.
    pub files: Vec<RecordNo>,
    pub bytes: u64,
}

/// Why a compaction stopped.
#[derive(Debug, thiserror::Error)]
pub(in crate::tree_actor) enum CompactionError {
    /// The checkpoint's own temporary file or a carry file could not be
    /// written or fsynced: nothing a restart reads changed. Logged as
    /// `tree CompactionFailed`, retried at the next built event.
    #[error("journal compaction failed before its checkpoint: {0}")]
    NotPublished(std::io::Error),
    /// The checkpoint is published but a covered segment could not be
    /// unlinked: it stays, and the next compaction tries again.
    #[error("journal compaction failed after its checkpoint: {0}")]
    NotDropped(std::io::Error),
    /// An fsync of the journal's directory or of a segment failed, or the
    /// actor has stopped: recorded through `Durability::fsync_failed`, so
    /// the actor stops, as a leader's failed fsync stops it.
    #[error(transparent)]
    Stopped(#[from] DurableError),
}

/// History's port onto one volume's journal: the only writer of the
/// checkpoint file and the carry files, and the only code that unlinks a
/// segment. Shared by the compactor thread and by `TreeActor::open`, which
/// compacts once before serving. Holds no actor state.
pub(in crate::tree_actor) struct JournalCompaction {
    dir: PathBuf,
    durability: Arc<Durability>,
}

impl JournalCompaction {
    /// The port onto the journal in `dir`, whose group commit and segment
    /// table are `durability`.
    pub(in crate::tree_actor) fn new(dir: PathBuf, durability: Arc<Durability>) -> Self {
        Self { dir, durability }
    }

    /// Every record at or below `through` made durable (group commit).
    ///
    /// # Errors
    ///
    /// [`DurableError`] as `Durability::make_durable` gives it.
    pub(in crate::tree_actor) fn wait_durable(
        &self,
        through: RecordNo,
    ) -> Result<Durable, DurableError> {
        todo!("{through:?}")
    }

    /// The recovery fold (`journal::fold`) over the published checkpoint and
    /// every record after it at or below `built`, read from the segments with
    /// no lock held. One definition of each transition: the fold recovery
    /// runs.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`] for a failed read or a record the
    /// fold refuses.
    pub(in crate::tree_actor) fn fold_through(
        &self,
        built: &Durable,
    ) -> Result<Folded, CompactionError> {
        todo!("{built:?}")
    }

    /// Copies the live frames (a call of an open `checkpoint.opens` lists, a
    /// page write in `checkpoint.page_writes`, a chunk in `checkpoint.chunks`)
    /// out of every segment the checkpoint will let go into
    /// `carry-<built>`, fsynced, and records each copy in the relocations. A
    /// carry file whose live frames outweigh its dead bytes is kept as it is.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`]: the new carry file is removed.
    pub(in crate::tree_actor) fn carry(
        &self,
        checkpoint: &Checkpoint,
    ) -> Result<Carried, CompactionError> {
        todo!("{:?}", checkpoint.built)
    }

    /// Writes `checkpoint` to `checkpoint.tmp`, fsyncs it, renames it to
    /// `checkpoint` and fsyncs the directory.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`] while the old checkpoint still
    /// stands; [`CompactionError::Stopped`] for a failed fsync of the
    /// directory.
    pub(in crate::tree_actor) fn publish(
        &self,
        checkpoint: &Checkpoint,
        durable: &Durable,
        carried: &Carried,
    ) -> Result<Published, CompactionError> {
        todo!("{:?} {durable:?} {carried:?}", checkpoint.built)
    }

    /// Unlinks every segment the actor has rolled past whose records are all
    /// at or below `published.built()`, and every carry file `published`
    /// does not name, then fsyncs the directory. Each leaves the segment
    /// table under Durability's lock in O(1); its unlink runs with no lock
    /// held. A reader holding an unlinked segment keeps reading it, and its
    /// bytes count in [`JournalCompaction::len`] until the last holder lets go.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotDropped`]: what was not unlinked stays;
    /// [`CompactionError::Stopped`] for a failed fsync of the directory.
    pub(in crate::tree_actor) fn unlink_covered(
        &self,
        published: &Published,
    ) -> Result<DropStats, CompactionError> {
        todo!("{published:?}")
    }

    /// The journal's length: every linked segment, every carry file, and
    /// every unlinked segment a reader still holds (C6). What compaction is
    /// due by.
    pub(in crate::tree_actor) fn len(&self) -> u64 {
        todo!()
    }
}
