//! R3e, its one home: a change lands whole or not at all (docs/target-architecture/git-model.md
//! §7).

use super::verdicts::Candidate;
use super::{Collision, CollisionKind, Decision, Snapshot};

/// Sorts `candidates` by their order. A loss anywhere: every candidate is a collision, its own loss
/// or `Whole` to the first loss's winner, and the snapshot is the upstream's. Else the snapshot is
/// `applied`.
pub(super) fn whole(
    upstream: &Snapshot,
    applied: Snapshot,
    mut candidates: Vec<Candidate>,
) -> Decision {
    candidates.sort_by_key(|candidate| candidate.order);
    let Some(first) = candidates
        .iter()
        .find_map(|candidate| candidate.loss.as_ref().map(|(winner, _)| winner.clone()))
    else {
        return Decision {
            snapshot: applied,
            collisions: Vec::new(),
        };
    };
    let collisions = candidates
        .into_iter()
        .map(|candidate| {
            let (winner, kind) = candidate
                .loss
                .unwrap_or_else(|| (first.clone(), CollisionKind::Whole));
            Collision {
                lost: candidate.lost,
                winner,
                kind,
            }
        })
        .collect();
    Decision {
        snapshot: upstream.clone(),
        collisions,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Version;
    use super::super::verdicts::Order;
    use super::*;
    use crate::tree_actor::tree::{EventNumber, LineVersion, MergeFileIdentity};

    fn line(file: u64, version: &str) -> Version {
        Version::Line {
            file: MergeFileIdentity(file),
            version: version.parse::<LineVersion>().unwrap(),
        }
    }

    fn candidate(
        line_: bool,
        file: u64,
        at: usize,
        lost: Version,
        loss: Option<Version>,
    ) -> Candidate {
        Candidate {
            order: Order {
                line: line_,
                file: MergeFileIdentity(file),
                at,
            },
            lost,
            loss: loss.map(|winner| (winner, CollisionKind::Line)),
        }
    }

    #[test]
    fn the_first_loss_in_order_names_every_whole() {
        let name = Version::Name {
            file: MergeFileIdentity(2),
            version: EventNumber::new(5),
        };
        // Pushed out of order: two files' lines, a later line's loss pushed first, then a name.
        let candidates = vec![
            candidate(true, 2, 1, line(2, "9:2"), Some(line(2, "8:1"))),
            candidate(true, 1, 0, line(1, "9:1"), None),
            candidate(true, 2, 0, line(2, "9:3"), Some(line(2, "7:1"))),
            candidate(false, 2, 0, name.clone(), None),
        ];
        let upstream = Snapshot::default();
        let decision = whole(&upstream, Snapshot::default(), candidates);
        let collisions: Vec<_> = decision
            .collisions
            .iter()
            .map(|c| (c.lost.clone(), c.winner.clone(), c.kind.clone()))
            .collect();
        assert_eq!(
            collisions,
            vec![
                (name, line(2, "7:1"), CollisionKind::Whole),
                (line(1, "9:1"), line(2, "7:1"), CollisionKind::Whole),
                (line(2, "9:3"), line(2, "7:1"), CollisionKind::Line),
                (line(2, "9:2"), line(2, "8:1"), CollisionKind::Line),
            ]
        );
    }

    #[test]
    fn no_loss_lands_what_applied() {
        let mut applied = Snapshot::default();
        applied.names.insert(
            MergeFileIdentity(1),
            crate::tree_actor::tree::NameHistory::created(EventNumber::new(1), "a".into()),
        );
        let decision = whole(
            &Snapshot::default(),
            applied,
            vec![candidate(true, 1, 0, line(1, "1:1"), None)],
        );
        assert!(decision.collisions.is_empty());
        assert_eq!(decision.snapshot.names.len(), 1);
    }
}
