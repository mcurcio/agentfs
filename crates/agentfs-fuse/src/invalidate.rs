//! Kernel cache invalidations across a daemon's FUSE sessions (design
//! §17.1; sandbox overlay slice 4).
//!
//! The main mount and the sandbox overlay are two FUSE sessions over one
//! [`crate::ops::Ops`], and node ids are the same in both. Each kernel
//! caches names (1 s) per session, and the pages of files open through it,
//! so a change made through one session is an event every other session's
//! kernel must hear: [`crate::fuse::FuseFs`] reports each change it served
//! ([`Invalidator::changed`]), and the invalidator sends it to the others.
//!
//! - **Events, not timers.** Every change is sent once, as it happens.
//! - **Never from a request thread.** The kernel may wait on a request of
//!   the session it invalidates to finish an invalidation, so a FUSE
//!   request thread only queues ([`Invalidator::changed`] never blocks);
//!   one worker thread sends.
//! - **Not to the session the change came through**: its kernel already
//!   knows.
//! - A session attached late ([`Invalidator::attach`]) has nothing cached
//!   from before, so changes queued before it attached are not sent to it.
//!
//! [`Invalidator::invalidate`] sends at once to every session, from a
//! thread that is not a FUSE request's: a restore made outside every
//! session ([`crate::mount::MountHooks`]).

use std::io;
use std::sync::Arc;

use crate::mount::MountKind;
use crate::ops::Cached;

/// Sends kernel cache invalidations to a daemon's FUSE sessions.
/// `Send + Sync`.
#[expect(
    dead_code,
    reason = "sandbox overlay slice 4 implements the invalidator"
)]
pub struct Invalidator {
    stub: (),
}

impl Invalidator {
    /// An invalidator with no session attached yet, and its worker thread.
    pub fn start() -> io::Result<Arc<Self>> {
        todo!("sandbox overlay slice 4")
    }

    /// Session `kind`'s kernel hears every later change made through
    /// another session. A second attach of the same kind replaces the
    /// first.
    pub fn attach(&self, kind: MountKind, notifier: fuser::Notifier) {
        let _ = (kind, notifier);
        todo!("sandbox overlay slice 4")
    }

    /// A change served through session `from`: every other attached
    /// session drops `cached`. Queues and returns; never blocks.
    pub fn changed(&self, from: MountKind, cached: Vec<Cached>) {
        let _ = (from, cached);
        todo!("sandbox overlay slice 4")
    }

    /// Every attached session drops `cached` now, sent from the calling
    /// thread (never a FUSE request's); the first error, if any. An inode
    /// or entry a kernel no longer holds is not an error.
    pub fn invalidate(&self, cached: &[Cached]) -> Result<(), String> {
        let _ = cached;
        todo!("sandbox overlay slice 4")
    }

    /// Sends what is queued, then stops the worker. Later calls are no-ops.
    pub fn stop(&self) {
        todo!("sandbox overlay slice 4")
    }
}
