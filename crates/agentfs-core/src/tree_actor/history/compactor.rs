//! The compactor: history's thread that compacts the journal behind the call
//! path, at its own built point, woken by built events and never taking the
//! actor's lock (docs/trees.md, "Recording and durability").
//!
//! Lock order: the actor, then History, then Durability, then the
//! relocations. The compactor takes History's lock only to clone its
//! persistent maps at the built point (O(1)), and Durability's only to read
//! or change the segment table; it holds neither across I/O or a wait (C9).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use super::built::{BuiltObjects, History};
use crate::tree_actor::TreeId;
use crate::tree_actor::journal::compaction::{
    Checkpoint, CompactionError, Gap, JournalCompaction, Sealed,
};
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::{DropStats, Folded, OpenRecord, Target};
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::stages::{Timer, Which};
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
}

/// The default floor of the due length: `Upkeep::default().compact_from`.
pub(in crate::tree_actor) const COMPACT_FROM: u64 = 64 << 20;

/// When a journal whose last compaction kept `baseline` bytes compacts
/// again: at twice that, and at least `from`. The due rule's one home, read
/// only by the compactor through [`Baseline::due_at`].
pub(in crate::tree_actor) fn compact_at(from: u64, baseline: u64) -> u64 {
    from.max(baseline.saturating_mul(2))
}

/// What the last compaction kept of what it covered (`Carried::kept`), less
/// what has died since for certain: the length [`compact_at`] doubles.
///
/// It holds no byte appended after that compaction's built point. A
/// compaction may run while a large open is being written and free nothing;
/// what the open appends after it is growth, so the built event of the
/// open's save is due. The frames the compaction kept for the open come off
/// when that save is built, so they hold nothing off either. The
/// compactor's alone.
#[derive(Debug, Default)]
pub(in crate::tree_actor) struct Baseline {
    /// The bytes at or below the last compaction's built point it left
    /// linked.
    kept: u64,
    /// Each open live at that compaction, with its call frames' bytes then.
    pinned: BTreeMap<u64, u64>,
    /// The bytes of the pinned opens whose saves are built since.
    released: u64,
}

impl Baseline {
    /// After a compaction that kept `kept` bytes of what it covered,
    /// `pinned` of them the call frames of each open live at its built
    /// point.
    pub(in crate::tree_actor) fn new(kept: u64, pinned: BTreeMap<u64, u64>) -> Self {
        Self {
            kept,
            pinned,
            released: 0,
        }
    }

    /// The saves of `opens` are built: the frames each had at the last
    /// compaction are dead.
    pub(in crate::tree_actor) fn ended(&mut self, opens: &[u64]) {
        for open in opens {
            if let Some(bytes) = self.pinned.remove(open) {
                self.released = self.released.saturating_add(bytes);
            }
        }
    }

    /// The journal length the next compaction is due at, with floor `from`.
    pub(in crate::tree_actor) fn due_at(&self, from: u64) -> u64 {
        compact_at(from, self.kept.saturating_sub(self.released))
    }
}

/// What one compaction did: what it carried and let go, and the next
/// [`Baseline`], which pins each open live at its built point with its call
/// frames' bytes (`FoldedOpen::frame_bytes`).
pub(in crate::tree_actor) type Compacted = (DropStats, Baseline);

/// The compactor thread of one volume, owned by the actor beside the builder
/// and joined by its shutdown before the builder's join and before the state
/// area's lease is released.
pub(in crate::tree_actor) struct Compactor {
    thread: Option<JoinHandle<()>>,
}

impl Compactor {
    /// Starts the thread: at each built event, with a gate
    /// (`journal.gated()`), compacts; without, compacts once
    /// `journal.len()` reaches [`Baseline::due_at`]: `compact_at` of
    /// `compact_from` and what the last compaction kept of what it covered,
    /// less the bytes of the opens live then whose saves are built since. Ends once
    /// History is stopping. A failure is logged
    /// (`tree CompactionFailed`) and retried at the next built event, except
    /// [`CompactionError::Stopped`]: recorded through
    /// `Durability::fsync_failed`, so the actor stops, then
    /// [`History::compaction_stopped`] ends every wait and the thread.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(in crate::tree_actor) fn start(
        history: History,
        journal: JournalCompaction,
        compact_from: u64,
    ) -> std::io::Result<Self> {
        let owner = crate::failpoints::owner();
        let thread = std::thread::Builder::new()
            .name("tree-compactor".into())
            .spawn(move || {
                crate::failpoints::adopt(owner);
                run(&history, &journal, compact_from);
            })?;
        Ok(Self {
            thread: Some(thread),
        })
    }

    /// Waits for the thread to end. A publish in flight finishes, or leaves
    /// only `checkpoint.tmp`, which open ignores.
    pub(in crate::tree_actor) fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread's loop: one compaction at each built event once due, then
/// History is told the compactor is done with that event.
fn run(history: &History, journal: &JournalCompaction, compact_from: u64) {
    let mut seen = history.ends();
    let mut baseline = Baseline::default();
    while let Some(ends) = history.wait_batch_end(seen.ended) {
        let built = ends.built > seen.built;
        seen = ends;
        if !built {
            continue;
        }
        let through = history.built_through();
        baseline.ended(&history.take_ended(through));
        if journal.gated() || journal.len() >= baseline.due_at(compact_from) {
            match compact_once(history, journal) {
                Ok(Some((_, kept))) => baseline = kept,
                Ok(None) => {}
                Err(e @ CompactionError::NotPublished(_)) => {
                    eprintln!("tree CompactionFailed: {e}");
                }
                // The checkpoint is published and its unlink failed: what it
                // covers is still linked, and nothing says what it pins. The
                // whole length holds the retry off until the journal doubles.
                Err(e @ CompactionError::NotDropped(_)) => {
                    eprintln!("tree CompactionFailed: {e}");
                    baseline = Baseline::new(journal.len(), BTreeMap::new());
                }
                Err(e @ CompactionError::Stopped(_)) => {
                    eprintln!("tree CompactionFailed: {e}");
                    history.compaction_stopped();
                    return;
                }
            }
        }
        history.compaction_ended(through);
    }
}

/// The checkpoint at `at`'s built point, from History's state there and the
/// fold of the records at or below it (C8). `carried` is left empty:
/// [`Sealed::new`] writes it.
fn checkpoint_at(at: &AtBuilt, folded: &Folded) -> Checkpoint {
    // The fold alone says which pointers stand at a step at B (C4): a
    // switch record at or below B has made its pointer an oid already.
    let step_heads: BTreeMap<String, RecordNo> = folded
        .pointers
        .iter()
        .filter_map(|(name, target)| match target {
            Target::Step(step) => Some((name.clone(), *step)),
            Target::Oid(_) => None,
        })
        .collect();
    let mut page_writes: Vec<RecordNo> = folded
        .page_writes
        .values()
        .flat_map(|runs| runs.iter().map(|(seq, _)| RecordNo::new(*seq)))
        .collect();
    page_writes.sort_unstable();
    page_writes.dedup();
    Checkpoint {
        salt: folded.salt,
        built: at.built,
        pointers: at.pointers.to_hex(),
        objects: at
            .objects
            .iter()
            .map(|(tree, objects)| {
                let objects = objects.iter().map(|(id, object)| (*id, *object)).collect();
                (tree.to_string(), objects)
            })
            .collect(),
        locals: at
            .trees
            .iter()
            .map(|(tree, t)| (tree.to_string(), t.shape.locals()))
            .collect(),
        upstreams: folded.upstreams.to_text(),
        opens: folded
            .opens
            .values()
            .map(|folded| OpenRecord {
                handle: folded.open.handle,
                tree: folded.open.tree.to_string(),
                by: folded.open.by.clone(),
                object: folded.open.object,
                anonymous: folded.open.anonymous,
                meta: folded.open.meta,
                close_begun: folded.open.close_begun,
            })
            .collect(),
        page_saved: folded
            .saved_through
            .iter()
            .map(|((tree, object), seq)| (tree.to_string(), *object, *seq))
            .collect(),
        page_writes,
        chunks: folded
            .bytes
            .iter()
            .map(|(seq, _)| RecordNo::new(*seq))
            .collect(),
        step_heads,
        carried: Vec::new(),
    }
}

/// One compaction at History's built point B, the thread's one sequence:
/// the gate at [`Gap::Covered`]; wait until every record at or below B is
/// durable (C1); fold the records at or below B; carry live frames; seal and
/// publish the checkpoint (the gate at [`Gap::Written`] and
/// [`Gap::Renamed`]); the gate at [`Gap::DirSynced`]; tell History it is
/// published (only then may it forget what it covers, C3); the gate at
/// [`Gap::BeforeUnlink`]; unlink what it covers (C2, C2a). With what it
/// dropped, the next baseline. `None` before
/// History's first built event since open, or when B is not past the last
/// published checkpoint's `built`.
///
/// # Errors
///
/// [`CompactionError`], each with the journal as the error says.
pub(in crate::tree_actor) fn compact_once(
    history: &History,
    journal: &JournalCompaction,
) -> Result<Option<Compacted>, CompactionError> {
    let Some(at) = history.at_built() else {
        return Ok(None);
    };
    if at.built <= history.checkpointed().built {
        return Ok(None);
    }
    let _timer = Timer::start(Which::Compaction);
    journal.gate(Gap::Covered)?;
    let durable = journal.wait_durable(at.built)?;
    let (folded, below) = journal.fold_through(&durable)?;
    let checkpoint = checkpoint_at(&at, &folded);
    let pinned = folded
        .opens
        .iter()
        .map(|(handle, open)| (*handle, open.frame_bytes()))
        .collect();
    drop(folded);
    let carried = journal.carry(&checkpoint, &below)?;
    let baseline = Baseline::new(carried.kept(), pinned);
    let made = carried.made();
    let published =
        Sealed::new(checkpoint, durable, carried).and_then(|sealed| journal.publish(&sealed));
    if let Err(CompactionError::NotPublished(_)) = &published {
        // The old checkpoint stands, and names no carry file made here.
        journal.abandon(made);
    }
    let published = published?;
    let dir_synced = journal.gate(Gap::DirSynced);
    history.published(&published);
    dir_synced?;
    journal.gate(Gap::BeforeUnlink)?;
    let stats = journal.unlink_covered(&published)?;
    Ok(Some((stats, baseline)))
}

#[cfg(test)]
mod tests;
