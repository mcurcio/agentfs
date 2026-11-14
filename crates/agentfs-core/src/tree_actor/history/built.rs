//! History as built: the one owner of a tree volume's Git store. The builder
//! makes each durable step's commits behind the call path; History hands out
//! only what is built, wakes waiters on built and failed events (never a
//! timer), and measures each tree's lag for the lag gate (docs/trees.md,
//! "History trails the files").
//!
//! Slice B2b builds the bodies; the signatures are frozen on its page.

use std::time::Duration;

use git2::Oid;

use super::super::TreeId;
use super::super::journal::durable::RecordNo;

/// Why the builder could not build a batch. Recording goes on; the batch is
/// retried at the next step appended or the next wait.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The state area is full.
    #[error("history build: no space")]
    NoSpace,
    /// A read or write of the store or the built log failed with this errno.
    #[error("history build: errno {0}")]
    Io(i32),
    /// The builder's replay of this step disagrees with its record (outcome,
    /// length or checksum): building stops with the alarm until an operator
    /// acts (docs/runbooks/recording-failures.md).
    #[error("history build disagrees with step {}", .0.get())]
    Disagrees(RecordNo),
    /// A commit the built log or a checkpoint names is not in the store.
    #[error("history build: object {0} is missing")]
    Missing(Oid),
    /// `git fast-import` exited non-zero, or was killed, with its stderr's first line.
    #[error("history build: git fast-import failed: {0}")]
    Packer(String),
}

/// Why a wait for the built point ended without reaching it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// The builder failed on a batch at or before the record waited for.
    #[error(transparent)]
    Failed(BuildError),
    /// The volume is shutting down or its actor stopped.
    #[error("the tree actor has stopped")]
    Stopped,
}

/// The builder's health, one state machine (lag over the alarm level, over
/// the limit, a failed batch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Current,
    /// Some tree's lag is over the alarm level: `tree HistoryLagging` is logged once.
    Lagging,
    /// Some tree's lag is over the limit: that tree's gated calls wait.
    Stalled,
    /// The last batch failed; the gate holds nothing while failed.
    Failed(BuildError),
}

/// One tree's lag: the builder's estimated work for its steps past the built point.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeLag {
    pub tree: TreeId,
    /// Estimated seconds: each unbuilt step's fixed cost plus its bytes over
    /// the throughput, both measured on the last batch.
    pub seconds: f64,
    pub steps: u64,
}

/// What the lag gauge reports: per tree, the oldest unbuilt step's age, health.
#[derive(Clone, Debug, PartialEq)]
pub struct Lag {
    pub trees: Vec<TreeLag>,
    /// The age of the oldest durable step not yet built: the alert's gauge.
    pub oldest_unbuilt: Option<Duration>,
    pub health: Health,
}

/// The built state at one built point: heads, refs and the commits behind
/// them. Only History constructs one, and only at or before the built point,
/// so nothing past it is reachable (the private field is the guarantee).
pub struct BuiltSnapshot {
    _private: (),
}

impl BuiltSnapshot {
    /// The built point this snapshot is at.
    pub fn at(&self) -> RecordNo {
        todo!("B2b")
    }

    /// The tree's head commit as built, or none for a tree not built yet.
    pub fn head(&self, tree: TreeId) -> Option<Oid> {
        let _ = tree;
        todo!("B2b")
    }

    /// Every ref as built: name and value, in name order.
    pub fn refs(&self) -> Vec<(String, Oid)> {
        todo!("B2b")
    }
}

/// The one owner of the Git store. Takes, push, the CLI and tests reach
/// history through it; nothing else opens the store.
pub struct History {
    _private: (),
}

impl History {
    /// The last step whose commits are in the store and in a durable
    /// built-log entry.
    pub fn built_through(&self) -> RecordNo {
        todo!("B2b")
    }

    /// Returns once the built point reaches `through`, on a built event.
    ///
    /// # Errors
    ///
    /// [`WaitError::Failed`] on a failed event before it reaches `through`,
    /// [`WaitError::Stopped`] at shutdown.
    pub fn wait_built(&self, through: RecordNo) -> Result<RecordNo, WaitError> {
        let _ = through;
        todo!("B2b")
    }

    /// The built state at the current built point.
    pub fn snapshot(&self) -> BuiltSnapshot {
        todo!("B2b")
    }

    /// Each tree's lag, the oldest unbuilt step's age and the builder's health.
    pub fn lag(&self) -> Lag {
        todo!("B2b")
    }
}
