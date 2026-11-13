//! The Volume domain's contract (docs/target-architecture/domains/volume.md):
//! identity, spec, lifecycle and the release verdict. M1 builds `ensure` (slice
//! S1) and `stage`, `unstage` and the Stop drain (slice S6).

use serde::{Deserialize, Serialize};

use crate::caps::LaunchCap;
use crate::clients::ClientSet;
use crate::name::VolumeName;
use crate::pod::PodContext;
use crate::tree::TreeId;

/// A volume's identity: random (RFC 9562 version 4), assigned once by `ensure`
/// at create, never changed. Text: the hyphenated lower-case form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub uuid::Uuid);

/// The volume identity file's content (`volume` in the state area, beside
/// `format`): written once at create, read before any write at every start.
/// Every commit's `Agentfs-Volume` and `Agentfs-Volume-Id` trailers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeIdentity {
    pub name: VolumeName,
    pub id: VolumeId,
}

/// What a launcher asks for, built from its configuration. No id: `ensure`
/// assigns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSpec {
    pub name: VolumeName,
    pub start: StartPoint,
}

/// Where the root's history begins.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPoint {
    /// The only start M1 builds; a line start arrives with its slice.
    Empty,
}

/// The spec field another launcher's spec disagrees on.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecField {
    Name,
}

/// What `ensure` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// No state area was there: this call created it, with a new id.
    Created,
    /// The volume was there under the same name; nothing was written.
    Found,
}

/// `ensure`'s answer. The id exists only once `ensure` has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ensured {
    pub id: VolumeId,
    pub outcome: EnsureOutcome,
}

/// A volume's lifecycle (volume.md §2). A volume `ensure` just created is
/// `Released`: nothing is staged and nothing could be lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeState {
    Staged,
    Draining(Drain),
    Released(Disposition),
}

/// Why a volume drains.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Drain {
    /// Its launcher stops it (`unstage`).
    Stop,
}

/// What may happen to a released volume's directory.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// Kept: the only disposition until `on_release` is built.
    Keep,
}

/// Whether the remotes hold the root's head (Replication's judgement).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// No remote: settled by definition. The only settlement until M4.
    NoRemotes,
}

/// One reason the volume may not release yet, named by the event that clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// `unstage`.
    Staged,
    /// `AttachmentEnded`.
    Attached(ClientSet),
    /// A take (`TreeMoved`): these trees' queues still hold entries.
    QueuesNotEmpty(Vec<TreeId>),
}

/// The pure release judgement of facts asked at that moment (VO1, VO4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseVerdict {
    /// At least one blocker.
    Wait(Vec<Blocker>),
    Release(Disposition),
}

/// The facts the release verdict judges, each asked of its owner at judgement time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseFacts {
    pub state: VolumeState,
    pub attached: ClientSet,
    pub queued: Vec<TreeId>,
    pub settlement: Settlement,
}

/// A failed I/O, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoKind {
    NoSpace,
    /// Any other errno.
    Other(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VolumeError {
    /// A fork launcher (VO6).
    #[error("only the volume's own launcher may do this")]
    NotPermitted,
    /// `stage` at a location `ensure` never created: mount never creates.
    #[error("no volume here: create it first (`agentfs trees init --volume NAME`)")]
    NotCreated,
    #[error("another volume is here: its {0:?} differs")]
    SpecMismatch(SpecField),
    /// Another format (the trial's format 3 included), or a directory that is no
    /// state area: refused, never converted, nothing written.
    #[error("not a state area of this format; it is refused, never converted")]
    OtherFormat,
    /// Another process serves the volume.
    #[error("the volume is busy: another process holds its state area")]
    Busy,
    #[error("the volume's state could not be made durable: {0:?}")]
    NotDurable(IoKind),
}

/// Published by Volume, each once its row is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeEvent {
    VolumeStaged,
    VolumeDraining(Drain),
    VolumeReleased(Disposition),
}

/// The Volume domain (volume.md §3, M1's surface).
pub trait Volume {
    /// Idempotent, at every start. Reads `format` and the volume identity file
    /// before any write: another format answers `OtherFormat`, another name
    /// `SpecMismatch(Name)`, each with the state area unchanged. No state area
    /// (or an empty directory, or a create a crash cut short): create, with a
    /// new id. Same name: `Found`, nothing written.
    ///
    /// # Errors
    ///
    /// `NotPermitted` for a fork launcher; the refusals above; `NotDurable`.
    fn ensure(
        &mut self,
        spec: VolumeSpec,
        at: &PodContext,
        by: &LaunchCap,
    ) -> Result<Ensured, VolumeError>;

    /// Recovers the volume and serves it: `Released` or `Draining(Stop)` →
    /// `Staged` (`VolumeStaged`). `at` is the history service's pod context,
    /// carried by its own commits (Root, Sync).
    ///
    /// # Errors
    ///
    /// `NotCreated`, `OtherFormat`, `Busy`, `NotPermitted`, `NotDurable`.
    fn stage(&mut self, at: &PodContext, by: &LaunchCap) -> Result<VolumeState, VolumeError>;

    /// `Staged` → `Draining(Stop)` (`VolumeDraining`); then each consumed event
    /// re-judges, and `Released` once nothing blocks. Idempotent.
    ///
    /// # Errors
    ///
    /// `NotPermitted`, `NotDurable`.
    fn unstage(&mut self, by: &LaunchCap) -> Result<VolumeState, VolumeError>;

    /// Asks every judge now; never stored.
    fn release_verdict(&self) -> ReleaseVerdict;
}
