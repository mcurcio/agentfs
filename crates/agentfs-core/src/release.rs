//! Releasing a worktree (F8 rule 3, design §14.2): the one decider of
//! whether DeleteVolume removes a worktree, keeps it, or waits.
//!
//! [`decide`] is pure. Its inputs are the worktree's declared remotes,
//! what each pushed remote advertises for the branch (and for the refs of
//! forks received into this history) in a listing made
//! for this call (rule 3: it asks the remote, never the push record), how
//! many journaled records history lacks, the class's `onDelete`, and, for
//! a fork, its status. It answers:
//!
//! - [`Release::Delete`]: every declared remote holds the head (and the
//!   refs of every fork received into this history), or the
//!   worktree has no history, its journal holds nothing history lacks,
//!   and `onDelete` is `delete`;
//! - [`Release::Keep`]: the volume is released but the worktree stays on
//!   its node's disk: `onDelete: retain` once every declared remote holds
//!   the head and the journal is committed, or nothing elsewhere can hold
//!   it (local-only, undeclared);
//! - [`Release::Wait`]: not yet. DeleteVolume answers `UNAVAILABLE`, the
//!   CO retries, and the node plugin's release scan commits (`agentfs
//!   flush`) and pushes the worktree meanwhile, raising `PushFailed` at
//!   each failed push. A journal history lacks is what a history service
//!   stopped with `SIGKILL`, or crash-looping, at NodeUnstage leaves: the
//!   head alone would call those records held, or, with no branch yet,
//!   call the worktree empty.
//!
//! Nothing else decides deletion: not CreateVolume, not a class
//! parameter's validation, not the release scan. A StorageClass's
//! `reclaimPolicy` is always `Delete` (the chart's admission policy
//! refuses any other), so DeleteVolume always runs and this decider
//! always answers; a static PV with `Retain` is never deleted by
//! Kubernetes and its worktree is pushed by the release scan while it is
//! not staged.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{Remotes, Settings};
use crate::fork::finish::ForkState;
use crate::push::{Claim, Unreachable};

/// What the class (or a static PV) says DeleteVolume does with the
/// worktree once nothing would be lost: the `onDelete` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnDelete {
    /// Keep the backing tree and history on the node (re-usable through a
    /// static PV on that node).
    #[default]
    Retain,
    /// Remove the worktree.
    Delete,
}

/// An `onDelete` value that is neither `retain` nor `delete`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("onDelete {0:?} must be retain or delete")]
pub struct BadOnDelete(pub String);

impl FromStr for OnDelete {
    type Err = BadOnDelete;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "retain" => Ok(Self::Retain),
            "delete" => Ok(Self::Delete),
            other => Err(BadOnDelete(other.to_owned())),
        }
    }
}

impl fmt::Display for OnDelete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retain => "retain",
            Self::Delete => "delete",
        })
    }
}

/// What one pushed remote advertises for the branch, judged against the
/// worktree's head ([`crate::push::RemotePusher::branch_claim`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub remote: String,
    pub branch: Result<Claim, Unreachable>,
    /// The first ref of a fork received into this history that the remote
    /// does not hold ([`crate::push::RemotePusher::received_lacking`]).
    pub received_lacking: Option<String>,
}

/// How the worktree declares its remotes, with what each pushed one
/// shows, mapped one to one from `config::Remotes` ([`Declared::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declared<'a> {
    /// One entry per valid push remote, in `Settings` order.
    Pushed(&'a [Seen]),
    /// Push sections, all invalid: nothing can be pushed until the
    /// profile is fixed.
    NoneValid,
    /// `agentfs.remotes = none` (F8 S3).
    LocalOnly,
    /// No push section and no declaration, or `none` beside a section.
    Undeclared,
}

impl<'a> Declared<'a> {
    /// How `settings` declares its remotes; `seen` must hold one entry per
    /// valid push remote of `settings` (ignored otherwise).
    pub fn of(settings: &Settings, seen: &'a [Seen]) -> Self {
        match settings.remotes {
            Remotes::Pushed(_) => Self::Pushed(seen),
            Remotes::NoneValid => Self::NoneValid,
            Remotes::LocalOnly => Self::LocalOnly,
            Remotes::Undeclared { .. } => Self::Undeclared,
        }
    }
}

/// Everything [`decide`] weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree<'a> {
    /// The branch exists locally. Without it only the journal can hold
    /// anything, and the remotes are not asked.
    pub has_history: bool,
    /// Journaled records history lacks ([`crate::volume::inspect`]'s
    /// backlog): durable, promised to their callers, and on no remote. A
    /// fork's finish commits them, so a fork's status is its gate instead.
    pub uncommitted: usize,
    pub on_delete: OnDelete,
    pub remotes: Declared<'a>,
    /// A fork's status (design §11.5): publishing is its gate, so its
    /// remotes are not asked; `None` for any other worktree.
    pub fork: Option<ForkState>,
}

/// What DeleteVolume does with a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    /// Remove the worktree.
    Delete,
    /// Release the volume; keep the worktree on the node's disk.
    Keep(Kept),
    /// Not yet: answer `UNAVAILABLE`, naming why; the CO retries.
    Wait(Waiting),
}

/// Why a released worktree stays on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// `onDelete: retain`, every declared remote holds the head, and the
    /// journal is committed.
    Retained,
    /// Declared local-only: no remote holds it, by declaration.
    LocalOnly,
    /// No remote and no declaration: nothing elsewhere holds it.
    Undeclared,
}

/// Why DeleteVolume waits. Every case but [`Waiting::ForkUnpublished`]
/// is the release scan's to push; a fork is finished and published by
/// it once its deletion asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The journal holds `records` history lacks: the release scan
    /// commits them (`agentfs flush`), then pushes.
    Uncommitted { records: usize },
    /// The remote does not hold the head ([`Claim::Create`] or
    /// [`Claim::FastForward`]).
    Lacking { remote: String },
    /// The remote holds the head but not `refname`, a ref of a fork
    /// received into this history (a local-only source's fork, before a
    /// remote was added): the release scan pushes it with the branch.
    ReceivedLacking { remote: String, refname: String },
    /// The remote could not be listed.
    Unreachable { remote: String, message: String },
    /// The remote's branch is another writer's ([`Claim::Diverged`]): the
    /// worktree is never removed until an operator resolves it
    /// (divergence runbook).
    Diverged { remote: String },
    /// Every push section is invalid.
    NoneValid,
    /// A fork not yet published or abandoned.
    ForkUnpublished { state: ForkState },
}

/// The release decision for `worktree` (see the module docs). Remotes are
/// weighed in order and the first that does not hold the head is named.
pub fn decide(worktree: &Worktree<'_>) -> Release {
    let held = || match worktree.on_delete {
        _ if worktree.fork.is_none() && worktree.uncommitted > 0 => {
            Release::Wait(Waiting::Uncommitted {
                records: worktree.uncommitted,
            })
        }
        OnDelete::Delete => Release::Delete,
        OnDelete::Retain => Release::Keep(Kept::Retained),
    };
    if !worktree.has_history {
        return held();
    }
    if let Some(state) = worktree.fork {
        return if state.may_delete() {
            held()
        } else {
            Release::Wait(Waiting::ForkUnpublished { state })
        };
    }
    match worktree.remotes {
        Declared::LocalOnly => Release::Keep(Kept::LocalOnly),
        Declared::Undeclared => Release::Keep(Kept::Undeclared),
        Declared::NoneValid => Release::Wait(Waiting::NoneValid),
        Declared::Pushed(seen) => {
            for remote in seen {
                let name = || remote.remote.clone();
                let waiting = match &remote.branch {
                    Ok(claim) if claim.holds_head() => match &remote.received_lacking {
                        None => continue,
                        Some(refname) => Waiting::ReceivedLacking {
                            remote: name(),
                            refname: refname.clone(),
                        },
                    },
                    Ok(Claim::Diverged) => Waiting::Diverged { remote: name() },
                    Ok(_) => Waiting::Lacking { remote: name() },
                    Err(why) => Waiting::Unreachable {
                        remote: name(),
                        message: why.message.clone(),
                    },
                };
                return Release::Wait(waiting);
            }
            held()
        }
    }
}

impl fmt::Display for Waiting {
    /// Why DeleteVolume waits, for its `UNAVAILABLE` message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncommitted { records } => write!(
                f,
                "its journal holds {records} record(s) not yet committed (the node plugin's \
                 release scan commits and pushes them)"
            ),
            Self::Lacking { remote } => write!(
                f,
                "remote {remote} does not hold the branch's head yet (the node plugin's release \
                 scan pushes it)"
            ),
            Self::ReceivedLacking { remote, refname } => write!(
                f,
                "remote {remote} does not hold {refname}, a fork received into this history \
                 (the node plugin's release scan pushes it)"
            ),
            Self::Unreachable { remote, message } => write!(
                f,
                "remote {remote} could not be listed ({message}); the node plugin's release scan \
                 keeps pushing"
            ),
            Self::Diverged { remote } => write!(
                f,
                "remote {remote}'s branch was written by another worktree (diverged); the \
                 worktree is kept until an operator resolves it (docs/runbooks/divergence.md)"
            ),
            Self::NoneValid => f.write_str(
                "every remote section of the volume's profile is invalid: nothing can be pushed \
                 until the profile is fixed",
            ),
            Self::ForkUnpublished { state } => write!(
                f,
                "the fork is {}: it is deleted once it is published or abandoned (the node \
                 plugin publishes it)",
                state.as_str()
            ),
        }
    }
}

impl fmt::Display for Kept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retained => "onDelete: retain",
            Self::LocalOnly => "local-only: no remote holds its history",
            Self::Undeclared => "no remote is declared: nothing elsewhere holds its history",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::PushFailureKind;

    fn seen(remote: &str, branch: Result<Claim, Unreachable>) -> Seen {
        Seen {
            remote: remote.into(),
            branch,
            received_lacking: None,
        }
    }

    fn worktree<'a>(on_delete: OnDelete, remotes: Declared<'a>) -> Worktree<'a> {
        Worktree {
            has_history: true,
            uncommitted: 0,
            on_delete,
            remotes,
            fork: None,
        }
    }

    fn unreachable() -> Unreachable {
        Unreachable {
            kind: PushFailureKind::Unreachable,
            message: "connection refused".into(),
        }
    }

    #[test]
    fn on_delete_parses_and_prints() {
        for (text, value) in [("retain", OnDelete::Retain), ("delete", OnDelete::Delete)] {
            assert_eq!(text.parse::<OnDelete>().unwrap(), value);
            assert_eq!(value.to_string(), text);
        }
        assert_eq!("keep".parse::<OnDelete>(), Err(BadOnDelete("keep".into())));
        assert_eq!(OnDelete::default(), OnDelete::Retain);
    }

    #[test]
    fn every_release_answer() {
        let held = [
            seen("a", Ok(Claim::UpToDate)),
            seen("b", Ok(Claim::UpToDate)),
        ];
        let decide_with = |on_delete, remotes| decide(&worktree(on_delete, remotes));
        assert_eq!(
            decide_with(OnDelete::Delete, Declared::Pushed(&held)),
            Release::Delete
        );
        assert_eq!(
            decide_with(OnDelete::Retain, Declared::Pushed(&held)),
            Release::Keep(Kept::Retained)
        );
        for on_delete in [OnDelete::Delete, OnDelete::Retain] {
            assert_eq!(
                decide_with(on_delete, Declared::LocalOnly),
                Release::Keep(Kept::LocalOnly)
            );
            assert_eq!(
                decide_with(on_delete, Declared::Undeclared),
                Release::Keep(Kept::Undeclared)
            );
            assert_eq!(
                decide_with(on_delete, Declared::NoneValid),
                Release::Wait(Waiting::NoneValid)
            );
        }
        // No history and an empty journal: nothing can be lost, and
        // nothing is asked.
        let none = Worktree {
            has_history: false,
            ..worktree(OnDelete::Delete, Declared::NoneValid)
        };
        assert_eq!(decide(&none), Release::Delete);
        let none = Worktree {
            on_delete: OnDelete::Retain,
            ..none
        };
        assert_eq!(decide(&none), Release::Keep(Kept::Retained));
    }

    #[test]
    fn retain_waits_for_the_push_like_delete() {
        for (claim, waiting) in [
            (Claim::Create, Waiting::Lacking { remote: "b".into() }),
            (Claim::FastForward, Waiting::Lacking { remote: "b".into() }),
            (Claim::Diverged, Waiting::Diverged { remote: "b".into() }),
        ] {
            let remotes = [seen("a", Ok(Claim::UpToDate)), seen("b", Ok(claim))];
            for on_delete in [OnDelete::Delete, OnDelete::Retain] {
                assert_eq!(
                    decide(&worktree(on_delete, Declared::Pushed(&remotes))),
                    Release::Wait(waiting.clone()),
                    "{claim:?}, {on_delete}"
                );
            }
        }
        let remotes = [seen("a", Err(unreachable()))];
        assert_eq!(
            decide(&worktree(OnDelete::Retain, Declared::Pushed(&remotes))),
            Release::Wait(Waiting::Unreachable {
                remote: "a".into(),
                message: "connection refused".into()
            })
        );
    }

    /// A source that received forks while local-only, then gained a
    /// remote: the remote holding the head is not enough while it lacks a
    /// received fork's ref, which only this worktree holds.
    #[test]
    fn a_received_fork_the_remote_lacks_holds_the_worktree() {
        let refname = "refs/heads/agentfs/src-1".to_owned();
        let remotes = [Seen {
            received_lacking: Some(refname.clone()),
            ..seen("origin", Ok(Claim::UpToDate))
        }];
        for on_delete in [OnDelete::Delete, OnDelete::Retain] {
            let Release::Wait(waiting) = decide(&worktree(on_delete, Declared::Pushed(&remotes)))
            else {
                panic!("must wait");
            };
            assert_eq!(
                waiting,
                Waiting::ReceivedLacking {
                    remote: "origin".into(),
                    refname: refname.clone()
                }
            );
            assert!(waiting.to_string().contains(&refname), "{waiting}");
        }
    }

    #[test]
    fn the_first_remote_that_does_not_hold_the_head_is_named() {
        let remotes = [
            seen("a", Ok(Claim::UpToDate)),
            seen("b", Err(unreachable())),
            seen("c", Ok(Claim::Diverged)),
        ];
        let Release::Wait(waiting) =
            decide(&worktree(OnDelete::Delete, Declared::Pushed(&remotes)))
        else {
            panic!("must wait");
        };
        assert!(matches!(&waiting, Waiting::Unreachable { remote, .. } if remote == "b"));
        assert!(waiting.to_string().contains("remote b"), "{waiting}");
    }

    #[test]
    fn a_forks_state_wins_over_its_remotes() {
        // A fork's remotes are not asked: publishing is its gate.
        let lacking = [seen("forks", Ok(Claim::Create))];
        let fork = |state, on_delete| Worktree {
            fork: Some(state),
            ..worktree(on_delete, Declared::Pushed(&lacking))
        };
        for state in [ForkState::Active, ForkState::Finished, ForkState::Refused] {
            assert_eq!(
                decide(&fork(state, OnDelete::Delete)),
                Release::Wait(Waiting::ForkUnpublished { state })
            );
        }
        for state in [ForkState::Published, ForkState::Abandoned] {
            assert_eq!(decide(&fork(state, OnDelete::Delete)), Release::Delete);
            assert_eq!(
                decide(&fork(state, OnDelete::Retain)),
                Release::Keep(Kept::Retained)
            );
        }
    }

    /// A journal history lacks holds deletion and retention alike, with a
    /// branch or without one, whatever the remotes hold; a fork's status
    /// is its gate instead (its finish commits the journal).
    #[test]
    fn an_uncommitted_journal_waits() {
        let held = [seen("a", Ok(Claim::UpToDate))];
        let waiting = Waiting::Uncommitted { records: 2 };
        for has_history in [true, false] {
            for on_delete in [OnDelete::Delete, OnDelete::Retain] {
                let journaled = Worktree {
                    has_history,
                    uncommitted: 2,
                    ..worktree(on_delete, Declared::Pushed(&held))
                };
                assert_eq!(
                    decide(&journaled),
                    Release::Wait(waiting.clone()),
                    "{has_history} {on_delete}"
                );
            }
        }
        assert!(waiting.to_string().contains("2 record(s)"), "{waiting}");
        // Nothing elsewhere can hold it: kept, journal and all.
        let local = Worktree {
            uncommitted: 2,
            ..worktree(OnDelete::Delete, Declared::LocalOnly)
        };
        assert_eq!(decide(&local), Release::Keep(Kept::LocalOnly));
        let fork = Worktree {
            uncommitted: 2,
            fork: Some(ForkState::Abandoned),
            ..worktree(OnDelete::Delete, Declared::Pushed(&held))
        };
        assert_eq!(decide(&fork), Release::Delete);
    }

    /// F8 S3 acceptance 2: `Declared::of` maps each mode one to one, and a
    /// local-only worktree is kept whatever `onDelete` says.
    #[test]
    fn declared_follows_the_remotes_mode() {
        let settings = |lines: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config");
            std::fs::write(&path, format!("[agentfs]\n\tvolume = v\n{lines}")).unwrap();
            Settings::from_config(&git2::Config::open(&path).unwrap()).unwrap()
        };
        let held = [seen("origin", Ok(Claim::UpToDate))];
        let pushed = settings("[remote \"origin\"]\n\turl = /o\n");
        assert_eq!(Declared::of(&pushed, &held), Declared::Pushed(&held));
        let none_valid = settings("[remote \"bad\"]\n\turl = /b\n\tagentfsMaxBlobSize = 0\n");
        assert_eq!(Declared::of(&none_valid, &[]), Declared::NoneValid);
        let local = settings("[agentfs]\n\tremotes = none\n");
        assert_eq!(Declared::of(&local, &[]), Declared::LocalOnly);
        assert_eq!(Declared::of(&settings(""), &[]), Declared::Undeclared);
        let beside = settings("[agentfs]\n\tremotes = none\n[remote \"origin\"]\n\turl = /o\n");
        assert_eq!(Declared::of(&beside, &[]), Declared::Undeclared);
        for on_delete in [OnDelete::Delete, OnDelete::Retain] {
            assert_eq!(
                decide(&worktree(on_delete, Declared::of(&local, &[]))),
                Release::Keep(Kept::LocalOnly)
            );
        }
    }
}
