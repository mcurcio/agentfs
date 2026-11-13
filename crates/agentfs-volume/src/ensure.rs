//! What `ensure` does with what the state area holds (volume.md, "Ensure"):
//! one pure decision, so every launcher's start is judged alike.

use agentfs_contracts::ports::Found;
use agentfs_contracts::volume::{SpecField, VolumeError, VolumeId, VolumeSpec};

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
    match found {
        Found::Nothing => EnsureStep::Create,
        Found::Volume(identity) if identity.name == spec.name => EnsureStep::Found(identity.id),
        Found::Volume(_) => EnsureStep::Refuse(VolumeError::SpecMismatch(SpecField::Name)),
        Found::Other => EnsureStep::Refuse(VolumeError::OtherFormat),
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::name::VolumeName;
    use agentfs_contracts::volume::{StartPoint, VolumeIdentity};

    use super::*;

    fn spec(name: &str) -> VolumeSpec {
        VolumeSpec {
            name: VolumeName::parse(name).unwrap(),
            start: StartPoint::Empty,
        }
    }

    #[test]
    fn each_finding_has_its_step() {
        let id = VolumeId(uuid::Uuid::from_u128(7));
        let here = Found::Volume(VolumeIdentity {
            name: VolumeName::parse("t").unwrap(),
            id,
        });
        assert_eq!(
            decide_ensure(&Found::Nothing, &spec("t")),
            EnsureStep::Create
        );
        assert_eq!(decide_ensure(&here, &spec("t")), EnsureStep::Found(id));
        assert_eq!(
            decide_ensure(&here, &spec("u")),
            EnsureStep::Refuse(VolumeError::SpecMismatch(SpecField::Name))
        );
        assert_eq!(
            decide_ensure(&Found::Other, &spec("t")),
            EnsureStep::Refuse(VolumeError::OtherFormat)
        );
    }
}
