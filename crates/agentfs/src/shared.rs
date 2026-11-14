//! One domain service shared by the composition root and another service
//! that asks it questions (docs/library/core.md, "Rows and domain events"):
//! Clients asks Trees `facts` while the root also calls Trees, and Volume asks
//! Clients `attached_any`, Clients being restored only once Volume has staged.
//! In-process and on one thread, as the services and their event queue are.

use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

use agentfs_clients::ClientsService;
use agentfs_contracts::clients::{AttachedLookup, ClientSet};
use agentfs_contracts::ports::{ClientStore, EventSink, ForkStore, QueueFacts, SessionBinder};
use agentfs_contracts::tree::ForkId;
use agentfs_contracts::trees::{ForkFacts, ForkLookup};
use agentfs_trees::TreesService;

/// A service behind a shared, checked borrow: every clone is the one service.
pub(crate) struct Shared<T>(Rc<RefCell<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<T> Shared<T> {
    pub(crate) fn new(service: T) -> Self {
        Self(Rc::new(RefCell::new(service)))
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the root writes Trees; Clients reads it through ForkLookup"
        )
    )]
    pub(crate) fn get(&self) -> Ref<'_, T> {
        self.0.borrow()
    }

    pub(crate) fn get_mut(&self) -> RefMut<'_, T> {
        self.0.borrow_mut()
    }
}

/// Clients' one question to Trees.
impl<S, Q, E> ForkLookup for Shared<TreesService<S, Q, E>>
where
    S: ForkStore,
    Q: QueueFacts,
    E: EventSink,
{
    fn facts(&self, fork: ForkId) -> Option<ForkFacts> {
        self.0.borrow().facts(fork)
    }
}

/// Volume's one question to Clients: none attached while Clients is not
/// restored yet (before `stage`).
impl<S, B, F, E> AttachedLookup for Shared<Option<ClientsService<S, B, F, E>>>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    fn attached_any(&self) -> ClientSet {
        self.0
            .borrow()
            .as_ref()
            .map(AttachedLookup::attached_any)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clone_is_the_one_service() {
        let one = Shared::new(1);
        let two = one.clone();
        *two.get_mut() += 1;
        assert_eq!(*one.get(), 2);
    }
}
