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
use super::refs;
use crate::tree_actor::pointers::Pointers;
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
pub(in crate::tree_actor) struct State {
    /// The built point.
    pub(in crate::tree_actor) built: RecordNo,
    /// Every pointer as built.
    pub(in crate::tree_actor) pointers: Arc<Pointers>,
    /// Each direct save's commit, by its step: what a sync asks of a save.
    pub(in crate::tree_actor) owners: Arc<BTreeMap<EventNumber, Oid>>,
    pub(in crate::tree_actor) failed: Option<BuildError>,
    /// The request count the failed batch started at: a wait asked after it
    /// waits for the retry.
    pub(in crate::tree_actor) failed_at: u64,
    /// The highest record the builder is asked to build through.
    pub(in crate::tree_actor) wanted: RecordNo,
    /// Bumped at each request: a failed builder retries at the next one.
    pub(in crate::tree_actor) requests: u64,
    /// Bumped at each built or failed event.
    pub(in crate::tree_actor) epoch: u64,
    /// When the first step past the built point was appended.
    pub(in crate::tree_actor) oldest_unbuilt: Option<Instant>,
    /// Steps appended past the built point, with when each was appended.
    pub(in crate::tree_actor) unbuilt: std::collections::VecDeque<(RecordNo, Instant)>,
    /// The volume is shutting down: build what is wanted, then stop.
    pub(in crate::tree_actor) stopping: bool,
    /// The builder has exited.
    pub(in crate::tree_actor) stopped: bool,
    /// Each tree's lag and the builder's rates.
    pub(in crate::tree_actor) gate: super::lag::LagGate,
    /// The journal's last checkpoint's redo start: built-log entries at or
    /// before it name nothing a restart reads.
    pub(in crate::tree_actor) checkpointed: RecordNo,
}

/// The built state at one built point: heads, refs and the commits behind
/// them. Only History constructs one, and only at or before the built point,
/// so nothing past it is reachable (the private fields are the guarantee).
pub struct BuiltSnapshot {
    inner: Arc<Inner>,
    at: RecordNo,
    pointers: Arc<Pointers>,
    owners: Arc<BTreeMap<EventNumber, Oid>>,
}

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
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                repo,
                state: Mutex::new(State {
                    built,
                    pointers: Arc::new(pointers),
                    owners: Arc::new(owners),
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
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub(in crate::tree_actor) fn state(&self) -> MutexGuard<'_, State> {
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

    /// A checkpoint whose redo start is `built` is durable: the builder may
    /// drop the built-log entries it covers.
    pub(in crate::tree_actor) fn checkpointed(&self, built: RecordNo) {
        let mut state = self.state();
        state.checkpointed = state.checkpointed.max(built);
    }

    /// The actor appended step `step` in `tree`, adding about `bytes` for the
    /// builder to write: the builder's event, and the gate's count.
    pub(in crate::tree_actor) fn appended_step(&self, step: RecordNo, tree: TreeId, bytes: u64) {
        let mut state = self.state();
        state.gate.appended(tree, step, bytes);
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

    /// A batch is built and its built-log entry durable: the built point moves.
    pub(in crate::tree_actor) fn built_event(
        &self,
        through: RecordNo,
        pointers: Pointers,
        owners: BTreeMap<EventNumber, Oid>,
        elapsed: Duration,
    ) {
        let mut state = self.state();
        state.gate.built(through, elapsed);
        state.built = state.built.max(through);
        state.pointers = Arc::new(pointers);
        state.owners = Arc::new(owners);
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

    /// Returns once the epoch passes `seen` or the builder stops.
    pub(in crate::tree_actor) fn wait_epoch(&self, seen: u64) -> u64 {
        let mut state = self.state();
        while state.epoch <= seen && !state.stopped {
            state = self.wait(state);
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

    /// Whether `tree`'s new work may start now, without waiting.
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
