//! The compactor: history's thread that compacts the journal behind the call
//! path, at its own built point, woken by built events and never taking the
//! actor's lock (docs/target-architecture/future/latency-upkeep.md).
//!
//! Interfaces only: nothing starts the compactor until the compaction slice
//! lands, and every body is `todo!()`.
//!
//! Lock order: the actor, then History, then Durability. The compactor takes
//! History's lock only to clone its persistent maps at the built point (O(1)),
//! and Durability's only to read or change the segment table (O(segments));
//! it holds neither across I/O or a wait (C9).

#![expect(
    dead_code,
    reason = "interfaces of the compaction slice (latency-upkeep.md, slice 2), wired by it"
)]

use std::sync::Arc;
use std::thread::JoinHandle;

use git2::Oid;

use super::built::{BuiltObjects, History};
use crate::tree_actor::TreeId;
use crate::tree_actor::journal::DropStats;
use crate::tree_actor::journal::compaction::{CompactionError, JournalCompaction};
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::tree::Tree;

/// What the compactor reads of History at its built point B, in one hold of
/// History's lock ([`History::at_built`]): every field a persistent map or an
/// `Arc`, so the hold is O(1) (C9).
#[derive(Clone)]
pub(in crate::tree_actor) struct AtBuilt {
    /// B: History's built point.
    pub built: RecordNo,
    /// Every pointer as built at B, each an oid.
    pub pointers: Arc<Pointers>,
    /// Each tree's object numbers as built at B.
    pub objects: BuiltObjects,
    /// The builder's trees at B: the checkpoint's `locals` come from their shapes.
    pub trees: imbl::OrdMap<TreeId, Tree>,
    /// Each pointer's latest step head at or below B, step to the oid it
    /// built, kept until the built point passes the record at which the
    /// actor switched that pointer to an oid (C3, C4). At most one per pointer.
    pub step_heads: imbl::OrdMap<RecordNo, Oid>,
    /// The first record of the newest segment the actor has rolled to
    /// ([`History::rolled`]): every segment that starts before it is rolled
    /// past (C2a).
    pub rolled: RecordNo,
}

/// When a journal this long after its last compaction compacts again: the
/// due rule's one home, read only by the compactor.
pub(in crate::tree_actor) fn compact_at(len_after_last: u64) -> u64 {
    todo!("{len_after_last}")
}

/// A gap in one compaction, where a test parks the compactor while the actor
/// appends (C2a's conformance leg): after the coverage check, after the
/// checkpoint's temporary file is written, after the rename, after the
/// directory's fsync, before the unlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Gap {
    Covered,
    Written,
    Renamed,
    DirSynced,
    BeforeUnlink,
}

/// The compactor thread of one volume, owned by the actor beside the builder
/// and joined by its shutdown before the state area's lease is released.
pub(in crate::tree_actor) struct Compactor {
    thread: Option<JoinHandle<()>>,
}

impl Compactor {
    /// Starts the thread: at each built event, compacts once when the
    /// journal is due ([`compact_at`]) and a segment the actor has rolled
    /// past is covered by the built point; ends once History is stopping.
    /// A failure is logged (`tree CompactionFailed`) and retried at the next
    /// built event, except [`CompactionError::Stopped`], which stops the actor.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(in crate::tree_actor) fn start(
        history: History,
        journal: JournalCompaction,
    ) -> std::io::Result<Self> {
        let _ = (history, journal);
        todo!()
    }

    /// Waits for the thread to end. A publish in flight finishes, or leaves
    /// only `checkpoint.tmp`, which open ignores.
    pub(in crate::tree_actor) fn join(self) {
        todo!("{}", self.thread.is_some())
    }
}

/// One compaction at History's built point B, the sequence the thread and
/// `TreeActor::open` share: wait until every record at or below B is durable
/// (C1); clone History's pointers, objects and trees at B; fold the records
/// at or below B; carry live frames; publish the checkpoint; tell History it
/// is published (only then may it forget what it covers, C3); unlink what it
/// covers (C2, C2a). `None` when nothing is covered past the last checkpoint.
///
/// # Errors
///
/// [`CompactionError`], each with the journal as the error says.
pub(in crate::tree_actor) fn compact_once(
    history: &History,
    journal: &JournalCompaction,
) -> Result<Option<DropStats>, CompactionError> {
    let _ = (history, journal);
    todo!()
}
