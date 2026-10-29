//! Acceptance rules: whether a cleanly merged patch may land (design §12,
//! §19.2 item 11; bead agentfs-mia.2).
//!
//! The owner, 2026-10-16: "Not A, because that assumes that a clean merge
//! is an acceptable merge"; 2026-10-29: "Configurable with a sensible
//! default". An [`AcceptanceRule`] judges each patch after its merge onto
//! the head is clean and before anything is written. One rule per volume,
//! chosen by `agentfs.patchAccept` ([`AcceptMode`]); the factory in this
//! module (bead agentfs-mia.2) is the only place a mode becomes a rule:
//!
//! - [`AcceptMode::CleanMerge`] (the default): a clean merge is accepted;
//! - [`AcceptMode::Approval`]: accepted once
//!   `refs/agentfs/<volume>/approved/<n>` names the patch's tip
//!   (`agentfs patch approve`), else awaiting approval;
//! - [`AcceptMode::Command`]: the operator's `$GIT_DIR/hooks/patch-accept`,
//!   run without a shell through the delivery command runner
//!   ([`crate::fork::deliver`]), the candidate as JSON on stdin, within
//!   `agentfs.patchAcceptTimeout`: exit 0 accepts, 1 refuses (its first
//!   line of stderr is the message), 75 awaits; anything else, a signal, a
//!   timeout or a hook that cannot start is [`RuleNotWorking`].
//!
//! A rule that is not working leaves the patch not judged (set aside,
//! retried at the next run) and raises an alarm; it never accepts.

use std::path::PathBuf;
use std::time::Duration;

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use super::integrate::PatchBranch;

/// `agentfs.patchAccept`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcceptMode {
    /// `cleanMerge`, the default.
    #[default]
    CleanMerge,
    /// `approval`.
    Approval,
    /// `command`: `$GIT_DIR/hooks/patch-accept`.
    Command,
}

/// What a rule is asked about: patch `inbox` of `volume`, whose merge onto
/// `head` is clean and gave the tree `merged`.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub volume: &'a str,
    pub inbox: u64,
    pub patch: &'a PatchBranch,
    pub head: Oid,
    pub merged: Oid,
}

/// What an acceptance rule wants done with a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptance {
    /// Land it.
    Accept,
    /// Do not land it: held, as `refused`, until replaced or skipped.
    Refuse { message: String },
    /// Not yet: set aside until the rule accepts it at a later run.
    Await { what: AwaitWhat },
}

/// What an awaiting patch waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwaitWhat {
    /// `agentfs patch approve <n>` for this tip.
    Approval,
    /// The `patch-accept` hook exited 75.
    Command,
}

/// Why a rule could not judge a patch (the patch is not judged, and an
/// alarm is raised).
#[derive(Debug, thiserror::Error)]
pub enum RuleNotWorking {
    #[error("the acceptance rule {rule} could not start: {detail}")]
    NotStarted { rule: &'static str, detail: String },
    #[error("the acceptance rule {rule} failed: {detail}")]
    Crashed { rule: &'static str, detail: String },
    #[error("the acceptance rule {rule} was killed after {after:?}")]
    TimedOut { rule: &'static str, after: Duration },
    /// The approval rule could not read the volume's refs.
    #[error("the acceptance rule could not read the repository: {0}")]
    Unreadable(#[from] git2::Error),
}

/// Judges whether a cleanly merged patch may land.
///
/// Invariants: a rule never writes a ref, a file or the journal; it is
/// asked only about clean merges; an error is never an acceptance.
pub trait AcceptanceRule {
    /// The rule's name in status and alarms (`cleanMerge`, `approval`,
    /// `command`).
    fn name(&self) -> &'static str;

    /// Judges `candidate`; `repo` is the volume's history, read only.
    fn judge(
        &self,
        repo: &Repository,
        candidate: &Candidate<'_>,
    ) -> Result<Acceptance, RuleNotWorking>;
}

/// [`AcceptMode::CleanMerge`]: always [`Acceptance::Accept`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanMerge;

/// [`AcceptMode::Approval`]: accepted when
/// `refs/agentfs/<volume>/approved/<n>` names the candidate's tip.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalRule;

/// [`AcceptMode::Command`]: the `patch-accept` hook.
#[derive(Debug, Clone)]
pub struct CommandRule {
    /// `$GIT_DIR/hooks/patch-accept`.
    #[allow(dead_code)] // read by its `AcceptanceRule` impl (bead agentfs-mia.2)
    hook: PathBuf,
    /// `agentfs.patchAcceptTimeout`.
    #[allow(dead_code)] // read by its `AcceptanceRule` impl (bead agentfs-mia.2)
    timeout: Duration,
}
