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
        let _ = value;
        todo!("F8 S1")
    }

    /// `refs/heads/<branch>`.
    pub fn refname(&self) -> String {
        todo!("F8 S1")
    }
}

impl std::fmt::Display for StartPoint {
    /// `<remote>:<branch>@<commit>`, which [`StartPoint::parse`] reads back.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f;
        todo!("F8 S1")
    }
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
