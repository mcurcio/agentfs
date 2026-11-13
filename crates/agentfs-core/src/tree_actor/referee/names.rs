//! Names: a landing's name onto a path a live upstream file holds (R11.2), and a sync's names,
//! the upstream's with the fork's undecided ones kept (docs/target-architecture/git-model.md §7,
//! §8).

use std::collections::{BTreeMap, BTreeSet};

use super::verdicts::Candidate;
use super::{CollisionKind, Snapshot, Version};
use crate::tree_actor::tree::{EventNumber, MergeFileIdentity, NameHistory};
use crate::tree_actor::{Error, Result};

/// R11.2 at a landing: an applied name onto a path a live upstream file holds loses to that
/// file's name, unless this change ended that file by a rename-over onto this name (its own
/// unlink frees the path even when it loses whole). One pass over `applied_names` by identity: it
/// reads only the upstream's names, so one pass is the fixed point.
pub(super) fn displace(
    upstream: &Snapshot,
    base: &Snapshot,
    incoming: &Snapshot,
    applied_names: &BTreeMap<MergeFileIdentity, NameHistory>,
    candidates: &mut [Candidate],
) -> Result<()> {
    for id in applied_names.keys() {
        let Some(path) = incoming.names.get(id).ok_or(Error::Format)?.path()? else {
            continue;
        };
        for (other, theirs) in &upstream.names {
            if other == id || theirs.path()? != Some(path) {
                continue;
            }
            let freed = match incoming.names.get(other) {
                Some(name) => {
                    name.is_ended()?
                        && name.head_version()?.replaced_by == Some(*id)
                        && name.moved_since(base.names.get(other))
                }
                None => false,
            };
            if !freed
                && let Some(candidate) = candidates
                    .iter_mut()
                    .find(|c| matches!(c.lost, Version::Name { file, .. } if file == *id))
            {
                candidate.loss = Some((
                    Version::Name {
                        file: *other,
                        version: theirs.head,
                    },
                    CollisionKind::Name,
                ));
            }
        }
    }
    Ok(())
}

/// A sync's names: the upstream's, a fork's undecided name kept; an upstream name that clashes
/// with one falls back to the fork's name for that file, repeated over chains; a file the fork
/// never had is hidden while it clashes.
pub(super) fn sync_names(
    upstream: &Snapshot,
    fork: &Snapshot,
    undecided: &BTreeSet<(MergeFileIdentity, EventNumber)>,
) -> Result<BTreeMap<MergeFileIdentity, NameHistory>> {
    let mut names = BTreeMap::new();
    let ids: BTreeSet<_> = upstream
        .names
        .keys()
        .chain(fork.names.keys())
        .copied()
        .collect();
    for id in ids {
        let theirs = upstream.names.get(&id);
        let own = fork.names.get(&id);
        let mut name = match (theirs, own) {
            (Some(theirs), Some(own)) => theirs.combine(own)?,
            (Some(theirs), None) => theirs.clone(),
            (None, Some(own)) if undecided.contains(&(id, own.head)) => own.clone(),
            (None, _) => continue,
        };
        if let Some(own) = own
            && (theirs.is_none() || undecided.contains(&(id, own.head)))
        {
            name.head = own.head;
        }
        name.path()?;
        names.insert(id, name);
    }
    loop {
        let mut occupied: BTreeMap<String, Vec<MergeFileIdentity>> = BTreeMap::new();
        for (&id, name) in &names {
            if let Some(path) = name.path()? {
                occupied.entry(path.to_owned()).or_default().push(id);
            }
        }
        let Some(ids) = occupied.values().find(|ids| ids.len() > 1) else {
            break;
        };
        // Both already show their fork names: the input fork is invalid.
        let id = ids
            .iter()
            .find(|id| fork.names.get(id).map(|h| h.head) != Some(names[*id].head))
            .copied()
            .ok_or(Error::Format)?;
        if let Some(own) = fork.names.get(&id) {
            names.get_mut(&id).ok_or(Error::Format)?.head = own.head;
        } else {
            // The fork never had this file; it is hidden until the clash ends.
            names.remove(&id);
        }
    }
    Ok(names)
}
