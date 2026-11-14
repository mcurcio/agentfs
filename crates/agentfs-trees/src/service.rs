//! The Trees domain over its ports: create and the reads (slice S3); keys, end,
//! end by key, the drop and the reaction to `LastDetached` (slice S5).

use std::collections::BTreeMap;

use agentfs_contracts::caps::LaunchCap;
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink, ForkStore, QueueFacts, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{
    ForkFacts, ForkLookup, ForkPhase, ForkSettings, TreeError, Trees, TreesEvent,
};
use agentfs_contracts::volume::IoKind;

/// `EIO`: a store failure that is no failed write's errno (a stopped actor,
/// damaged state) as `TreeError::Io` names it.
const EIO: i32 = 5;

/// One volume's Trees domain: every fork's row, loaded at boot.
pub struct TreesService<S, Q, E> {
    store: S,
    #[expect(dead_code, reason = "slice S5's settle asks the queues")]
    queues: Q,
    events: E,
    /// Every fork's row value, dropped ones included.
    forks: BTreeMap<ForkId, ForkFacts>,
}

/// Trees' word for a store failure.
fn tree_error(error: StoreError) -> TreeError {
    match error {
        StoreError::NotDurable(kind) => TreeError::Io(kind),
        StoreError::Refused(_) => TreeError::UpstreamUnknown,
        StoreError::Stopped | StoreError::Damaged(_) => TreeError::Io(IoKind::Other(EIO)),
    }
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

    /// Consumes `LastDetached`: an open unkeyed fork ends (`ForkEnded`), then
    /// [`TreesService::settle`]. The root and keyed forks: nothing.
    ///
    /// # Errors
    ///
    /// The store's writes.
    pub fn on_last_detached(&mut self, tree: TreeId) -> Result<(), StoreError> {
        let _ = tree;
        todo!("S5")
    }

    /// Drops every ended fork that may drop now: its upstream's queue holds
    /// nothing of it and no fork names it as upstream (`ForkDropped`). Called
    /// at boot and after each take: level-triggered, idempotent.
    ///
    /// # Errors
    ///
    /// The store's writes.
    pub fn settle(&mut self) -> Result<Vec<ForkId>, StoreError> {
        todo!("S5")
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
    /// An unkeyed create is one store step: the branch and its row with
    /// `ForkCreated`, published once the step is durable. A keyed create is
    /// slice S5's.
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        by: &LaunchCap,
    ) -> Result<ForkId, TreeError> {
        if key.is_some() {
            todo!("S5: keyed")
        }
        if !upstream.is_root() {
            return Err(TreeError::NestedNotBuilt);
        }
        // `Trees::create` carries no pod context: the row's event has none.
        let row = self
            .store
            .create(
                upstream,
                None,
                settings,
                by.launcher(),
                &PodContext::default(),
            )
            .map_err(tree_error)?;
        let fork = row.value.fork;
        self.forks.insert(fork, row.value);
        self.events
            .publish(Event::Trees(TreesEvent::ForkCreated(fork)));
        Ok(fork)
    }

    fn end(&mut self, fork: ForkId, by: &LaunchCap) -> Result<(), TreeError> {
        let _ = (fork, by);
        todo!("S5")
    }

    fn end_keyed(&mut self, key: &ForkKey, by: &LaunchCap) -> Result<(), TreeError> {
        let _ = (key, by);
        todo!("S5")
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
    use agentfs_contracts::audit::{Audited, Principal, Stamp, UnixNanos};
    use agentfs_contracts::caps::{LaunchScope, Minter};
    use agentfs_contracts::name::LauncherId;
    use agentfs_contracts::trees::Landing;

    use super::*;

    /// Forks in memory, numbered as the actor numbers them: by step.
    #[derive(Default)]
    struct Store {
        rows: Vec<Audited<ForkFacts>>,
        step: u64,
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

        fn end(&mut self, _: ForkId, _: &Principal, _: &PodContext) -> Result<(), StoreError> {
            unreachable!("S5")
        }

        fn drop_fork(
            &mut self,
            _: ForkId,
            _: &Principal,
            _: &PodContext,
        ) -> Result<(), StoreError> {
            unreachable!("S5")
        }
    }

    struct NoQueues;

    impl QueueFacts for NoQueues {
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
    fn cap() -> LaunchCap {
        LaunchCap::mint(
            &Minter::new(),
            LauncherId::parse("cli").unwrap(),
            LaunchScope::Root,
        )
    }

    const SETTINGS: ForkSettings = ForkSettings {
        landing: Landing::Automatic,
    };

    fn trees() -> TreesService<Store, NoQueues, Events> {
        TreesService::restore(Store::default(), NoQueues, Events::default()).unwrap()
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
                Event::Trees(TreesEvent::ForkCreated(one)),
                Event::Trees(TreesEvent::ForkCreated(two)),
            ]
        );
        // A restore reads the same rows back.
        let restored = TreesService::restore(trees.store, NoQueues, Events::default()).unwrap();
        assert_eq!(restored.live(), [one, two]);
        assert_eq!(restored.facts(two).unwrap().fork, two);
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
        let trees = TreesService::restore(store, NoQueues, Events::default()).unwrap();
        assert!(trees.live().is_empty());
        assert_eq!(trees.facts(ForkId(1)).unwrap().phase, ForkPhase::Dropped);
    }

    #[test]
    fn each_store_failure_has_trees_word() {
        assert_eq!(
            tree_error(StoreError::Refused("x".into())),
            TreeError::UpstreamUnknown
        );
        assert_eq!(
            tree_error(StoreError::NotDurable(IoKind::NoSpace)),
            TreeError::Io(IoKind::NoSpace)
        );
        assert_eq!(
            tree_error(StoreError::Stopped),
            TreeError::Io(IoKind::Other(EIO))
        );
    }
}
