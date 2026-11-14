//! The call path as a view host, the CLI and the conformance replay reach it:
//! the tree actor behind its one lock, with the lock discipline in this one
//! place. Under the lock a call decides, appends (no fsync) and applies;
//! durability waits and lag-gate waits run outside it (docs/trees.md,
//! "Recording and durability").
//!
//! Slices B2a (open, actor, durability) and B2b (history, gate, takes) build
//! the bodies; the signatures are frozen on their pages.

use std::path::Path;
use std::sync::{Arc, MutexGuard};

use agentfs_contracts::pod::PodContext;
use agentfs_contracts::volume::VolumeIdentity;

use super::history::built::History;
use super::journal::durable::RecordNo;
use super::{Error, Took, TreeActor, TreeId};

/// What runs when a deferred call may go on: given the durability or gate
/// outcome, it sends the FUSE reply. Runs on the journal's waiter thread or
/// the gate's, never with the actor locked.
pub type Then = Box<dyn FnOnce(Result<(), Error>) + Send + 'static>;

/// What [`Recording::progress`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A take landed: ask again.
    Landed,
    /// Every queued entry waits (held, refused, or not built yet).
    Waiting,
    /// No tree has a queued entry.
    Empty,
}

/// One tree volume, served: the actor, its journal's durability, and (B2b)
/// History with the builder behind it. Shared as `Arc<Recording>`. Dropping
/// the last one makes every record durable and builds every step appended
/// (B2b), so a clean shutdown leaves history built through its last record.
pub struct Recording {
    _private: (),
}

impl Recording {
    /// Creates a volume as [`TreeActor::create`] does, then serves it.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::create`].
    pub fn create(
        root: &Path,
        identity: &VolumeIdentity,
        service: &PodContext,
    ) -> Result<Arc<Self>, Error> {
        let _ = (root, identity, service);
        todo!("B2a")
    }

    /// Recovers a volume as [`TreeActor::open`] does, makes every recovered
    /// record durable, then serves it.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::open`], and a failed fsync of the recovered journal.
    pub fn open(root: &Path, service: &PodContext) -> Result<Arc<Self>, Error> {
        let _ = (root, service);
        todo!("B2a")
    }

    /// The actor, locked, for work that decides, appends and applies. The
    /// actor has no journal fsync and no wait on its call path, so nothing
    /// done through this guard waits on a disk flush or the builder.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped or its lock is poisoned.
    pub fn actor(&self) -> Result<MutexGuard<'_, TreeActor>, Error> {
        todo!("B2a")
    }

    /// FLUSH, FSYNC, `msync(MS_SYNC)` and each write of an `O_SYNC` or
    /// `O_DSYNC` open: the open's records made durable. Locks the actor only
    /// to read the open's last record; the fsync runs outside the lock.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open, and a failed fsync (the actor stops).
    pub fn durable(&self, open: u64) -> Result<(), Error> {
        let _ = open;
        todo!("B2a")
    }

    /// [`Recording::durable`] without blocking the caller (a view's one FUSE
    /// thread): `then` runs once the records are durable or the fsync failed.
    /// Every call one fsync covers is answered by that fsync (group commit).
    pub fn durable_then(&self, open: u64, then: Then) {
        let _ = (open, then);
        todo!("B2a")
    }

    /// [`Recording::durable_all`] without blocking the caller.
    pub fn durable_all_then(&self, then: Then) {
        let _ = then;
        todo!("B2a")
    }

    /// FSYNCDIR and a read-only descriptor's fsync: every record appended so far.
    ///
    /// # Errors
    ///
    /// A failed fsync (the actor stops).
    pub fn durable_all(&self) -> Result<(), Error> {
        todo!("B2a")
    }

    /// The lag gate, before a call that starts new work in `tree` (a create,
    /// a write-open, `O_TMPFILE`, a link, `rename(2)`, `unlink`, a path
    /// `truncate(2)`): returns at once while the tree's lag is under the
    /// limit or the builder has failed, else on the built event that brings
    /// it under. Never called with the actor locked; never before a write
    /// call or a RELEASE.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] at shutdown.
    pub fn admit(&self, tree: TreeId) -> Result<(), Error> {
        let _ = tree;
        todo!("B2b")
    }

    /// [`Recording::admit`] without blocking the caller: `then` runs at once on
    /// this thread when admitted, else on the gate's thread at the built or
    /// failed event that admits it. The call it wraps is concurrent with every
    /// later request, as an unreturned syscall is.
    pub fn admit_then(&self, tree: TreeId, then: Then) {
        let _ = (tree, then);
        todo!("B2b")
    }

    /// History, the one owner of the Git store.
    pub fn history(&self) -> &History {
        todo!("B2b")
    }

    /// At most one take: decided off the actor on the built snapshot, then
    /// landed under the lock if nothing it read changed. A refused or failed
    /// take stays queued for the next event.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped.
    pub fn progress(&self) -> Result<Progress, Error> {
        todo!("B2b")
    }

    /// One take in `tree`, as [`Recording::progress`] takes one: the
    /// conformance replay's `take_next`, after [`Recording::settle`].
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped; a failed decision or land.
    pub fn take_next(&self, tree: TreeId) -> Result<Took, Error> {
        let _ = tree;
        todo!("B2b")
    }

    /// Blocks until something a take may wait for has happened since `seen`
    /// (a built event, a last close, a queue entry): returns the new epoch.
    /// Event-driven; no timer.
    pub fn wait_for_work(&self, seen: u64) -> u64 {
        let _ = seen;
        todo!("B2b")
    }

    /// Waits until every record appended so far is built: what a test, the
    /// CLI or a replay calls before it reads history.
    ///
    /// # Errors
    ///
    /// As [`History::wait_built`], as an [`Error`].
    pub fn settle(&self) -> Result<RecordNo, Error> {
        todo!("B2b")
    }
}
