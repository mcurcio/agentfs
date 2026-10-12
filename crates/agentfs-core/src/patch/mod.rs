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
//! - the first patch that cannot be applied blocks the queue
//!   ([`BlockReason`]): it and the later ones stay queued, an alarm is
//!   raised, and the volume mounts anyway;
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

pub mod fetch;
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
                "submit the missing number, or clear it with `agentfs patch submit --skip <n>`"
            }
            Self::ParentNotInHistory => {
                "the patch must be written against a commit of the volume's branch; rebase it \
                 and resubmit with --replace <n>"
            }
            Self::MergeCommit | Self::Unsupported | Self::TooLarge => {
                "replace it with a single-parent commit of at most 4096 regular files and \
                 symlinks (--replace <n>), or skip it (--skip <n>)"
            }
            Self::Unrecorded => {
                "the tree holds changes history does not have yet; let the mount record them \
                 (or finish the repair), then remount"
            }
            Self::Failed => {
                "see the message; fix the cause (an LFS object the producer did not upload, a \
                 refused path, disk space) and remount, or skip the patch (--skip <n>)"
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

/// An inbox entry the queue does not apply and that does not block it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ignored {
    /// The ref name.
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub inbox: Option<u64>,
    pub patch: Option<String>,
    /// `malformed` (not ten digits), `stale` (numbered at or below the
    /// last applied patch, with another commit than the one applied).
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
