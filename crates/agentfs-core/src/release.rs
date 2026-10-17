//! Releasing a worktree (F8 rule 3, design §14.2): the one decider of
//! whether DeleteVolume removes a worktree, keeps it, or waits.
//!
//! [`decide`] is pure. Its inputs are the worktree's declared remotes,
//! what each pushed remote advertises for the branch in a listing made
//! for this call (rule 3: it asks the remote, never the push record), the
//! class's `onDelete`, and, for a fork, its status. It answers:
//!
//! - [`Release::Delete`]: every declared remote holds the head, or the
//!   worktree has no history, and `onDelete` is `delete`;
//! - [`Release::Keep`]: the volume is released but the worktree stays on
//!   its node's disk: `onDelete: retain` once every declared remote holds
//!   the head, or nothing elsewhere can hold it (local-only, undeclared);
//! - [`Release::Wait`]: not yet. DeleteVolume answers `UNAVAILABLE`, the
//!   CO retries, and the node plugin's release scan pushes the worktree
//!   meanwhile, raising `PushFailed` at each failed attempt.
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

use crate::config::Settings;
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
        let _ = s;
        todo!("F8 S1: moved from agentfs-csi::params")
    }
}

impl fmt::Display for OnDelete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!("F8 S1: moved from agentfs-csi::params")
    }
}

/// What one pushed remote advertises for the branch, judged against the
/// worktree's head ([`crate::push::RemotePusher::branch_claim`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub remote: String,
    pub branch: Result<Claim, Unreachable>,
}

/// How the worktree declares its remotes, with what each pushed one
/// shows. F8 S1 builds it from today's `Settings::remotes`; F8 S3 from
/// `config::Remotes`, without changing [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declared<'a> {
    /// One entry per valid push remote, in `Settings` order.
    Pushed(&'a [Seen]),
    /// Push sections, all invalid: nothing can be pushed until the
    /// profile is fixed.
    NoneValid,
    /// `agentfs.remotes = none` (F8 S3).
    LocalOnly,
    /// No push section and no declaration.
    Undeclared,
}

impl<'a> Declared<'a> {
    /// How `settings` declares its remotes; `seen` must hold one entry per
    /// valid push remote of `settings` (ignored otherwise).
    pub fn of(settings: &Settings, seen: &'a [Seen]) -> Self {
        let _ = (settings, seen);
        todo!("F8 S1; F8 S3 reads config::Remotes")
    }
}

/// Everything [`decide`] weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree<'a> {
    /// The branch exists locally. Without it nothing can be lost, and the
    /// remotes are not asked.
    pub has_history: bool,
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
    /// `onDelete: retain`, and every declared remote holds the head.
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
    /// The remote does not hold the head ([`Claim::Create`] or
    /// [`Claim::FastForward`]).
    Lacking { remote: String },
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
    let _ = worktree;
    todo!("F8 S1")
}
