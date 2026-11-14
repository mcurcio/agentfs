//! The ports the domains reach their adapters through (docs/target-architecture/
//! system.md, "Deployment compositions and crate layout"). agentfs-core
//! implements them over the tree actor: every write here is one journal step,
//! the volume's one commit point, and each event is a commit on a row chain
//! beside the tree it concerns (`refs/agentfs/{volume,trees/<tree>,clients/<tree>}`).
//! No database: the journal decides, and the chains are on record in git.

use crate::audit::{Audited, Principal};
use crate::caps::ClientCap;
use crate::clients::{AttachmentEndReason, ClientEvent, ClientId, Grant, SessionId};
use crate::name::{ClientLabel, ForkKey, LauncherId, VolumeName};
use crate::pod::PodContext;
use crate::tree::{ForkId, TreeId};
use crate::trees::{ForkFacts, ForkSettings, TreesEvent};
use crate::volume::{EnsureOutcome, IoKind, VolumeError, VolumeEvent, VolumeIdentity, VolumeState};

/// A store write or read that failed. A failed write changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The tree actor stopped after a recording failure; reopen to recover.
    #[error("the tree actor has stopped")]
    Stopped,
    #[error("not durable: {0:?}")]
    NotDurable(IoKind),
    /// A row or ref the journal names is damaged (operator recovery).
    #[error("damaged state: {0}")]
    Damaged(String),
    /// A branch step the git model refused (a fork drop while its queue holds
    /// its entries): a domain bug, never a workload's.
    #[error("refused by the tree actor: {0}")]
    Refused(String),
}

/// What a state area holds, read without the lock and without writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// No directory, an empty one (its lock file at most), or a create a crash
    /// cut short (its `initializing` marker).
    Nothing,
    /// A volume of this format, with its identity.
    Volume(VolumeIdentity),
    /// Another format (the trial's format 3), or a directory that is no state area.
    Other,
}

/// The volume's location (a state path) as Volume's `ensure` and `stage` see it.
pub trait StateArea {
    /// Reads `format` and the identity file only.
    ///
    /// # Errors
    ///
    /// `NotDurable` for a read that failed.
    fn inspect(&self) -> Result<Found, VolumeError>;

    /// Under the state area's lock, inspects again: still `Nothing`, writes the
    /// identity file (a new id), the repository and its Root commit (carrying
    /// `at`), the journal, then `format`, all under the `initializing` marker,
    /// and answers `Created`; a volume there by now (another launcher raced),
    /// writes nothing and answers its identity with `Found`.
    ///
    /// # Errors
    ///
    /// `Busy`, `OtherFormat`, `NotDurable`.
    fn create(
        &mut self,
        name: &VolumeName,
        at: &PodContext,
    ) -> Result<(VolumeIdentity, EnsureOutcome), VolumeError>;

    /// Takes the lock, recovers the tree actor (git-model §12) and keeps it
    /// open for the other adapters. `at` is the history service's pod context.
    ///
    /// # Errors
    ///
    /// `NotCreated`, `OtherFormat`, `Busy`, `NotDurable`.
    fn open(&mut self, at: &PodContext) -> Result<VolumeIdentity, VolumeError>;
}

/// Volume's row: `refs/agentfs/volume`.
pub trait VolumeStore {
    /// The lifecycle row, as the journal leaves it; `None` before the first stage.
    ///
    /// # Errors
    ///
    /// `Stopped`, `Damaged`.
    fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError>;

    /// One step: the row's new state and the event commit that moved it.
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`.
    fn record(
        &mut self,
        state: VolumeState,
        event: &VolumeEvent,
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError>;
}

/// Facts of the git model's queues, asked at judgement time.
pub trait QueueFacts {
    /// Every tree whose queue holds an entry.
    fn queued(&self) -> Vec<TreeId>;

    /// Whether `fork`'s upstream's queue still holds an entry `fork` put there.
    fn pending_from(&self, fork: ForkId) -> bool;

    /// Whether any fork names `fork` as its upstream.
    fn has_child(&self, fork: ForkId) -> bool;
}

/// Trees' rows: `refs/agentfs/trees/<fork>`, one chain per fork, kept after
/// its drop.
pub trait ForkStore {
    /// Every fork's row, dropped ones included.
    ///
    /// # Errors
    ///
    /// `Stopped`, `Damaged`.
    fn load(&self) -> Result<Vec<Audited<ForkFacts>>, StoreError>;

    /// One step: a fork of `upstream` at its saved head (its number is the
    /// step's), its working tree, and its first row with `ForkCreated`.
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`, `Refused` for an upstream that is no tree.
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        created_by: &LauncherId,
        at: &PodContext,
    ) -> Result<Audited<ForkFacts>, StoreError>;

    /// One step: the row's phase to `Ended`, with `ForkEnded`.
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`.
    fn end(&mut self, fork: ForkId, by: &Principal, at: &PodContext) -> Result<(), StoreError>;

    /// One step: the fork's branch, object table and queue entries removed, its
    /// working tree deleted after the step, its row to `Dropped` with
    /// `ForkDropped`. Its collision refs stay.
    ///
    /// # Errors
    ///
    /// `Refused` while its upstream's queue holds an entry of it, a write-open
    /// is live in it, or a fork names it as upstream; `Stopped`, `NotDurable`.
    fn drop_fork(
        &mut self,
        fork: ForkId,
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError>;
}

/// A client being attached, before its ordinal exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClient {
    pub launcher: LauncherId,
    pub label: ClientLabel,
    pub grant: Grant,
    pub at: PodContext,
}

/// One attached client in its tree's row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientRow {
    pub id: ClientId,
    pub grant: Grant,
    pub at: PodContext,
}

/// One tree's Clients row: `refs/agentfs/clients/<tree>`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TreeClients {
    pub tree: TreeId,
    pub attached: Vec<Audited<ClientRow>>,
    /// A fork's one writer, for life (C4): kept after its client ends, removed
    /// only at `ForkDropped`.
    pub writer_claim: Option<Audited<ClientId>>,
}

/// Clients' rows.
pub trait ClientStore {
    /// Every tree's row.
    ///
    /// # Errors
    ///
    /// `Stopped`, `Damaged`.
    fn load(&self) -> Result<Vec<TreeClients>, StoreError>;

    /// One step: the client's row on its tree's chain with `Attached`, and the
    /// fork's writer claim when `claim`. Its ordinal is the step's number.
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`.
    fn attach(
        &mut self,
        client: &NewClient,
        claim: bool,
        by: &Principal,
    ) -> Result<ClientId, StoreError>;

    /// One step: the client's row's `at` replaced (a restored client bound
    /// again after a restart), with `Rebound`. Its id and its fork's claim
    /// stay.
    ///
    /// # Errors
    ///
    /// `Refused` for a client without a row; `Stopped`, `NotDurable`.
    fn rebind(
        &mut self,
        client: &ClientId,
        at: &PodContext,
        by: &Principal,
    ) -> Result<(), StoreError>;

    /// One step: each ended client's row removed, in order, with its
    /// `AttachmentEnded`, then `LastDetached` for each of `emptied`. Nothing
    /// to end writes nothing.
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`. No room is reserved for an end: a failed one
    /// changes nothing, and the next boot's reconcile ends what it left.
    fn end(
        &mut self,
        ended: &[(ClientId, AttachmentEndReason)],
        emptied: &[TreeId],
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError>;

    /// One step: the fork's writer claim removed (at `ForkDropped`).
    ///
    /// # Errors
    ///
    /// `Stopped`, `NotDurable`.
    fn release_claim(&mut self, fork: ForkId, by: &Principal) -> Result<(), StoreError>;
}

/// What a session may do: write as a client, or read one tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAuthority {
    Write(ClientCap),
    Read(TreeId),
}

/// The history service's sessions (in-process in M1): Clients binds each
/// attached client's authority to its session, so a view's calls name no client
/// and no tree.
pub trait SessionBinder {
    fn bind(&mut self, session: SessionId, authority: SessionAuthority);
    fn unbind(&mut self, session: SessionId);
}

/// Every domain event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Volume(VolumeEvent),
    Clients(ClientEvent),
    Trees(TreesEvent),
}

/// Where a domain publishes its events once their step is durable. The
/// composition root's in-process queue: it delivers each to its consumers
/// after the call that caused it returns. Delivery is not durable: at boot
/// every consumer re-judges from asked facts (level-triggered), so a crash
/// between a step and its delivery loses no reaction.
pub trait EventSink {
    fn publish(&mut self, event: Event);
}
