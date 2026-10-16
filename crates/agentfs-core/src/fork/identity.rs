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
