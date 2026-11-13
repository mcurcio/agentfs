//! The Volume domain over its ports: `ensure` (slice S1), `stage`, `unstage`
//! and the Stop drain (slice S6).

use agentfs_contracts::caps::{LaunchCap, LaunchScope};
use agentfs_contracts::clients::AttachedLookup;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::Found;
use agentfs_contracts::ports::{EventSink, QueueFacts, StateArea, VolumeStore};
use agentfs_contracts::volume::{
    EnsureOutcome, Ensured, ReleaseVerdict, Volume, VolumeError, VolumeSpec, VolumeState,
};

use crate::ensure::{EnsureStep, decide_ensure};

/// One volume's Volume domain. Its state area `A` is also its row store and
/// its queue facts: the volume's one tree actor, which the state area opens at
/// `stage`, holds all three (before it, the row reads `None` and no queue holds
/// anything). `C` and `A`'s queues are asked at judgement time; the composition
/// root re-judges by [`VolumeService::rejudge`] on each consumed event
/// (`AttachmentEnded`, `LastDetached`, a take). Built before `ensure`, so `C` is
/// a handle the root fills once Clients is restored at `stage`.
#[expect(dead_code, reason = "slice S6 builds the bodies that read these")]
pub struct VolumeService<A, C, E> {
    area: A,
    clients: C,
    events: E,
}

impl<A, C, E> VolumeService<A, C, E>
where
    A: StateArea + VolumeStore + QueueFacts,
    C: AttachedLookup,
    E: EventSink,
{
    pub fn new(area: A, clients: C, events: E) -> Self {
        Self {
            area,
            clients,
            events,
        }
    }

    /// While `Draining`: judges now; with nothing blocking, records `Released`
    /// and publishes `VolumeReleased`. Otherwise does nothing. Answers the state.
    ///
    /// # Errors
    ///
    /// `NotDurable`.
    pub fn rejudge(&mut self) -> Result<VolumeState, VolumeError> {
        todo!("S6")
    }
}

impl<A, C, E> Volume for VolumeService<A, C, E>
where
    A: StateArea + VolumeStore + QueueFacts,
    C: AttachedLookup,
    E: EventSink,
{
    fn ensure(
        &mut self,
        spec: VolumeSpec,
        at: &PodContext,
        by: &LaunchCap,
    ) -> Result<Ensured, VolumeError> {
        if by.may() != LaunchScope::Root {
            return Err(VolumeError::NotPermitted);
        }
        match decide_ensure(&self.area.inspect()?, &spec) {
            EnsureStep::Found(id) => Ok(Ensured {
                id,
                outcome: EnsureOutcome::Found,
            }),
            EnsureStep::Refuse(refused) => Err(refused),
            EnsureStep::Create => {
                let (identity, outcome) = self.area.create(&spec.name, at)?;
                // Another launcher may have created it since `inspect`: judge
                // what it made by the same rule.
                match outcome {
                    EnsureOutcome::Created => Ok(Ensured {
                        id: identity.id,
                        outcome,
                    }),
                    EnsureOutcome::Found => match decide_ensure(&Found::Volume(identity), &spec) {
                        EnsureStep::Found(id) => Ok(Ensured { id, outcome }),
                        EnsureStep::Refuse(refused) => Err(refused),
                        EnsureStep::Create => Err(VolumeError::OtherFormat),
                    },
                }
            }
        }
    }

    fn stage(&mut self, at: &PodContext, by: &LaunchCap) -> Result<VolumeState, VolumeError> {
        let _ = (at, by);
        todo!("S6")
    }

    fn unstage(&mut self, by: &LaunchCap) -> Result<VolumeState, VolumeError> {
        let _ = by;
        todo!("S6")
    }

    fn release_verdict(&self) -> ReleaseVerdict {
        todo!("S6")
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::audit::{Audited, Principal};
    use agentfs_contracts::caps::Minter;
    use agentfs_contracts::clients::ClientSet;
    use agentfs_contracts::name::{LauncherId, VolumeName};
    use agentfs_contracts::ports::{Event, StoreError};
    use agentfs_contracts::tree::{ForkId, TreeId};
    use agentfs_contracts::volume::{SpecField, StartPoint, VolumeEvent, VolumeId, VolumeIdentity};

    use super::*;

    /// A state area in memory: what it holds, and how many creates wrote.
    #[derive(Default)]
    struct Area {
        held: Option<VolumeIdentity>,
        other: bool,
        creates: u32,
    }

    impl StateArea for Area {
        fn inspect(&self) -> Result<Found, VolumeError> {
            Ok(match (&self.held, self.other) {
                (_, true) => Found::Other,
                (Some(identity), false) => Found::Volume(identity.clone()),
                (None, false) => Found::Nothing,
            })
        }

        fn create(
            &mut self,
            name: &VolumeName,
            _: &PodContext,
        ) -> Result<(VolumeIdentity, EnsureOutcome), VolumeError> {
            if let Some(identity) = &self.held {
                return Ok((identity.clone(), EnsureOutcome::Found));
            }
            self.creates += 1;
            let identity = VolumeIdentity {
                name: name.clone(),
                id: VolumeId(uuid::Uuid::from_u128(u128::from(self.creates))),
            };
            self.held = Some(identity.clone());
            Ok((identity, EnsureOutcome::Created))
        }

        fn open(&mut self, _: &PodContext) -> Result<VolumeIdentity, VolumeError> {
            unreachable!("ensure never opens")
        }
    }

    impl VolumeStore for Area {
        fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError> {
            unreachable!("ensure reads no row")
        }

        fn record(
            &mut self,
            _: VolumeState,
            _: &VolumeEvent,
            _: &Principal,
            _: &PodContext,
        ) -> Result<(), StoreError> {
            unreachable!("ensure writes no row")
        }
    }

    impl QueueFacts for Area {
        fn queued(&self) -> Vec<TreeId> {
            Vec::new()
        }

        fn pending_from(&self, _: ForkId) -> bool {
            false
        }

        fn has_child(&self, _: ForkId) -> bool {
            false
        }
    }

    struct NoClients;

    impl AttachedLookup for NoClients {
        fn attached_any(&self) -> ClientSet {
            ClientSet::new()
        }
    }

    struct Events(Vec<Event>);

    impl EventSink for Events {
        fn publish(&mut self, event: Event) {
            self.0.push(event);
        }
    }

    fn service(area: Area) -> VolumeService<Area, NoClients, Events> {
        VolumeService::new(area, NoClients, Events(Vec::new()))
    }

    fn spec(name: &str) -> VolumeSpec {
        VolumeSpec {
            name: VolumeName::parse(name).unwrap(),
            start: StartPoint::Empty,
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a test is its own composition root"
    )]
    fn cap(may: LaunchScope) -> LaunchCap {
        LaunchCap::mint(&Minter::new(), LauncherId::parse("cli").unwrap(), may)
    }

    #[test]
    fn ensure_three_times_gives_one_id() {
        let mut volume = service(Area::default());
        let root = cap(LaunchScope::Root);
        let at = PodContext::default();
        let first = volume.ensure(spec("t"), &at, &root).unwrap();
        assert_eq!(first.outcome, EnsureOutcome::Created);
        for _ in 0..2 {
            let again = volume.ensure(spec("t"), &at, &root).unwrap();
            assert_eq!(again.id, first.id);
            assert_eq!(again.outcome, EnsureOutcome::Found);
        }
        assert_eq!(volume.area.creates, 1);
    }

    #[test]
    fn another_name_or_format_is_refused_without_a_create() {
        let mut volume = service(Area::default());
        let root = cap(LaunchScope::Root);
        let at = PodContext::default();
        volume.ensure(spec("t"), &at, &root).unwrap();
        assert_eq!(
            volume.ensure(spec("u"), &at, &root),
            Err(VolumeError::SpecMismatch(SpecField::Name))
        );
        let mut other = service(Area {
            other: true,
            ..Area::default()
        });
        assert_eq!(
            other.ensure(spec("t"), &at, &root),
            Err(VolumeError::OtherFormat)
        );
        assert_eq!(other.area.creates, 0);
    }

    #[test]
    fn a_fork_launcher_cannot_ensure() {
        let mut volume = service(Area::default());
        assert_eq!(
            volume.ensure(
                spec("t"),
                &PodContext::default(),
                &cap(LaunchScope::ForksItCreates)
            ),
            Err(VolumeError::NotPermitted)
        );
        assert_eq!(volume.area.creates, 0);
    }
}
