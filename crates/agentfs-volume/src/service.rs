//! The Volume domain over its ports: `ensure` (slice S1), `stage`, `unstage`
//! and the Stop drain (slice S6).

use agentfs_contracts::caps::LaunchCap;
use agentfs_contracts::clients::AttachedLookup;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{EventSink, QueueFacts, StateArea, VolumeStore};
use agentfs_contracts::volume::{
    Ensured, ReleaseVerdict, Volume, VolumeError, VolumeSpec, VolumeState,
};

/// One volume's Volume domain. Its state area `A` is also its row store and
/// its queue facts: the volume's one tree actor, which the state area opens at
/// `stage`, holds all three (before it, the row reads `None` and no queue holds
/// anything). `C` and `A`'s queues are asked at judgement time; the composition
/// root re-judges by [`VolumeService::rejudge`] on each consumed event
/// (`AttachmentEnded`, `LastDetached`, a take). Built before `ensure`, so `C` is
/// a handle the root fills once Clients is restored at `stage`.
#[expect(
    dead_code,
    reason = "slices S1 and S6 build the bodies that read these"
)]
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
        let _ = (spec, at, by);
        todo!("S1")
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
