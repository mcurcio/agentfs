//! The call path as a view host, the CLI and the conformance replay reach it:
//! the tree actor behind its one lock, with the lock discipline in this one
//! place. Under the lock a call decides, appends (no fsync) and applies;
//! durability waits and lag-gate waits run outside it (docs/trees.md,
//! "Recording and durability").
//!

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use agentfs_contracts::pod::PodContext;
use agentfs_contracts::volume::VolumeIdentity;

use super::history::built::History;
use super::journal::durable::{Durability, RecordNo};
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

/// One tree volume, served: the actor, its journal's durability, and
/// History with the builder behind it. Shared as `Arc<Recording>`. Dropping
/// the last one makes every record durable and builds every step appended,
/// so a clean shutdown leaves history built through its last record.
pub struct Recording {
    actor: Mutex<TreeActor>,
    /// The actor's journal's group commit, reached without its lock.
    durability: Arc<Durability>,
    /// The one owner of the Git store, reached without the actor's lock.
    history: History,
}

impl Drop for Recording {
    fn drop(&mut self) {
        // A failure is logged where the fsync failed; the next open recovers.
        let _ = self.durability.make_all_durable();
    }
}

impl Recording {
    fn serve(actor: TreeActor) -> Arc<Self> {
        let durability = Arc::clone(actor.durability());
        let history = actor.history().clone();
        Arc::new(Self {
            actor: Mutex::new(actor),
            durability,
            history,
        })
    }

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
        Ok(Self::serve(TreeActor::create(root, identity, service)?))
    }

    /// Recovers a volume as [`TreeActor::open`] does, makes every recovered
    /// record durable, then serves it.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::open`], and a failed fsync of the recovered journal.
    pub fn open(root: &Path, service: &PodContext) -> Result<Arc<Self>, Error> {
        let actor = TreeActor::open(root, service)?;
        actor.durability().make_all_durable()?;
        Ok(Self::serve(actor))
    }

    /// The actor, locked, for work that decides, appends and applies. The
    /// actor has no journal fsync and no wait on its call path, so nothing
    /// done through this guard waits on a disk flush or the builder.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped or its lock is poisoned.
    pub fn actor(&self) -> Result<MutexGuard<'_, TreeActor>, Error> {
        let actor = self.actor.lock().map_err(|_| Error::Stopped)?;
        if actor.is_stopped() {
            return Err(Error::Stopped);
        }
        Ok(actor)
    }

    /// The last record a FLUSH of `open` must cover, read under the lock.
    fn durable_through(&self, open: u64) -> Result<RecordNo, Error> {
        self.actor()?.durable_through(open)
    }

    /// FLUSH, FSYNC, `msync(MS_SYNC)` and each write of an `O_SYNC` or
    /// `O_DSYNC` open: the open's records made durable. Locks the actor only
    /// to read the open's last record; the fsync runs outside the lock.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open, and a failed fsync (the actor stops).
    pub fn durable(&self, open: u64) -> Result<(), Error> {
        let through = self.durable_through(open)?;
        self.durability.make_durable(through)?;
        Ok(())
    }

    /// [`Recording::durable`] without blocking the caller (a view's one FUSE
    /// thread): `then` runs once the records are durable or the fsync failed.
    /// Every call one fsync covers is answered by that fsync (group commit).
    pub fn durable_then(&self, open: u64, then: Then) {
        match self.durable_through(open) {
            Ok(through) => self.durability.then(through, then),
            Err(e) => then(Err(e)),
        }
    }

    /// [`Recording::durable_all`] without blocking the caller.
    pub fn durable_all_then(&self, then: Then) {
        self.durability.then(self.durability.appended(), then);
    }

    /// FSYNCDIR and a read-only descriptor's fsync: every record appended so far.
    ///
    /// # Errors
    ///
    /// A failed fsync (the actor stops).
    pub fn durable_all(&self) -> Result<(), Error> {
        self.durability.make_all_durable()?;
        Ok(())
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
        Ok(self.history.admit(tree)?)
    }

    /// [`Recording::admit`] without blocking the caller: `then` runs at once on
    /// this thread when admitted, else on a gate thread at the built or failed
    /// event that admits it. The call it wraps is concurrent with every later
    /// request, as an unreturned syscall is.
    pub fn admit_then(&self, tree: TreeId, then: Then) {
        if self.history.admits(tree) {
            then(Ok(()));
            return;
        }
        let history = self.history.clone();
        let slot = Arc::new(Mutex::new(Some(then)));
        let waiting = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("tree-gate".into())
            .spawn(move || {
                let admitted = history.admit(tree).map_err(Error::from);
                if let Some(then) = waiting.lock().ok().and_then(|mut t| t.take()) {
                    then(admitted);
                }
            });
        if let Err(e) = spawned {
            // No gate thread: answer here rather than drop the reply.
            if let Some(then) = slot.lock().ok().and_then(|mut t| t.take()) {
                then(Err(Error::Io(e)));
            }
        }
    }

    /// History, the one owner of the Git store.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// At most one take, decided on history built through every record
    /// appended before it, and landed under the lock. A failed take stays
    /// queued for the next event.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped.
    pub fn progress(&self) -> Result<Progress, Error> {
        let mut actor = self.actor()?;
        if actor.pointers.under(super::refs::QUEUES).is_empty() {
            return Ok(Progress::Empty);
        }
        Ok(if actor.progress()? {
            Progress::Landed
        } else {
            Progress::Waiting
        })
    }

    /// One take in `tree`, as [`Recording::progress`] takes one: the
    /// conformance replay's `take_next`.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped; a failed decision or land.
    pub fn take_next(&self, tree: TreeId) -> Result<Took, Error> {
        self.actor()?.take_next(tree)
    }

    /// Blocks until something a take may wait for has happened since `seen`
    /// (a built or failed event): returns the new epoch. Event-driven; no
    /// timer.
    pub fn wait_for_work(&self, seen: u64) -> u64 {
        self.history.wait_epoch(seen)
    }

    /// Waits until every record appended so far is built: what a test, the
    /// CLI or a replay calls before it reads history.
    ///
    /// # Errors
    ///
    /// As [`History::wait_built`], as an [`Error`].
    pub fn settle(&self) -> Result<RecordNo, Error> {
        Ok(self.history.wait_built(self.durability.appended())?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, sync_channel};

    use super::super::test_support::{new_actor, writer};
    use super::*;

    #[test]
    fn a_flush_waits_without_the_actor_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path()));
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), true)
            .unwrap();
        rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        rec.durability.hold_fsync_with(move || {
            // Later fsyncs (the drop's) find both ends gone and pass.
            let _ = entered.send(());
            let _ = released.lock().map(|r| r.recv());
        });
        std::thread::scope(|scope| {
            let flush = scope.spawn(|| rec.durable(h));
            inside.recv().unwrap();
            // The leader is inside its fsync: the actor still serves calls.
            rec.actor().unwrap().write(h, 1, b"y", false).unwrap();
            release.send(()).unwrap();
            flush.join().unwrap().unwrap();
        });
        assert!(rec.durability.mark() >= RecordNo::new(h));
        assert!(rec.durability.mark() < rec.durability.appended());
    }

    #[test]
    fn a_flush_covers_its_open_and_a_deferred_one_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path()));
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), true)
            .unwrap();
        rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
        let through = rec.actor().unwrap().durable_through(h).unwrap();
        assert_eq!(through, rec.durability.appended());
        let (answered, answer) = mpsc::channel();
        rec.durable_then(h, Box::new(move |r| answered.send(r.is_ok()).unwrap()));
        assert!(answer.recv().unwrap());
        assert!(rec.durability.mark() >= through);
        let (answered, answer) = mpsc::channel();
        rec.durable_then(
            999,
            Box::new(move |r| answered.send(r.err().map(|e| e.errno())).unwrap()),
        );
        assert_eq!(answer.recv().unwrap(), Some(libc::ENOENT));
    }
}
