//! Deferred drops: the large values a catch-up with history lets go (replaced
//! saved maps, shapes, a map's base file), handed to History and dropped
//! after the actor's lock is released, on a thread that serves no requests
//! (docs/target-architecture/future/latency-upkeep.md).
//!
//! Until that slice lands, [`DeferredDrops::defer`] drops its values at once,
//! on the caller's thread, as the catch-up did before: the values' path from
//! the actor to History is in place, their drop is not moved yet.

use crate::tree_actor::stages::{Hold, section};

/// Values let go under the actor's lock, to be dropped after it: a saved map
/// a catch-up or a rebase replaced, a shape a catch-up replaced, and what
/// they hold (pieces, `Arc`s of frames, a map's base file).
///
/// Invariant: nothing in it is read again. It is only dropped, whole, by
/// [`DeferredDrops`].
#[derive(Default)]
#[must_use = "retired values go to `History::defer_drops`, or they drop where they are"]
pub(in crate::tree_actor) struct Retired {
    values: Vec<Box<dyn Send>>,
}

impl Retired {
    /// Adds `value`, to be dropped with the rest. O(1).
    pub(in crate::tree_actor) fn push<T: Send + 'static>(&mut self, value: T) {
        self.values.push(Box::new(value));
    }

    /// Adds every value of `other`. O(other).
    pub(in crate::tree_actor) fn append(&mut self, mut other: Retired) {
        self.values.append(&mut other.values);
    }

    /// Whether it holds no value.
    pub(in crate::tree_actor) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// History's deferred drops: every [`Retired`] the actor hands over is
/// dropped off the caller's thread, in the order handed over.
///
/// Contract (once built): [`DeferredDrops::defer`] is O(1) and never waits
/// for a drop; a deferred value is dropped on a thread that answers no FUSE
/// request and takes no lock the call path takes; [`DeferredDrops::finish`]
/// returns once every value deferred before it is dropped and that thread
/// has ended, so a base file's link is gone before the state area's lease is
/// released. The same path serves the actor behind [`Recording`] and a
/// `TreeActor` used directly: there is no second mode.
///
/// [`Recording`]: crate::tree_actor::Recording
pub(in crate::tree_actor) struct DeferredDrops {
    _private: (),
}

impl DeferredDrops {
    /// Deferred drops for one volume's History.
    pub(in crate::tree_actor) fn new() -> Self {
        Self { _private: () }
    }

    /// Takes `values` to drop them after the actor's lock is released.
    /// Until the slice lands: drops them now, as the catch-up's last drop
    /// (`Stages::lock_hold.drops`).
    pub(in crate::tree_actor) fn defer(&self, values: Retired) {
        let _kind = section(Hold::Drops);
        drop(values);
    }

    /// Returns once every value deferred so far is dropped and the drop
    /// thread has ended. Once only; what the actor's shutdown runs after the
    /// builder has stopped. Until the slice lands: nothing is left to drop.
    pub(in crate::tree_actor) fn finish(&self) {}
}
