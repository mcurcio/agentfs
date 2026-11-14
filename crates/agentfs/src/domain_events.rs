//! The composition root's event queue (docs/library/core.md, "Rows and
//! domain events"): each domain publishes to it once its step is durable, and
//! the root hands each event, in order, to its consumers after the call that
//! caused it returns. In-process and not durable: at boot every consumer
//! re-judges from asked facts.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use agentfs_contracts::ports::{Event, EventSink};

/// One queue, shared by every domain service of one volume: each holds a
/// clone.
#[derive(Clone, Default)]
pub(crate) struct DomainEvents(Rc<RefCell<VecDeque<Event>>>);

impl EventSink for DomainEvents {
    fn publish(&mut self, event: Event) {
        self.0.borrow_mut().push_back(event);
    }
}

impl DomainEvents {
    /// The oldest event not yet handed on.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "slice S5's dispatcher hands each event on")
    )]
    pub(crate) fn next(&self) -> Option<Event> {
        self.0.borrow_mut().pop_front()
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::tree::ForkId;
    use agentfs_contracts::trees::TreesEvent;

    use super::*;

    #[test]
    fn every_clone_publishes_to_one_queue_in_order() {
        let queue = DomainEvents::default();
        let mut trees = queue.clone();
        let mut clients = queue.clone();
        let created = |n| Event::Trees(TreesEvent::ForkCreated(ForkId(n)));
        trees.publish(created(1));
        clients.publish(created(2));
        assert_eq!(queue.next(), Some(created(1)));
        assert_eq!(queue.next(), Some(created(2)));
        assert_eq!(queue.next(), None);
    }
}
