//! History as built: the one owner of a tree volume's Git store. The builder
//! makes each durable step's commits behind the call path; History hands out
//! only what is built, wakes waiters on built and failed events (never a
//! timer), and measures each tree's lag for the lag gate (docs/trees.md,
//! "History trails the files").
//!
use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use git2::Oid;

use super::super::TreeId;
use super::super::journal::durable::RecordNo;
use super::{Objects, refs};
use crate::tree_actor::call_path::Then;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::FileMap;
use crate::tree_actor::tree::EventNumber;
use crate::tree_git::TreeGit;

/// Why the builder could not build a batch. Recording goes on; the batch is
/// retried at the next step appended or the next wait.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The state area is full.
    #[error("history build: no space")]
    NoSpace,
    /// A read or write of the store or the built log failed with this errno.
    #[error("history build: errno {0}")]
    Io(i32),
    /// The builder's replay of this step disagrees with its record (outcome,
    /// length or checksum): building stops with the alarm until an operator
    /// acts (docs/runbooks/recording-failures.md).
    #[error("history build disagrees with step {}", .0.get())]
    Disagrees(RecordNo),
    /// A commit the built log or a checkpoint names is not in the store.
    #[error("history build: object {0} is missing")]
    Missing(Oid),
    /// `git fast-import` exited non-zero, or was killed, with its stderr's first line.
    #[error("history build: git fast-import failed: {0}")]
    Packer(String),
}

impl From<io::Error> for BuildError {
    fn from(e: io::Error) -> Self {
        match e.raw_os_error() {
            Some(libc::ENOSPC) => Self::NoSpace,
            errno => Self::Io(errno.unwrap_or(libc::EIO)),
        }
    }
}

impl From<crate::tree_git::Error> for BuildError {
    fn from(e: crate::tree_git::Error) -> Self {
        match e {
            crate::tree_git::Error::NoSpace(_) => Self::NoSpace,
            crate::tree_git::Error::Io(e) => e.into(),
            other => Self::Packer(other.to_string()),
        }
    }
}

impl From<crate::tree_actor::Error> for BuildError {
    fn from(e: crate::tree_actor::Error) -> Self {
        match e {
            crate::tree_actor::Error::Io(e) => e.into(),
            crate::tree_actor::Error::TreeGit(e) => e.into(),
            crate::tree_actor::Error::MissingObject { oid, .. } => Self::Missing(oid),
            other => Self::Packer(other.to_string()),
        }
    }
}

/// Why a wait for the built point ended without reaching it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// The builder failed on a batch at or before the record waited for.
    #[error(transparent)]
    Failed(BuildError),
    /// The volume is shutting down or its actor stopped.
    #[error("the tree actor has stopped")]
    Stopped,
}

/// The builder's health, one state machine (lag over the alarm level, over
/// the limit, a failed batch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Current,
    /// Some tree's lag is over the alarm level: `tree HistoryLagging` is logged once.
    Lagging,
    /// Some tree's lag is over the limit: that tree's gated calls wait.
    Stalled,
    /// The last batch failed; the gate holds nothing while failed.
    Failed(BuildError),
}

/// One tree's lag: the builder's estimated work for its steps past the built point.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeLag {
    pub tree: TreeId,
    /// Estimated seconds: each unbuilt step's fixed cost plus its bytes over
    /// the throughput, both measured on the last batch.
    pub seconds: f64,
    pub steps: u64,
}

/// What the lag gauge reports: per tree, the oldest unbuilt step's age, health.
#[derive(Clone, Debug, PartialEq)]
pub struct Lag {
    pub trees: Vec<TreeLag>,
    /// The age of the oldest durable step not yet built: the alert's gauge.
    pub oldest_unbuilt: Option<Duration>,
    pub health: Health,
}

/// What History shares with the builder and every snapshot.
pub(in crate::tree_actor) struct Inner {
    /// The one handle on the store; reads only, refreshed at each built event.
    repo: TreeGit,
    state: Mutex<State>,
    changed: Condvar,
}

/// History's state, behind one lock. Every change notifies `changed`.
struct State {
    /// The built point.
    built: RecordNo,
    /// Every pointer as built.
    pointers: Arc<Pointers>,
    /// Each direct save's commit, by its step: what a sync asks of a save.
    owners: Arc<BTreeMap<EventNumber, Oid>>,
    /// Each tree's object numbers as built.
    objects: Arc<BuiltObjects>,
    failed: Option<BuildError>,
    /// The request count the failed batch started at: a wait asked after it
    /// waits for the retry.
    failed_at: u64,
    /// The highest record the builder is asked to build through.
    wanted: RecordNo,
    /// Bumped at each request: a failed builder retries at the next one.
    requests: u64,
    /// Bumped at each built or failed event.
    epoch: u64,
    /// When the first step past the built point was appended.
    oldest_unbuilt: Option<Instant>,
    /// Steps appended past the built point, with when each was appended.
    unbuilt: std::collections::VecDeque<(RecordNo, Instant)>,
    /// The volume is shutting down: build what is wanted, then stop.
    stopping: bool,
    /// The builder has exited.
    stopped: bool,
    /// Each tree's lag and the builder's rates.
    gate: super::lag::LagGate,
    /// The journal's last checkpoint's redo start: built-log entries at or
    /// before it name nothing a restart reads.
    checkpointed: RecordNo,
    /// Built files' maps the actor has not taken yet, in the order built:
    /// each save, create and take, and each map rebased onto its checkout.
    maps: Vec<(TreeId, u64, FileMap)>,
    /// Gated calls waiting for their tree's lag to fall, in arrival order:
    /// the one gate thread answers them ([`History::serve_gate`]).
    gated: Vec<(TreeId, Then)>,
}

/// The built state at one built point: heads, refs and the commits behind
/// them. Only History constructs one, and only at or before the built point,
/// so nothing past it is reachable (the private fields are the guarantee).
pub struct BuiltSnapshot {
    inner: Arc<Inner>,
    at: RecordNo,
    pointers: Arc<Pointers>,
    owners: Arc<BTreeMap<EventNumber, Oid>>,
    objects: Arc<BuiltObjects>,
}

/// Every tree's object numbers at one built point.
pub(in crate::tree_actor) type BuiltObjects = BTreeMap<TreeId, Arc<Objects>>;

impl BuiltSnapshot {
    /// The built point this snapshot is at.
    pub fn at(&self) -> RecordNo {
        self.at
    }

    /// The tree's head commit as built, or none for a tree not built yet.
    pub fn head(&self, tree: TreeId) -> Option<Oid> {
        self.pointers.get(&refs::branch(tree))
    }

    /// Every ref as built: name and value, in name order.
    pub fn refs(&self) -> Vec<(String, Oid)> {
        self.pointers
            .iter()
            .map(|(name, oid)| (name.clone(), *oid))
            .collect()
    }

    /// The store, for reads of what this snapshot reaches.
    pub(in crate::tree_actor) fn repo(&self) -> &TreeGit {
        &self.inner.repo
    }

    pub(in crate::tree_actor) fn pointers(&self) -> &Pointers {
        &self.pointers
    }

    pub(in crate::tree_actor) fn owners(&self) -> &BTreeMap<EventNumber, Oid> {
        &self.owners
    }

    /// `tree`'s object numbers as built; none for a tree not built.
    pub(in crate::tree_actor) fn objects(&self, tree: TreeId) -> &Objects {
        static NONE: Objects = Objects::new();
        self.objects.get(&tree).map_or(&NONE, |o| o)
    }

    /// Every tree's object numbers as built.
    pub(in crate::tree_actor) fn all_objects(&self) -> BTreeMap<TreeId, Objects> {
        self.objects
            .iter()
            .map(|(tree, objects)| (*tree, (**objects).clone()))
            .collect()
    }

    /// `tree`'s queue as built: each entry's ref and commit, in queue order.
    pub(in crate::tree_actor) fn queue(&self, tree: TreeId) -> Vec<(String, Oid)> {
        self.pointers.under(&refs::queue(tree))
    }
}

/// The one owner of the Git store. Takes, push, the CLI and tests reach
/// history through it; nothing else opens the store.
#[derive(Clone)]
pub struct History {
    inner: Arc<Inner>,
}

impl History {
    /// History over `repo`, built through `built` with these pointers.
    pub(in crate::tree_actor) fn new(
        repo: TreeGit,
        built: RecordNo,
        pointers: Pointers,
        owners: BTreeMap<EventNumber, Oid>,
        objects: BuiltObjects,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                repo,
                state: Mutex::new(State {
                    built,
                    pointers: Arc::new(pointers),
                    owners: Arc::new(owners),
                    objects: Arc::new(objects),
                    failed: None,
                    failed_at: 0,
                    wanted: built,
                    requests: 0,
                    epoch: 0,
                    oldest_unbuilt: None,
                    unbuilt: std::collections::VecDeque::new(),
                    stopping: false,
                    stopped: false,
                    checkpointed: RecordNo::ZERO,
                    gate: super::lag::LagGate::default(),
                    maps: Vec::new(),
                    gated: Vec::new(),
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every holder leaves the state whole: a poisoned lock is still sound.
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.inner
            .changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        self.inner.changed.notify_all();
    }

    /// The store, for the builder and the copies it makes.
    pub(in crate::tree_actor) fn repo(&self) -> &TreeGit {
        &self.inner.repo
    }

    /// The last checkpoint's redo start: built-log entries at or before it
    /// name nothing a restart reads.
    pub(in crate::tree_actor) fn checkpointed_through(&self) -> RecordNo {
        self.state().checkpointed
    }

    /// How many times the builder was asked: a test's sign that a waiter
    /// has asked and waits.
    #[cfg(all(test, feature = "failpoints"))]
    pub(in crate::tree_actor) fn requests(&self) -> u64 {
        self.state().requests
    }

    /// A checkpoint whose redo start is `built` is durable: the builder may
    /// drop the built-log entries it covers.
    pub(in crate::tree_actor) fn checkpointed(&self, built: RecordNo) {
        let mut state = self.state();
        state.checkpointed = state.checkpointed.max(built);
    }

    /// The actor appended step `step`, with `cost` its tree and about the
    /// bytes it adds for the builder to write: the builder's event, and the
    /// gate's count. `None` (rows alone) adds to no tree's lag.
    pub(in crate::tree_actor) fn appended_step(&self, step: RecordNo, cost: Option<(TreeId, u64)>) {
        let mut state = self.state();
        if let Some((tree, bytes)) = cost {
            state.gate.appended(tree, step, bytes);
        }
        let now = Instant::now();
        state.oldest_unbuilt.get_or_insert(now);
        state.unbuilt.push_back((step, now));
        state.wanted = state.wanted.max(step);
        state.requests += 1;
        drop(state);
        self.notify();
    }

    /// The builder's wait for work: the record to build through, once one is
    /// wanted past the built point and, after a failure, asked again since
    /// `seen` requests. `None` when it should stop.
    pub(in crate::tree_actor) fn next_work(&self, seen: &mut u64) -> Option<RecordNo> {
        let mut state = self.state();
        loop {
            if state.wanted > state.built {
                if state.failed.is_none() || state.requests > *seen {
                    *seen = state.requests;
                    return Some(state.wanted);
                }
                if state.stopping {
                    break;
                }
            } else if state.stopping {
                break;
            }
            state = self.wait(state);
        }
        state.stopped = true;
        drop(state);
        self.notify();
        None
    }

    /// Whether every step appended is built: what a compaction waits for.
    pub(in crate::tree_actor) fn steps_built(&self) -> bool {
        self.state().unbuilt.is_empty()
    }

    /// A compaction due long enough that the lag gate holds every tree's new
    /// work until the builder has built every step (or `false`: no longer).
    pub(in crate::tree_actor) fn compaction_overdue(&self, overdue: bool) {
        let mut state = self.state();
        if state.gate.compaction_overdue(overdue) {
            drop(state);
            self.notify();
        }
    }

    /// A batch is built and its built-log entry durable: the built point moves.
    pub(in crate::tree_actor) fn built_event(
        &self,
        through: RecordNo,
        pointers: Pointers,
        owners: BTreeMap<EventNumber, Oid>,
        objects: Arc<BuiltObjects>,
        elapsed: Duration,
    ) {
        let mut state = self.state();
        state.gate.built(through, elapsed);
        state.built = state.built.max(through);
        state.pointers = Arc::new(pointers);
        state.owners = Arc::new(owners);
        state.objects = objects;
        state.failed = None;
        while state
            .unbuilt
            .front()
            .is_some_and(|(step, _)| *step <= through)
        {
            state.unbuilt.pop_front();
        }
        state.oldest_unbuilt = state.unbuilt.front().map(|(_, at)| *at);
        state.epoch += 1;
        drop(state);
        self.notify();
    }

    /// The builder's maps of files it built or rebased, for the actor.
    pub(in crate::tree_actor) fn publish_maps(&self, maps: Vec<(TreeId, u64, FileMap)>) {
        if !maps.is_empty() {
            self.state().maps.extend(maps);
        }
    }

    /// Every map published since the last call, in the order built: the
    /// actor takes each that is the same save as its own (`FileMap::step`).
    pub(in crate::tree_actor) fn take_maps(&self) -> Vec<(TreeId, u64, FileMap)> {
        std::mem::take(&mut self.state().maps)
    }

    /// A batch that started at request count `at` failed: waiters that asked
    /// at or before it get the error.
    pub(in crate::tree_actor) fn failed_event(&self, error: BuildError, at: u64) {
        let mut state = self.state();
        state.failed_at = at;
        state.gate.failed();
        if state.failed.as_ref() != Some(&error) {
            eprintln!("tree HistoryBuildFailed: {error}");
        }
        state.failed = Some(error);
        state.epoch += 1;
        drop(state);
        self.notify();
    }

    /// Shutdown: the builder builds through `through`, then stops; returns
    /// once it has.
    pub(in crate::tree_actor) fn stop(&self, through: RecordNo) {
        let mut state = self.state();
        state.wanted = state.wanted.max(through);
        state.stopping = true;
        state.requests += 1;
        self.notify();
        while !state.stopped {
            state = self.wait(state);
        }
    }

    /// The builder exited without a stop asked (it could not start).
    pub(in crate::tree_actor) fn builder_gone(&self) {
        let mut state = self.state();
        state.stopped = true;
        state.stopping = true;
        drop(state);
        self.notify();
    }

    /// Wakes every [`History::wait_epoch`] caller with a new epoch.
    pub(in crate::tree_actor) fn wake_waiters(&self) {
        self.state().epoch += 1;
        self.notify();
    }

    /// Returns once the epoch passes `seen` or the builder stops.
    pub(in crate::tree_actor) fn wait_epoch(&self, seen: u64) -> u64 {
        let mut state = self.state();
        while state.epoch <= seen && !state.stopped {
            state = self.wait(state);
        }
        state.epoch
    }

    /// [`History::wait_epoch`], returning by `at_most` at the latest: an
    /// export that must also show what changes between events.
    pub(in crate::tree_actor) fn wait_epoch_for(&self, seen: u64, at_most: Duration) -> u64 {
        let deadline = Instant::now() + at_most;
        let mut state = self.state();
        while state.epoch <= seen && !state.stopped {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.epoch
    }

    /// The lag gate: returns once `tree`'s lag is under the limit or the
    /// builder has failed, on a built or failed event (never a timer).
    ///
    /// # Errors
    ///
    /// [`WaitError::Stopped`] at shutdown.
    pub(in crate::tree_actor) fn admit(&self, tree: TreeId) -> Result<(), WaitError> {
        let mut state = self.state();
        loop {
            if state.gate.admits(tree) {
                return Ok(());
            }
            if state.stopped || state.stopping {
                return Err(WaitError::Stopped);
            }
            state = self.wait(state);
        }
    }

    /// [`History::admit`] without blocking the caller: `then` runs at once on
    /// this thread when admitted (or at shutdown), else on the gate thread at
    /// the built or failed event that admits it. One thread answers every
    /// waiting call ([`History::serve_gate`]).
    pub(in crate::tree_actor) fn admit_then(&self, tree: TreeId, then: Then) {
        let mut state = self.state();
        if state.gate.admits(tree) {
            drop(state);
            then(Ok(()));
        } else if state.stopped || state.stopping {
            drop(state);
            then(Err(WaitError::Stopped.into()));
        } else {
            state.gated.push((tree, then));
            drop(state);
            self.notify();
        }
    }

    /// The gate thread: answers each waiting call once its tree is admitted,
    /// on the event that admits it; at shutdown answers the rest with
    /// [`WaitError::Stopped`] and returns once the builder has stopped.
    pub(in crate::tree_actor) fn serve_gate(&self) {
        let mut state = self.state();
        loop {
            let shutting = state.stopped || state.stopping;
            let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut state.gated)
                .into_iter()
                .partition(|(tree, _)| shutting || state.gate.admits(*tree));
            state.gated = waiting;
            if !ready.is_empty() {
                drop(state);
                for (_, then) in ready {
                    then(if shutting {
                        Err(WaitError::Stopped.into())
                    } else {
                        Ok(())
                    });
                }
                state = self.state();
                continue;
            }
            if state.stopped {
                return;
            }
            state = self.wait(state);
        }
    }

    /// The lag gate with these levels: a test's limit, so a call waits.
    #[cfg(all(test, feature = "failpoints"))]
    pub(in crate::tree_actor) fn set_lag_limits(&self, alarm: Duration, limit: Duration) {
        self.state().gate = super::lag::LagGate::with_limits(alarm, limit);
    }

    /// Whether `tree`'s new work may start now, without waiting.
    #[cfg(all(test, feature = "failpoints"))]
    pub(in crate::tree_actor) fn admits(&self, tree: TreeId) -> bool {
        self.state().gate.admits(tree)
    }

    /// The last step whose commits are in the store and in a durable
    /// built-log entry.
    pub fn built_through(&self) -> RecordNo {
        self.state().built
    }

    /// Returns once the built point reaches `through`, on a built event.
    ///
    /// # Errors
    ///
    /// [`WaitError::Failed`] on a failed event before it reaches `through`,
    /// [`WaitError::Stopped`] at shutdown.
    pub fn wait_built(&self, through: RecordNo) -> Result<RecordNo, WaitError> {
        let mut state = self.state();
        if state.built >= through {
            return Ok(state.built);
        }
        state.wanted = state.wanted.max(through);
        state.requests += 1;
        let asked = state.requests;
        self.notify();
        loop {
            if state.built >= through {
                return Ok(state.built);
            }
            if state.failed_at >= asked
                && let Some(error) = &state.failed
            {
                return Err(WaitError::Failed(error.clone()));
            }
            if state.stopped {
                return Err(WaitError::Stopped);
            }
            state = self.wait(state);
        }
    }

    /// The built state at the current built point.
    pub fn snapshot(&self) -> BuiltSnapshot {
        let state = self.state();
        BuiltSnapshot {
            inner: Arc::clone(&self.inner),
            at: state.built,
            pointers: Arc::clone(&state.pointers),
            owners: Arc::clone(&state.owners),
            objects: Arc::clone(&state.objects),
        }
    }

    /// Each tree's lag, the oldest unbuilt step's age and the builder's health.
    pub fn lag(&self) -> Lag {
        let state = self.state();
        Lag {
            trees: state.gate.trees(),
            oldest_unbuilt: state.oldest_unbuilt.map(|at| at.elapsed()),
            health: match &state.failed {
                Some(error) => Health::Failed(error.clone()),
                None => state.gate.health(),
            },
        }
    }
}
