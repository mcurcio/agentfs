//! Finishing and publishing a fork (design §19.1, "Forked volumes"): the
//! finisher is the one owner of `finished` and `published`.
//!
//! [`Finisher::finish`] runs once, with the state-area lock held. It
//! commits what is journaled and records what repairs left, then writes
//! the change (one commit whose parent is base and whose tree is the
//! fork's final tree; none when that tree is base's) and the finish
//! commit: the last commit of the fork's history, the single home of
//! base, change and fork remote, after which the fork takes no changes.
//! [`Finisher::publish`] pushes the history, the change and the finish
//! commit to the fork remote, with §11.2 backoff, until the remote holds
//! the change and finish refs at exactly their ids, and only then writes
//! the published mark. [`status`] reads, without a lock or credentials,
//! what DeleteVolume asks: whether the fork is published.

use std::path::Path;
use std::time::Instant;

use git2::Oid;

use crate::events::{Events, PushFailureKind};
use crate::history::HistoryError;
use crate::identity::Context;
use crate::lock::StateLock;
use crate::telemetry::Telemetry;
use crate::volume::{StateDir, VolumeError};

use super::identity::ForkIdentity;

/// `Agentfs-Trigger` of a finish commit.
pub const FINISH_TRIGGER: &str = "finish";
/// The fork's name, on the change and the finish commit.
pub const FORK_TRAILER: &str = "Agentfs-Fork";
/// The source's name, on the change and the finish commit.
pub const SOURCE_TRAILER: &str = "Agentfs-Source";
/// Base, on the change and the finish commit.
pub const BASE_TRAILER: &str = "Agentfs-Base";
/// The change, on the finish commit; absent when nothing changed.
pub const CHANGE_TRAILER: &str = "Agentfs-Change";
/// The fork remote's name, on the finish commit.
pub const FORK_REMOTE_TRAILER: &str = "Agentfs-Fork-Remote";
/// How many paths the finish could not record, on the finish commit.
pub const UNRECORDED_TRAILER: &str = "Agentfs-Unrecorded";

/// What a finish recorded: read back from the finish commit's last
/// paragraph, which only agentfs writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishRecord {
    pub fork: String,
    pub source: String,
    pub base: Oid,
    /// The finish commit: the last commit of the fork's history.
    pub finish: Oid,
    /// The change; `None` when the fork's final tree is base's.
    pub change: Option<Oid>,
    /// The fork remote, by name, as the fork's settings named it at
    /// finish. "Published" is judged against this remote.
    pub remote: String,
    /// Paths the finish could not record, each alarmed.
    pub unrecorded: u64,
}

/// Where a fork is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkState {
    /// Taking changes. Restarts of agentfs, the node plugin or the kubelet
    /// leave a fork here.
    Active,
    /// The finish commit is written; publishing is pending.
    Finished,
    /// The fork remote holds the change and finish refs at exactly their
    /// ids, as the finisher saw after pushing.
    Published,
    /// The fork remote refuses for good; waits for the operator (forked
    /// volumes unit 2).
    Refused,
    /// The operator's recorded discard (forked volumes unit 2).
    Abandoned,
}

impl ForkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Finished => "finished",
            Self::Published => "published",
            Self::Refused => "refused",
            Self::Abandoned => "abandoned",
        }
    }

    /// Whether DeleteVolume may apply `onDelete`: only once the fork is
    /// published or abandoned.
    pub fn may_delete(self) -> bool {
        matches!(self, Self::Published | Self::Abandoned)
    }
}

/// A fork as [`status`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkStatus {
    pub identity: ForkIdentity,
    pub state: ForkState,
    /// The finish record, once the fork is finished.
    pub record: Option<FinishRecord>,
}

/// Why a publish cannot succeed without the operator (forked volumes
/// unit 2 produces these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedReason {
    /// A create-only ref on the fork remote holds another object.
    AnotherWriter,
    /// The remote's size limit refuses a single commit (§11.3).
    TooLarge,
    /// The remote refuses the refs or their content.
    Rejected,
    /// The fork's settings no longer define the remote the finish named.
    RemoteGone,
}

/// How a publish went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The fork remote holds both refs at exactly their ids; the
    /// published mark is written.
    Published,
    /// Not yet: the deadline passed while the last attempt failed in a way
    /// that is retried (§11.3).
    Pending {
        kind: PushFailureKind,
        message: String,
    },
    /// The remote refuses for good: alarmed, and not retried until the
    /// operator acts.
    Refused {
        reason: RefusedReason,
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("not built yet: forked volumes unit 1 (design §19.1)")]
    NotBuilt,
    #[error("the fork's published mark cannot be read: {0}")]
    Mark(std::io::Error),
    #[error(transparent)]
    History(#[from] HistoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum FinishError {
    #[error("not built yet: forked volumes unit 1 (design §19.1)")]
    NotBuilt,
    #[error("this volume is not a fork")]
    NotAFork,
    #[error("fork {fork} has no valid fork remote in its settings; it stays active")]
    NoForkRemote { fork: String },
    #[error("fork {fork} was abandoned; nothing agentfs runs pushes it")]
    Abandoned { fork: String },
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A fork opened for finishing and publishing.
pub struct Finisher {
    _unbuilt: (),
}

impl Finisher {
    /// Opens the fork at `state` for finishing: crash recovery as at every
    /// open. `lock` is the state-area lock this process holds on `state`.
    pub fn open(
        state: &StateDir,
        tree: &Path,
        context: Context,
        events: Events,
        telemetry: &Telemetry,
        lock: &StateLock,
    ) -> Result<Self, FinishError> {
        let _ = (state, tree, context, events, telemetry, lock);
        Err(FinishError::NotBuilt)
    }

    /// Finishes the fork once (see the module docs). A fork already
    /// finished returns its record unchanged.
    pub fn finish(&mut self) -> Result<FinishRecord, FinishError> {
        Err(FinishError::NotBuilt)
    }

    /// Publishes the finished fork: attempts with §11.2 backoff until it
    /// is published or refused, or, with a `deadline`, until then. Safe to
    /// run again; a fork already published returns at once.
    pub fn publish(&mut self, deadline: Option<Instant>) -> Result<PublishOutcome, FinishError> {
        let _ = deadline;
        Err(FinishError::NotBuilt)
    }
}

/// The fork status of the state area at `state`, read without a lock or
/// credentials: `Ok(None)` for a volume that is not a fork. What cannot be
/// read is an error, never a state, so a caller deciding whether to
/// delete waits.
pub fn status(state: &StateDir) -> Result<Option<ForkStatus>, StatusError> {
    let _ = state;
    Err(StatusError::NotBuilt)
}
