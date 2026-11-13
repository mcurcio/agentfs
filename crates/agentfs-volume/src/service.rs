//! The Volume domain over its ports: `ensure` (slice S1), `stage`, `unstage`
//! and the Stop drain (slice S6).

use agentfs_contracts::caps::LaunchCap;
use agentfs_contracts::clients::AttachedLookup;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{EventSink, QueueFacts, StateArea, VolumeStore};
use agentfs_contracts::volume::{
    Ensured, ReleaseVerdict, Volume, VolumeError, VolumeSpec, VolumeState,
};

/// One volume's Volume domain. `C` and `Q` are asked at judgement time; the
/// composition root re-judges by [`VolumeService::rejudge`] on each consumed
/// event (`AttachmentEnded`, `LastDetached`, a take).
#[expect(
    dead_code,
    reason = "slices S1 and S6 build the bodies that read these"
)]
pub struct VolumeService<A, S, C, Q, E> {
    area: A,
    store: S,
    clients: C,
    queues: Q,
    events: E,
}

impl<A, S, C, Q, E> VolumeService<A, S, C, Q, E>
where
    A: StateArea,
    S: VolumeStore,
    C: AttachedLookup,
    Q: QueueFacts,
    E: EventSink,
{
    pub fn new(area: A, store: S, clients: C, queues: Q, events: E) -> Self {
        Self {
            area,
            store,
            clients,
            queues,
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

impl<A, S, C, Q, E> Volume for VolumeService<A, S, C, Q, E>
where
    A: StateArea,
    S: VolumeStore,
    C: AttachedLookup,
    Q: QueueFacts,
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
