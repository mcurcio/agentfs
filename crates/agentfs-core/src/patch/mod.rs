//! The patch queue for iac (design §12).
//!
//! A producer outside the volume (iac, an operator, CI) submits a patch:
//! a single commit whose parent is a commit of the volume's branch, pushed
//! to `refs/agentfs/<volume>/inbox/<n>` on the patch remote, `<n>` being
//! ten zero-padded digits from `0000000001`. [`submit`] does that with the
//! producer's own git, numbering safely when submitters race.
//!
//! At mount, after journal replay and before the workload can see the
//! tree, [`fetch`] brings the inbox refs over (best effort, within
//! `agentfs.patchFetchTimeout`) and [`queue::Queue`] applies them in
//! order:
//!
//! - each patch is a three-way merge against its parent onto the branch
//!   head, written into the backing tree by the restore engine and
//!   recorded as one atomic operation: one commit, authored by the patch
//!   author, committed by agentfs, with `Agentfs-Trigger: patch`,
//!   `Agentfs-Patch: <oid>` and `Agentfs-Inbox: <n>`;
//! - a patch that cannot land is set aside whole with a [`Verdict`] and
//!   the queue goes on (bead agentfs-mia.2): held, waiting behind an
//!   earlier set-aside barrier or patch on the same paths, or awaiting or
//!   not judged by the acceptance rule ([`accept`]); a local
//!   `held/<n>` mark keeps it queued. Only a gap, unrecorded changes or a
//!   failure stop the queue ([`BlockReason::holds_whole`]): that patch and
//!   the later ones stay queued, an alarm is raised, and the volume mounts
//!   anyway;
//! - whether a patch is applied is decided by the `Agentfs-Inbox`
//!   trailers on the branch, so a crash anywhere never applies one twice;
//!   `applied/<n>` refs and deleting `inbox/<n>` are bookkeeping;
//! - the queue's state goes to `refs/agentfs/<volume>/status` (a commit
//!   holding `status.json`, [`status::StatusDoc`]) and is cached in
//!   `meta/patch.json` ([`status::PatchState`]) for `agentfs status` and
//!   the telemetry gauges.
//!
//! The queue's refs reach the remote with their own push
//! ([`crate::push::RemotePusher::push_once`], stage `queue`), never in
//! the branch push.

pub mod accept;
pub mod fetch;
pub mod integrate;
pub mod queue;
pub mod status;
pub mod submit;

#[cfg(test)]
mod tests;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::history::{INBOX_TRAILER, inbox_from_message};

/// The most paths one patch may change (design §12).
pub const MAX_PATCH_PATHS: usize = 4096;

/// Digits of an inbox number in a ref name.
pub const SEQ_DIGITS: usize = 10;

/// `refs/agentfs/<volume>/inbox/`.
pub fn inbox_prefix(volume: &str) -> String {
    format!("refs/agentfs/{volume}/inbox/")
}

/// `refs/agentfs/<volume>/applied/`.
pub fn applied_prefix(volume: &str) -> String {
    format!("refs/agentfs/{volume}/applied/")
}

/// Where patch `n` is queued.
pub fn inbox_ref(volume: &str, n: u64) -> String {
    format!("{}{}", inbox_prefix(volume), format_seq(n))
}

/// Where applied patch `n` is kept: the patch commit itself.
pub fn applied_ref(volume: &str, n: u64) -> String {
    format!("{}{}", applied_prefix(volume), format_seq(n))
}

/// `refs/agentfs/<volume>/held/` (bead agentfs-mia.2).
pub fn held_prefix(volume: &str) -> String {
    format!("refs/agentfs/{volume}/held/")
}

/// Where patch `n` is marked as set aside (held, waiting, awaiting or not
/// judged) by this volume's queue: the patch commit it set aside. The mark
/// keeps `inbox/<n>` from being judged stale once later numbers land; it
/// is deleted when `n` lands or its inbox ref is withdrawn (bead
/// agentfs-mia.2). Local only: never pushed.
pub fn held_ref(volume: &str, n: u64) -> String {
    format!("{}{}", held_prefix(volume), format_seq(n))
}

/// `refs/agentfs/<volume>/approved/` (bead agentfs-mia.2).
pub fn approved_prefix(volume: &str) -> String {
    format!("refs/agentfs/{volume}/approved/")
}

/// Where `agentfs patch approve <n>` records its approval on the patch
/// remote: the tip it approved (bead agentfs-mia.2).
pub fn approved_ref(volume: &str, n: u64) -> String {
    format!("{}{}", approved_prefix(volume), format_seq(n))
}

/// The trailer that makes a patch a barrier, read only from the last
/// message paragraph of the patch's tip, value `yes` (bead agentfs-mia.2).
pub const BARRIER_TRAILER: &str = "Agentfs-Barrier";

/// The queue's status ref.
pub fn status_ref(volume: &str) -> String {
    format!("refs/agentfs/{volume}/status")
}

/// `n`, ten digits, zero-padded.
pub fn format_seq(n: u64) -> String {
    format!("{n:0width$}", width = SEQ_DIGITS)
}

/// The number of a ref's last component: exactly ten ASCII digits, at
/// least 1. Anything else is not a queue entry.
pub fn parse_seq(name: &str) -> Option<u64> {
    if name.len() != SEQ_DIGITS || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().filter(|&n| n >= 1)
}

/// The inbox number of `refname` under `prefix`.
pub fn seq_of_ref(prefix: &str, refname: &str) -> Option<u64> {
    refname.strip_prefix(prefix).and_then(parse_seq)
}

/// Why the queue stopped at a patch (design §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// The three-way merge onto the branch head conflicts.
    Conflict,
    /// The next number is missing from the inbox while later ones exist.
    Gap,
    /// The patch's parent is not the branch head or one of its ancestors.
    ParentNotInHistory,
    /// The patch has more than one parent.
    MergeCommit,
    /// Not a patch agentfs applies: a root commit, a gitlink, a path
    /// agentfs cannot write or would not record.
    Unsupported,
    /// More than [`MAX_PATCH_PATHS`] paths.
    TooLarge,
    /// The tree differs from history where the patch writes (changes not
    /// recorded yet, or something untracked in the way).
    Unrecorded,
    /// Writing or recording failed, or an LFS object is missing.
    Failed,
    /// The acceptance rule refused it.
    Refused,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Gap => "gap",
            Self::ParentNotInHistory => "parent_not_in_history",
            Self::MergeCommit => "merge_commit",
            Self::Unsupported => "unsupported",
            Self::TooLarge => "too_large",
            Self::Unrecorded => "unrecorded",
            Self::Failed => "failed",
            Self::Refused => "refused",
        }
    }

    /// Whether a patch refused for this reason is held whole while the
    /// queue goes on (conflict, merge_commit, parent_not_in_history,
    /// unsupported, too_large, refused); false: the run stops at it (gap,
    /// unrecorded, failed: they are about the volume or the queue, not the
    /// patch). The owner, 2026-10-16: "Whole seems best, right? A partial
    /// application has unknown side effects".
    pub fn holds_whole(self) -> bool {
        match self {
            Self::Conflict
            | Self::MergeCommit
            | Self::ParentNotInHistory
            | Self::Unsupported
            | Self::TooLarge
            | Self::Refused => true,
            Self::Gap | Self::Unrecorded | Self::Failed => false,
        }
    }

    /// What the producer or operator can do (docs/runbooks/patch-queue.md).
    pub fn hint(self) -> &'static str {
        match self {
            Self::Conflict => {
                "rebase the patch onto the current branch head and resubmit it with \
                 `agentfs patch submit --replace <n>`"
            }
            Self::Gap => {
                "a patch number is missing: submit the missing patch with `agentfs patch submit \
                 --replace <n>`, or clear it with `agentfs patch submit --skip <n>` (both fill \
                 the gap create-only)"
            }
            Self::ParentNotInHistory => {
                "the patch must build on a commit of the volume's branch; rebase it and \
                 resubmit with --replace <n>"
            }
            Self::MergeCommit => {
                "replace it with a line of single-parent commits on the volume's branch \
                 (--replace <n>), or skip it (--skip <n>)"
            }
            Self::Unsupported | Self::TooLarge => {
                "replace it with a patch changing at most 4096 regular files and symlinks \
                 (--replace <n>), or skip it (--skip <n>)"
            }
            Self::Unrecorded => {
                "the tree differs from history at the paths named (changes not recorded yet, or \
                 something untracked in the way); record them with `agentfs record <paths>` (or \
                 finish the repair or restore in progress), then remount"
            }
            Self::Failed => {
                "see the message; fix the cause (an LFS object the producer did not upload, a \
                 refused path, disk space) and remount, or skip the patch (--skip <n>)"
            }
            Self::Refused => {
                "the acceptance rule (agentfs.patchAccept) refused it: replace it (--replace <n>) \
                 or skip it (--skip <n>)"
            }
        }
    }
}

impl fmt::Display for BlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the queue stopped (design §12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub inbox: u64,
    /// The blocked patch; `None` for a gap.
    pub patch: Option<String>,
    pub reason: BlockReason,
    pub message: String,
    /// The paths involved (conflicts, refusals), if any.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Blocked {
    /// [`BlockReason::hint`] with this patch's number in place of `<n>`;
    /// for a queue that could not run at all (inbox 0), what to do then.
    pub fn hint(&self) -> String {
        if self.inbox == 0 {
            return "the queue could not run (see the message); fix the cause and remount, or \
                    run `agentfs patch apply` with the volume stopped"
                .to_owned();
        }
        self.reason.hint().replace("<n>", &self.inbox.to_string())
    }
}

/// What the queue decided about a patch it did not land (bead
/// agentfs-mia.2): kept in `status.json` beside each queued entry. A
/// patch with a verdict is set aside and the queue goes on past it; a
/// queued entry with none was not reached (the queue stopped before it,
/// [`Blocked`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// Not landed until it is replaced or skipped, or (for `conflict`) a
    /// later run merges it cleanly. `reason` is one that
    /// `BlockReason::holds_whole` (bead agentfs-mia.2) says holds.
    Held {
        reason: BlockReason,
        message: String,
        #[serde(default)]
        paths: Vec<String>,
    },
    /// Behind earlier set-aside patches: `behind` are their numbers.
    Waiting { why: WaitReason, behind: Vec<u64> },
    /// The acceptance rule has not accepted it yet.
    Awaiting { what: accept::AwaitWhat },
    /// The acceptance rule could not judge it (an alarm is raised).
    NotJudged { rule: String, message: String },
}

impl Verdict {
    /// `held`, `waiting`, `awaiting` or `not_judged`, as in `status.json`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Held { .. } => "held",
            Self::Waiting { .. } => "waiting",
            Self::Awaiting { .. } => "awaiting",
            Self::NotJudged { .. } => "not_judged",
        }
    }
}

impl WaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameFiles => "same_files",
            Self::Barrier => "barrier",
        }
    }
}

/// Why a patch waits (bead agentfs-mia.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// It changes a path an earlier set-aside patch changes.
    SameFiles,
    /// An earlier set-aside patch is a barrier.
    Barrier,
}

/// An inbox entry the queue does not apply and that does not block it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ignored {
    /// The ref name.
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub inbox: Option<u64>,
    pub patch: Option<String>,
    /// `malformed` (not ten digits), `stale` (numbered at or below the
    /// last applied patch, with another commit than the one applied, and
    /// not set aside), `withdrawn` (a set-aside patch whose inbox ref is
    /// gone: its `held/<n>` mark is deleted).
    pub why: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    History(#[from] crate::history::HistoryError),
    #[error(transparent)]
    Volume(#[from] crate::volume::VolumeError),
    #[error(transparent)]
    Restore(#[from] crate::restore::RestoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
