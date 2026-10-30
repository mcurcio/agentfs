//! A fork's identity (design §19.1, "Forked volumes"): fixed at create,
//! kept in the fork's own state-area config beside `agentfs.volume`, and
//! never set by an include. [`crate::config::Settings`], the single
//! decider of a volume's refs and remotes, reads it; every other setting
//! of a fork is read from the source's settings files at every open.

use git2::Oid;

/// `agentfs.fork.source`: the source's volume name.
pub const SOURCE_KEY: &str = "agentfs.fork.source";
/// `agentfs.fork.base`: the full id of the commit the fork started from.
pub const BASE_KEY: &str = "agentfs.fork.base";
/// `agentfs.fork.pushTarget`: `branch` (unset) or `hidden`.
pub const PUSH_TARGET_KEY: &str = "agentfs.fork.pushTarget";
/// `agentfs.fork.link`: `linked` (unset) or `unlinked` ([`ForkLink`]).
pub const LINK_KEY: &str = "agentfs.fork.link";
/// `agentfs.fork.remotes`: an unlinked fork's own remotes, an absolute
/// path to a config file of `[remote "<name>"]` sections (design §11.5,
/// §19.1 "Unlinked fork remotes"); unset, the fork is local-only.
pub const REMOTES_KEY: &str = "agentfs.fork.remotes";
/// `agentfs.fork.sourceIsFork`: `true` when the source was itself a fork
/// at create; unset otherwise. Identity: written by create, never by an
/// include.
pub const SOURCE_IS_FORK_KEY: &str = "agentfs.fork.sourceIsFork";

/// Whether a fork delivers back to its source, fixed at create (design
/// §11.5; §19.1 "Fork link", 2026-10-17). The owner: "A fork can be
/// created with or without a remote link back to its parent.
/// Configurable."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForkLink {
    /// Its finish publishes the change to its fork remote, or into a
    /// local-only source's history through the source's state area
    /// recorded at create. A linked fork that cannot reach its source
    /// waits and alarms; it never delivers anywhere else.
    #[default]
    Linked,
    /// An independent fork: it records and pushes its own history ref
    /// where any fork's goes, but nothing is ever delivered to its source.
    /// It never finishes, has no fork status, and records no source path.
    Unlinked,
}

impl ForkLink {
    /// The value as `agentfs.fork.link`, `fork create --link` and a fork
    /// class's `link` parameter write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::Unlinked => "unlinked",
        }
    }

    /// `linked` or `unlinked`; anything else is `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "linked" => Some(Self::Linked),
            "unlinked" => Some(Self::Unlinked),
            _ => None,
        }
    }
}

/// Where a fork's history is kept and pushed, fixed at create.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PushTarget {
    /// `refs/heads/agentfs/<fork>` on the fork remote, where users' CI and
    /// pull requests can act on it.
    #[default]
    Branch,
    /// `refs/agentfs/<fork>/history`, off every branch (forked volumes
    /// unit 4).
    Hidden,
}

impl PushTarget {
    /// The ref `fork`'s history is kept and pushed at.
    pub fn history_ref(self, fork: &str) -> String {
        match self {
            Self::Branch => format!("refs/heads/agentfs/{fork}"),
            Self::Hidden => format!("refs/agentfs/{fork}/history"),
        }
    }
}

/// What a fork is. Written once by [`super::create::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkIdentity {
    /// The fork's volume name (`agentfs.volume`). It ends in a part unique
    /// at create: `{pvName}` under CSI, a random suffix from the CLI.
    pub fork: String,
    /// The source's volume name ([`SOURCE_KEY`]).
    pub source: String,
    /// The source branch's head when the fork was created, read from the
    /// source's own history, never from a remote ([`BASE_KEY`]).
    pub base: Oid,
    /// Where the fork's history goes ([`PUSH_TARGET_KEY`]).
    pub push_target: PushTarget,
    /// Whether it delivers back to its source ([`LINK_KEY`]).
    pub link: ForkLink,
}

impl ForkIdentity {
    /// The ref the fork's history is kept and pushed at.
    pub fn history_ref(&self) -> String {
        self.push_target.history_ref(&self.fork)
    }
}

/// `refs/agentfs/<fork>/change`: the change, written once and pushed
/// create-only.
pub fn change_ref(fork: &str) -> String {
    format!("refs/agentfs/{fork}/change")
}

/// `refs/agentfs/<fork>/finish`: the finish commit, pushed create-only.
pub fn finish_ref(fork: &str) -> String {
    format!("refs/agentfs/{fork}/finish")
}

/// `agentfs.start.remote`: the push remote a started volume came from
/// (F8 S1). The three start keys are identity: set together by
/// [`super::start::start_from`] in the volume's own config, never by an
/// include, and never changed.
pub const START_REMOTE_KEY: &str = "agentfs.start.remote";
/// `agentfs.start.branch`: the branch it came from, without `refs/heads/`.
pub const START_BRANCH_KEY: &str = "agentfs.start.branch";
/// `agentfs.start.commit`: the full id of the commit it was pinned at.
pub const START_COMMIT_KEY: &str = "agentfs.start.commit";

/// Where a started volume began (F8 R2: continuing elsewhere is a new
/// volume started from the old branch): a StorageClass's
/// `startFrom: "<remote>:<branch>@<commit>"`, kept in the new worktree's
/// identity. Only a branch: a `diverged/*` or any other `refs/agentfs/`
/// ref never moves, so the start-branch alarm could never watch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPoint {
    /// A push section of the new volume's own profile.
    pub remote: String,
    /// A branch name, e.g. `agentfs/V1`; never this volume's own branch.
    pub branch: String,
    /// Pinned in the class; must be the branch's tip or an ancestor of it
    /// on the remote when the volume is created.
    pub commit: Oid,
}

/// A `startFrom` value that cannot be a [`StartPoint`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadStartPoint {
    #[error("startFrom {value:?} is not <remote>:<branch>@<commit>")]
    Shape { value: String },
    #[error("startFrom remote {remote:?} is not a valid remote name")]
    BadRemote { remote: String },
    #[error(
        "startFrom names {branch:?}, which is not a branch: a start point is a branch such as \
         agentfs/<volume>, never a refs/... name (a diverged/* ref never moves, so nothing could \
         alarm on it)"
    )]
    NotABranch { branch: String },
    #[error("startFrom commit {commit:?} is not a full commit id (40 lower-case hex digits)")]
    BadCommit { commit: String },
}

impl StartPoint {
    /// `<remote>:<branch>@<commit>`, as a StorageClass writes it. The
    /// branch must be a valid git branch name not starting with `refs/`;
    /// the commit a full id.
    pub fn parse(value: &str) -> Result<Self, BadStartPoint> {
        let shape = || BadStartPoint::Shape {
            value: value.to_owned(),
        };
        // The commit has no `@` and the remote no `:`, so the last `@` and
        // the first `:` split it.
        let (rest, commit) = value.rsplit_once('@').ok_or_else(shape)?;
        let (remote, branch) = rest.split_once(':').ok_or_else(shape)?;
        if remote.is_empty() || branch.is_empty() || commit.is_empty() {
            return Err(shape());
        }
        if crate::config::validate_name(START_REMOTE_KEY, remote).is_err() {
            return Err(BadStartPoint::BadRemote {
                remote: remote.to_owned(),
            });
        }
        if branch.starts_with("refs/")
            || !git2::Reference::is_valid_name(&format!("refs/heads/{branch}"))
        {
            return Err(BadStartPoint::NotABranch {
                branch: branch.to_owned(),
            });
        }
        let commit = full_commit(commit).ok_or_else(|| BadStartPoint::BadCommit {
            commit: commit.to_owned(),
        })?;
        Ok(Self {
            remote: remote.to_owned(),
            branch: branch.to_owned(),
            commit,
        })
    }

    /// `refs/heads/<branch>`.
    pub fn refname(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }
}

impl std::fmt::Display for StartPoint {
    /// `<remote>:<branch>@<commit>`, which [`StartPoint::parse`] reads back.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}@{}", self.remote, self.branch, self.commit)
    }
}

/// `value` as a commit id, when it is a full one as agentfs writes it
/// ([`crate::config::is_full_oid`]: never a prefix, which git would pad).
/// What `startFrom` and `agentfs start accept --tip` take.
pub fn full_commit(value: &str) -> Option<Oid> {
    crate::config::is_full_oid(value)
        .then(|| Oid::from_str(value).ok())
        .flatten()
}

/// `refs/agentfs/<volume>/accepted/`: the prefix of the start-branch tips
/// an operator accepted (`agentfs start accept`, F8 S1).
pub fn accepted_prefix(volume: &str) -> String {
    format!("refs/agentfs/{volume}/accepted/")
}

/// `refs/agentfs/<volume>/accepted/<tip>`: a start-branch tip the operator
/// accepted, created once (never moved) in this volume's own namespace and
/// pushed create-only with the branch. The start-branch alarm stops
/// watching every commit it reaches, and the old commits are saved under
/// this volume's name.
pub fn accepted_ref(volume: &str, tip: Oid) -> String {
    format!("{}{tip}", accepted_prefix(volume))
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_start_point_round_trips() {
        let value = format!("origin:agentfs/ns.data.pvc-1@{C}");
        let start = StartPoint::parse(&value).unwrap();
        assert_eq!(
            start,
            StartPoint {
                remote: "origin".into(),
                branch: "agentfs/ns.data.pvc-1".into(),
                commit: Oid::from_str(C).unwrap(),
            }
        );
        assert_eq!(start.refname(), "refs/heads/agentfs/ns.data.pvc-1");
        assert_eq!(start.to_string(), value);
        assert_eq!(StartPoint::parse(&start.to_string()).unwrap(), start);
        // An `@` inside the branch: the last one splits.
        let at = StartPoint::parse(&format!("o:team@x/v@{C}")).unwrap();
        assert_eq!(at.branch, "team@x/v");
    }

    #[test]
    fn every_bad_start_point() {
        let shape = |v: &str| BadStartPoint::Shape { value: v.into() };
        for value in [
            "",
            "origin",
            "origin:agentfs/v",
            &format!("agentfs/v@{C}"),
            &format!(":agentfs/v@{C}"),
            &format!("origin:@{C}"),
            "origin:agentfs/v@",
        ] {
            assert_eq!(StartPoint::parse(value), Err(shape(value)), "{value:?}");
        }
        assert_eq!(
            StartPoint::parse(&format!("../x:agentfs/v@{C}")),
            Err(BadStartPoint::BadRemote {
                remote: "../x".into()
            })
        );
        for branch in [
            "refs/agentfs/v1/diverged/20261016T000000Z",
            "refs/heads/agentfs/v1",
            "a..b",
            "a b",
            "x.lock",
        ] {
            let err = StartPoint::parse(&format!("origin:{branch}@{C}")).unwrap_err();
            assert_eq!(
                err,
                BadStartPoint::NotABranch {
                    branch: branch.into()
                }
            );
        }
        let diverged = StartPoint::parse(&format!("origin:refs/agentfs/v/diverged/x@{C}"))
            .unwrap_err()
            .to_string();
        assert!(diverged.contains("not a branch"), "{diverged}");
        for commit in ["abc", &C.to_uppercase(), &format!("{C}0")] {
            assert_eq!(
                StartPoint::parse(&format!("origin:agentfs/v@{commit}")),
                Err(BadStartPoint::BadCommit {
                    commit: commit.into()
                })
            );
        }
    }

    #[test]
    fn accepted_refs_live_in_the_volumes_namespace() {
        let tip = Oid::from_str(C).unwrap();
        assert_eq!(accepted_prefix("v2"), "refs/agentfs/v2/accepted/");
        assert_eq!(
            accepted_ref("v2", tip),
            format!("refs/agentfs/v2/accepted/{C}")
        );
    }
}
