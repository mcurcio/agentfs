//! The Clients domain over its ports (slice S4): attach, detach, reconcile and
//! the per-tree index, rebuilt from the rows at boot.

use agentfs_contracts::caps::{LaunchCap, Minter};
use agentfs_contracts::clients::{
    AttachedLookup, Binding, ClientError, ClientId, ClientSet, Clients, Grant, SessionId,
};
use agentfs_contracts::name::ClientLabel;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{ClientStore, EventSink, SessionBinder, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::ForkLookup;

/// One volume's Clients domain. It mints `ClientCap`s with its own [`Minter`],
/// lent by the composition root, and binds each to its session.
#[expect(dead_code, reason = "slice S4 builds the bodies that read these")]
pub struct ClientsService<S, B, F, E> {
    store: S,
    sessions: B,
    forks: F,
    events: E,
    minter: Minter,
}

impl<S, B, F, E> ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    /// Loads every row and rebuilds the per-tree index.
    ///
    /// # Errors
    ///
    /// The store's load.
    pub fn restore(
        store: S,
        sessions: B,
        forks: F,
        events: E,
        minter: Minter,
    ) -> Result<Self, StoreError> {
        let _ = (store, sessions, forks, events, minter);
        todo!("S4")
    }

    /// Consumes `ForkDropped`: the fork's writer claim is released (C4).
    ///
    /// # Errors
    ///
    /// The store's write.
    pub fn on_fork_dropped(&mut self, fork: ForkId) -> Result<(), StoreError> {
        let _ = fork;
        todo!("S5")
    }
}

impl<S, B, F, E> AttachedLookup for ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    fn attached_any(&self) -> ClientSet {
        todo!("S4")
    }
}

impl<S, B, F, E> Clients for ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    fn attach(
        &mut self,
        grant: Grant,
        label: ClientLabel,
        at: PodContext,
        session: SessionId,
        by: &LaunchCap,
    ) -> Result<Binding, ClientError> {
        let _ = (grant, label, at, session, by);
        todo!("S4")
    }

    fn detach(&mut self, client: &ClientId, by: &LaunchCap) -> Result<(), ClientError> {
        let _ = (client, by);
        todo!("S4")
    }

    fn reconcile(&mut self, live: &ClientSet, by: &LaunchCap) -> Vec<ClientId> {
        let _ = (live, by);
        todo!("S4")
    }

    fn attached(&self, tree: TreeId) -> ClientSet {
        let _ = tree;
        todo!("S4")
    }
}
