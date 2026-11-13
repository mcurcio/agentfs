//! What `ensure` does with what the state area holds (volume.md, "Ensure"):
//! one pure decision, so every launcher's start is judged alike.

use agentfs_contracts::ports::Found;
use agentfs_contracts::volume::{VolumeError, VolumeId, VolumeSpec};

/// The step `ensure` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureStep {
    /// Nothing is there: create, with a new id.
    Create,
    /// The same name is there: answer its id, write nothing.
    Found(VolumeId),
    /// `SpecMismatch(Name)` or `OtherFormat`: answer it, write nothing.
    Refuse(VolumeError),
}

/// `Nothing` → `Create`; `Volume` with the spec's name → `Found`, another
/// name → `Refuse(SpecMismatch(Name))`; `Other` → `Refuse(OtherFormat)`.
pub fn decide_ensure(found: &Found, spec: &VolumeSpec) -> EnsureStep {
    let _ = (found, spec);
    todo!("S1")
}
