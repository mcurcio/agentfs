//! The Trees domain over its ports: create and the reads (slice S3); keys, end,
//! end by key and the settle that ends and drops forks (slice S5)
//! (docs/trees.md, "Forks: keys, end and the drop").

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use agentfs_contracts::audit::Principal;
use agentfs_contracts::caps::{LaunchCap, LaunchScope};
use agentfs_contracts::clients::AttachedLookup;
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink, ForkStore, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{
    ForkFacts, ForkLookup, ForkPhase, ForkSettings, Landing, TreeError, Trees, TreesEvent,
};

/// One volume's Trees domain: every fork's row, loaded at boot.
pub struct TreesService<S, E> {
    store: S,
    events: E,
    /// Every fork's row value, dropped ones included.
    forks: BTreeMap<ForkId, ForkFacts>,
}

impl<S, E> TreesService<S, E>
where
    S: ForkStore,
    E: EventSink,
{
    /// Loads every fork's row.
    ///
    /// # Errors
    ///
    /// The store's load.
    pub fn restore(store: S, events: E) -> Result<Self, StoreError> {
        let forks = store
            .load()?
            .into_iter()
            .map(|row| (row.value.fork, row.value))
            .collect();
        Ok(Self {
            store,
            events,
            forks,
        })
    }

    /// The one judgement of a fork's end and drop, from facts asked now
    /// (level-triggered, idempotent): every open unkeyed fork with no client
    /// attached (asked of `clients`) ends, by `agentfs` (`ForkEnded`); then
    /// every ended fork the store may drop drops (`ForkDropped`). Called at
    /// boot after Clients' reconcile, at each `LastDetached` and after each
    /// take, so an end or a drop a crash or a failed step left undone is
    /// judged again. A fork is attached in the run that creates it before
    /// the next settle. Answers the forks it dropped.
    ///
    /// # Errors
    ///
    /// The store's writes.
    pub fn settle(&mut self, clients: &impl AttachedLookup) -> Result<Vec<ForkId>, StoreError> {
        let attached = clients.attached_any();
        let unattached: Vec<ForkId> = self
            .forks
            .values()
            .filter(|facts| facts.phase == ForkPhase::Open && facts.key.is_none())
            .map(|facts| facts.fork)
            .filter(|fork| !attached.iter().any(|c| c.tree == TreeId::from(*fork)))
            .collect();
        for fork in unattached {
            self.end_open(fork, &Principal::Agentfs, None)?;
        }
        self.drop_ended()
    }

    /// Asks the store to drop every ended fork: the store is the one judge of
    /// whether anything holds it (`Refused`: the next settle asks again).
    fn drop_ended(&mut self) -> Result<Vec<ForkId>, StoreError> {
        let ended: Vec<ForkId> = self
            .forks
            .values()
            .filter(|facts| facts.phase == ForkPhase::Ended)
            .map(|facts| facts.fork)
            .collect();
        let mut dropped = Vec::new();
        for fork in ended {
            match self.store.drop_fork(fork) {
                Ok(()) => {}
                Err(StoreError::Refused(_)) => continue,
                Err(e) => return Err(e),
            }
            if let Some(facts) = self.forks.get_mut(&fork) {
                facts.phase = ForkPhase::Dropped;
            }
            self.events
                .publish(Event::Trees(TreesEvent::ForkDropped(fork)));
            dropped.push(fork);
        }
        Ok(dropped)
    }

    /// An open fork's end: its row's `Ended`, then `ForkEnded`. `at`: the
    /// launcher's pod context; `None` for agentfs's own end.
    fn end_open(
        &mut self,
        fork: ForkId,
        by: &Principal,
        at: Option<&PodContext>,
    ) -> Result<(), StoreError> {
        self.store.end(fork, by, at)?;
        if let Some(facts) = self.forks.get_mut(&fork) {
            facts.phase = ForkPhase::Ended;
        }
        self.events
            .publish(Event::Trees(TreesEvent::ForkEnded(fork)));
        Ok(())
    }

    /// `end` and `end_keyed` once the caller may end `fork`: an open fork
    /// ends and the drops follow; an ended or dropped one answers `Ok`.
    fn end_permitted(
        &mut self,
        fork: ForkId,
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<(), TreeError> {
        let phase = self.forks.get(&fork).ok_or(TreeError::ForkUnknown)?.phase;
        if phase == ForkPhase::Open {
            self.end_open(fork, &Principal::Launcher(by.launcher().clone()), Some(at))?;
        }
        self.drop_ended()?;
        Ok(())
    }

    /// The fork held under `key`, whatever its phase: how a launcher that
    /// names forks by key finds one's tree, never creating it.
    pub fn keyed(&self, key: &ForkKey) -> Option<&ForkFacts> {
        self.forks
            .values()
            .find(|facts| facts.key.as_ref() == Some(key))
    }
}

impl<S, E> ForkLookup for TreesService<S, E>
where
    S: ForkStore,
    E: EventSink,
{
    fn facts(&self, fork: ForkId) -> Option<ForkFacts> {
        self.forks.get(&fork).cloned()
    }
}

impl<S, E> Trees for TreesService<S, E>
where
    S: ForkStore,
    E: EventSink,
{
    /// A create is one store step: the branch and its row with
    /// `ForkCreated`, published once the step is durable. Under a key Trees
    /// holds, the open fork is answered and nothing is written. Approval
    /// landing is refused before anything else is asked or written.
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<ForkId, TreeError> {
        if settings.landing == Landing::Approval {
            return Err(TreeError::ApprovalNotBuilt);
        }
        if let Some(facts) = key.as_ref().and_then(|key| self.keyed(key)) {
            return match facts.phase {
                ForkPhase::Open => Ok(facts.fork),
                ForkPhase::Ended | ForkPhase::Dropped => Err(TreeError::Ended),
            };
        }
        if !upstream.is_root() {
            return Err(TreeError::NestedNotBuilt);
        }
        let row = self
            .store
            .create(upstream, key, settings, by.launcher(), at)
            .map_err(|e| match e {
                StoreError::Refused(_) => TreeError::UpstreamUnknown,
                other => other.into(),
            })?;
        let fork = row.value.fork;
        self.forks.insert(fork, row.value);
        self.events
            .publish(Event::Trees(TreesEvent::ForkCreated(fork)));
        Ok(fork)
    }

    fn end(&mut self, fork: ForkId, by: &LaunchCap, at: &PodContext) -> Result<(), TreeError> {
        let facts = self.forks.get(&fork).ok_or(TreeError::ForkUnknown)?;
        let its_launcher = by.may() == LaunchScope::Root || facts.created_by == *by.launcher();
        if !its_launcher || facts.key.is_some() {
            return Err(TreeError::NotPermitted);
        }
        self.end_permitted(fork, by, at)
    }

    fn end_keyed(
        &mut self,
        key: &ForkKey,
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<(), TreeError> {
        let fork = self.keyed(key).ok_or(TreeError::KeyUnknown)?.fork;
        self.end_permitted(fork, by, at)
    }

    fn live(&self) -> Vec<ForkId> {
        self.forks
            .values()
            .filter(|facts| facts.phase != ForkPhase::Dropped)
            .map(|facts| facts.fork)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use agentfs_contracts::audit::{Audited, Stamp, UnixNanos};
    use agentfs_contracts::caps::Minter;
    use agentfs_contracts::clients::{ClientId, ClientSet, Ordinal};
    use agentfs_contracts::name::{ClientLabel, LauncherId};
    use agentfs_contracts::volume::IoKind;

    use super::*;

    /// Forks in memory, numbered as the actor numbers them: by step. Each
    /// write is counted; a fork in `held` (a queued landing, a child, an open
    /// file: the store is the one judge) refuses its drop. Each end's `at`
    /// is kept.
    #[derive(Default)]
    struct Store {
        rows: Vec<Audited<ForkFacts>>,
        step: u64,
        writes: u64,
        held: BTreeSet<ForkId>,
        fail: Option<StoreError>,
        ends_at: Vec<Option<PodContext>>,
    }

    impl Store {
        fn phase(&mut self, fork: ForkId, phase: ForkPhase) -> Result<(), StoreError> {
            if let Some(e) = self.fail.clone() {
                return Err(e);
            }
            let row = self
                .rows
                .iter_mut()
                .find(|row| row.value.fork == fork)
                .ok_or_else(|| StoreError::Refused("no such fork".into()))?;
            row.value.phase = phase;
            self.writes += 1;
            Ok(())
        }
    }

    impl ForkStore for Store {
        fn load(&self) -> Result<Vec<Audited<ForkFacts>>, StoreError> {
            Ok(self.rows.clone())
        }

        fn create(
            &mut self,
            upstream: TreeId,
            key: Option<ForkKey>,
            settings: ForkSettings,
            created_by: &LauncherId,
            _: &PodContext,
        ) -> Result<Audited<ForkFacts>, StoreError> {
            if upstream != TreeId::Root {
                return Err(StoreError::Refused("no such tree".into()));
            }
            self.step += 1;
            self.writes += 1;
            let stamp = Stamp {
                at: UnixNanos(self.step),
                by: Principal::Launcher(created_by.clone()),
            };
            let row = Audited {
                value: ForkFacts {
                    fork: ForkId(self.step),
                    upstream,
                    created_by: created_by.clone(),
                    key,
                    settings,
                    phase: ForkPhase::Open,
                },
                created: stamp.clone(),
                changed: stamp,
            };
            self.rows.push(row.clone());
            Ok(row)
        }

        fn end(
            &mut self,
            fork: ForkId,
            _: &Principal,
            at: Option<&PodContext>,
        ) -> Result<(), StoreError> {
            self.phase(fork, ForkPhase::Ended)?;
            self.ends_at.push(at.cloned());
            Ok(())
        }

        fn drop_fork(&mut self, fork: ForkId) -> Result<(), StoreError> {
            if self.held.contains(&fork) {
                return Err(StoreError::Refused("busy".into()));
            }
            self.phase(fork, ForkPhase::Dropped)
        }
    }

    /// Clients as a test sets them: the attached clients' trees.
    #[derive(Default)]
    struct Attached(Vec<TreeId>);

    impl AttachedLookup for Attached {
        fn attached_any(&self) -> ClientSet {
            self.0
                .iter()
                .enumerate()
                .map(|(n, tree)| ClientId {
                    launcher: LauncherId::parse("cli").unwrap(),
                    tree: *tree,
                    label: ClientLabel::parse("fork-client").unwrap(),
                    n: Ordinal(n as u64 + 1),
                })
                .collect()
        }
    }

    fn pod() -> PodContext {
        PodContext {
            pod: Some(agentfs_contracts::name::Label::parse("p").unwrap()),
            ..PodContext::default()
        }
    }

    #[derive(Default)]
    struct Events(Vec<Event>);

    impl EventSink for Events {
        fn publish(&mut self, event: Event) {
            self.0.push(event);
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a test is its own composition root"
    )]
    fn cap_of(launcher: &str, may: LaunchScope) -> LaunchCap {
        LaunchCap::mint(&Minter::new(), LauncherId::parse(launcher).unwrap(), may)
    }

    fn cap() -> LaunchCap {
        cap_of("cli", LaunchScope::Root)
    }

    fn key(text: &str) -> ForkKey {
        ForkKey::parse(text).unwrap()
    }

    const SETTINGS: ForkSettings = ForkSettings {
        landing: Landing::Automatic,
    };

    type Service = TreesService<Store, Events>;

    fn trees() -> Service {
        TreesService::restore(Store::default(), Events::default()).unwrap()
    }

    fn trees_event(event: TreesEvent) -> Event {
        Event::Trees(event)
    }

    #[test]
    fn create_twice_unkeyed_makes_two_forks() {
        let mut trees = trees();
        let one = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        let two = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        assert_ne!(one, two);
        assert_eq!(trees.live(), [one, two]);
        let facts = trees.facts(one).unwrap();
        assert_eq!(facts.upstream, TreeId::Root);
        assert_eq!(facts.created_by, LauncherId::parse("cli").unwrap());
        assert_eq!(facts.phase, ForkPhase::Open);
        assert_eq!(
            trees.events.0,
            [
                trees_event(TreesEvent::ForkCreated(one)),
                trees_event(TreesEvent::ForkCreated(two)),
            ]
        );
        // A restore reads the same rows back.
        let restored = TreesService::restore(trees.store, Events::default()).unwrap();
        assert_eq!(restored.live(), [one, two]);
        assert_eq!(restored.facts(two).unwrap().fork, two);
    }

    /// A second create under a key returns its open fork and writes
    /// nothing; once the fork has ended, the key answers `Ended`.
    #[test]
    fn a_second_create_under_a_key_returns_its_fork() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap(), &pod())
            .unwrap();
        let writes = trees.store.writes;
        assert_eq!(
            trees.create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap(), &pod()),
            Ok(fork)
        );
        assert_eq!(trees.store.writes, writes, "nothing written");
        assert_eq!(trees.events.0.len(), 1, "one ForkCreated");
        let other = trees
            .create(TreeId::Root, Some(key("pvc-2")), SETTINGS, &cap(), &pod())
            .unwrap();
        assert_ne!(other, fork);
        assert_eq!(trees.facts(fork).unwrap().key, Some(key("pvc-1")));
        trees.end_keyed(&key("pvc-1"), &cap(), &pod()).unwrap();
        assert_eq!(
            trees.create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap(), &pod()),
            Err(TreeError::Ended)
        );
        // A restore knows the key.
        let restored = TreesService::restore(trees.store, Events::default());
        let mut restored = restored.unwrap();
        assert_eq!(
            restored.create(TreeId::Root, Some(key("pvc-2")), SETTINGS, &cap(), &pod()),
            Ok(other)
        );
    }

    /// A fork that needs approval is refused before anything is written,
    /// under a key or not, and takes no fork number: the next create gets the
    /// number it would have had.
    #[test]
    fn approval_is_refused_and_writes_nothing() {
        let approval = ForkSettings {
            landing: Landing::Approval,
        };
        let mut trees = trees();
        let one = trees
            .create(TreeId::Root, Some(key("k1")), SETTINGS, &cap(), &pod())
            .unwrap();
        let writes = trees.store.writes;
        for key in [None, Some(key("k1")), Some(key("k3"))] {
            assert_eq!(
                trees.create(TreeId::Root, key, approval, &cap(), &pod()),
                Err(TreeError::ApprovalNotBuilt)
            );
        }
        assert_eq!(trees.store.writes, writes, "nothing written");
        assert_eq!(trees.events.0.len(), 1, "one ForkCreated");
        let two = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        assert_eq!(two, ForkId(one.0 + 1), "no fork number consumed");
    }

    #[test]
    fn nested_is_refused() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        assert_eq!(
            trees.create(fork.into(), None, SETTINGS, &cap(), &pod()),
            Err(TreeError::NestedNotBuilt)
        );
        assert_eq!(
            trees.create(TreeId::Fork(99), None, SETTINGS, &cap(), &pod()),
            Err(TreeError::NestedNotBuilt)
        );
        assert_eq!(trees.live(), [fork]);
        assert_eq!(
            trees.events.0.len(),
            1,
            "a refused create publishes nothing"
        );
    }

    /// An ended fork drops only once the store lets it (nothing holds it);
    /// the drop is published once.
    #[test]
    fn an_ended_fork_drops_once_nothing_holds_it() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        trees.store.held.insert(fork);
        trees.end(fork, &cap(), &pod()).unwrap();
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Ended);
        assert_eq!(trees.store.ends_at, [Some(pod())], "the launcher's pod");
        assert_eq!(trees.settle(&Attached::default()), Ok(vec![]), "held");
        trees.store.held.clear();
        assert_eq!(trees.settle(&Attached::default()), Ok(vec![fork]));
        assert_eq!(
            trees.settle(&Attached::default()),
            Ok(vec![]),
            "dropped once"
        );
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Dropped);
        assert!(trees.live().is_empty());
        assert_eq!(
            trees.events.0,
            [
                trees_event(TreesEvent::ForkCreated(fork)),
                trees_event(TreesEvent::ForkEnded(fork)),
                trees_event(TreesEvent::ForkDropped(fork)),
            ]
        );
        // Ending it again answers `Ok` and writes nothing.
        let writes = trees.store.writes;
        trees.end(fork, &cap(), &pod()).unwrap();
        assert_eq!(trees.store.writes, writes);
    }

    /// A key whose fork has ended but not dropped yet (its landing still
    /// queued) answers `Ended`, and nothing is written: no writer comes back
    /// on a fork that is ending.
    #[test]
    fn a_create_under_a_key_whose_fork_is_ending_answers_ended() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, Some(key("k")), SETTINGS, &cap(), &pod())
            .unwrap();
        trees.store.held.insert(fork);
        trees.end_keyed(&key("k"), &cap(), &pod()).unwrap();
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Ended);
        let (writes, events) = (trees.store.writes, trees.events.0.len());
        assert_eq!(
            trees.create(TreeId::Root, Some(key("k")), SETTINGS, &cap(), &pod()),
            Err(TreeError::Ended)
        );
        assert_eq!(trees.store.writes, writes, "nothing written");
        assert_eq!(trees.events.0.len(), events, "nothing published");
    }

    /// `end`: only the volume's launcher or the fork's creator, and never a
    /// keyed fork; `end_keyed`: any launcher with the key.
    #[test]
    fn who_may_end_a_fork() {
        let mut trees = trees();
        let forks_cap = cap_of("csi", LaunchScope::ForksItCreates);
        let other_cap = cap_of("other", LaunchScope::ForksItCreates);
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &forks_cap, &pod())
            .unwrap();
        let keyed = trees
            .create(TreeId::Root, Some(key("k")), SETTINGS, &forks_cap, &pod())
            .unwrap();
        assert_eq!(
            trees.end(ForkId(99), &cap(), &pod()),
            Err(TreeError::ForkUnknown)
        );
        assert_eq!(
            trees.end(fork, &other_cap, &pod()),
            Err(TreeError::NotPermitted)
        );
        assert_eq!(
            trees.end(keyed, &cap(), &pod()),
            Err(TreeError::NotPermitted)
        );
        assert_eq!(
            trees.end_keyed(&key("nope"), &cap(), &pod()),
            Err(TreeError::KeyUnknown)
        );
        trees.end(fork, &forks_cap, &pod()).unwrap();
        trees.end_keyed(&key("k"), &other_cap, &pod()).unwrap();
        assert_eq!(
            [fork, keyed].map(|f| trees.facts(f).unwrap().phase),
            [ForkPhase::Dropped; 2]
        );
        let mut trees = self::trees();
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &forks_cap, &pod())
            .unwrap();
        trees.end(fork, &cap(), &pod()).unwrap();
    }

    /// Settle ends an open unkeyed fork no client is attached to, as
    /// `agentfs` with no pod context of a caller, and drops it; a keyed fork
    /// and an attached one stay open. Asked, never told: a fork whose
    /// `LastDetached` was lost (a crash between the Clients step and the end,
    /// or a create whose attach never came) ends at the next settle.
    #[test]
    fn settle_ends_an_unkeyed_fork_no_client_is_attached_to() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        let attached = trees
            .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
            .unwrap();
        let keyed = trees
            .create(TreeId::Root, Some(key("k")), SETTINGS, &cap(), &pod())
            .unwrap();
        let clients = Attached(vec![TreeId::Root, attached.into()]);
        assert_eq!(trees.settle(&clients), Ok(vec![fork]));
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Dropped);
        assert_eq!(trees.store.ends_at, [None], "agentfs's own end");
        assert_eq!(trees.live(), [attached, keyed]);
        assert_eq!(trees.settle(&Attached::default()), Ok(vec![attached]));
        assert_eq!(trees.live(), [keyed]);
    }

    #[test]
    fn a_dropped_fork_is_not_live_but_has_facts() {
        let mut store = Store::default();
        let mut row = store
            .create(
                TreeId::Root,
                None,
                SETTINGS,
                &LauncherId::parse("cli").unwrap(),
                &PodContext::default(),
            )
            .unwrap();
        row.value.phase = ForkPhase::Dropped;
        store.rows = vec![row];
        let trees = TreesService::restore(store, Events::default()).unwrap();
        assert!(trees.live().is_empty());
        assert_eq!(trees.facts(ForkId(1)).unwrap().phase, ForkPhase::Dropped);
    }

    #[test]
    fn each_store_failure_has_trees_word() {
        let mut trees = trees();
        assert_eq!(
            trees
                .create(TreeId::Root, None, SETTINGS, &cap(), &pod())
                .map(|_| ()),
            Ok(())
        );
        trees.store.fail = Some(StoreError::NotDurable(IoKind::NoSpace));
        assert_eq!(
            trees.end(ForkId(1), &cap(), &pod()),
            Err(TreeError::Io(IoKind::NoSpace))
        );
        trees.store.fail = Some(StoreError::Stopped);
        assert_eq!(
            trees.end(ForkId(1), &cap(), &pod()),
            Err(TreeError::Stopped)
        );
        assert_eq!(
            TreeError::from(StoreError::Damaged("x".into())),
            TreeError::Stopped
        );
        assert_eq!(
            TreeError::from(StoreError::Refused("x".into())),
            TreeError::ForkUnknown
        );
    }
}
