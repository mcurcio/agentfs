//! The Trees domain's contract (docs/target-architecture/domains/trees.md), M1's
//! surface: a fork's lifecycle (create in slice S3; keys, end and drop in S5).

use serde::{Deserialize, Serialize};

use crate::caps::LaunchCap;
use crate::name::{ForkKey, LauncherId};
use crate::ports::StoreError;
use crate::tree::{ForkId, TreeId};

/// How a fork's changes land in its upstream.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Landing {
    /// The default and the only landing M1 builds (the owner, 2026-11-12: "The
    /// default should be automated landing"); approvals arrive with their slice.
    Automatic,
}

/// A fork's settings, given at create and kept with its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForkSettings {
    pub landing: Landing,
}

/// Where a fork is in its life (trees.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForkPhase {
    Open,
    /// It will not be written again; it drops once its upstream's queue holds
    /// nothing of it and no live child names it.
    Ended,
    /// Its branch and working tree are gone; its row stays on record.
    Dropped,
}

/// What Trees knows of a fork: its row's value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForkFacts {
    pub fork: ForkId,
    pub upstream: TreeId,
    pub created_by: LauncherId,
    pub key: Option<ForkKey>,
    pub settings: ForkSettings,
    pub phase: ForkPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("no such upstream tree")]
    UpstreamUnknown,
    /// A fork as upstream: nested forks are deferred.
    #[error("forks of forks are not built")]
    NestedNotBuilt,
    /// A fork launcher ending another launcher's fork.
    #[error("not this launcher's fork")]
    NotPermitted,
    #[error("the fork has ended")]
    Ended,
    #[error("no fork under that key")]
    KeyUnknown,
    #[error("no such fork")]
    ForkUnknown,
    #[error("the fork's row could not be recorded: {0:?}")]
    Io(crate::volume::IoKind),
    #[error("the volume has stopped; reopen it")]
    Stopped,
}

impl From<StoreError> for TreeError {
    /// A write that changed nothing for want of room or I/O is `Io`; every
    /// other store failure leaves the volume unable to record. `create` reads
    /// `Refused` as `UpstreamUnknown` before this.
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotDurable(kind) => Self::Io(kind),
            StoreError::Stopped | StoreError::Damaged(_) | StoreError::Refused(_) => Self::Stopped,
        }
    }
}

/// Published by Trees, each once its row is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreesEvent {
    ForkCreated(ForkId),
    ForkEnded(ForkId),
    ForkDropped(ForkId),
}

/// Clients' one question to Trees (clients.md §3, "Calls out").
pub trait ForkLookup {
    fn facts(&self, fork: ForkId) -> Option<ForkFacts>;
}

/// The Trees domain (trees.md §5, M1's surface).
pub trait Trees: ForkLookup {
    /// One step: the fork's branch at its upstream's saved head and its row
    /// (`ForkCreated`). A second create under a key Trees holds for an open fork
    /// returns that fork and writes nothing.
    ///
    /// # Errors
    ///
    /// `UpstreamUnknown`; `NestedNotBuilt` for a fork upstream; `Ended` for a
    /// key whose fork has ended; `Io`, `Stopped`.
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        by: &LaunchCap,
    ) -> Result<ForkId, TreeError>;

    /// Ends an unkeyed fork; only its creating launcher or the volume's own.
    /// Idempotent. The fork drops once nothing holds it (`ForkDropped`).
    ///
    /// # Errors
    ///
    /// `ForkUnknown`, `NotPermitted` (another's fork, or a keyed one), `Io`,
    /// `Stopped`.
    fn end(&mut self, fork: ForkId, by: &LaunchCap) -> Result<(), TreeError>;

    /// Ends a keyed fork, for any launcher presenting the key. Idempotent.
    ///
    /// # Errors
    ///
    /// `KeyUnknown`, `Io`, `Stopped`.
    fn end_keyed(&mut self, key: &ForkKey, by: &LaunchCap) -> Result<(), TreeError>;

    /// Every fork not yet dropped.
    fn live(&self) -> Vec<ForkId>;
}
