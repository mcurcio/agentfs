//! The Volume domain over its ports: `ensure` (slice S1), `stage`, `unstage`
//! and the Stop drain (slice S6; docs/trees.md, "Volume lifecycle").

use agentfs_contracts::audit::Principal;
use agentfs_contracts::caps::{LaunchCap, LaunchScope};
use agentfs_contracts::clients::AttachedLookup;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, Found, StoreError};
use agentfs_contracts::ports::{EventSink, QueueFacts, StateArea, VolumeStore};
use agentfs_contracts::volume::{
    Disposition, Drain, EnsureOutcome, Ensured, IoKind, ReleaseFacts, ReleaseVerdict, Settlement,
    Volume, VolumeError, VolumeEvent, VolumeSpec, VolumeState,
};

use crate::ensure::{EnsureStep, decide_ensure};
use crate::release::{decide, verdict};

/// `EIO`: the errno of a store that cannot record at all.
const EIO: i32 = 5;

/// One volume's Volume domain. Its state area `A` is also its row store and
/// its queue facts: the volume's one tree actor, which the state area opens at
/// `stage`, holds all three (before it, the row reads `None` and no queue holds
/// anything). `C` and `A`'s queues are asked at judgement time; the composition
/// root re-judges by [`VolumeService::rejudge`] on each consumed event
/// (`AttachmentEnded`, `LastDetached`, a take). Built before `ensure`, so `C` is
/// a handle the root fills once Clients is restored at `stage`.
pub struct VolumeService<A, C, E> {
    area: A,
    clients: C,
    events: E,
    /// The pod context `stage` was given, the history service's: the `at` of
    /// the volume's later events, whose calls carry none.
    served_at: PodContext,
}

/// The Volume domain's word for a store failure: a write that changed nothing
/// for want of room or I/O keeps its kind; a store that cannot record at all
/// is `EIO`.
fn not_durable(error: StoreError) -> VolumeError {
    match error {
        StoreError::NotDurable(kind) => VolumeError::NotDurable(kind),
        StoreError::Stopped | StoreError::Damaged(_) | StoreError::Refused(_) => {
            VolumeError::NotDurable(IoKind::Other(EIO))
        }
    }
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
            served_at: PodContext::default(),
        }
    }

    /// The state area: the composition root reaches the tree actor `stage`
    /// opened through it.
    pub fn area(&self) -> &A {
        &self.area
    }

    /// The lifecycle row's state; `Released(Keep)` before the first stage.
    fn state(&self) -> Result<VolumeState, StoreError> {
        Ok(self
            .area
            .load()?
            .map_or(VolumeState::Released(Disposition::Keep), |row| row.value))
    }

    /// Every judge asked now.
    fn facts(&self, state: VolumeState) -> ReleaseFacts {
        ReleaseFacts {
            state,
            attached: self.clients.attached_any(),
            queued: self.area.queued(),
            settlement: Settlement::NoRemotes,
        }
    }

    /// One step moving the row to `state`, then its event.
    fn record(
        &mut self,
        state: VolumeState,
        event: VolumeEvent,
        by: &Principal,
    ) -> Result<VolumeState, VolumeError> {
        self.area
            .record(state, &event, by, &self.served_at)
            .map_err(not_durable)?;
        self.events.publish(Event::Volume(event));
        Ok(state)
    }

    /// While `Draining`: judges now; with nothing blocking, records `Released`
    /// and publishes `VolumeReleased`. Otherwise does nothing. Answers the state.
    ///
    /// # Errors
    ///
    /// `NotDurable`.
    pub fn rejudge(&mut self) -> Result<VolumeState, VolumeError> {
        let state = self.state().map_err(not_durable)?;
        if !matches!(state, VolumeState::Draining(_)) {
            return Ok(state);
        }
        match decide(&self.facts(state)) {
            Ok(cleared) => {
                let disposition = cleared.disposition();
                self.record(
                    VolumeState::Released(disposition),
                    VolumeEvent::VolumeReleased(disposition),
                    &Principal::Agentfs,
                )
            }
            Err(_) => Ok(state),
        }
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

    /// A crash while staged leaves it staged: nothing is written.
    fn stage(&mut self, at: &PodContext, by: &LaunchCap) -> Result<VolumeState, VolumeError> {
        if by.may() != LaunchScope::Root {
            return Err(VolumeError::NotPermitted);
        }
        self.area.open(at)?;
        self.served_at = at.clone();
        match self.state().map_err(not_durable)? {
            VolumeState::Staged => Ok(VolumeState::Staged),
            VolumeState::Released(_) | VolumeState::Draining(_) => self.record(
                VolumeState::Staged,
                VolumeEvent::VolumeStaged,
                &Principal::Launcher(by.launcher().clone()),
            ),
        }
    }

    fn unstage(&mut self, by: &LaunchCap) -> Result<VolumeState, VolumeError> {
        if by.may() != LaunchScope::Root {
            return Err(VolumeError::NotPermitted);
        }
        match self.state().map_err(not_durable)? {
            VolumeState::Staged => {
                self.record(
                    VolumeState::Draining(Drain::Stop),
                    VolumeEvent::VolumeDraining(Drain::Stop),
                    &Principal::Launcher(by.launcher().clone()),
                )?;
                self.rejudge()
            }
            state => Ok(state),
        }
    }

    /// A row that cannot be read is judged staged: never a release on a
    /// state nobody could read.
    fn release_verdict(&self) -> ReleaseVerdict {
        let state = self.state().unwrap_or(VolumeState::Staged);
        verdict(&decide(&self.facts(state)))
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::audit::{Audited, Stamp, UnixNanos};
    use agentfs_contracts::caps::Minter;
    use agentfs_contracts::clients::{ClientId, ClientSet, Ordinal};
    use agentfs_contracts::name::{ClientLabel, LauncherId, VolumeName};
    use agentfs_contracts::tree::{ForkId, TreeId};
    use agentfs_contracts::volume::{
        Blocker, SpecField, StartPoint, VolumeEvent, VolumeId, VolumeIdentity,
    };

    use super::*;

    /// A state area in memory: what it holds, how many creates wrote, its
    /// lifecycle row once opened, every row step, and the queued trees.
    #[derive(Default)]
    struct Area {
        held: Option<VolumeIdentity>,
        other: bool,
        creates: u32,
        opened: bool,
        row: Option<VolumeState>,
        steps: Vec<(VolumeState, VolumeEvent)>,
        queued: Vec<TreeId>,
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
            self.opened = true;
            self.held.clone().ok_or(VolumeError::NotCreated)
        }
    }

    impl VolumeStore for Area {
        fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError> {
            assert!(self.opened, "the row is read only once staged");
            let stamp = Stamp {
                at: UnixNanos(0),
                by: Principal::Agentfs,
            };
            Ok(self.row.map(|value| Audited {
                value,
                created: stamp.clone(),
                changed: stamp,
            }))
        }

        fn record(
            &mut self,
            state: VolumeState,
            event: &VolumeEvent,
            _: &Principal,
            _: &PodContext,
        ) -> Result<(), StoreError> {
            assert!(self.opened, "the row is written only once staged");
            self.row = Some(state);
            self.steps.push((state, event.clone()));
            Ok(())
        }
    }

    impl QueueFacts for Area {
        fn queued(&self) -> Vec<TreeId> {
            self.queued.clone()
        }

        fn pending_from(&self, _: ForkId) -> bool {
            false
        }

        fn has_child(&self, _: ForkId) -> bool {
            false
        }
    }

    /// The clients attached, as Clients would answer.
    #[derive(Default)]
    struct Attached(ClientSet);

    impl AttachedLookup for Attached {
        fn attached_any(&self) -> ClientSet {
            self.0.clone()
        }
    }

    struct Events(Vec<Event>);

    impl EventSink for Events {
        fn publish(&mut self, event: Event) {
            self.0.push(event);
        }
    }

    fn service(area: Area) -> VolumeService<Area, Attached, Events> {
        VolumeService::new(area, Attached::default(), Events(Vec::new()))
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

    /// A created volume, staged.
    fn staged() -> VolumeService<Area, Attached, Events> {
        let mut volume = service(Area::default());
        let root = cap(LaunchScope::Root);
        volume
            .ensure(spec("t"), &PodContext::default(), &root)
            .unwrap();
        assert_eq!(
            volume.stage(&PodContext::default(), &root),
            Ok(VolumeState::Staged)
        );
        volume
    }

    fn client() -> ClientId {
        ClientId {
            launcher: LauncherId::parse("cli").unwrap(),
            tree: TreeId::Root,
            label: ClientLabel::parse("root-client").unwrap(),
            n: Ordinal(2),
        }
    }

    fn volume_events(volume: &VolumeService<Area, Attached, Events>) -> Vec<VolumeEvent> {
        volume
            .events
            .0
            .iter()
            .filter_map(|event| match event {
                Event::Volume(event) => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// `stage` moves a released volume to `Staged`; a crash while staged
    /// leaves it staged, with nothing written; a fork launcher may not.
    #[test]
    fn stage_records_once_and_a_crash_while_staged_writes_nothing() {
        let mut volume = staged();
        assert_eq!(
            volume.area.steps,
            [(VolumeState::Staged, VolumeEvent::VolumeStaged)]
        );
        assert_eq!(volume_events(&volume), [VolumeEvent::VolumeStaged]);
        let root = cap(LaunchScope::Root);
        assert_eq!(
            volume.stage(&PodContext::default(), &root),
            Ok(VolumeState::Staged)
        );
        assert_eq!(volume.area.steps.len(), 1, "nothing written");
        assert_eq!(
            volume.stage(&PodContext::default(), &cap(LaunchScope::ForksItCreates)),
            Err(VolumeError::NotPermitted)
        );
        assert_eq!(
            volume.unstage(&cap(LaunchScope::ForksItCreates)),
            Err(VolumeError::NotPermitted)
        );
        let mut nothing = service(Area::default());
        assert_eq!(
            nothing.stage(&PodContext::default(), &root),
            Err(VolumeError::NotCreated)
        );
    }

    /// VO1: never `Released` while a client is attached; the end of the last
    /// one, re-judged, releases.
    #[test]
    fn released_needs_no_client_attached() {
        let mut volume = staged();
        volume.clients.0.insert(client());
        let root = cap(LaunchScope::Root);
        assert_eq!(
            volume.unstage(&root),
            Ok(VolumeState::Draining(Drain::Stop))
        );
        assert_eq!(
            volume.release_verdict(),
            ReleaseVerdict::Wait(vec![Blocker::Attached(ClientSet::from([client()]))])
        );
        assert_eq!(volume.rejudge(), Ok(VolumeState::Draining(Drain::Stop)));
        assert!(!volume_events(&volume).contains(&VolumeEvent::VolumeReleased(Disposition::Keep)));
        volume.clients.0.clear();
        assert_eq!(
            volume.release_verdict(),
            ReleaseVerdict::Release(Disposition::Keep)
        );
        assert_eq!(
            volume.rejudge(),
            Ok(VolumeState::Released(Disposition::Keep))
        );
        assert_eq!(
            volume_events(&volume),
            [
                VolumeEvent::VolumeStaged,
                VolumeEvent::VolumeDraining(Drain::Stop),
                VolumeEvent::VolumeReleased(Disposition::Keep),
            ]
        );
        // Released: idempotent, nothing more written.
        assert_eq!(
            volume.unstage(&root),
            Ok(VolumeState::Released(Disposition::Keep))
        );
        assert_eq!(
            volume.rejudge(),
            Ok(VolumeState::Released(Disposition::Keep))
        );
        assert_eq!(volume.area.steps.len(), 3);
    }

    /// A queued entry holds the release, and a staged volume is never
    /// judged: `rejudge` writes nothing while `Staged`.
    #[test]
    fn a_queued_entry_holds_the_drain() {
        let mut volume = staged();
        volume.area.queued = vec![TreeId::Root];
        assert_eq!(volume.rejudge(), Ok(VolumeState::Staged));
        assert_eq!(
            volume.release_verdict(),
            ReleaseVerdict::Wait(vec![
                Blocker::Staged,
                Blocker::QueuesNotEmpty(vec![TreeId::Root])
            ])
        );
        let root = cap(LaunchScope::Root);
        assert_eq!(
            volume.unstage(&root),
            Ok(VolumeState::Draining(Drain::Stop))
        );
        volume.area.queued.clear();
        assert_eq!(
            volume.rejudge(),
            Ok(VolumeState::Released(Disposition::Keep))
        );
    }

    /// volume.md §5: `stage` during a Stop drain cancels it, `Staged` again
    /// with `VolumeStaged`, and nothing releases while staged.
    #[test]
    fn stage_during_a_stop_drain_cancels_it() {
        let mut volume = staged();
        volume.clients.0.insert(client());
        let root = cap(LaunchScope::Root);
        assert_eq!(
            volume.unstage(&root),
            Ok(VolumeState::Draining(Drain::Stop))
        );
        assert_eq!(
            volume.stage(&PodContext::default(), &root),
            Ok(VolumeState::Staged)
        );
        volume.clients.0.clear();
        assert_eq!(volume.rejudge(), Ok(VolumeState::Staged));
        assert_eq!(
            volume_events(&volume),
            [
                VolumeEvent::VolumeStaged,
                VolumeEvent::VolumeDraining(Drain::Stop),
                VolumeEvent::VolumeStaged,
            ]
        );
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
