//! A fork's undecided versions and records: those whose save is not in the upstream's history
//! (R3a), the questions the referee asks of history (docs/target-architecture/git-model.md §8).

use std::collections::BTreeSet;

use super::{Record, Snapshot};
use crate::tree_actor::Result;
use crate::tree_actor::tree::{EventNumber, LineVersion, MergeFileIdentity};
use crate::tree_reconcile::Id;

/// History's answers the referee needs: whether the save numbered `save` is in the upstream's
/// history, and whether the commit that wrote the fork's value of a record is.
pub(in crate::tree_actor) trait Landed {
    fn landed(&mut self, save: EventNumber) -> Result<bool>;
    fn record_landed(&mut self, record: Record<'_>) -> Result<bool>;
}

/// The fork's versions and records the upstream has not decided.
#[derive(Debug, Default)]
pub(in crate::tree_actor) struct Undecided {
    pub lines: BTreeSet<LineVersion>,
    pub names: BTreeSet<(MergeFileIdentity, EventNumber)>,
    /// Files both hold whose metadata differs and is the fork's undecided value (rule 13).
    pub metas: BTreeSet<MergeFileIdentity>,
    /// Directory records that differ and are the fork's undecided value (rule 14).
    pub dirs: BTreeSet<String>,
}

/// The fork's versions the upstream at `upstream` has not decided (R3a): files in identity
/// order, lines in map order, then names; then the records where the two differ: each file's
/// metadata, each directory's (rules 13 and 14).
pub(in crate::tree_actor) fn undecided<L: Landed + ?Sized>(
    fork: &Snapshot,
    upstream: &Snapshot,
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
    for (id, file) in &fork.files {
        let differs = upstream.files.get(id).is_some_and(|f| f.meta != file.meta);
        if differs && !landed.record_landed(Record::Meta(*id))? {
            undecided.metas.insert(*id);
        }
    }
    let paths: BTreeSet<&String> = fork.dirs.keys().chain(upstream.dirs.keys()).collect();
    for path in paths {
        if fork.dirs.get(path) != upstream.dirs.get(path)
            && !landed.record_landed(Record::Dir(path))?
        {
            undecided.dirs.insert(path.clone());
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
