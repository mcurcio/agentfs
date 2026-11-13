//! The Trees domain over its ports: create and the reads (slice S3); keys, end,
//! end by key, the drop and the reaction to `LastDetached` (slice S5).

use agentfs_contracts::caps::LaunchCap;
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::ports::{EventSink, ForkStore, QueueFacts, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{ForkFacts, ForkLookup, ForkSettings, TreeError, Trees};

/// One volume's Trees domain: every fork's row, loaded at boot.
#[expect(
    dead_code,
    reason = "slices S3 and S5 build the bodies that read these"
)]
pub struct TreesService<S, Q, E> {
    store: S,
    queues: Q,
    events: E,
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
        let _ = (store, queues, events);
        todo!("S3")
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
        let _ = fork;
        todo!("S3")
    }
}

impl<S, Q, E> Trees for TreesService<S, Q, E>
where
    S: ForkStore,
    Q: QueueFacts,
    E: EventSink,
{
    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        by: &LaunchCap,
    ) -> Result<ForkId, TreeError> {
        let _ = (upstream, key, settings, by);
        todo!("S3: unkeyed; S5: keyed")
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
        todo!("S3")
    }
}
