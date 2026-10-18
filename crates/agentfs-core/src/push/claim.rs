//! The branch rule's two pure deciders (design §11.3; F8 "one branch per
//! worktree", rule 2).
//!
//! - [`claim`]: may this worktree write a ref the remote advertises? It
//!   may create a ref the remote lacks and fast-forward one whose value is
//!   on its own history; nothing else. Every write a worktree makes to its
//!   branch and to `refs/agentfs/<volume>/*` asks it, from the old value
//!   the remote reports in the connection that sends the update.
//! - [`start_branch_moved`]: has the branch a started volume came from
//!   (its [`StartPoint`]) moved past the commit it was pinned at, to
//!   commits this worktree does not have? The one decider of the
//!   start-branch alarm, raised by the new volume, never by the old node.
//! - [`lease_claim`]: may the operator's lease push (design §15) drop the
//!   commits the remote's branch holds beyond the head? Only those no
//!   agentfs recorder made (divergence case A), or ones the head holds
//!   rewritten (a history rewrite); never history a recorder wrote that
//!   the head lacks (case B, another worktree).
//!
//! All are pure: the inputs are what the remote advertised and a
//! predicate over, or facts from, this worktree's own history. The push record
//! ([`super::PushState`]) is never an input: it stays bookkeeping
//! (§11.2), and `measure_base` stays a hint. A remote's value that is not
//! in this repository is never fetched to decide: it counts as not on
//! this history.

use std::collections::HashSet;

use git2::Oid;

use crate::fork::identity::StartPoint;

/// What this worktree may do with one ref, from the remote's value in
/// this connection and this worktree's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The remote has no such ref: push it with a zero old value, so a
    /// ref another writer created first is never overwritten.
    Create,
    /// The remote holds the head already.
    UpToDate,
    /// The remote holds a commit on this worktree's history, an ancestor
    /// of the head: push the head, leased on that value.
    FastForward,
    /// The remote holds a commit unknown here, or one that is not an
    /// ancestor of the head: someone else wrote it (§11.3). Nothing is
    /// written to it; the branch's divergence is saved and alarmed.
    Diverged,
}

impl Claim {
    /// Whether the remote's ref is on this worktree's history
    /// ([`Claim::UpToDate`] or [`Claim::FastForward`]). For the branch,
    /// this is the condition every other write to `refs/agentfs/<volume>/*`
    /// needs in its own connection (status, `applied/*`, inbox deletions,
    /// `accepted/*`).
    pub fn is_ours(self) -> bool {
        matches!(self, Self::UpToDate | Self::FastForward)
    }

    /// Whether the remote holds the head: what the release decider counts
    /// as held ([`crate::release::decide`]).
    pub fn holds_head(self) -> bool {
        self == Self::UpToDate
    }
}

/// The claim on a ref the remote advertises at `advertised` (`None`: the
/// remote has no such ref), for a push of `head`. `on_history(oid)` is
/// true when `oid` is in this repository and is `head` or an ancestor of
/// it; it is never called for `None`, and not for `head` itself.
///
/// Callers: [`super::RemotePusher`]'s branch push (in libgit2's push
/// negotiation, from each update's old value), its queue push, its
/// divergence save, and the fork finisher through it.
pub fn claim(advertised: Option<Oid>, head: Oid, on_history: impl Fn(Oid) -> bool) -> Claim {
    match advertised {
        None => Claim::Create,
        Some(remote) if remote == head => Claim::UpToDate,
        Some(remote) if on_history(remote) => Claim::FastForward,
        Some(_) => Claim::Diverged,
    }
}

/// The start-branch alarm: the branch this volume started from holds
/// commits this volume does not have (R9, "saved and alarmed").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartBranchMoved {
    /// The start remote ([`StartPoint::remote`]).
    pub remote: String,
    /// The old branch, as a full ref name ([`StartPoint::refname`]).
    pub refname: String,
    /// Where this volume started ([`StartPoint::commit`]).
    pub pinned: Oid,
    /// The old branch now; the alarm names `pinned..tip`. `tip` need not
    /// be in this repository: nothing is fetched to raise the alarm.
    pub tip: Oid,
}

/// Whether `start`'s branch, which the start remote advertises at
/// `advertised` in this push attempt, has moved past the pinned commit to
/// commits this volume does not have.
///
/// `None` (no alarm) when the remote has no such branch, when it is still
/// at the pinned commit, or when `on_history(tip)`: the tip is in this
/// repository and is the head or an ancestor of it, or of one of this
/// volume's `refs/agentfs/<volume>/accepted/*` refs (an operator's
/// `agentfs start accept`). Otherwise `Some`, at every attempt to the
/// start remote while it holds: the push loop raises
/// `Event::StartBranchMoved` and keeps it in the push state for
/// `agentfs status`.
pub fn start_branch_moved(
    start: &StartPoint,
    advertised: Option<Oid>,
    on_history: impl Fn(Oid) -> bool,
) -> Option<StartBranchMoved> {
    let tip = advertised?;
    if tip == start.commit || on_history(tip) {
        return None;
    }
    Some(StartBranchMoved {
        remote: start.remote.clone(),
        refname: start.refname(),
        pinned: start.commit,
        tip,
    })
}

/// A commit as an agentfs recorder made it (design §9): its
/// `Agentfs-Seq` and its author and committer. A history rewrite (`git
/// filter-repo`, design §15) keeps all of these and changes the tree, the
/// id and the ids quoted in the message; the same sequence number recorded
/// again from a stale state area differs in its time at least.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Recording {
    pub seq: u64,
    pub author: Stamp,
    pub committer: Stamp,
}

/// One signature of a [`Recording`]: name, email and time, as git keeps
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stamp {
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    pub seconds: i64,
    pub offset_minutes: i32,
}

/// What the operator's lease push may do with the remote's branch, from
/// the commits it would drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseClaim {
    /// Every commit dropped is a foreign writer's (no recorder made it) or
    /// one the head holds rewritten: replace the branch.
    Replace,
    /// This commit, which a recorder made, is on the remote's branch and on
    /// the head neither as is nor rewritten: the branch holds history this
    /// state area did not write (a stale state area, divergence case B, or
    /// another worktree under the same name). Never replaced.
    Recorded(Oid),
}

/// Whether the lease push may drop `dropped`: the commits on the remote's
/// branch that the head lacks, each with its [`Recording`] (`None`: no
/// recorder made it). `kept` holds the recordings of the commits on the
/// head that the remote's branch lacks: a dropped recording found there is
/// the same record rewritten (design §15). The first dropped recording not
/// found there is [`LeaseClaim::Recorded`].
///
/// Git keeps no record of who created a remote branch, and the push record
/// is never an input (it stays bookkeeping, §11.2): who made each commit
/// (its trailers, §9) is what tells a foreign writer (case A) from this
/// volume's own newer history (case B). A foreign commit that copies an
/// agentfs message, a cherry-pick, counts as recorded and is refused: the
/// safe side.
pub fn lease_claim(
    dropped: impl IntoIterator<Item = (Oid, Option<Recording>)>,
    kept: &HashSet<Recording>,
) -> LeaseClaim {
    dropped
        .into_iter()
        .find_map(|(oid, recording)| recording.filter(|r| !kept.contains(r)).map(|_| oid))
        .map_or(LeaseClaim::Replace, LeaseClaim::Recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    fn never(_: Oid) -> bool {
        panic!("on_history must not be asked")
    }

    #[test]
    fn each_claim_from_what_the_remote_advertises() {
        let head = oid(1);
        // Never asked for a missing ref or for the head itself.
        assert_eq!(claim(None, head, never), Claim::Create);
        assert_eq!(claim(Some(head), head, never), Claim::UpToDate);
        assert_eq!(
            claim(Some(oid(2)), head, |o| o == oid(2)),
            Claim::FastForward
        );
        assert_eq!(claim(Some(oid(3)), head, |_| false), Claim::Diverged);
    }

    #[test]
    fn a_remote_behind_the_head_waits() {
        let all = [
            Claim::Create,
            Claim::UpToDate,
            Claim::FastForward,
            Claim::Diverged,
        ];
        let ours: Vec<Claim> = all.into_iter().filter(|c| c.is_ours()).collect();
        assert_eq!(ours, [Claim::UpToDate, Claim::FastForward]);
        // Only a remote at the head holds it: one behind (FastForward), or
        // without the branch (Create), lacks commits.
        let held: Vec<Claim> = all.into_iter().filter(|c| c.holds_head()).collect();
        assert_eq!(held, [Claim::UpToDate]);
    }

    fn start() -> StartPoint {
        StartPoint {
            remote: "origin".into(),
            branch: "agentfs/v1".into(),
            commit: oid(1),
        }
    }

    #[test]
    fn the_start_branch_alarm() {
        let start = start();
        // The branch is gone, or still at the pin: no alarm, nothing asked.
        assert_eq!(start_branch_moved(&start, None, never), None);
        assert_eq!(start_branch_moved(&start, Some(oid(1)), never), None);
        // The tip is on this history: taken in, or reached by an accepted
        // ref (the caller's predicate covers both).
        assert_eq!(
            start_branch_moved(&start, Some(oid(2)), |o| o == oid(2)),
            None
        );
        // A tip unknown here.
        assert_eq!(
            start_branch_moved(&start, Some(oid(3)), |_| false),
            Some(StartBranchMoved {
                remote: "origin".into(),
                refname: "refs/heads/agentfs/v1".into(),
                pinned: oid(1),
                tip: oid(3),
            })
        );
    }

    fn recording(seq: u64, seconds: i64) -> Recording {
        let stamp = |name: &[u8]| Stamp {
            name: name.to_vec(),
            email: b"e@example.invalid".to_vec(),
            seconds,
            offset_minutes: 0,
        };
        Recording {
            seq,
            author: stamp(b"agent"),
            committer: stamp(b"agentfs"),
        }
    }

    /// Case A replaces, a rewrite replaces, case B is refused.
    #[test]
    fn a_lease_push_drops_only_foreign_or_rewritten_commits() {
        let none = HashSet::new();
        assert_eq!(lease_claim([], &none), LeaseClaim::Replace);
        // Case A: a person's commits.
        assert_eq!(
            lease_claim([(oid(1), None), (oid(2), None)], &none),
            LeaseClaim::Replace
        );
        // Case B: a recorder's commit the head lacks, under a foreign one.
        assert_eq!(
            lease_claim([(oid(1), None), (oid(2), Some(recording(7, 100)))], &none),
            LeaseClaim::Recorded(oid(2))
        );
        // A rewrite: the head holds the same record under another id.
        let kept = HashSet::from([recording(7, 100)]);
        assert_eq!(
            lease_claim([(oid(2), Some(recording(7, 100)))], &kept),
            LeaseClaim::Replace
        );
        // The same sequence number recorded again later is another record.
        let later = HashSet::from([recording(7, 160)]);
        assert_eq!(
            lease_claim([(oid(2), Some(recording(7, 100)))], &later),
            LeaseClaim::Recorded(oid(2))
        );
    }
}
