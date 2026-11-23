//! The call path as a view host, the CLI and the conformance replay reach it:
//! the tree actor behind its one lock, with the lock discipline in this one
//! place. Under the lock a call decides, appends (no fsync) and applies;
//! durability waits, lag-gate waits and a close's decision run outside it
//! (docs/trees.md, "Recording and durability"). Each hold is measured
//! (`Stages::actor_lock`, and by kind of work in `Stages::lock_hold`), and
//! so is each wait to take the lock (`Stages::lock_wait`).
//!
//! A RELEASE is begun under the lock ([`Recording::start_close_with`]) and
//! its save, a pending save, is decided and landed later by
//! [`SaveToDecide::finish`], on a view's save worker. A file's saves land in
//! the order their closes began: a later one waits on the close epoch, a
//! counter bumped at every landing or failure of a pending save. Lock order:
//! the actor, then the waiters of [`Recording::when_saved_then`], then the
//! epoch (docs/trees.md, "Recording and durability").

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::volume::VolumeIdentity;

use super::history::built::History;
use super::journal::durable::{Durability, RecordNo};
use super::recording::{CloseStart, CloseWork};
use super::stages::{self, Held};
use super::takes::{self, Lands};
use super::{Error, Saved, Took, TreeActor, TreeId, Upkeep};

/// The actor, locked through the call path: [`TreeActor`] by `Deref`. Each
/// hold, from the lock taken to its release, adds one run of
/// `Stages::actor_lock`, with each kind of work's share of it in
/// `Stages::lock_hold`, and reports each tree's overlay to the lag gate
/// before the lock is released. [`ActorGuard::unlocked`] ends one hold and
/// begins another.
pub struct ActorGuard<'a> {
    /// The lock: `Some` except while [`ActorGuard::unlocked`] runs its
    /// work, when the guard itself is borrowed mutably and unreachable.
    guard: Option<MutexGuard<'a, TreeActor>>,
    held: Option<Held>,
    recording: &'a Recording,
}

impl Drop for ActorGuard<'_> {
    /// Ends the hold: see `ActorGuard::release`.
    fn drop(&mut self) {
        self.release();
    }
}

impl<'a> ActorGuard<'a> {
    /// A hold of `recording`'s actor, its lock just taken.
    fn hold(recording: &'a Recording, guard: MutexGuard<'a, TreeActor>) -> Self {
        Self {
            guard: Some(guard),
            held: Some(Held::start()),
            recording,
        }
    }

    /// Ends this hold. Every step and every rebuild at a built event runs
    /// under a guard, so the gate sees each tree's overlay grow at each
    /// step and shrink at each rebuild: event-driven. The overlays are
    /// reported with the actor still locked (lock order: the actor, then
    /// History), then the lock is released, then the hold's measure ends.
    fn release(&mut self) {
        if let Some(guard) = self.guard.take() {
            guard.history().overlays(
                guard
                    .trees
                    .iter()
                    .map(|(tree, saved)| (*tree, saved.overlay_bytes())),
            );
            drop(guard);
        }
        drop(self.held.take());
    }

    /// Runs `work` with the actor's lock released, then takes it again, as
    /// parking_lot's `MutexGuard::unlocked` does: the one way a call path
    /// that holds the lock does slow work without it (a pending fold,
    /// `PendingFold::fold`). Each side is its own measured hold, its
    /// overlays reported as at a drop, and the wait to take the lock again
    /// is measured as every wait is. Whatever the caller read through the
    /// guard before may have moved by the time it returns. The guard cannot
    /// be reached while `work` runs: it is borrowed mutably.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped meanwhile, or its lock was
    /// poisoned; `work` has run, and the guard holds the lock again.
    pub(crate) fn unlocked<R>(&mut self, work: impl FnOnce() -> R) -> Result<R, Error> {
        self.release();
        #[cfg(test)]
        if let Some(hook) = &*self
            .recording
            .in_unlocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            hook();
        }
        let done = work();
        let waiting = std::time::Instant::now();
        let (guard, poisoned) = match self.recording.actor.lock() {
            Ok(guard) => (guard, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        stages::waited(waiting);
        self.held = Some(Held::start());
        let stopped = poisoned || guard.is_stopped();
        self.guard = Some(guard);
        if stopped {
            return Err(Error::Stopped);
        }
        Ok(done)
    }

    /// The lock, held whenever the guard can be reached: only
    /// [`ActorGuard::unlocked`]'s work runs without it, with the guard
    /// borrowed mutably.
    fn locked(&self) -> &MutexGuard<'a, TreeActor> {
        self.guard
            .as_ref()
            .expect("the actor's guard is reached only while it holds the lock")
    }

    /// [`ActorGuard::locked`], mutably.
    fn locked_mut(&mut self) -> &mut MutexGuard<'a, TreeActor> {
        self.guard
            .as_mut()
            .expect("the actor's guard is reached only while it holds the lock")
    }
}

impl Deref for ActorGuard<'_> {
    type Target = TreeActor;

    fn deref(&self) -> &TreeActor {
        self.locked()
    }
}

impl DerefMut for ActorGuard<'_> {
    fn deref_mut(&mut self) -> &mut TreeActor {
        self.locked_mut()
    }
}

/// What runs when a deferred call may go on: given the durability or gate
/// outcome, it sends the FUSE reply. Runs on the journal's waiter thread or
/// the gate's, never with the actor locked.
pub type Then = Box<dyn FnOnce(Result<(), Error>) + Send + 'static>;

/// How a RELEASE goes on after its first half
/// ([`Recording::start_close_with`]): saved already, under the one hold, or
/// a pending save for the view's save worker to decide and land
/// (docs/trees.md, "Pending saves").
#[must_use = "a pending save must be finished or dropped, which leaves it a failed pending save"]
pub enum Closing {
    /// Saved under the lock: an unchanged open, a quick save, an anonymous
    /// or orphaned file's, or one after the tree's failed closes.
    Saved(Saved),
    /// The file is a pending save until [`SaveToDecide::finish`] lands it:
    /// held for takes (model rule 9), its content served to its own client.
    Decide(SaveToDecide),
}

/// A pending save: a closed file's save, begun under the lock and not yet
/// landed. Its open takes no more calls; its file counts as open for every
/// take, sync and landing until it lands; opens by its own client read its
/// pending content ([`crate::session::SessionGuard::open_map`]).
///
/// Dropped unfinished, a panic's unwinding included, it becomes a failed
/// pending save: its handle stays open, saved again at its tree's next step
/// and at restart, and every [`Recording::when_saved_then`] waiting on its
/// file runs.
pub struct SaveToDecide {
    recording: Arc<Recording>,
    handle: u64,
    /// The file it saves, by tree and object: whose waiters run once it
    /// lands or fails.
    file: (TreeId, u64),
    /// The snapshot taken when the close began; None for a close queued
    /// behind an earlier save of its file, which snapshots at its turn.
    work: Option<Box<CloseWork>>,
    /// Finished: its drop leaves the save alone.
    done: bool,
}

impl SaveToDecide {
    /// The open this save closes.
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// Decides the save with the lock released and lands it under the lock.
    /// Same-file order: when an earlier-begun save of this file has not
    /// landed, waits for its landing event (never a timer), then takes its
    /// snapshot. Waits only on saves begun before it, so no two saves wait
    /// on each other. Runs the [`Recording::when_saved_then`] callbacks of
    /// its file once no save of it is pending, after the lock is released.
    /// Never call it with the actor locked.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`]; a failed decision or land, which leaves the save
    /// pending as [`TreeActor::close`]'s failure does.
    pub fn finish(mut self) -> Result<Saved, Error> {
        let recording = Arc::clone(&self.recording);
        let (saved, waiters) = match self.land() {
            Ok(landed) => landed,
            // The actor stopped: nothing lands, and every waiter of the
            // file, and of the close epoch, hears so.
            Err(e) => (Err(e), recording.stopped_settled(self.file)),
        };
        self.done = true;
        recording.run_waiters(waiters);
        saved
    }

    /// The decision and its land. `Ok` carries the save's outcome and the
    /// file's waiters, taken in the landing's hold once no save of the file
    /// is in flight; `Err` only when the actor's lock could not be taken.
    fn land(&mut self) -> Result<(Result<Saved, Error>, Vec<Then>), Error> {
        let recording = Arc::clone(&self.recording);
        let handle = self.handle;
        let work = match self.work.take() {
            Some(work) => *work,
            None => loop {
                let mut actor = recording.actor()?;
                match actor.close_resume(handle) {
                    Ok(CloseStart::Queued) => {
                        // Read under the actor's lock with the state it
                        // checked: a landing after this moves it.
                        let seen = recording.epoch();
                        drop(actor);
                        recording.wait_epoch(seen);
                    }
                    Ok(CloseStart::Decide(work)) => break work,
                    Ok(CloseStart::Done(saved)) => {
                        return Ok((Ok(saved), recording.settled(&actor, self.file)));
                    }
                    Err(e) => return Ok((Err(e), recording.settled(&actor, self.file))),
                }
            },
        };
        let decided = crate::failpoints::hit("tree.close_decide")
            .map_err(Error::from)
            .and_then(|()| {
                #[cfg(test)]
                if let Some(hook) = &*recording
                    .in_decide
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                {
                    hook();
                }
                work.decide()
            });
        let mut actor = recording.actor()?;
        let saved = match decided {
            Ok(decision) => actor.close_finish(decision),
            Err(e) => Err(actor.close_failed(handle, e)),
        };
        Ok((saved, recording.settled(&actor, self.file)))
    }
}

impl Drop for SaveToDecide {
    /// Unfinished, a panic's unwinding included: the save stays pending as
    /// a failed one ([`TreeActor::close_failed`], counted in
    /// `failed_saves`), saved at its tree's next step or at restart, and its
    /// file's [`Recording::when_saved_then`] callbacks run.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let recording = Arc::clone(&self.recording);
        let waiters = match recording.actor() {
            Ok(mut actor) => {
                let _ = actor.close_failed(self.handle, Error::Stopped);
                recording.settled(&actor, self.file)
            }
            Err(_) => recording.stopped_settled(self.file),
        };
        eprintln!(
            "tree SaveFailed: tree={} open={}: dropped unfinished, it stays pending",
            self.file.0, self.handle
        );
        recording.run_waiters(waiters);
    }
}

/// A path call refused, nothing done, because a file it would delete or
/// replace has a pending save: an `unlink(2)`, a `rename(2)` onto it, a
/// whole-file replace of it, a path `truncate(2)`. Made only by the core;
/// handed to [`Recording::when_saved_then`] to retry the call once that
/// save has landed or failed (the owner, 2026-11-23: the delete waits).
#[must_use = "the refused call must be retried once the save lands"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavePending {
    tree: TreeId,
    object: u64,
}

impl SavePending {
    /// The refusal for `object` in `tree`: the core's alone.
    pub(in crate::tree_actor) fn new(tree: TreeId, object: u64) -> Self {
        Self { tree, object }
    }

    /// The tree whose file has the pending save.
    pub fn tree(&self) -> TreeId {
        self.tree
    }

    /// The file's object.
    pub fn object(&self) -> u64 {
        self.object
    }
}

/// What a path call that may delete or replace a file did: ran, or was
/// refused for that file's pending save.
#[must_use = "a refused call must be retried once the save lands"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep<T> {
    /// The call ran: recorded before this returned.
    Done(T),
    /// Nothing was done: retry the call through
    /// [`Recording::when_saved_then`].
    WaitFor(SavePending),
}

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
    /// The operator's rules, read once at open, reached without the actor's lock.
    rules: Arc<agentfs_rules::RuleSet>,
    /// The gate thread, joined at drop: it holds the store (and the state
    /// area's lease) until the builder has stopped.
    gate: Option<std::thread::JoinHandle<()>>,
    /// Tests: runs where a close's decision runs, with the lock released.
    #[cfg(test)]
    in_decide: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Tests: runs as a close or a settle begins to wait on the close
    /// epoch, with the epoch's lock held, so no bump is missed.
    #[cfg(test)]
    in_wait: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Tests: runs inside every [`ActorGuard::unlocked`], lock released.
    #[cfg(test)]
    in_unlocked: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Each file's [`Recording::when_saved_then`] callbacks, run once no
    /// save of it is in flight. Locked only under the actor's lock.
    waiters: Mutex<BTreeMap<(TreeId, u64), Vec<Then>>>,
    /// The close epoch: bumped at every landing or failure of a pending
    /// save; read under the actor's lock with the state a waiter checks.
    epoch: Mutex<u64>,
    epoch_moved: Condvar,
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
        let rules = Arc::clone(actor.rules());
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
            rules,
            gate: Some(gate),
            #[cfg(test)]
            in_decide: Mutex::new(None),
            #[cfg(test)]
            in_wait: Mutex::new(None),
            #[cfg(test)]
            in_unlocked: Mutex::new(None),
            waiters: Mutex::default(),
            epoch: Mutex::new(0),
            epoch_moved: Condvar::new(),
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
        root_owner: Option<FileOwner>,
        service: &PodContext,
    ) -> Result<Arc<Self>, Error> {
        Self::create_with(root, identity, root_owner, service, Upkeep::default())
    }

    /// [`Recording::create`] with the volume's `upkeep`
    /// ([`TreeActor::create_with`]).
    ///
    /// # Errors
    ///
    /// As [`TreeActor::create`].
    pub fn create_with(
        root: &Path,
        identity: &VolumeIdentity,
        root_owner: Option<FileOwner>,
        service: &PodContext,
        upkeep: Upkeep,
    ) -> Result<Arc<Self>, Error> {
        Self::serve(TreeActor::create_with(
            root, identity, root_owner, service, upkeep,
        )?)
    }

    /// Recovers a volume as [`TreeActor::open`] does, then serves it.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::open`].
    pub fn open(root: &Path, service: &PodContext) -> Result<Arc<Self>, Error> {
        Self::open_with(root, service, Upkeep::default())
    }

    /// [`Recording::open`] with the volume's `upkeep`
    /// ([`TreeActor::open_with`]).
    ///
    /// # Errors
    ///
    /// As [`TreeActor::open`].
    pub fn open_with(
        root: &Path,
        service: &PodContext,
        upkeep: Upkeep,
    ) -> Result<Arc<Self>, Error> {
        Self::serve(TreeActor::open_with(root, service, upkeep)?)
    }

    /// The operator's rules, read when the volume opened: where each new
    /// name of a tree lives (docs/trees.md, "Rules and the untracked area").
    pub fn rules(&self) -> &Arc<agentfs_rules::RuleSet> {
        &self.rules
    }

    /// `tree`'s untracked area, made if it has none.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such tree; [`Error::Io`].
    pub fn untracked_area(&self, tree: TreeId) -> Result<super::UntrackedArea, Error> {
        self.actor()?.untracked_area(tree)
    }

    /// The actor, locked, for work that decides, appends and applies. The
    /// actor has no journal fsync and no wait on its call path, so nothing
    /// done through this guard waits on a disk flush or the builder.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped or its lock is poisoned.
    pub fn actor(&self) -> Result<ActorGuard<'_>, Error> {
        let waiting = std::time::Instant::now();
        let actor = self.actor.lock().map_err(|_| Error::Stopped)?;
        stages::waited(waiting);
        let guard = ActorGuard::hold(self, actor);
        if guard.is_stopped() {
            return Err(Error::Stopped);
        }
        Ok(guard)
    }

    /// RELEASE on the caller's thread, for the CLI, tests and recovery:
    /// [`Recording::start_close_with`], then [`SaveToDecide::finish`]. Other
    /// calls of the volume run while the save is decided. The save is
    /// recorded before this returns, or the close stays pending as any
    /// failed close does.
    ///
    /// # Errors
    ///
    /// `check`'s error, with nothing done; as [`TreeActor::close`].
    pub fn close_with(
        self: &Arc<Self>,
        handle: u64,
        check: impl FnOnce(&TreeActor) -> Result<(), Error>,
    ) -> Result<Saved, Error> {
        match self.start_close_with(handle, check)? {
            Closing::Saved(saved) => Ok(saved),
            Closing::Decide(save) => save.finish(),
        }
    }

    /// RELEASE's first half: `check` and [`TreeActor::close_begin`] under one
    /// hold, O(1) plus the file's page writes; no decision runs here. Never
    /// saved ahead of an earlier-begun save of the same file in flight: such
    /// a close is a [`Closing::Decide`] that snapshots at its turn.
    /// [`Recording::close_with`] is this and [`SaveToDecide::finish`] on the
    /// caller's thread.
    ///
    /// # Errors
    ///
    /// `check`'s error, with nothing done; as [`TreeActor::close`] for a
    /// save made under this hold.
    pub fn start_close_with(
        self: &Arc<Self>,
        handle: u64,
        check: impl FnOnce(&TreeActor) -> Result<(), Error>,
    ) -> Result<Closing, Error> {
        let mut actor = self.actor()?;
        check(&actor)?;
        let file = (actor.open_by(handle)?.0, actor.open_object(handle)?);
        let work = match actor.close_begin(handle)? {
            CloseStart::Done(saved) => return Ok(Closing::Saved(saved)),
            CloseStart::Decide(work) => Some(Box::new(work)),
            CloseStart::Queued => None,
        };
        Ok(Closing::Decide(SaveToDecide {
            recording: Arc::clone(self),
            handle,
            file,
            work,
            done: false,
        }))
    }

    /// Runs `then` once no save of `pending`'s file is pending: at once on
    /// this thread when its save has already landed or failed, else on the
    /// thread that lands or fails the last of them (a view's save worker,
    /// or a [`Recording::close_with`] caller), with the actor unlocked. The
    /// check and the registration are under the actor's lock, so no landing
    /// is missed. `then` gets [`Error::Stopped`] when the actor stops first;
    /// it retries the refused call, which may be refused again for a save
    /// begun meanwhile.
    pub fn when_saved_then(&self, pending: SavePending, then: Then) {
        let file = (pending.tree, pending.object);
        let actor = match self.actor() {
            Ok(actor) => actor,
            Err(e) => return then(Err(e)),
        };
        if actor.save_in_flight(file.0, file.1, None) {
            self.waiters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(file)
                .or_default()
                .push(then);
            return;
        }
        drop(actor);
        then(Ok(()));
    }

    /// In a landing's or failure's hold of `actor`: the close epoch bumped,
    /// and `file`'s waiters taken once no save of it is in flight.
    fn settled(&self, actor: &TreeActor, file: (TreeId, u64)) -> Vec<Then> {
        let waiters = if actor.save_in_flight(file.0, file.1, None) {
            Vec::new()
        } else {
            self.take_waiters(file)
        };
        self.bump_epoch();
        waiters
    }

    /// A save ended with the actor stopped or its lock poisoned: `file`'s
    /// waiters taken, and the close epoch bumped, so a close queued behind
    /// it and [`Recording::settle`] wake and hear [`Error::Stopped`] rather
    /// than wait for a landing that never comes.
    fn stopped_settled(&self, file: (TreeId, u64)) -> Vec<Then> {
        let waiters = self.take_waiters(file);
        self.bump_epoch();
        waiters
    }

    /// `file`'s waiters, taken.
    fn take_waiters(&self, file: (TreeId, u64)) -> Vec<Then> {
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&file)
            .unwrap_or_default()
    }

    /// Runs `waiters` with the actor unlocked: each retries its refused
    /// call, or hears [`Error::Stopped`] once the actor has stopped.
    fn run_waiters(&self, waiters: Vec<Then>) {
        if waiters.is_empty() {
            return;
        }
        let stopped = self.is_stopped();
        for then in waiters {
            then(if stopped { Err(Error::Stopped) } else { Ok(()) });
        }
    }

    /// The close epoch now.
    fn epoch(&self) -> u64 {
        *self.epoch.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bumps the close epoch and wakes its waiters.
    fn bump_epoch(&self) {
        let mut epoch = self.epoch.lock().unwrap_or_else(PoisonError::into_inner);
        *epoch = epoch.wrapping_add(1);
        self.epoch_moved.notify_all();
    }

    /// Blocks until the close epoch moves past `seen`: an event, never a
    /// timer.
    fn wait_epoch(&self, seen: u64) {
        let epoch = self.epoch.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        if *epoch == seen
            && let Some(hook) = &*self.in_wait.lock().unwrap_or_else(PoisonError::into_inner)
        {
            hook();
        }
        drop(
            self.epoch_moved
                .wait_while(epoch, |now| *now == seen)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Tests: `hook` runs in every close's decision, with the lock released.
    #[cfg(test)]
    pub(in crate::tree_actor) fn hold_decide_with(&self, hook: impl Fn() + Send + 'static) {
        *self
            .in_decide
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Tests: `hook` runs whenever a close or a settle begins to wait on
    /// the close epoch.
    #[cfg(test)]
    pub(in crate::tree_actor) fn on_wait_with(&self, hook: impl Fn() + Send + 'static) {
        *self
            .in_wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Tests: `hook` runs inside every [`ActorGuard::unlocked`], with the
    /// actor's lock released, before its work.
    #[cfg(test)]
    pub(crate) fn on_unlocked_with(&self, hook: impl Fn() + Send + 'static) {
        *self
            .in_unlocked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// The last record an fsync of `open` must cover, read under the lock
    /// ([`TreeActor::durable_through`]).
    fn durable_through(&self, open: u64) -> Result<RecordNo, Error> {
        self.actor()?.durable_through(open)
    }

    /// FSYNC, `msync(MS_SYNC)` and each write of an `O_SYNC` or `O_DSYNC`
    /// open: every record so far made durable, the file's metadata steps
    /// included. A FLUSH does not wait ([`Recording::flush`]). Locks the actor only to read the last record; the
    /// fsync runs outside the lock.
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

    /// FLUSH (close(2)) of write-open `open`: its records were appended at
    /// each call, so it answers at once, never waiting for an fsync and never
    /// returning a journal fsync's error. A power cut then loses a suffix of
    /// the journal: every record since the last fsync, FSYNCDIR or `O_SYNC`
    /// write that returned, or since the builder's last batch
    /// (docs/trees.md, "Recording and durability"). Takes neither the
    /// actor's lock nor Durability's fsync, so it does not look `open` up: a
    /// view answers a FLUSH of a handle it does not hold itself.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] once a journal fsync has failed: sticky, as for
    /// every later call (`Durability::has_failed`).
    pub fn flush(&self, open: u64) -> Result<(), Error> {
        let _ = open;
        if self.durability.has_failed() {
            return Err(Error::Stopped);
        }
        Ok(())
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
    /// a write-open, `O_TMPFILE`, a link, `mkdir(2)`, `rmdir(2)`,
    /// `rename(2)`, `unlink`, `chmod(2)`, a path `truncate(2)`): returns at once while the tree's lag is under the
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

    /// The lag gate's overlay limit (`agentfs.overlayLimit`, read by the
    /// launcher from the operator settings): over it, a tree's new work waits
    /// for the builder as it does over the lag limit.
    pub fn set_overlay_limit(&self, bytes: u64) {
        self.history.set_overlay_limit(bytes);
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
            let mut actor = self.actor()?;
            actor.on_built();
            actor.queued()
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

    /// Waits until no save begun before it is in flight (being decided or
    /// queued; a failed one waits for a step): those whose close record is
    /// below the journal's next record at its first hold. A close begun
    /// after it, of any file, does not hold it. Then waits until every
    /// record appended by then is built: what a test, the CLI or a replay
    /// calls before it reads history. Never called from a save worker or
    /// with a [`SaveToDecide`] held: it would wait for itself.
    ///
    /// # Errors
    ///
    /// As [`History::wait_built`], as an [`Error`].
    pub fn settle(&self) -> Result<RecordNo, Error> {
        let mut before = None;
        let through = loop {
            let Ok(actor) = self.actor() else {
                break self.durability.appended();
            };
            // Read under the lock: every close begun so far has its record.
            let before = *before.get_or_insert_with(|| self.durability.appended().next());
            if !actor.saves_in_flight(before) {
                break self.durability.appended();
            }
            // Read under the actor's lock with the state it checked.
            let seen = self.epoch();
            drop(actor);
            self.wait_epoch(seen);
        };
        Ok(self.history.wait_built(through)?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, sync_channel};

    use super::super::meta::{Attrs, Perm, SetTime};
    use super::super::test_support::{ATTRS, new_actor, writer};
    use super::*;

    #[test]
    fn a_flush_waits_without_the_actor_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
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
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
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

    /// A bound on a hang, never a coordination: each answer comes on an event.
    const HANG: std::time::Duration = std::time::Duration::from_secs(10);

    /// The fsync's leader held in its fsync: `inside` says it is there, and
    /// it goes on at `release`. Later fsyncs find both ends gone and pass.
    fn hold_fsync(rec: &Recording) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        rec.durability.hold_fsync_with(move || {
            let _ = entered.send(());
            let _ = released.lock().map(|r| r.recv());
        });
        (inside, release)
    }

    /// A served volume at rest, with one write of an open not yet durable.
    fn unsynced_write(state: &std::path::Path) -> (Arc<Recording>, u64) {
        let rec = Recording::serve(new_actor(state)).unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        // The create is a step: once it is built, the builder has made its
        // fsync and waits for the next step, so the next fsync is the test's.
        rec.settle().unwrap();
        rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
        (rec, h)
    }

    /// close(2) does not wait for the disk: with the journal's fsync held
    /// and an FSYNCDIR waiting on it, a FLUSH of the open answers.
    #[test]
    fn a_close_answers_while_the_fsync_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let (rec, h) = unsynced_write(tmp.path());
        let (inside, release) = hold_fsync(&rec);
        std::thread::scope(|scope| {
            let rec = &rec;
            let (synced, sync) = mpsc::channel();
            scope.spawn(move || synced.send(rec.durable_all()).unwrap());
            inside.recv().unwrap();
            let (flushed, flush) = mpsc::channel();
            scope.spawn(move || flushed.send(rec.flush(h)).unwrap());
            let answered = flush.recv_timeout(HANG);
            let waiting = sync.try_recv().is_err();
            // Released before any assertion, so a failure never hangs the scope.
            release.send(()).unwrap();
            assert!(
                matches!(answered, Ok(Ok(()))),
                "the flush waited: {answered:?}"
            );
            assert!(waiting, "the FSYNCDIR answered inside the held fsync");
            sync.recv().unwrap().unwrap();
        });
        assert_eq!(rec.durability.mark(), rec.durability.appended());
    }

    /// A journal fsync's failure is never a FLUSH's own error: before any
    /// failure it answers, and once one has stopped the actor it says so.
    #[test]
    fn flush_reports_only_a_stopped_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let (rec, h) = unsynced_write(tmp.path());
        assert!(rec.durability.mark() < rec.durability.appended());
        assert!(matches!(rec.flush(h), Ok(())));
        assert!(
            rec.durability.mark() < rec.durability.appended(),
            "the flush made no fsync"
        );
        rec.durability
            .fsync_failed(&std::io::Error::from_raw_os_error(libc::EIO));
        let stopped = rec.flush(h);
        assert!(matches!(stopped, Err(Error::Stopped)), "{stopped:?}");
        assert_eq!(stopped.unwrap_err().errno(), libc::EIO);
    }

    /// fsync(2), and so FSYNCDIR and an `O_SYNC` write, still wait for the
    /// disk: with the journal's fsync held, none has answered.
    #[test]
    fn fsync_still_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let (rec, h) = unsynced_write(tmp.path());
        let through = rec.durability.appended();
        let (inside, release) = hold_fsync(&rec);
        let (answered, answer) = mpsc::channel();
        let (all_answered, all_answer) = mpsc::channel();
        // Queued for the waiter thread, which leads the fsync held.
        rec.durable_then(h, Box::new(move |r| answered.send(r.is_ok()).unwrap()));
        inside.recv().unwrap();
        rec.durable_all_then(Box::new(move |r| all_answered.send(r.is_ok()).unwrap()));
        let waiting = answer.try_recv().is_err() && all_answer.try_recv().is_err();
        let unmoved = rec.durability.mark() < through;
        // Released before any assertion, so a failure never hangs the drop.
        release.send(()).unwrap();
        assert!(waiting, "an fsync answered inside the held fsync");
        assert!(unmoved, "the mark moved inside the held fsync");
        assert_eq!(answer.recv_timeout(HANG), Ok(true));
        assert_eq!(all_answer.recv_timeout(HANG), Ok(true));
        assert!(rec.durability.mark() >= through);
    }

    /// fsync(2) covers a file's metadata: a chmod and a `futimens` of the
    /// open's file, each a step of its own, are inside the flush's records.
    #[test]
    fn a_flush_covers_its_files_metadata_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let mut actor = rec.actor().unwrap();
        let h = actor
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        actor.write(h, 0, b"x", false).unwrap();
        let written = actor.durable_through(h).unwrap();
        let exec = Attrs {
            perm: Perm::from_mode(0o755),
            ..ATTRS
        };
        actor
            .set_meta(TreeId::Root, "a", exec, &writer("w"))
            .unwrap();
        let chmod = actor.durable_through(h).unwrap();
        assert!(chmod > written, "the chmod's step is covered");
        actor
            .set_times(TreeId::Root, "a", Some(SetTime::Now), None, &writer("w"))
            .unwrap();
        assert!(actor.durable_through(h).unwrap() > chmod);
        assert_eq!(actor.durable_through(h).unwrap(), rec.durability.appended());
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
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
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
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
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

    /// A saved file `a` of ten bytes, and an open of it whose close keeps the
    /// length: decided with the lock released.
    fn held_close(rec: &Arc<Recording>) -> u64 {
        let w = writer("w");
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &w, Some(ATTRS))
            .unwrap();
        rec.actor()
            .unwrap()
            .write(h, 0, b"0123456789", false)
            .unwrap();
        rec.close_with(h, |_| Ok(())).unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &w, None)
            .unwrap();
        rec.actor().unwrap().write(h, 2, b"AB", false).unwrap();
        h
    }

    /// The first close's decision waits for `release`; `inside` says it is
    /// there. Later decisions pass.
    fn hold_first_decide(rec: &Recording) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, inside) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let first = std::sync::atomic::AtomicBool::new(true);
        rec.hold_decide_with(move || {
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = entered.send(());
                let _ = released.lock().map(|r| r.recv());
            }
        });
        (inside, release)
    }

    /// A3: while one close is being decided, the actor's lock is free and
    /// another client's open, write and close complete.
    #[test]
    fn a_close_decides_with_the_lock_released() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let (inside, release) = hold_first_decide(&rec);
        std::thread::scope(|scope| {
            let closing = scope.spawn(|| rec.close_with(h, |_| Ok(())));
            inside.recv().unwrap();
            if rec.actor.try_lock().is_err() {
                // Let the close go first, so the failure is a failure, not a hang.
                release.send(()).unwrap();
                let _ = closing.join();
                panic!("the lock is held in the decision");
            }
            let other = writer("other");
            let g = rec
                .actor()
                .unwrap()
                .write_open(TreeId::Root, "b", &other, Some(ATTRS))
                .unwrap();
            rec.actor().unwrap().write(g, 0, b"b", false).unwrap();
            let saved = rec.close_with(g, |_| Ok(())).unwrap();
            assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
            release.send(()).unwrap();
            let saved = closing.join().unwrap().unwrap();
            assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
        });
        let actor = rec.actor().unwrap();
        assert_eq!(actor.read(TreeId::Root, "a").unwrap(), b"01AB456789");
        assert_eq!(actor.read(TreeId::Root, "b").unwrap(), b"b");
    }

    /// A second open of `a`, by another client, its write `bytes` at 2.
    fn second_open(rec: &Recording, bytes: &[u8]) -> u64 {
        let g = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "a", &writer("other"), None)
            .unwrap();
        rec.actor().unwrap().write(g, 2, bytes, false).unwrap();
        g
    }

    /// Says on the returned receiver whenever a close or a settle begins
    /// to wait on the close epoch.
    fn note_waits(rec: &Recording) -> mpsc::Receiver<()> {
        let (waiting, waits) = mpsc::channel::<()>();
        let waiting = Mutex::new(waiting);
        rec.on_wait_with(move || {
            let _ = waiting.lock().map(|w| w.send(()));
        });
        waits
    }

    /// A close of a file whose earlier-begun save is being decided waits
    /// for that save's landing, then saves on it: the later close's bytes
    /// win where the two overlap.
    #[test]
    fn a_queued_close_waits_for_the_earlier_landing() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let g = second_open(&rec, b"zz");
        let (inside, release) = hold_first_decide(&rec);
        let waits = note_waits(&rec);
        let first = {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || rec.close_with(h, |_| Ok(())))
        };
        inside.recv_timeout(HANG).unwrap();
        let second = {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || rec.close_with(g, |_| Ok(())))
        };
        waits
            .recv_timeout(HANG)
            .expect("the later close waits its turn");
        release.send(()).unwrap();
        assert!(matches!(first.join().unwrap(), Ok(Saved::Commit(_))));
        assert!(matches!(second.join().unwrap(), Ok(Saved::Commit(_))));
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "a").unwrap(),
            b"01zz456789"
        );
    }

    /// `settle` waits only for the closes begun before it: a save of
    /// another file begun while it waits does not hold it, so a workload
    /// that keeps closing files cannot keep a settle waiting.
    #[test]
    fn settle_waits_only_for_closes_begun_before_it() {
        const BOUND: std::time::Duration = std::time::Duration::from_secs(60);
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let w = writer("w");
        let g = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "b", &w, Some(ATTRS))
            .unwrap();
        rec.actor().unwrap().write(g, 0, b"0123", false).unwrap();
        rec.close_with(g, |_| Ok(())).unwrap();
        let g = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "b", &w, None)
            .unwrap();
        rec.actor().unwrap().write(g, 0, b"BB", false).unwrap();
        let (inside, release) = hold_first_decide(&rec);
        let waits = note_waits(&rec);
        let first = {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || rec.close_with(h, |_| Ok(())))
        };
        inside.recv_timeout(HANG).unwrap();
        let (answer, answered) = mpsc::channel();
        {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || answer.send(rec.settle()));
        }
        waits.recv_timeout(HANG).expect("the settle waits");
        let Closing::Decide(later) = rec.start_close_with(g, |_| Ok(())).unwrap() else {
            panic!("b's close saved at its start");
        };
        release.send(()).unwrap();
        assert!(matches!(first.join().unwrap(), Ok(Saved::Commit(_))));
        let settled = answered.recv_timeout(BOUND);
        // Released either way, so a failure is a failure, not a hang.
        assert!(matches!(later.finish(), Ok(Saved::Commit(_))));
        assert!(
            matches!(settled, Ok(Ok(_))),
            "settle waited for a close begun after it: {settled:?}"
        );
    }

    /// The actor stops while a save is decided: the close of its file
    /// queued behind it, and a settle, wake and hear so rather than wait for
    /// a landing that never comes.
    #[test]
    fn a_stop_mid_decision_wakes_a_queued_close_and_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let g = second_open(&rec, b"zz");
        let (inside, release) = hold_first_decide(&rec);
        let waits = note_waits(&rec);
        let first = {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || rec.close_with(h, |_| Ok(())))
        };
        inside.recv_timeout(HANG).unwrap();
        let (answer, answered) = mpsc::channel();
        {
            let rec = Arc::clone(&rec);
            let answer = answer.clone();
            std::thread::spawn(move || answer.send(rec.close_with(g, |_| Ok(())).map(drop)));
        }
        waits
            .recv_timeout(HANG)
            .expect("the later close waits its turn");
        {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || answer.send(rec.settle().map(drop)));
        }
        waits.recv_timeout(HANG).expect("the settle waits");
        rec.durability
            .fsync_failed(&std::io::Error::from_raw_os_error(libc::EIO));
        release.send(()).unwrap();
        assert!(matches!(first.join().unwrap(), Err(Error::Stopped)));
        let mut heard = [
            answered.recv_timeout(HANG).expect("a waiter woke"),
            answered.recv_timeout(HANG).expect("both waiters woke"),
        ];
        heard.sort_by_key(Result::is_ok);
        assert!(matches!(heard[0], Err(Error::Stopped)), "{heard:?}");
    }

    /// The overlay is let go once the builder catches up, though the last
    /// record is no step: a batch reads its record to build through after
    /// the last save, and a write to another open is appended before that
    /// batch's built event. The saves' entries are sealed at the write's
    /// record, which no step asks the builder to build; the actor asks for
    /// it, so the overlay empties with no step after it and no settle.
    #[test]
    fn the_overlay_goes_when_the_last_record_is_no_step() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The view's take worker: at each built event it moves the actor
        // onto the built point.
        let worker = {
            let (rec, stop) = (Arc::clone(&rec), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut epoch = 0;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    rec.progress().unwrap();
                    epoch = rec.wait_for_work(epoch);
                }
            })
        };
        let w = writer("w");
        let log = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "log", &w, Some(ATTRS))
            .unwrap();
        rec.settle().unwrap();
        // Quiet before the hook goes in: every built event taken by the
        // actor and nothing left for the builder. A batch begun before the
        // hook (one the actor asked for at the create's built event) would
        // take no gate, and its built event, taken after the saves and
        // before the write, would seal the saves' entries short of the
        // write: the overlay would go at a build that stops before it.
        let history = rec.history().clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut epoch = 0;
        while history.overlay_bytes(TreeId::Root) > 0
            || history.built_through() < rec.durability.appended()
        {
            assert!(std::time::Instant::now() < deadline, "never quiet");
            epoch = history.wait_epoch_for(epoch, std::time::Duration::from_millis(100));
        }
        let (wrote, written) = mpsc::channel::<RecordNo>();
        let (open_gate, gate) = mpsc::channel::<()>();
        let last_save = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        {
            let (writer_rec, last_save) = (Arc::clone(&rec), Arc::clone(&last_save));
            let (mut gate, mut once) = (Some(gate), Some(wrote));
            rec.history().on_batch_read(move |read| match read {
                // The next batch waits at its start until every save is
                // appended: no built event comes between the saves and the
                // write, so no rebuild dates the saves' entries before it.
                None => {
                    if let Some(gate) = gate.take() {
                        let _ = gate.recv();
                    }
                }
                // The batch that reads through the last save: the write
                // comes after its read and before its built event.
                Some(through) => {
                    if through.get() >= last_save.load(std::sync::atomic::Ordering::Acquire)
                        && let Some(wrote) = once.take()
                    {
                        let mut actor = writer_rec.actor().unwrap();
                        actor.write(log, 0, b"a line", false).unwrap();
                        let _ = wrote.send(actor.durability().appended());
                    }
                }
            });
        }
        {
            let mut actor = rec.actor().unwrap();
            for i in 0..20 {
                let h = actor
                    .write_open(TreeId::Root, &format!("f{i}"), &w, Some(ATTRS))
                    .unwrap();
                actor.write(h, 0, b"saved", false).unwrap();
                actor.close(h).unwrap();
            }
            last_save.store(
                actor.durability().appended().get(),
                std::sync::atomic::Ordering::Release,
            );
        }
        open_gate.send(()).unwrap();
        let call = written
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        let history = rec.history().clone();
        // Event-driven: each built event wakes this; the deadline only
        // guards against a hang.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut epoch = 0;
        while history.overlay_bytes(TreeId::Root) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the overlay stayed at {} B, built through {:?}, the write at {call:?}",
                history.overlay_bytes(TreeId::Root),
                history.built_through()
            );
            epoch = history.wait_epoch_for(epoch, std::time::Duration::from_millis(100));
        }
        assert!(history.built_through() >= call);
        rec.actor().unwrap().close(log).unwrap();
        rec.settle().unwrap();
        history.on_batch_read(|_| {});
        stop.store(true, std::sync::atomic::Ordering::Release);
        rec.wake_workers();
        worker.join().unwrap();
    }

    /// A write to the open being closed, while its close is decided with
    /// the lock released, is refused: the decision is on the calls the open
    /// had, so an accepted write would be journalled and left out of the
    /// save.
    #[test]
    fn a_write_to_an_open_being_closed_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let (inside, release) = hold_first_decide(&rec);
        let saved = std::thread::scope(|scope| {
            let closing = scope.spawn(|| rec.close_with(h, |_| Ok(())));
            inside.recv().unwrap();
            let late = rec.actor().unwrap().write(h, 5, b"ZZ", false);
            release.send(()).unwrap();
            assert!(matches!(late, Err(Error::NotFound)), "{late:?}");
            closing.join().unwrap().unwrap()
        });
        assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
        rec.settle().unwrap();
        let actor = rec.actor().unwrap();
        assert_eq!(actor.read(TreeId::Root, "a").unwrap(), b"01AB456789");
        assert_eq!(
            actor.verification().counts().unresolved,
            0,
            "no save is flagged"
        );
    }

    /// A5: another client's mapping writes the file's pages back while a
    /// close is being decided (a page write does not wait for the file's
    /// pending save): the decision is stale, the close is redone under the
    /// lock with the page write, and its save is in the journal before
    /// `close_with` returns.
    #[test]
    fn a_close_whose_file_moved_meanwhile_is_redone_and_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let h = held_close(&rec);
        let (inside, release) = hold_first_decide(&rec);
        let saved = std::thread::scope(|scope| {
            let closing = scope.spawn(|| rec.close_with(h, |_| Ok(())));
            inside.recv().unwrap();
            let other = writer("other");
            let g = rec
                .actor()
                .unwrap()
                .write_open(TreeId::Root, "a", &other, None)
                .unwrap();
            rec.actor().unwrap().page_writeback(g, 0, b"zz").unwrap();
            release.send(()).unwrap();
            closing.join().unwrap().unwrap()
        });
        let Saved::Commit(step) = saved else {
            panic!("the close saves: {saved:?}");
        };
        let (_, frames) = super::super::journal::Journal::open(
            &tmp.path().join(super::super::JOURNAL),
            RecordNo::ZERO,
        )
        .unwrap();
        let recorded = frames.iter().any(|(record, _)| {
            record.seq == step
                && matches!(
                    &record.entry,
                    super::super::journal::Entry::Step {
                        change: super::super::journal::Change::Save {
                            handle,
                            outcome: super::super::journal::SaveOutcome::Saved { len: 10, .. },
                            ..
                        },
                        ..
                    } if *handle == h
                )
        });
        assert!(recorded, "the close's save is in the journal");
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "a").unwrap(),
            b"zzAB456789"
        );
    }

    #[test]
    fn the_builder_never_reads_past_the_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        for name in ["a", "b", "c"] {
            let h = rec
                .actor()
                .unwrap()
                .write_open(TreeId::Root, name, &writer("w"), Some(ATTRS))
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
                .write_open(TreeId::Root, &format!("f{n}"), &writer("w"), Some(ATTRS))
                .unwrap();
            rec.actor().unwrap().write(h, 0, b"x", false).unwrap();
            rec.actor().unwrap().close(h).unwrap();
        }
        // Calls after the last step: no step asks the builder for them.
        rec.settle().unwrap();
        let h = rec
            .actor()
            .unwrap()
            .write_open(TreeId::Root, "f0", &writer("w"), None)
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

    /// A session bound to write the root as client `label`.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test is its own composition root"
    )]
    fn client(rec: &Arc<Recording>, label: &str) -> crate::session::TreeSession {
        use agentfs_contracts::caps::{ClientCap, Minter};
        use agentfs_contracts::clients::{ClientId, Ordinal};
        use agentfs_contracts::name::{ClientLabel, LauncherId};
        use agentfs_contracts::pod::WriterAt;
        use agentfs_contracts::ports::{SessionAuthority, SessionBinder};
        let mut sessions = crate::session::Sessions::new(Arc::clone(rec));
        let id = ClientId {
            launcher: LauncherId::parse("cli").unwrap(),
            tree: TreeId::Root,
            label: ClientLabel::parse(label).unwrap(),
            n: Ordinal(1),
        };
        let writes_as = WriterAt {
            writer: id.writer(),
            at: PodContext::default(),
        };
        let session = sessions.open();
        let cap = ClientCap::mint(&Minter::new(), id, TreeId::Root, writes_as);
        sessions.bind(session, SessionAuthority::Write(cap));
        sessions.session(session).unwrap()
    }

    /// `f` saved as `00000000` by `session`'s client, then two of its opens
    /// of `f`, `AA` written at 0 through the first and `BB` at 4 through the
    /// second, each closed and its save held pending: an open by the client
    /// must fold both.
    fn two_pending_saves(session: &crate::session::TreeSession) -> (SaveToDecide, SaveToDecide) {
        let (one, two) = {
            let mut guard = session.lock().unwrap();
            let h = guard.write_open("f", Some(ATTRS)).unwrap();
            guard.write(h, 0, b"00000000", false).unwrap();
            guard.close(h).unwrap();
            let one = guard.write_open("f", None).unwrap();
            guard.write(one, 0, b"AA", false).unwrap();
            let two = guard.write_open("f", None).unwrap();
            guard.write(two, 4, b"BB", false).unwrap();
            (one, two)
        };
        let mut held = [one, two].map(|h| match session.start_close(h).unwrap() {
            Closing::Decide(save) => Some(save),
            Closing::Saved(saved) => panic!("the close saved at its start: {saved:?}"),
        });
        (held[0].take().unwrap(), held[1].take().unwrap())
    }

    /// An open of a file with two pending saves by its own client folds
    /// them with the volume's lock released: inside the fold the lock is
    /// free, and the open reads both saves in close order.
    #[test]
    fn an_opens_fold_runs_with_the_lock_released() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let a = client(&rec, "a");
        let (first, second) = two_pending_saves(&a);
        let (seen, free) = mpsc::channel::<bool>();
        let seen = Mutex::new(seen);
        let weak = Arc::downgrade(&rec);
        rec.on_unlocked_with(move || {
            let unlocked = weak
                .upgrade()
                .is_some_and(|rec| rec.actor.try_lock().is_ok());
            let _ = seen.lock().map(|s| s.send(unlocked));
        });
        let map = a.lock().unwrap().open_map("f").unwrap();
        assert_eq!(map.to_vec().unwrap(), b"AA00BB00");
        assert_eq!(free.try_recv(), Ok(true), "the fold ran under the lock");
        assert!(free.try_recv().is_err(), "the lock was released twice");
        first.finish().unwrap();
        second.finish().unwrap();
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "f").unwrap(),
            b"AA00BB00"
        );
    }

    /// A write-open's fold runs with the lock released, and a page write of
    /// the file made meanwhile by another client's open is on its view, as
    /// it is on every open view of the file at its call.
    #[test]
    fn a_write_opens_fold_keeps_page_writes_made_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let a = client(&rec, "a");
        let (first, second) = two_pending_saves(&a);
        let (seen, free) = mpsc::channel::<bool>();
        let seen = Mutex::new(seen);
        let weak = Arc::downgrade(&rec);
        rec.on_unlocked_with(move || {
            let Some(rec) = weak.upgrade() else { return };
            let mut actor = rec.actor().unwrap();
            let g = actor
                .write_open(TreeId::Root, "f", &writer("other"), None)
                .unwrap();
            actor.page_writeback(g, 8, b"PP").unwrap();
            let _ = seen.lock().map(|s| s.send(true));
        });
        let mut guard = a.lock().unwrap();
        let h = guard.write_open("f", None).unwrap();
        assert_eq!(free.try_recv(), Ok(true), "the fold ran under the lock");
        assert_eq!(guard.read_open(h).unwrap().to_vec().unwrap(), b"AA00BB00PP");
        drop(guard);
        first.finish().unwrap();
        second.finish().unwrap();
    }

    /// A failed fold closes the write-open as a client's end closes an
    /// open: saved, unchanged, after the pending saves it would have
    /// folded, and never a failed save of its own.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_failed_fold_closes_the_write_open_after_the_saves_it_folded() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let a = client(&rec, "a");
        let (first, second) = two_pending_saves(&a);
        {
            let _fold = crate::failpoints::configure("tree.pending_fold=error").unwrap();
            assert!(a.lock().unwrap().write_open("f", None).is_err());
        }
        let metrics = rec.actor().unwrap().metrics();
        assert_eq!(
            (metrics.failed_saves, metrics.pending_saves),
            (0, 3),
            "the open is held behind both saves, not failed"
        );
        first.finish().unwrap();
        second.finish().unwrap();
        let actor = rec.actor().unwrap();
        assert_eq!(actor.metrics().pending_saves, 0);
        assert_eq!(actor.metrics().failed_saves, 0);
        assert_eq!(actor.read(TreeId::Root, "f").unwrap(), b"AA00BB00");
    }

    /// A client's end while its write-open folds with the lock released:
    /// the open is closed with the client's, and the write-open answers
    /// NotFound rather than hand out an open already closing. Also when an
    /// earlier open's close record could not be appended, which leaves the
    /// rest of the client's opens pending with no record.
    #[cfg(feature = "failpoints")]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a client's end, below the sessions that make one"
    )]
    fn a_client_end_during_a_write_opens_fold_answers_not_found() {
        for append_fails in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let rec = Recording::serve(new_actor(tmp.path())).unwrap();
            let a = client(&rec, "a");
            let (first, second) = two_pending_saves(&a);
            let older = a.lock().unwrap().write_open("g", Some(ATTRS)).unwrap();
            let by = rec.actor().unwrap().open_by(older).unwrap().1.clone();
            let weak = Arc::downgrade(&rec);
            rec.on_unlocked_with(move || {
                let Some(rec) = weak.upgrade() else { return };
                let _append = append_fails
                    .then(|| crate::failpoints::configure("tree.wal_before_header=error").unwrap());
                let ended = rec.actor().unwrap().close_writer(TreeId::Root, &by);
                assert_eq!(ended.is_err(), append_fails, "{ended:?}");
            });
            let late = a.lock().unwrap().write_open("f", None);
            assert!(matches!(late, Err(Error::NotFound)), "{late:?}");
            rec.on_unlocked_with(|| {});
            first.finish().unwrap();
            second.finish().unwrap();
            if append_fails {
                rec.actor()
                    .unwrap()
                    .mkdir(TreeId::Root, "d", ATTRS, &writer("w"))
                    .unwrap();
            }
            let actor = rec.actor().unwrap();
            assert_eq!(actor.metrics().pending_saves, 0);
            assert_eq!(actor.read(TreeId::Root, "f").unwrap(), b"AA00BB00");
        }
    }

    /// The actor stopping while the lock is released is the answer of the
    /// work's guard: the caller learns it before it uses what it read.
    #[test]
    fn a_stop_while_the_lock_is_released_is_the_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let mut guard = rec.actor().unwrap();
        let ran = guard.unlocked(|| {
            rec.durability
                .fsync_failed(&std::io::Error::from_raw_os_error(libc::EIO));
        });
        assert!(matches!(ran, Err(Error::Stopped)), "{ran:?}");
    }

    /// A path truncate of a file its client has failed saves of reads no
    /// fold of them: a fold that would fail does not fail it, and the
    /// truncate lands after both saves.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_path_truncate_reads_no_fold_of_its_clients_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recording::serve(new_actor(tmp.path())).unwrap();
        let a = client(&rec, "a");
        let (first, second) = two_pending_saves(&a);
        {
            let _decide = crate::failpoints::configure("tree.close_decide=error").unwrap();
            assert!(first.finish().is_err());
        }
        // Dropped unfinished: failed too.
        drop(second);
        let metrics = rec.actor().unwrap().metrics();
        assert_eq!((metrics.failed_saves, metrics.pending_saves), (2, 2));
        {
            let _fold = crate::failpoints::configure("tree.pending_fold=error@*").unwrap();
            let truncated = a.lock().unwrap().truncate_path("f", 6).unwrap();
            assert!(matches!(truncated, PathStep::Done(())));
        }
        assert_eq!(
            rec.actor().unwrap().read(TreeId::Root, "f").unwrap(),
            b"AA00BB"
        );
    }
}
