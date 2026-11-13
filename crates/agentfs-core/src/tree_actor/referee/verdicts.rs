//! Rule 3 for one change, for names and lines alike: each changed version of the change against
//! the upstream's, and its loss so far (docs/target-architecture/git-model.md §7).

use std::collections::{BTreeMap, BTreeSet};

use super::ancestry::{self, Broken, Graph, descends};
use super::{CollisionKind, File, PriorDecisions, Snapshot, Verdict, Version};
use crate::tree_actor::tree::{LineVersion, Lines, MergeFileIdentity, NameHistory};
use crate::tree_actor::{Error, Result};
use crate::tree_reconcile::Line;

/// A version as rule 3 reads it: which one, and whether it is live (a line's bytes, a file's
/// name) or ended.
#[derive(Clone, Copy)]
pub(super) struct Held<V> {
    pub version: V,
    pub live: bool,
}

/// Drop: the upstream holds this version or wrote over it (R3e.2). Apply: written over the
/// upstream's, or both are deletes. Else Lose to the upstream's (R3b). `upstream` None: Apply.
pub(super) fn verdict<G: Graph + ?Sized>(
    g: &G,
    upstream: Option<Held<G::Version>>,
    incoming: Held<G::Version>,
) -> std::result::Result<Verdict<G::Version>, Broken> {
    let Some(upstream) = upstream else {
        return Ok(Verdict::Apply);
    };
    if upstream.version == incoming.version {
        return Ok(Verdict::Drop);
    }
    if descends(g, incoming.version, upstream.version)? {
        return Ok(Verdict::Apply);
    }
    if descends(g, upstream.version, incoming.version)? {
        return Ok(Verdict::Drop);
    }
    if !upstream.live && !incoming.live {
        return Ok(Verdict::Apply);
    }
    Ok(Verdict::Lose {
        winner: upstream.version,
    })
}

/// A candidate's place: names before lines, then file identity, then the line's index in the
/// incoming map. The order of `Agentfs-Collisions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Order {
    pub line: bool,
    pub file: MergeFileIdentity,
    pub at: usize,
}

/// A version of the change that is not dropped, with its own loss so far.
#[derive(Clone, Debug)]
pub(super) struct Candidate {
    pub order: Order,
    pub lost: Version,
    pub loss: Option<(Version, CollisionKind)>,
}

#[derive(Default)]
pub(super) struct Verdicts {
    /// Every version of the change not dropped, with its own loss so far.
    pub candidates: Vec<Candidate>,
    /// Each name that applies (no loss): both histories combined, the head the change's.
    pub applied_names: BTreeMap<MergeFileIdentity, NameHistory>,
    /// Each file whose lines are placed, with its changed lines' verdicts by line identity. A
    /// file the upstream ended (by name, or by a prior decision against its create) is not here:
    /// its versions are candidates losing by Name.
    pub files: BTreeMap<MergeFileIdentity, BTreeMap<LineVersion, Verdict<LineVersion>>>,
}

fn held_name(name: &NameHistory) -> Result<Held<crate::tree_actor::tree::EventNumber>> {
    Ok(Held {
        version: name.head,
        live: name.path()?.is_some(),
    })
}

fn held_line(line: &Line<LineVersion>) -> Held<LineVersion> {
    Held {
        version: line.version,
        live: line.bytes.is_some(),
    }
}

/// R3c.2: an unlink removes only versions its writer could have seen on its first parent. The
/// upstream's first live line not seen there (a line in that snapshot may itself be written over
/// the upstream's).
fn unseen_upstream_line(
    base: Option<&File>,
    upstream: Option<&File>,
) -> Result<Option<LineVersion>> {
    let Some(upstream) = upstream else {
        return Ok(None);
    };
    let Some(base) = base else {
        return Ok(upstream
            .lines
            .lines
            .iter()
            .find(|line| line.bytes.is_some())
            .map(|line| line.version));
    };
    let graph = ancestry::union(&base.lines.ancestors, &upstream.lines.ancestors)?;
    let seen: BTreeMap<_, _> = base.lines.lines.iter().map(|l| (&l.identity, l)).collect();
    for line in upstream
        .lines
        .lines
        .iter()
        .filter(|line| line.bytes.is_some())
    {
        match seen.get(&line.identity) {
            Some(old) if descends(&graph, old.version, line.version)? => {}
            _ => return Ok(Some(line.version)),
        }
    }
    Ok(None)
}

/// The decision that ended a file the upstream has no file for: its unlink or rename-over in
/// the upstream's names, or, for a file the upstream never named, a prior decision against a
/// version of its name (its create lost).
fn ended_by(
    upstream: &Snapshot,
    base: &Snapshot,
    id: MergeFileIdentity,
    prior: &PriorDecisions,
) -> Result<Option<Version>> {
    if let Some(name) = upstream.names.get(&id) {
        return Ok(name.path()?.is_none().then_some(Version::Name {
            file: id,
            version: name.head,
        }));
    }
    let Some(name) = base.names.get(&id) else {
        return Ok(None);
    };
    let mut current = Some(name.head);
    while let Some(version) = current {
        if let Some(decider) = prior.get(&Version::Name { file: id, version }) {
            return Ok(Some(decider.clone()));
        }
        current = name.versions.get(&version).ok_or(Error::Format)?.over;
    }
    Ok(None)
}

/// Rule 3 for every changed name and line of `incoming` over `base`, an unlink against an
/// upstream line its writer never saw (R3c.2), and an edit of a file the upstream ended.
pub(super) fn verdicts(
    upstream: &Snapshot,
    base: &Snapshot,
    incoming: &Snapshot,
    prior: &PriorDecisions,
) -> Result<Verdicts> {
    let mut out = Verdicts::default();
    let ids: BTreeSet<MergeFileIdentity> = base
        .names
        .keys()
        .chain(incoming.names.keys())
        .chain(base.files.keys())
        .chain(incoming.files.keys())
        .copied()
        .collect();
    for id in &ids {
        let Some(name) = incoming.names.get(id) else {
            continue;
        };
        if base.names.get(id).is_some_and(|old| old.head == name.head) {
            continue;
        }
        let theirs = upstream.names.get(id);
        let mut combined = match theirs {
            Some(theirs) => theirs.combine(name)?,
            None => name.clone(),
        };
        let held = theirs.map(held_name).transpose()?;
        let mut loss = match verdict(&combined, held, held_name(name)?)? {
            Verdict::Drop => continue,
            Verdict::Apply => None,
            Verdict::Lose { winner } => Some((
                Version::Name {
                    file: *id,
                    version: winner,
                },
                CollisionKind::Name,
            )),
        };
        if name.path()?.is_none()
            && loss.is_none()
            && let Some(line) = unseen_upstream_line(base.files.get(id), upstream.files.get(id))?
        {
            loss = Some((
                Version::Line {
                    file: *id,
                    version: line,
                },
                CollisionKind::Name,
            ));
        }
        if loss.is_none() {
            combined.head = name.head;
            out.applied_names.insert(*id, combined);
        }
        out.candidates.push(Candidate {
            order: Order {
                line: false,
                file: *id,
                at: 0,
            },
            lost: Version::Name {
                file: *id,
                version: name.head,
            },
            loss,
        });
    }
    let empty = Lines::default();
    for id in &ids {
        let Some(file) = incoming.files.get(id) else {
            continue;
        };
        let old = base.files.get(id);
        if old.is_some_and(|old| old.lines == file.lines) {
            continue;
        }
        let theirs = upstream.files.get(id);
        let base_lines = old.map_or(&empty, |old| &old.lines);
        let seen: BTreeMap<&LineVersion, &LineVersion> = base_lines
            .lines
            .iter()
            .map(|line| (&line.identity, &line.version))
            .collect();
        let changed = file
            .lines
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| seen.get(&line.identity) != Some(&&line.version));
        let lost = |at: usize, line: &Line<LineVersion>| -> (Order, Version) {
            (
                Order {
                    line: true,
                    file: *id,
                    at,
                },
                Version::Line {
                    file: *id,
                    version: line.version,
                },
            )
        };
        if theirs.is_none()
            && let Some(winner) = ended_by(upstream, base, *id, prior)?
        {
            for (at, line) in changed {
                let (order, lost) = lost(at, line);
                out.candidates.push(Candidate {
                    order,
                    lost,
                    loss: Some((winner.clone(), CollisionKind::Name)),
                });
            }
            continue;
        }
        let upstream_lines = theirs.map_or(&empty, |theirs| &theirs.lines);
        let graph = ancestry::union(&upstream_lines.ancestors, &file.lines.ancestors)?;
        let current: BTreeMap<&LineVersion, &Line<LineVersion>> = upstream_lines
            .lines
            .iter()
            .map(|line| (&line.identity, line))
            .collect();
        let mut verdicts = BTreeMap::new();
        for (at, line) in changed {
            let held = current.get(&line.identity).map(|l| held_line(l));
            let verdict = verdict(&graph, held, held_line(line))?;
            let loss = match &verdict {
                Verdict::Drop => None,
                Verdict::Apply => Some(None),
                Verdict::Lose { winner } => Some(Some((
                    Version::Line {
                        file: *id,
                        version: *winner,
                    },
                    CollisionKind::Line,
                ))),
            };
            if let Some(loss) = loss {
                let (order, lost) = lost(at, line);
                out.candidates.push(Candidate { order, lost, loss });
            }
            verdicts.insert(line.identity, verdict);
        }
        out.files.insert(*id, verdicts);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::tree::{EventNumber, Versions};

    fn save(base: &Lines, bytes: &[u8], event: u64) -> Lines {
        let mut versions = Versions::of(EventNumber::new(event));
        base.align(bytes, || versions.next()).unwrap()
    }

    fn judge(
        upstream: &Line<LineVersion>,
        incoming: &Line<LineVersion>,
        graph: &ancestry::Ancestors,
    ) -> Verdict<LineVersion> {
        verdict(graph, Some(held_line(upstream)), held_line(incoming)).unwrap()
    }

    #[test]
    fn concurrent_edits_lose_and_deletes_agree() {
        let root = save(&Lines::default(), b"one\ntwo\n", 1);
        let parent = save(&root, b"parent\ntwo\n", 2);
        let child = save(&root, b"child\ntwo\n", 3);
        let mut graph = parent.ancestors.clone();
        graph.extend(child.ancestors.clone());
        assert_eq!(
            judge(&parent.lines[0], &child.lines[0], &graph),
            Verdict::Lose {
                winner: parent.lines[0].version
            }
        );
        let deleted_p = save(&root, b"two\n", 4);
        let deleted_c = save(&root, b"two\n", 5);
        graph = deleted_p.ancestors.clone();
        graph.extend(deleted_c.ancestors.clone());
        assert_eq!(
            judge(&deleted_p.lines[0], &deleted_c.lines[0], &graph),
            Verdict::Apply
        );
    }

    #[test]
    fn later_write_over_parent_wins_and_old_version_drops() {
        let root = save(&Lines::default(), b"one\n", 1);
        let parent = save(&root, b"parent\n", 2);
        let later = save(&parent, b"later\n", 6);
        assert_eq!(
            judge(&parent.lines[0], &later.lines[0], &later.ancestors),
            Verdict::Apply
        );
        assert_eq!(
            judge(&later.lines[0], &parent.lines[0], &later.ancestors),
            Verdict::Drop
        );
    }
}
