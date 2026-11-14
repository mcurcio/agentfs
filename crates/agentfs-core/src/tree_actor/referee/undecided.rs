//! A fork's undecided versions: those whose save is not in the upstream's history (R3a), the one
//! question the referee asks of history (docs/target-architecture/git-model.md §8).

use std::collections::BTreeSet;

use super::Snapshot;
use crate::tree_actor::Result;
use crate::tree_actor::tree::{EventNumber, LineVersion, MergeFileIdentity};
use crate::tree_reconcile::Id;

/// History's one answer the referee needs: whether the save numbered `save` is in the upstream's
/// history.
pub(in crate::tree_actor) trait Landed {
    fn landed(&mut self, save: EventNumber) -> Result<bool>;
}

/// The fork's versions the upstream has not decided.
#[derive(Debug, Default)]
pub(in crate::tree_actor) struct Undecided {
    pub lines: BTreeSet<LineVersion>,
    pub names: BTreeSet<(MergeFileIdentity, EventNumber)>,
}

/// The fork's versions the upstream has not decided (R3a): files in identity order, lines in map
/// order, then names.
pub(in crate::tree_actor) fn undecided<L: Landed + ?Sized>(
    fork: &Snapshot,
    landed: &mut L,
) -> Result<Undecided> {
    let mut undecided = Undecided::default();
    for file in fork.files.values() {
        for run in file.lines.runs() {
            // A run's versions are one event's.
            if !landed.landed(run.version.event())? {
                undecided
                    .lines
                    .extend((0..run.count).map(|i| run.version.nth(i)));
            }
        }
    }
    for (id, history) in &fork.names {
        if !landed.landed(history.head)? {
            undecided.names.insert((*id, history.head));
        }
    }
    Ok(undecided)
}

/// The files the upstream ended that the fork keeps under an undecided name (finding 44): the
/// upstream has the file's name history and no file.
pub(in crate::tree_actor) fn ended_files(
    upstream: &Snapshot,
    undecided: &Undecided,
) -> Result<BTreeSet<MergeFileIdentity>> {
    let mut ended = BTreeSet::new();
    for (id, _) in &undecided.names {
        let gone = !upstream.files.contains_key(id)
            && match upstream.names.get(id) {
                Some(name) => name.is_ended()?,
                None => false,
            };
        if gone {
            ended.insert(*id);
        }
    }
    Ok(ended)
}
