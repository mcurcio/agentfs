//! The Volume domain's contract (docs/trees.md, "Volume identity and trailers",
//! "Volume lifecycle"): identity, spec, `ensure`, `stage`, `unstage`, the Stop
//! drain and the release verdict. The start line, the upgrade drain and
//! settlement by remotes are not built (docs/target-architecture/domains/volume.md).

use serde::{Deserialize, Serialize};

use crate::caps::LaunchCap;
use crate::clients::{ClientSet, FileOwner};
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
    /// The root directory's owner when `ensure` creates the volume; `None`
    /// is the creating process's effective uid and gid. Never compared and
    /// never applied to a found volume: its root's owner is content.
    pub root_owner: Option<FileOwner>,
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

/// A volume's lifecycle (docs/trees.md, "Volume lifecycle"). A volume `ensure` just created is
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

/// The pure release judgement of facts asked at that moment (docs/library/engine.md, VO1 and VO4).
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
    /// A fork launcher (docs/library/engine.md, VO6).
    #[error("only the volume's own launcher may do this")]
    NotPermitted,
    /// `stage` at a location `ensure` never created: mount never creates.
    #[error("no volume here: create it first (`agentfs init --volume NAME`)")]
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
    /// The state area could not be read: its path and the I/O failure.
    /// Nothing was written.
    #[error("the volume's state area could not be read: {0}")]
    Unreadable(String),
    /// The operator's rules hold one the volume does not build: refused
    /// before anything was written. Carries the refusal, naming the file and
    /// line.
    #[error("the operator's rules refuse the volume: {0}")]
    RulesRefused(String),
    /// The tree actor stopped after a recording failure, or its state is
    /// damaged, or the volume is not served: nothing more is recorded until it
    /// is reopened.
    #[error("the volume has stopped; reopen it")]
    Stopped,
}

impl From<crate::ports::StoreError> for VolumeError {
    /// A write that changed nothing for want of room or I/O is `NotDurable`;
    /// every other store failure leaves the volume unable to record (the
    /// lifecycle row is never refused).
    fn from(error: crate::ports::StoreError) -> Self {
        use crate::ports::StoreError;
        match error {
            StoreError::NotDurable(kind) => Self::NotDurable(kind),
            StoreError::Stopped | StoreError::Damaged(_) | StoreError::Refused(_) => Self::Stopped,
        }
    }
}

/// Published by Volume, each once its row is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeEvent {
    VolumeStaged,
    VolumeDraining(Drain),
    VolumeReleased(Disposition),
}

/// The Volume domain (docs/trees.md, "Volume lifecycle").
pub trait Volume {
    /// Idempotent, at every start. Reads `format` and the volume identity file
    /// before any write: another format answers `OtherFormat`, another name
    /// `SpecMismatch(Name)`, each with the state area unchanged. No state area
    /// (or an empty directory, or a create a crash cut short): create, with a
    /// new id. Same name: `Found`, nothing written.
    ///
    /// # Errors
    ///
    /// `NotPermitted` for a fork launcher; the refusals above; `Unreadable`,
    /// `NotDurable`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::OwnerError;
    use crate::ports::StoreError;

    #[test]
    fn an_owner_is_two_decimal_numbers() {
        let owner: FileOwner = "10000:10000".parse().unwrap();
        assert_eq!(
            owner,
            FileOwner {
                uid: 10000,
                gid: 10000
            }
        );
        assert_eq!(owner.to_string().parse::<FileOwner>(), Ok(owner));
        assert_eq!(FileOwner { uid: 0, gid: 7 }.to_string(), "0:7");
        for refused in [
            "10000",
            "a:b",
            "1:2:3",
            "-1:0",
            "+1:0",
            ":1",
            "1:",
            "4294967296:0",
        ] {
            assert_eq!(
                refused.parse::<FileOwner>(),
                Err(OwnerError(refused.to_owned())),
                "{refused}"
            );
        }
    }

    /// A store that cannot record is `Stopped`, as for Clients and Trees;
    /// a write that changed nothing keeps its kind.
    #[test]
    fn each_store_failure_has_its_volume_word() {
        assert_eq!(
            VolumeError::from(StoreError::NotDurable(IoKind::NoSpace)),
            VolumeError::NotDurable(IoKind::NoSpace)
        );
        for stopped in [
            StoreError::Stopped,
            StoreError::Damaged("x".into()),
            StoreError::Refused("x".into()),
        ] {
            assert_eq!(VolumeError::from(stopped), VolumeError::Stopped);
        }
    }
}
