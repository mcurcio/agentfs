//! A local-only source receiving its fork's change (F8 S3; R4: a
//! no-remote source's fork publishes `agentfs/<fork>` into its own
//! history, never applied).
//!
//! When the source declares no remote and no section is flagged
//! `agentfsForkRemote` (`config::ForkRemote::SourceHistory`), the fork's
//! finisher publishes into the source's repository instead of a remote:
//! [`receive_fork_change`] copies the fork's objects and writes
//! `refs/heads/agentfs/<fork>` (by [`crate::push::claim`]: created, or
//! fast-forwarded from a value on the fork's history), then the change
//! and finish refs, create-only. It never touches the source's own
//! branch, its patch queue or its tree: whatever reads the source's
//! history does the gating (principle 9). The fork and the source are on
//! one node (forks are placed on their source's node). The CLI and the
//! node plugin run it through the source's control socket when its
//! service is up (design §2.3), else directly under the source's state
//! lock.

use std::io;

use git2::Oid;

use crate::history::HistoryError;
use crate::volume::StateDir;

use super::identity::ForkIdentity;

#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    /// The source's state area is gone (removed by hand): the fork can be
    /// abandoned (`agentfs fork abandon`).
    #[error("the source volume's state area is gone")]
    SourceRemoved,
    /// The fork has no finish commit yet.
    #[error("the fork is not finished: nothing to receive")]
    NotPublished,
    /// The source holds `refname` at a commit that is not on the fork's
    /// history ([`crate::push::Claim::Diverged`]): nothing was written.
    #[error("{refname} in the source's history is another writer's; nothing was written")]
    Diverged { refname: String },
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Writes `fork`'s branch, change and finish refs from the fork's state
/// area `from` into the source's state area `source` (see the module
/// docs). Returns the change commit. Idempotent: a retry after a crash
/// finds the refs at the same ids and succeeds.
pub fn receive_fork_change(
    source: &StateDir,
    fork: &ForkIdentity,
    from: &StateDir,
) -> Result<Oid, ReceiveError> {
    let _ = (source, fork, from);
    todo!("F8 S3")
}
