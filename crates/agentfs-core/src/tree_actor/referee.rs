//! Whole-save namespace and content referee (target git-model §7).
//!
//! Inputs are snapshots at the parent's current head, the incoming save's
//! first parent, and the save. The actor owns queue ordering and durability.

use super::tree::{EventNumber, LineVersion, Lines};
use super::{Error, MergeFileIdentity, NameHistory, NameVersion, Result};
use crate::tree_reconcile::{self, Verdict};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    pub files: BTreeMap<MergeFileIdentity, File>,
    pub names: BTreeMap<MergeFileIdentity, NameHistory>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct File {
    pub object: u64,
    pub lines: Lines,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) enum Version {
    Name {
        file: MergeFileIdentity,
        version: EventNumber,
    },
    Line {
        file: MergeFileIdentity,
        version: LineVersion,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum CollisionKind {
    Name,
    Line,
    Whole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Collision {
    pub lost: Version,
    pub winner: Version,
    pub kind: CollisionKind,
}

#[derive(Clone, Debug)]
pub(super) struct Decision {
    pub snapshot: Snapshot,
    pub collisions: Vec<Collision>,
}

fn history(snapshot: &Snapshot, id: MergeFileIdentity) -> Option<&NameHistory> {
    snapshot.names.get(&id)
}

fn head(history: &NameHistory) -> Result<&NameVersion> {
    history.versions.get(&history.head).ok_or(Error::Format)
}

fn path(history: &NameHistory) -> Result<Option<&str>> {
    Ok(head(history)?.path.as_deref())
}

fn name_descends(
    history: &NameHistory,
    mut current: EventNumber,
    ancestor: EventNumber,
) -> Result<bool> {
    let mut seen = BTreeSet::new();
    loop {
        if current == ancestor {
            return Ok(true);
        }
        if !seen.insert(current) {
            return Err(Error::Format);
        }
        match history.versions.get(&current).ok_or(Error::Format)?.over {
            Some(previous) => current = previous,
            None => return Ok(false),
        }
    }
}

fn combine_names(a: &NameHistory, b: &NameHistory) -> Result<NameHistory> {
    let mut combined = a.clone();
    for (key, value) in &b.versions {
        if let Some(existing) = combined.versions.get(key) {
            if existing.path != value.path
                || existing.over != value.over
                || existing.replaced_by != value.replaced_by
            {
                return Err(Error::Format);
            }
        } else {
            combined.versions.insert(*key, value.clone());
        }
    }
    Ok(combined)
}

fn name_verdict(
    parent: Option<&NameHistory>,
    incoming: &NameHistory,
) -> Result<(Verdict<EventNumber>, NameHistory)> {
    let Some(parent) = parent else {
        return Ok((Verdict::Apply, incoming.clone()));
    };
    let combined = combine_names(parent, incoming)?;
    if parent.head == incoming.head {
        return Ok((Verdict::Drop, combined));
    }
    if name_descends(&combined, incoming.head, parent.head)? {
        return Ok((Verdict::Apply, combined));
    }
    if name_descends(&combined, parent.head, incoming.head)? {
        return Ok((Verdict::Drop, combined));
    }
    if path(parent)?.is_none() && path(incoming)?.is_none() {
        return Ok((Verdict::Apply, combined));
    }
    Ok((
        Verdict::Lose {
            winner: parent.head,
        },
        combined,
    ))
}

fn line_descends<'a>(
    map: &'a Lines,
    mut current: &'a LineVersion,
    ancestor: &LineVersion,
) -> Result<bool> {
    let mut seen = BTreeSet::new();
    loop {
        if current == ancestor {
            return Ok(true);
        }
        if !seen.insert(current) {
            return Err(Error::Format);
        }
        match map.ancestors.get(current).ok_or(Error::Format)? {
            Some(previous) => current = previous,
            None => return Ok(false),
        }
    }
}

// An unlink can remove only versions its writer could have seen on its first
// parent. A line in that snapshot may itself descend from the parent's line.
fn unseen_parent_line(base: Option<&File>, parent: Option<&File>) -> Result<Option<LineVersion>> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let Some(base) = base else {
        return Ok(parent
            .lines
            .lines
            .iter()
            .find(|line| line.bytes.is_some())
            .map(|line| line.version));
    };
    let mut graph = base.lines.clone();
    for (version, over) in &parent.lines.ancestors {
        if graph.ancestors.get(version).is_some_and(|old| old != over) {
            return Err(Error::Format);
        }
        graph.ancestors.insert(*version, *over);
    }
    let previous: BTreeMap<_, _> = base.lines.lines.iter().map(|l| (&l.identity, l)).collect();
    for line in parent
        .lines
        .lines
        .iter()
        .filter(|line| line.bytes.is_some())
    {
        match previous.get(&line.identity) {
            Some(old) if line_descends(&graph, &old.version, &line.version)? => {}
            _ => return Ok(Some(line.version)),
        }
    }
    Ok(None)
}

/// Judge all versions in one save together. A losing save returns the parent
/// snapshot unchanged, while every applicable incoming version is reported.
pub(super) fn judge(
    parent: &Snapshot,
    base: &Snapshot,
    incoming: &Snapshot,
    prior_decisions: &BTreeMap<Version, Version>,
) -> Result<Decision> {
    let mut result = parent.clone();
    let mut candidates: Vec<(Version, Option<(Version, CollisionKind)>)> = Vec::new();
    let ids: BTreeSet<_> = base
        .names
        .keys()
        .chain(incoming.names.keys())
        .chain(base.files.keys())
        .chain(incoming.files.keys())
        .copied()
        .collect();
    let mut applied_names = BTreeSet::new();
    for id in &ids {
        let Some(new_name) = history(incoming, *id) else {
            continue;
        };
        if history(base, *id).is_some_and(|old| old.head == new_name.head) {
            continue;
        }
        let (verdict, mut combined) = name_verdict(history(parent, *id), new_name)?;
        let lost = Version::Name {
            file: *id,
            version: new_name.head,
        };
        let mut loss = match verdict {
            Verdict::Drop => continue,
            Verdict::Apply => None,
            Verdict::Lose { .. } => Some((
                Version::Name {
                    file: *id,
                    version: parent.names[id].head,
                },
                CollisionKind::Name,
            )),
        };
        if path(new_name)?.is_none()
            && loss.is_none()
            && let Some(line) = unseen_parent_line(base.files.get(id), parent.files.get(id))?
        {
            loss = Some((
                Version::Line {
                    file: *id,
                    version: line,
                },
                CollisionKind::Name,
            ));
        }
        candidates.push((lost, loss.clone()));
        if loss.is_none() {
            combined.head = new_name.head;
            result.names.insert(*id, combined);
            applied_names.insert(*id);
            if path(new_name)?.is_none() {
                result.files.remove(id);
            } else if let std::collections::btree_map::Entry::Vacant(entry) =
                result.files.entry(*id)
            {
                // Accepting a live name for an unborn identity creates an
                // empty file. Only content versions in this save are applied;
                // content from an earlier losing save remains absent.
                let object = incoming.files.get(id).ok_or(Error::Format)?.object;
                entry.insert(File {
                    object,
                    lines: Lines::default(),
                });
            }
        }
    }
    // A name cannot displace another live file unless this same save deletes
    // that identity (rename-over). Name decisions are considered together.
    for id in &applied_names {
        let Some(new_path) = path(&incoming.names[id])? else {
            continue;
        };
        for (other, parent_name) in &parent.names {
            if other == id || path(parent_name)? != Some(new_path) {
                continue;
            }
            // A rename-over's own unlink frees the destination for its name
            // claim even when that unlink loses the whole change.
            let deleted = match incoming.names.get(other) {
                Some(name) => {
                    path(name)?.is_none()
                        && head(name)?.replaced_by == Some(*id)
                        && base
                            .names
                            .get(other)
                            .is_none_or(|old| old.head != name.head)
                }
                None => false,
            };
            if !deleted
                && let Some((_, loss)) = candidates
                    .iter_mut()
                    .find(|(v, _)| matches!(v, Version::Name { file, .. } if file == id))
            {
                *loss = Some((
                    Version::Name {
                        file: *other,
                        version: parent_name.head,
                    },
                    CollisionKind::Name,
                ));
            }
        }
    }
    for id in &ids {
        let Some(new_file) = incoming.files.get(id) else {
            continue;
        };
        let old_file = base.files.get(id);
        if old_file.is_some_and(|old| old.lines == new_file.lines) {
            continue;
        }
        let original = parent.files.get(id);
        let rejected_name = if original.is_none() && !parent.names.contains_key(id) {
            if let Some(name) = base.names.get(id) {
                let mut current = Some(name.head);
                let mut winner = None;
                while let Some(version) = current {
                    if let Some(decider) =
                        prior_decisions.get(&Version::Name { file: *id, version })
                    {
                        winner = Some(decider.clone());
                        break;
                    }
                    current = name.versions.get(&version).ok_or(Error::Format)?.over;
                }
                winner
            } else {
                None
            }
        } else {
            None
        };
        let parent_deleted = match parent.names.get(id) {
            Some(name) => path(name)?.is_none(),
            None => rejected_name.is_some(),
        };
        if original.is_none() && parent_deleted {
            let winner = match rejected_name {
                Some(winner) => winner,
                None => Version::Name {
                    file: *id,
                    version: parent.names[id].head,
                },
            };
            let old_versions: BTreeMap<_, _> = old_file
                .into_iter()
                .flat_map(|f| f.lines.lines.iter())
                .map(|l| (&l.identity, &l.version))
                .collect();
            for line in &new_file.lines.lines {
                if old_versions.get(&line.identity) != Some(&&line.version) {
                    candidates.push((
                        Version::Line {
                            file: *id,
                            version: line.version,
                        },
                        Some((winner.clone(), CollisionKind::Name)),
                    ));
                }
            }
            continue;
        }
        let empty = Lines::default();
        let parent_map = original.map_or(&empty, |f| &f.lines);
        let base_map = old_file.map_or(&empty, |f| &f.lines);
        let landing = tree_reconcile::land_save(parent_map, base_map, &new_file.lines)?;
        let mut graph = parent_map.ancestors.clone();
        graph.extend(new_file.lines.ancestors.clone());
        let old_versions: BTreeMap<_, _> = base_map
            .lines
            .iter()
            .map(|l| (&l.identity, &l.version))
            .collect();
        let parent_lines: BTreeMap<_, _> =
            parent_map.lines.iter().map(|l| (&l.identity, l)).collect();
        for line in &new_file.lines.lines {
            if old_versions.get(&line.identity) == Some(&&line.version) {
                continue;
            }
            let own_collision = landing.collisions.iter().find(|c| c.lost == line.version);
            let loss = own_collision.and_then(|c| match c.kind {
                tree_reconcile::CollisionKind::Line => Some((
                    Version::Line {
                        file: *id,
                        version: c.winner,
                    },
                    CollisionKind::Line,
                )),
                tree_reconcile::CollisionKind::Whole => None,
            });
            if own_collision.is_some()
                || tree_reconcile::verdict(parent_lines.get(&line.identity).copied(), line, &graph)?
                    != Verdict::Drop
            {
                candidates.push((
                    Version::Line {
                        file: *id,
                        version: line.version,
                    },
                    loss,
                ));
            }
        }
        if landing.collisions.is_empty() {
            result.files.insert(
                *id,
                File {
                    object: original.map_or(new_file.object, |f| f.object),
                    lines: landing.content,
                },
            );
        }
    }
    let winner = candidates
        .iter()
        .find_map(|(_, loss)| loss.as_ref().map(|(v, _)| v.clone()));
    if let Some(winner) = winner {
        let collisions = candidates
            .into_iter()
            .map(|(lost, loss)| {
                let (decider, kind) =
                    loss.unwrap_or_else(|| (winner.clone(), CollisionKind::Whole));
                Collision {
                    lost,
                    winner: decider,
                    kind,
                }
            })
            .collect();
        Ok(Decision {
            snapshot: parent.clone(),
            collisions,
        })
    } else {
        Ok(Decision {
            snapshot: result,
            collisions: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::tree::Versions;

    fn file(snapshot: &mut Snapshot, id: u64, path: &str, bytes: &[u8]) {
        let key = MergeFileIdentity(id);
        let mut versions = Versions::of(EventNumber::new(id));
        let lines = Lines::default().align(bytes, || versions.next()).unwrap();
        snapshot
            .names
            .insert(key, NameHistory::created(EventNumber::new(id), path.into()));
        snapshot.files.insert(key, File { object: id, lines });
    }
    fn edit(snapshot: &mut Snapshot, id: u64, bytes: &[u8], seq: u64) {
        let mut versions = Versions::of(EventNumber::new(seq));
        let file = snapshot.files.get_mut(&MergeFileIdentity(id)).unwrap();
        file.lines = file.lines.align(bytes, || versions.next()).unwrap();
    }
    fn rename(snapshot: &mut Snapshot, id: u64, path: &str, seq: u64) {
        snapshot
            .names
            .get_mut(&MergeFileIdentity(id))
            .unwrap()
            .advance(EventNumber::new(seq), Some(path.into()), None);
    }
    fn unlink(snapshot: &mut Snapshot, id: u64, seq: u64, replaced_by: Option<u64>) {
        snapshot
            .names
            .get_mut(&MergeFileIdentity(id))
            .unwrap()
            .advance(
                EventNumber::new(seq),
                None,
                replaced_by.map(MergeFileIdentity),
            );
        snapshot.files.remove(&MergeFileIdentity(id));
    }
    fn base() -> Snapshot {
        let mut snapshot = Snapshot::default();
        file(&mut snapshot, 1, "a", b"one\ntwo\n");
        file(&mut snapshot, 2, "b", b"other\n");
        snapshot
    }

    #[test]
    fn rename_and_concurrent_edit_follow_the_identity() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"ONE\ntwo\n", 10);
        rename(&mut child, 1, "renamed", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("renamed")
        );
        assert_eq!(
            decision.snapshot.files[&MergeFileIdentity(1)].lines.bytes(),
            b"ONE\ntwo\n"
        );
    }

    #[test]
    fn competing_names_keep_parent_and_reject_the_whole_save() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        rename(&mut parent, 1, "shared", 10);
        rename(&mut child, 2, "shared", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("shared")
        );
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(2)]).unwrap(),
            Some("b")
        );
    }

    #[test]
    fn unlink_loses_against_an_unseen_parent_edit() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"one\nPARENT\n", 10);
        unlink(&mut child, 1, 11, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert_eq!(
            decision.snapshot.files[&MergeFileIdentity(1)].lines.bytes(),
            b"one\nPARENT\n"
        );
    }

    #[test]
    fn unlink_can_apply_when_parent_only_removed_an_unseen_line() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"one\n", 10);
        unlink(&mut child, 1, 11, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    #[test]
    fn parent_unlink_beats_an_incoming_edit() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        unlink(&mut parent, 1, 10, None);
        edit(&mut child, 1, b"one\nCHILD\n", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    #[test]
    fn an_edit_does_not_create_an_identity_whose_create_lost() {
        let mut base = base();
        file(&mut base, 3, "contested", b"first\n");
        let mut parent = base.clone();
        parent.files.remove(&MergeFileIdentity(3));
        parent.names.remove(&MergeFileIdentity(3));
        let mut child = base.clone();
        edit(&mut child, 3, b"later\n", 11);
        let lost = Version::Name {
            file: MergeFileIdentity(3),
            version: EventNumber::new(3),
        };
        let winner = Version::Name {
            file: MergeFileIdentity(2),
            version: EventNumber::new(2),
        };
        let prior = [(lost, winner.clone())].into_iter().collect();
        let decision = judge(&parent, &base, &child, &prior).unwrap();
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(3)));
        assert!(decision.collisions.iter().any(|c| c.winner == winner));
    }

    #[test]
    fn later_rename_of_rejected_create_has_no_rejected_content() {
        let mut base = base();
        file(&mut base, 3, "contested", b"rejected content\n");
        let mut parent = base.clone();
        parent.files.remove(&MergeFileIdentity(3));
        parent.names.remove(&MergeFileIdentity(3));
        rename(&mut parent, 2, "contested", 10);
        let mut child = base.clone();
        rename(&mut child, 3, "available", 11);
        let lost = Version::Name {
            file: MergeFileIdentity(3),
            version: EventNumber::new(3),
        };
        let winner = Version::Name {
            file: MergeFileIdentity(2),
            version: EventNumber::new(10),
        };
        let prior = [(lost, winner)].into_iter().collect();
        let decision = judge(&parent, &base, &child, &prior).unwrap();
        assert!(decision.collisions.is_empty());
        let renamed = MergeFileIdentity(3);
        assert_eq!(
            path(&decision.snapshot.names[&renamed]).unwrap(),
            Some("available")
        );
        assert_eq!(decision.snapshot.files[&renamed].object, 3);
        assert_eq!(decision.snapshot.files[&renamed].lines.bytes(), b"");
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(2)]).unwrap(),
            Some("contested")
        );
    }

    #[test]
    fn rename_over_loses_as_one_when_replaced_file_changed() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 2, b"parent\n", 10);
        unlink(&mut child, 2, 11, Some(1));
        rename(&mut child, 1, "b", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert_eq!(decision.collisions.len(), 2);
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("a")
        );
        assert_eq!(
            decision.snapshot.files[&MergeFileIdentity(2)].lines.bytes(),
            b"parent\n"
        );
        assert!(
            decision
                .collisions
                .iter()
                .any(|c| c.kind == CollisionKind::Whole)
        );
    }

    // R10.3, finding 31: an agreeing delete of a file a rename-over named.
    #[test]
    fn rename_over_then_unlink_agrees_with_a_sandbox_unlink() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        unlink(&mut parent, 2, 10, Some(1));
        rename(&mut parent, 1, "b", 10);
        unlink(&mut parent, 1, 11, None);
        unlink(&mut child, 1, 12, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        let name = &decision.snapshot.names[&MergeFileIdentity(1)];
        assert_eq!(path(name).unwrap(), None);
        assert_eq!(name.head, EventNumber::new(12));
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    // R10.8 through the whole-save judge: the later save, written over a save
    // that lost, loses whole to the parent's next line, by a line collision.
    #[test]
    fn unfinished_line_before_a_kept_line_is_a_line_collision() {
        let base = base();
        let mut first = base.clone();
        edit(&mut first, 1, b"y", 11);
        let mut parent = base.clone();
        edit(&mut parent, 1, b"one\nPARENT\n", 10);
        assert!(
            !judge(&parent, &base, &first, &BTreeMap::new())
                .unwrap()
                .collisions
                .is_empty()
        );
        let mut later = first.clone();
        edit(&mut later, 1, b"n6", 12);
        let decision = judge(&parent, &first, &later, &BTreeMap::new()).unwrap();
        assert_eq!(decision.collisions.len(), 1);
        assert_eq!(decision.collisions[0].kind, CollisionKind::Line);
        assert_eq!(
            decision.snapshot.files[&MergeFileIdentity(1)].lines.bytes(),
            b"one\nPARENT\n"
        );
    }

    #[test]
    fn atomic_rewrite_keeps_target_identity_and_merges_clean_lines() {
        let mut base = base();
        file(&mut base, 3, "temp", b"one\nCHILD\n");
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"PARENT\ntwo\n", 10);
        unlink(&mut child, 3, 11, None);
        edit(&mut child, 1, b"one\nCHILD\n", 12);
        child.files.get_mut(&MergeFileIdentity(1)).unwrap().object = 3;
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert_eq!(decision.snapshot.files[&MergeFileIdentity(1)].object, 1);
        assert_eq!(
            decision.snapshot.files[&MergeFileIdentity(1)].lines.bytes(),
            b"PARENT\nCHILD\n"
        );
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(3)));
    }
}
