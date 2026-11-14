//! The Trees domain over its ports: create and the reads (slice S3); keys, end,
//! end by key, the drop and the reaction to `LastDetached` (slice S5).

use std::collections::BTreeMap;

use agentfs_contracts::audit::Principal;
use agentfs_contracts::caps::{LaunchCap, LaunchScope};
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink, ForkStore, QueueFacts, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{
    ForkFacts, ForkLookup, ForkPhase, ForkSettings, TreeError, Trees, TreesEvent,
};

/// One volume's Trees domain: every fork's row, loaded at boot.
pub struct TreesService<S, Q, E> {
    store: S,
    queues: Q,
    events: E,
    /// Every fork's row value, dropped ones included.
    forks: BTreeMap<ForkId, ForkFacts>,
}

impl<S, Q, E> TreesService<S, Q, E>
where
    S: ForkStore,
    Q: QueueFacts,
    E: EventSink,
{
    /// Loads every fork's row.
    ///
    /// # Errors
    ///
    /// The store's load.
    pub fn restore(store: S, queues: Q, events: E) -> Result<Self, StoreError> {
        let forks = store
            .load()?
            .into_iter()
            .map(|row| (row.value.fork, row.value))
            .collect();
        Ok(Self {
            store,
            queues,
            events,
            forks,
        })
    }

    /// Consumes `LastDetached`: an open unkeyed fork ends (`ForkEnded`, by
    /// `agentfs`), then [`TreesService::settle`]. The root and keyed forks:
    /// nothing.
    ///
    /// # Errors
    ///
    /// The store's writes.
    pub fn on_last_detached(&mut self, tree: TreeId) -> Result<(), StoreError> {
        let Some(fork) = tree.fork() else {
            return Ok(());
        };
        match self.forks.get(&fork) {
            Some(facts) if facts.phase == ForkPhase::Open && facts.key.is_none() => {}
            _ => return Ok(()),
        }
        self.end_open(fork, &Principal::Agentfs)?;
        self.settle()?;
        Ok(())
    }

    /// Drops every ended fork that may drop now: its upstream's queue holds
    /// nothing of it and no fork names it as upstream (`ForkDropped`). One
    /// with an open file in it waits for the next settle. Called at boot and
    /// after each take: level-triggered, idempotent. Answers the forks it
    /// dropped.
    ///
    /// # Errors
    ///
    /// The store's writes.
    pub fn settle(&mut self) -> Result<Vec<ForkId>, StoreError> {
        let ended: Vec<ForkId> = self
            .forks
            .values()
            .filter(|facts| facts.phase == ForkPhase::Ended)
            .map(|facts| facts.fork)
            .filter(|fork| !self.queues.pending_from(*fork) && !self.queues.has_child(*fork))
            .collect();
        let mut dropped = Vec::new();
        for fork in ended {
            // Drops carry no pod context: no caller gives one.
            match self
                .store
                .drop_fork(fork, &Principal::Agentfs, &PodContext::default())
            {
                Ok(()) => {}
                // An open file in it: the next settle drops it.
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

    /// An open fork's end: its row's `Ended`, then `ForkEnded`.
    fn end_open(&mut self, fork: ForkId, by: &Principal) -> Result<(), StoreError> {
        // `end` carries no pod context: the row's event has none.
        self.store.end(fork, by, &PodContext::default())?;
        if let Some(facts) = self.forks.get_mut(&fork) {
            facts.phase = ForkPhase::Ended;
        }
        self.events
            .publish(Event::Trees(TreesEvent::ForkEnded(fork)));
        Ok(())
    }

    /// `end` and `end_keyed` once the caller may end `fork`: an open fork
    /// ends and the drops follow; an ended or dropped one answers `Ok`.
    fn end_permitted(&mut self, fork: ForkId, by: &LaunchCap) -> Result<(), TreeError> {
        let phase = self.forks.get(&fork).ok_or(TreeError::ForkUnknown)?.phase;
        if phase == ForkPhase::Open {
            self.end_open(fork, &Principal::Launcher(by.launcher().clone()))?;
        }
        self.settle()?;
        Ok(())
    }

    /// The fork held under `key`, whatever its phase.
    fn keyed(&self, key: &ForkKey) -> Option<&ForkFacts> {
        self.forks
            .values()
            .find(|facts| facts.key.as_ref() == Some(key))
    }
}

impl<S, Q, E> ForkLookup for TreesService<S, Q, E>
where
    S: ForkStore,
    Q: QueueFacts,
    E: EventSink,
{
    fn facts(&self, fork: ForkId) -> Option<ForkFacts> {
        self.forks.get(&fork).cloned()
    }
}

impl<S, Q, E> Trees for TreesService<S, Q, E>
where
    S: ForkStore,
    Q: QueueFacts,
    E: EventSink,
{
    /// A create is one store step: the branch and its row with
    /// `ForkCreated`, published once the step is durable. Under a key Trees
    /// holds, the open fork is answered and nothing is written.
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        by: &LaunchCap,
    ) -> Result<ForkId, TreeError> {
        if let Some(facts) = key.as_ref().and_then(|key| self.keyed(key)) {
            return match facts.phase {
                ForkPhase::Open => Ok(facts.fork),
                ForkPhase::Ended | ForkPhase::Dropped => Err(TreeError::Ended),
            };
        }
        if !upstream.is_root() {
            return Err(TreeError::NestedNotBuilt);
        }
        // `Trees::create` carries no pod context: the row's event has none.
        let row = self
            .store
            .create(
                upstream,
                key,
                settings,
                by.launcher(),
                &PodContext::default(),
            )
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

    fn end(&mut self, fork: ForkId, by: &LaunchCap) -> Result<(), TreeError> {
        let facts = self.forks.get(&fork).ok_or(TreeError::ForkUnknown)?;
        let its_launcher = by.may() == LaunchScope::Root || facts.created_by == *by.launcher();
        if !its_launcher || facts.key.is_some() {
            return Err(TreeError::NotPermitted);
        }
        self.end_permitted(fork, by)
    }

    fn end_keyed(&mut self, key: &ForkKey, by: &LaunchCap) -> Result<(), TreeError> {
        let fork = self.keyed(key).ok_or(TreeError::KeyUnknown)?.fork;
        self.end_permitted(fork, by)
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
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

    use agentfs_contracts::audit::{Audited, Stamp, UnixNanos};
    use agentfs_contracts::caps::Minter;
    use agentfs_contracts::name::LauncherId;
    use agentfs_contracts::trees::Landing;
    use agentfs_contracts::volume::IoKind;

    use super::*;

    /// Forks in memory, numbered as the actor numbers them: by step. Each
    /// write is counted; a fork in `open_files` refuses its drop.
    #[derive(Default)]
    struct Store {
        rows: Vec<Audited<ForkFacts>>,
        step: u64,
        writes: u64,
        open_files: BTreeSet<ForkId>,
        fail: Option<StoreError>,
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

        fn end(&mut self, fork: ForkId, _: &Principal, _: &PodContext) -> Result<(), StoreError> {
            self.phase(fork, ForkPhase::Ended)
        }

        fn drop_fork(
            &mut self,
            fork: ForkId,
            _: &Principal,
            _: &PodContext,
        ) -> Result<(), StoreError> {
            if self.open_files.contains(&fork) {
                return Err(StoreError::Refused("busy".into()));
            }
            self.phase(fork, ForkPhase::Dropped)
        }
    }

    /// Queue facts a test sets: the forks with a pending landing, and with
    /// a child.
    #[derive(Clone, Default)]
    struct Queues(Rc<RefCell<(BTreeSet<ForkId>, BTreeSet<ForkId>)>>);

    impl QueueFacts for Queues {
        fn queued(&self) -> Vec<TreeId> {
            Vec::new()
        }
        fn pending_from(&self, fork: ForkId) -> bool {
            self.0.borrow().0.contains(&fork)
        }
        fn has_child(&self, fork: ForkId) -> bool {
            self.0.borrow().1.contains(&fork)
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

    type Service = TreesService<Store, Queues, Events>;

    fn trees() -> Service {
        TreesService::restore(Store::default(), Queues::default(), Events::default()).unwrap()
    }

    fn trees_with(queues: &Queues) -> Service {
        TreesService::restore(Store::default(), queues.clone(), Events::default()).unwrap()
    }

    fn trees_event(event: TreesEvent) -> Event {
        Event::Trees(event)
    }

    #[test]
    fn create_twice_unkeyed_makes_two_forks() {
        let mut trees = trees();
        let one = trees.create(TreeId::Root, None, SETTINGS, &cap()).unwrap();
        let two = trees.create(TreeId::Root, None, SETTINGS, &cap()).unwrap();
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
        let restored =
            TreesService::restore(trees.store, Queues::default(), Events::default()).unwrap();
        assert_eq!(restored.live(), [one, two]);
        assert_eq!(restored.facts(two).unwrap().fork, two);
    }

    /// A second create under a key returns its open fork and writes
    /// nothing; once the fork has ended, the key answers `Ended`.
    #[test]
    fn a_second_create_under_a_key_returns_its_fork() {
        let mut trees = trees();
        let fork = trees
            .create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap())
            .unwrap();
        let writes = trees.store.writes;
        assert_eq!(
            trees.create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap()),
            Ok(fork)
        );
        assert_eq!(trees.store.writes, writes, "nothing written");
        assert_eq!(trees.events.0.len(), 1, "one ForkCreated");
        let other = trees
            .create(TreeId::Root, Some(key("pvc-2")), SETTINGS, &cap())
            .unwrap();
        assert_ne!(other, fork);
        assert_eq!(trees.facts(fork).unwrap().key, Some(key("pvc-1")));
        trees.end_keyed(&key("pvc-1"), &cap()).unwrap();
        assert_eq!(
            trees.create(TreeId::Root, Some(key("pvc-1")), SETTINGS, &cap()),
            Err(TreeError::Ended)
        );
        // A restore knows the key.
        let restored = TreesService::restore(trees.store, Queues::default(), Events::default());
        let mut restored = restored.unwrap();
        assert_eq!(
            restored.create(TreeId::Root, Some(key("pvc-2")), SETTINGS, &cap()),
            Ok(other)
        );
    }

    #[test]
    fn nested_is_refused() {
        let mut trees = trees();
        let fork = trees.create(TreeId::Root, None, SETTINGS, &cap()).unwrap();
        assert_eq!(
            trees.create(fork.into(), None, SETTINGS, &cap()),
            Err(TreeError::NestedNotBuilt)
        );
        assert_eq!(
            trees.create(TreeId::Fork(99), None, SETTINGS, &cap()),
            Err(TreeError::NestedNotBuilt)
        );
        assert_eq!(trees.live(), [fork]);
        assert_eq!(
            trees.events.0.len(),
            1,
            "a refused create publishes nothing"
        );
    }

    /// An ended fork drops only once its landing is taken and no child
    /// names it; the drop is published once.
    #[test]
    fn an_ended_fork_drops_once_nothing_holds_it() {
        let queues = Queues::default();
        let mut trees = trees_with(&queues);
        let fork = trees.create(TreeId::Root, None, SETTINGS, &cap()).unwrap();
        queues.0.borrow_mut().0.insert(fork);
        trees.end(fork, &cap()).unwrap();
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Ended);
        assert_eq!(trees.settle(), Ok(vec![]), "its landing holds it");
        queues.0.borrow_mut().0.clear();
        queues.0.borrow_mut().1.insert(fork);
        assert_eq!(trees.settle(), Ok(vec![]), "a child holds it");
        queues.0.borrow_mut().1.clear();
        trees.store.open_files.insert(fork);
        assert_eq!(trees.settle(), Ok(vec![]), "an open file holds it");
        trees.store.open_files.clear();
        assert_eq!(trees.settle(), Ok(vec![fork]));
        assert_eq!(trees.settle(), Ok(vec![]), "dropped once");
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
        trees.end(fork, &cap()).unwrap();
        assert_eq!(trees.store.writes, writes);
    }

    /// `end`: only the volume's launcher or the fork's creator, and never a
    /// keyed fork; `end_keyed`: any launcher with the key.
    #[test]
    fn who_may_end_a_fork() {
        let mut trees = trees();
        let forks_cap = cap_of("csi", LaunchScope::ForksItCreates);
        let other_cap = cap_of("other", LaunchScope::ForksItCreates);
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &forks_cap)
            .unwrap();
        let keyed = trees
            .create(TreeId::Root, Some(key("k")), SETTINGS, &forks_cap)
            .unwrap();
        assert_eq!(trees.end(ForkId(99), &cap()), Err(TreeError::ForkUnknown));
        assert_eq!(trees.end(fork, &other_cap), Err(TreeError::NotPermitted));
        assert_eq!(trees.end(keyed, &cap()), Err(TreeError::NotPermitted));
        assert_eq!(
            trees.end_keyed(&key("nope"), &cap()),
            Err(TreeError::KeyUnknown)
        );
        trees.end(fork, &forks_cap).unwrap();
        trees.end_keyed(&key("k"), &other_cap).unwrap();
        assert_eq!(
            [fork, keyed].map(|f| trees.facts(f).unwrap().phase),
            [ForkPhase::Dropped; 2]
        );
        let mut trees = self::trees();
        let fork = trees
            .create(TreeId::Root, None, SETTINGS, &forks_cap)
            .unwrap();
        trees.end(fork, &cap()).unwrap();
    }

    /// `LastDetached` ends an open unkeyed fork as `agentfs`; a keyed fork
    /// and the root stay as they are.
    #[test]
    fn last_detached_ends_an_unkeyed_fork_not_a_keyed_one() {
        let mut trees = trees();
        let fork = trees.create(TreeId::Root, None, SETTINGS, &cap()).unwrap();
        let keyed = trees
            .create(TreeId::Root, Some(key("k")), SETTINGS, &cap())
            .unwrap();
        trees.on_last_detached(TreeId::Root).unwrap();
        trees.on_last_detached(keyed.into()).unwrap();
        assert_eq!(trees.facts(keyed).unwrap().phase, ForkPhase::Open);
        trees.on_last_detached(fork.into()).unwrap();
        assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Dropped);
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
        let trees = TreesService::restore(store, Queues::default(), Events::default()).unwrap();
        assert!(trees.live().is_empty());
        assert_eq!(trees.facts(ForkId(1)).unwrap().phase, ForkPhase::Dropped);
    }

    #[test]
    fn each_store_failure_has_trees_word() {
        let mut trees = trees();
        assert_eq!(
            trees
                .create(TreeId::Root, None, SETTINGS, &cap())
                .map(|_| ()),
            Ok(())
        );
        trees.store.fail = Some(StoreError::NotDurable(IoKind::NoSpace));
        assert_eq!(
            trees.end(ForkId(1), &cap()),
            Err(TreeError::Io(IoKind::NoSpace))
        );
        trees.store.fail = Some(StoreError::Stopped);
        assert_eq!(trees.end(ForkId(1), &cap()), Err(TreeError::Stopped));
        assert_eq!(
            TreeError::from(StoreError::Damaged("x".into())),
            TreeError::Stopped
        );
        assert_eq!(
            TreeError::from(StoreError::Refused("x".into())),
            TreeError::Stopped
        );
    }
}
