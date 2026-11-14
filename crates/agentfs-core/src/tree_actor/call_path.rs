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
use super::takes::{self, Lands};
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
    /// The gate thread, joined at drop: it holds the store (and the state
    /// area's lease) until the builder has stopped.
    gate: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        // A failure is logged where the fsync failed; the next open recovers.
        let _ = self.durability.make_all_durable();
        // Build every record appended, then stop the builder and the gate
        // thread, so the lease is free once the drop returns. A drop on the
        // gate thread itself (its last reply held the last handle) leaves it
        // to end on its own.
        self.actor
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shut_down();
        if let Some(gate) = self.gate.take()
            && gate.thread().id() != std::thread::current().id()
        {
            let _ = gate.join();
        }
    }
}

impl Lands for &Recording {
    fn with<R>(&mut self, f: impl FnOnce(&mut TreeActor) -> Result<R, Error>) -> Result<R, Error> {
        let mut actor = self.actor()?;
        f(&mut actor)
    }
}

impl Recording {
    /// Serves `actor`, with the volume's gate thread: one thread answers
    /// every gated call that waits ([`Recording::admit_then`]) and ends once
    /// the builder has stopped.
    fn serve(actor: TreeActor) -> Result<Arc<Self>, Error> {
        let durability = Arc::clone(actor.durability());
        let history = actor.history().clone();
        let gate = history.clone();
        let owner = crate::failpoints::owner();
        let gate = std::thread::Builder::new()
            .name("tree-gate".into())
            .spawn(move || {
                crate::failpoints::adopt(owner);
                gate.serve_gate();
            })?;
        Ok(Arc::new(Self {
            actor: Mutex::new(actor),
            durability,
            history,
            gate: Some(gate),
        }))
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
        Self::serve(TreeActor::create(root, identity, service)?)
    }

    /// Recovers a volume as [`TreeActor::open`] does, then serves it.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::open`].
    pub fn open(root: &Path, service: &PodContext) -> Result<Arc<Self>, Error> {
        Self::serve(TreeActor::open(root, service)?)
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
    /// this thread when admitted, else on the volume's one gate thread at the
    /// built or failed event that admits it. The call it wraps is concurrent
    /// with every later request, as an unreturned syscall is.
    pub fn admit_then(&self, tree: TreeId, then: Then) {
        self.history.admit_then(tree, then);
    }

    /// History, the one owner of the Git store.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// At most one take: the root first, then the forks by number. Each is
    /// decided on history built through every record appended before it,
    /// with the actor unlocked, and landed under the lock (docs/trees.md,
    /// "Takes"). A failed take is logged and stays queued for the next event.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped.
    pub fn progress(&self) -> Result<Progress, Error> {
        let trees: Vec<TreeId> = {
            let actor = self.actor()?;
            actor
                .trees()
                .into_iter()
                .filter(|tree| actor.queued(*tree))
                .collect()
        };
        if trees.is_empty() {
            return Ok(Progress::Empty);
        }
        for tree in trees {
            match self.take_next(tree) {
                Ok(Took::Taken) => return Ok(Progress::Landed),
                Ok(Took::Waiting | Took::Empty) => {}
                Err(e) if !self.is_stopped() => eprintln!("tree SaveFailed: {e}"),
                Err(e) => return Err(e),
            }
        }
        Ok(Progress::Waiting)
    }

    /// Whether the actor has stopped (or its lock is poisoned).
    fn is_stopped(&self) -> bool {
        self.actor.lock().map_or(true, |actor| actor.is_stopped())
    }

    /// One take in `tree`, as [`Recording::progress`] takes one: the
    /// conformance replay's `take_next`. Takes run one at a time: callers
    /// take from one thread (the view's worker, or a test).
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped; a failed decision or land.
    pub fn take_next(&self, tree: TreeId) -> Result<Took, Error> {
        let mut this = self;
        takes::take_one(&mut this, &self.history, tree)
    }

    /// Objects takes changed since the last ask: the pages a view drops.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped.
    pub fn drain_changed(&self) -> Result<Vec<(TreeId, u64)>, Error> {
        Ok(self.actor()?.drain_changed())
    }

    /// Wakes every [`Recording::wait_for_work`] caller, as an event would:
    /// what a view's shutdown calls before it joins its worker.
    pub fn wake_workers(&self) {
        self.history.wake_waiters();
    }

    /// Blocks until something a take may wait for has happened since `seen`
    /// (a built or failed event): returns the new epoch. Event-driven; no
    /// timer.
    pub fn wait_for_work(&self, seen: u64) -> u64 {
        self.history.wait_epoch(seen)
    }

    /// [`Recording::wait_for_work`], returning by `at_most` at the latest: what
    /// a metrics export waits on, so each built or failed event is written
    /// at once and the gauges between events still move.
    pub fn wait_for_event_or(&self, seen: u64, at_most: std::time::Duration) -> u64 {
        self.history.wait_epoch_for(seen, at_most)
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
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), true)
            .unwrap();
        // The create is a step: once it is built, the builder has made its
        // fsync and waits for the next step, so the flush leads the one held.
        rec.settle().unwrap();
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
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
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

    #[test]
    fn a_save_is_visible_before_it_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        rec.settle().unwrap();
        let before = rec.history().snapshot();
        // Hold the builder in the fsync it makes before each batch.
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        rec.durability.hold_fsync_with(move || {
            let _ = entered.send(());
            let _ = released.lock().map(|r| r.recv());
        });
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), true)
            .unwrap();
        rec.actor().unwrap().write(h, 0, b"saved", false).unwrap();
        let saved = rec.actor().unwrap().close(h).unwrap();
        let super::super::Saved::Commit(step) = saved else {
            panic!("the close saves: {saved:?}");
        };
        inside.recv().unwrap();
        // The builder is held before its batch: the save is visible, not built.
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "a").unwrap(),
            b"saved"
        );
        let snapshot = rec.history().snapshot();
        assert!(snapshot.at() < step);
        assert_eq!(snapshot.head(TreeId::Root), before.head(TreeId::Root));
        // Later fsyncs find both ends gone and pass.
        release.send(()).unwrap();
        drop((release, inside));
        rec.settle().unwrap();
        assert!(rec.history().snapshot().at() >= step);
        assert_ne!(
            rec.history().snapshot().head(TreeId::Root),
            before.head(TreeId::Root)
        );
    }

    /// A failed batch releases every waiter on its event: a gated create
    /// held over the lag limit goes on (a failed builder holds nothing back),
    /// a wait for the built point gets the failure, recording goes on, and a
    /// retry builds.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_failed_builder_releases_the_gate_and_recording_goes_on() {
        use std::time::Duration;
        // A bound on a hang, never a coordination: each answer comes on an event.
        const HANG: Duration = Duration::from_secs(60);
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        rec.settle().unwrap();
        rec.history.set_lag_limits(Duration::ZERO, Duration::ZERO);
        let failing = crate::failpoints::configure("tree.build_after_pack=error@*").unwrap();
        // Hold the builder in the fsync it makes before its batch.
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        rec.durability.hold_fsync_with(move || {
            let _ = entered.send(());
            let _ = released.lock().map(|r| r.recv());
        });
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), true)
            .unwrap();
        inside.recv().unwrap();
        // The create is unbuilt and the limit is zero: a gated call waits.
        assert!(!rec.history.admits(TreeId::Root));
        let (admitted, admission) = mpsc::channel();
        rec.admit_then(
            TreeId::Root,
            Box::new(move |r| admitted.send(r.is_ok()).unwrap()),
        );
        assert!(admission.try_recv().is_err(), "held by the gate");
        let through = rec.durability.appended();
        let (waited, wait) = mpsc::channel();
        let history = rec.history.clone();
        let asked = history.requests();
        std::thread::spawn(move || waited.send(history.wait_built(through)).unwrap());
        // The waiter asks under History's lock and waits in the same hold.
        while rec.history.requests() == asked {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        drop((release, inside));
        assert_eq!(admission.recv_timeout(HANG), Ok(true), "the gate let go");
        assert!(matches!(
            wait.recv_timeout(HANG),
            Ok(Err(super::super::history::built::WaitError::Failed(_)))
        ));
        // Recording goes on while the builder fails.
        rec.actor().unwrap().write(h, 0, b"saved", false).unwrap();
        rec.actor().unwrap().close(h).unwrap();
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "a").unwrap(),
            b"saved"
        );
        drop(failing);
        let built = rec.settle().unwrap();
        assert_eq!(built, rec.durability.appended(), "a retry built it all");
    }

    #[test]
    fn the_builder_never_reads_past_the_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        for name in ["a", "b", "c"] {
            let h = rec
                .actor()
                .unwrap()
                .write_open(TreeId::Root, name, &writer("w"), true)
                .unwrap();
            rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
            rec.actor().unwrap().close(h).unwrap();
        }
        // No caller asked for durability: whatever is built, the builder made
        // durable first.
        let built = rec.settle().unwrap();
        assert_eq!(built, rec.durability.appended());
        assert!(rec.durability.mark() >= rec.history().built_through());
    }

    #[test]
    fn drop_builds_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        for n in 0..20 {
            let h = rec
                .actor()
                .unwrap()
                .write_open(TreeId::Root, &format!("f{n}"), &writer("w"), true)
                .unwrap();
            rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
            rec.actor().unwrap().close(h).unwrap();
        }
        // Calls after the last step: no step asks the builder for them.
        rec.settle().unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "f0", &writer("w"), false)
            .unwrap();
        rec.actor().unwrap().write(h, 0, b"y", false).unwrap();
        let last = rec.durability.appended();
        assert!(rec.history().built_through() < last);
        drop(rec);
        // Read before the reopen, which would build what the drop did not.
        let (_, entries) =
            super::super::history::built_log::BuiltLog::open(&tmp.path().join("built.log"))
                .unwrap();
        assert_eq!(
            super::super::history::built_log::built_through(&entries),
            last,
            "the drop built through the last record"
        );
        let v = super::super::test_support::reopen(tmp.path());
        assert_eq!(v.history().built_through(), last);
    }
}
