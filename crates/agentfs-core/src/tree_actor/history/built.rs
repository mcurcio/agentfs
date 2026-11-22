//! History as built: the one owner of a tree volume's Git store. The builder
//! makes each durable step's commits behind the call path; History hands out
//! only what is built, wakes waiters on built and failed events (never a
//! timer), and measures each tree's lag for the lag gate (docs/trees.md,
//! "History trails the files").
//!
use std::io;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use git2::Oid;

use super::super::TreeId;
use super::super::journal::durable::RecordNo;
use super::retired::{DeferredDrops, Retired};
use super::{Objects, refs};
use crate::tree_actor::call_path::Then;
use crate::tree_actor::journal::compaction::Published;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::FileMap;
use crate::tree_actor::tree::{EventNumber, Tree};
use crate::tree_git::TreeGit;

/// Each direct save's commit by its event, as built: the snapshot the
/// builder hands History at each built event. Persistent, so the handover is
/// O(1) and a batch adds only its own saves.
pub(in crate::tree_actor) type Owners = imbl::OrdMap<EventNumber, Oid>;

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
    /// The builder cannot build this step as recorded: a save's recorded
    /// layout cannot be laid (a place no linked journal record holds,
    /// malformed chunks, or bytes kept "as before" past the end of the file
    /// the builder holds), or the step names a file or open the builder
    /// lacks. Building stops with the alarm until an operator acts
    /// (docs/runbooks/recording-failures.md). A save the builder can lay but
    /// not reproduce is built and flagged instead (`super::verify`).
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
    /// What the actor's catch-ups let go, dropped after its lock is released.
    drops: DeferredDrops,
    /// Tests: runs at each batch's start and once it has read the record it
    /// builds through ([`History::batch_read`]).
    #[cfg(test)]
    batch_read: Mutex<Option<BatchRead>>,
}

/// Tests: a hook given `None` at each batch's start and the record it
/// builds through once read.
#[cfg(test)]
type BatchRead = Box<dyn FnMut(Option<RecordNo>) + Send>;

/// History's state, behind one lock. Every change notifies `changed`.
struct State {
    /// The built point.
    built: RecordNo,
    /// Every pointer as built.
    pointers: Arc<Pointers>,
    /// Each direct save's commit, by its step: what a sync asks of a save.
    owners: Owners,
    /// Each tree's object numbers as built.
    objects: BuiltObjects,
    /// The builder's trees as built: what the actor's saved trees rest on
    /// (`SavedTree::rebuilt`) and a checkpoint's locals come from. None until
    /// the first built event since open.
    trees: Option<imbl::OrdMap<TreeId, Tree>>,
    failed: Option<BuildError>,
    /// The request count the failed batch started at: a wait asked after it
    /// waits for the retry.
    failed_at: u64,
    /// The highest record the builder is asked to build through.
    wanted: RecordNo,
    /// Bumped at each request: a failed builder retries at the next one.
    requests: u64,
    /// Bumped at each built or failed event, and at [`History::wake_waiters`].
    epoch: u64,
    /// The batches ended so far: what the repack task waits on.
    ends: Ends,
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
    /// The last checkpoint counted: this open's compactor's publishes, or
    /// the one the journal opened at.
    checkpointed: Checkpointed,
    /// Built-log entries and builder heads at or before it name nothing a
    /// restart reads: the last checkpoint's `built`, or the record before
    /// its oldest step head when that is earlier.
    checkpointed_through: RecordNo,
    /// Each pointer the actor switched from a step to an oid: the step, and
    /// the record before which it switched. Kept until a published
    /// checkpoint's `built` reaches that record (C3, C4).
    switched: imbl::OrdMap<String, (RecordNo, RecordNo)>,
    /// A compaction stopped the actor: every checkpoint wait ends.
    compaction_stopped: bool,
    /// Built files' maps the actor has not taken yet, in the order built:
    /// each save, create and take, and each map rebased onto its checkout.
    maps: Vec<(TreeId, u64, FileMap)>,
    /// Gated calls waiting for their tree's lag to fall, in arrival order:
    /// the one gate thread answers them ([`History::serve_gate`]).
    gated: Vec<Gated>,
}

/// A gated call: its tree, its reply, and why and since when it waits.
struct Gated {
    tree: TreeId,
    then: Then,
    cause: super::lag::GateCause,
    since: Instant,
}

/// How many batches have ended since History started: `built` counts the
/// built events, `ended` the built and the failed ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct Ends {
    pub built: u64,
    pub ended: u64,
}

/// The built state at one built point: heads, refs and the commits behind
/// them. Only History constructs one, and only at or before the built point,
/// so nothing past it is reachable (the private fields are the guarantee).
pub struct BuiltSnapshot {
    inner: Arc<Inner>,
    at: RecordNo,
    pointers: Arc<Pointers>,
    owners: Owners,
    objects: BuiltObjects,
}

/// Every tree's object numbers at one built point: persistent, as
/// [`Objects`].
pub(in crate::tree_actor) type BuiltObjects = imbl::OrdMap<TreeId, Objects>;

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

    /// Each direct save's commit by its event, as built at this point.
    pub(in crate::tree_actor) fn owners(&self) -> &Owners {
        &self.owners
    }

    /// `tree`'s object numbers as built; none for a tree not built.
    pub(in crate::tree_actor) fn objects(&self, tree: TreeId) -> &Objects {
        static NONE: LazyLock<Objects> = LazyLock::new(Objects::new);
        self.objects.get(&tree).unwrap_or(&NONE)
    }

    /// `tree`'s queue as built: each entry's ref and commit, in queue order.
    pub(in crate::tree_actor) fn queue(&self, tree: TreeId) -> Vec<(String, Oid)> {
        self.pointers.under(&refs::queue(tree))
    }
}

/// A checkpoint History has counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpointed {
    /// Checkpoints this open's compactor has published: 0 for the one
    /// `Journal::open` read.
    pub generation: u64,
    /// Its built point B.
    pub built: RecordNo,
}

/// What [`History::checkpointed_through`] reports for a checkpoint at
/// `built` whose oldest step head is `oldest_head`.
fn redo_through(built: RecordNo, oldest_head: Option<RecordNo>) -> RecordNo {
    oldest_head.map_or(built, |head| {
        built.min(RecordNo::new(head.get().saturating_sub(1)))
    })
}

/// The one owner of the Git store. Takes, push, the CLI and tests reach
/// history through it; nothing else opens the store.
#[derive(Clone)]
pub struct History {
    inner: Arc<Inner>,
}

impl History {
    /// History over `repo`, built through `built` with these pointers.
    /// `opened` is the checkpoint `Journal::open` read: generation 0, and
    /// the redo start [`History::checkpointed_through`] reports until the
    /// first publish.
    ///
    /// # Errors
    ///
    /// The deferred drops' thread could not be spawned ([`DeferredDrops::start`]).
    pub(in crate::tree_actor) fn new(
        repo: TreeGit,
        built: RecordNo,
        pointers: Pointers,
        owners: Owners,
        objects: BuiltObjects,
        opened: &Published,
    ) -> std::io::Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                repo,
                state: Mutex::new(State {
                    built,
                    pointers: Arc::new(pointers),
                    owners,
                    objects,
                    trees: None,
                    failed: None,
                    failed_at: 0,
                    wanted: built,
                    requests: 0,
                    epoch: 0,
                    ends: Ends::default(),
                    oldest_unbuilt: None,
                    unbuilt: std::collections::VecDeque::new(),
                    stopping: false,
                    stopped: false,
                    checkpointed: Checkpointed {
                        generation: 0,
                        built: opened.built(),
                    },
                    checkpointed_through: redo_through(opened.built(), opened.oldest_head()),
                    switched: imbl::OrdMap::new(),
                    compaction_stopped: false,
                    gate: super::lag::LagGate::default(),
                    maps: Vec::new(),
                    gated: Vec::new(),
                }),
                changed: Condvar::new(),
                drops: DeferredDrops::start()?,
                #[cfg(test)]
                batch_read: Mutex::new(None),
            }),
        })
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

    /// What the last checkpoint lets go: built-log entries and builder heads
    /// at or before it name nothing a restart reads. Its `built`, or the
    /// record before its oldest step head when that is earlier.
    pub(in crate::tree_actor) fn checkpointed_through(&self) -> RecordNo {
        self.state().checkpointed_through
    }

    /// The last checkpoint counted. O(1).
    pub fn checkpointed(&self) -> Checkpointed {
        self.state().checkpointed
    }

    /// Returns once a checkpoint of a generation after `after` is published,
    /// with it.
    ///
    /// # Errors
    ///
    /// [`WaitError::Stopped`] at shutdown, or once a compaction stopped the
    /// actor (a failed fsync, which `Durability` reports first);
    /// [`WaitError::Failed`] at a failed built event after the call, as
    /// [`History::wait_built`]. A compaction that failed and will be retried
    /// does not end the wait.
    pub fn wait_checkpointed(&self, after: u64) -> Result<Checkpointed, WaitError> {
        let mut state = self.state();
        let ended = state.ends.ended;
        loop {
            if state.checkpointed.generation > after {
                return Ok(state.checkpointed);
            }
            if state.compaction_stopped || state.stopped || state.stopping {
                return Err(WaitError::Stopped);
            }
            if state.ends.ended > ended
                && let Some(error) = &state.failed
            {
                return Err(WaitError::Failed(error.clone()));
            }
            state = self.wait(state);
        }
    }

    /// A compaction ended in `Stopped`: every [`History::wait_checkpointed`]
    /// ends, and the compactor thread with it.
    pub(in crate::tree_actor) fn compaction_stopped(&self) {
        self.state().compaction_stopped = true;
        self.notify();
    }

    /// How many times the builder was asked: a test's sign that a waiter
    /// has asked and waits.
    #[cfg(all(test, feature = "failpoints"))]
    pub(in crate::tree_actor) fn requests(&self) -> u64 {
        self.state().requests
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

    /// Tests: `hook` runs at each batch's start (`None`), before it reads
    /// anything, and once it has read the record it builds through, before
    /// it builds anything.
    #[cfg(test)]
    pub(in crate::tree_actor) fn on_batch_read(
        &self,
        hook: impl FnMut(Option<RecordNo>) + Send + 'static,
    ) {
        *self
            .inner
            .batch_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Tests: the builder's batch starts (`None`) or has read `through`
    /// ([`History::on_batch_read`]).
    #[cfg(test)]
    pub(in crate::tree_actor) fn batch_read(&self, through: Option<RecordNo>) {
        let hook = self
            .inner
            .batch_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut hook) = hook {
            hook(through);
            self.inner
                .batch_read
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(hook);
        }
    }

    /// Asks the builder to build through record `through`, steps or not,
    /// without waiting: the actor's overlay holds entries sealed at a record
    /// after the last step (a call, an open), which only a build through it
    /// lets go ([`TreeActor::rebuild`](crate::tree_actor::TreeActor)).
    pub(in crate::tree_actor) fn want_through(&self, through: RecordNo) {
        let mut state = self.state();
        if state.wanted >= through {
            return;
        }
        state.wanted = through;
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

    /// A batch is built and its built-log entry durable: the built point
    /// moves. `owners`, `objects` and `trees` are the builder's own persistent
    /// maps, shared: O(1).
    pub(in crate::tree_actor) fn built_event(
        &self,
        through: RecordNo,
        pointers: Pointers,
        (owners, objects, trees): (Owners, BuiltObjects, imbl::OrdMap<TreeId, Tree>),
        elapsed: Duration,
    ) {
        let mut state = self.state();
        state.gate.built(through, elapsed);
        state.built = state.built.max(through);
        state.pointers = Arc::new(pointers);
        state.owners = owners;
        state.objects = objects;
        state.trees = Some(trees);
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
        state.ends.built += 1;
        state.ends.ended += 1;
        drop(state);
        self.notify();
    }

    /// The builder's maps of files it built or rebased, for the actor.
    pub(in crate::tree_actor) fn publish_maps(&self, maps: Vec<(TreeId, u64, FileMap)>) {
        if !maps.is_empty() {
            self.state().maps.extend(maps);
        }
    }

    /// The compactor's view of History at its built point, read in one hold
    /// (C8, C9): O(1), every field shared. `None` until the first built
    /// event since open, when History has no trees at the built point.
    pub(in crate::tree_actor) fn at_built(&self) -> Option<super::compactor::AtBuilt> {
        let state = self.state();
        Some(super::compactor::AtBuilt {
            built: state.built,
            pointers: Arc::clone(&state.pointers),
            objects: state.objects.clone(),
            trees: state.trees.clone()?,
            switched: state.switched.clone(),
        })
    }

    /// The actor switched `pointer` from step `step` to the oid it was built
    /// as, under its lock, before appending record `at`: no record from `at`
    /// on names that step. Kept until a published checkpoint's `built`
    /// reaches `at` (C3, C4); a later switch of the pointer replaces it.
    /// O(log pointers).
    pub(in crate::tree_actor) fn switched(&self, pointer: &str, step: RecordNo, at: RecordNo) {
        self.state().switched.insert(pointer.to_owned(), (step, at));
    }

    /// A checkpoint at `published.built()` is in place: only now may History
    /// forget the built-log entries it covers (C3). From now
    /// [`History::checkpointed_through`] reports `published.built()`, or the
    /// record before `published.oldest_head()` when that is earlier, so the
    /// builder keeps every step head a later record may name. Drops each
    /// switch the checkpoint counted (`at <= built`), moves the generation
    /// and the epoch, and counts the checkpoint (`Stages::checkpoints`). The
    /// compactor's alone. O(switches).
    pub(in crate::tree_actor) fn published(
        &self,
        published: &crate::tree_actor::journal::compaction::Published,
    ) {
        let mut state = self.state();
        let built = published.built();
        state.switched = state
            .switched
            .iter()
            .filter(|(_, (_, at))| *at > built)
            .map(|(pointer, switch)| (pointer.clone(), *switch))
            .collect();
        state.checkpointed = Checkpointed {
            generation: state.checkpointed.generation + 1,
            built,
        };
        state.checkpointed_through = redo_through(built, published.oldest_head());
        state.epoch += 1;
        drop(state);
        crate::tree_actor::stages::checkpointed();
        self.notify();
    }

    /// Values the actor let go under its lock (replaced saved maps and
    /// shapes), to be dropped after it is released ([`DeferredDrops`]).
    pub(in crate::tree_actor) fn defer_drops(&self, values: Retired) {
        self.inner.drops.defer(values);
    }

    /// Every value deferred so far dropped: the actor's shutdown, after the
    /// builder has stopped and before the lease is released.
    pub(in crate::tree_actor) fn finish_drops(&self) {
        self.inner.drops.finish();
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
        state.ends.ended += 1;
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

    /// The batches ended so far, built and failed.
    pub(in crate::tree_actor) fn ends(&self) -> Ends {
        self.state().ends
    }

    /// Returns once more than `after` batches have ended, built or failed,
    /// with the counts then; `None` once the volume is stopping. Only a
    /// batch's end moves it, never [`History::wake_waiters`]: what the
    /// repack task runs on ([`super::repack::Repacker`]).
    pub(in crate::tree_actor) fn wait_batch_end(&self, after: u64) -> Option<Ends> {
        let mut state = self.state();
        while state.ends.ended <= after && !state.stopping && !state.stopped {
            state = self.wait(state);
        }
        (!state.stopping && !state.stopped).then_some(state.ends)
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
        let mut waited: Option<(super::lag::GateCause, Instant)> = None;
        loop {
            let Some(cause) = state.gate.refusal(tree) else {
                if let Some((cause, since)) = waited {
                    crate::tree_actor::stages::gate_waited(cause, since);
                }
                return Ok(());
            };
            if state.stopped || state.stopping {
                return Err(WaitError::Stopped);
            }
            waited.get_or_insert((cause, Instant::now()));
            state = self.wait(state);
        }
    }

    /// [`History::admit`] without blocking the caller: `then` runs at once on
    /// this thread when admitted (or at shutdown), else on the gate thread at
    /// the built or failed event that admits it. One thread answers every
    /// waiting call ([`History::serve_gate`]).
    pub(in crate::tree_actor) fn admit_then(&self, tree: TreeId, then: Then) {
        let mut state = self.state();
        match state.gate.refusal(tree) {
            None => {
                drop(state);
                then(Ok(()));
            }
            Some(_) if state.stopped || state.stopping => {
                drop(state);
                then(Err(WaitError::Stopped.into()));
            }
            Some(cause) => {
                state.gated.push(Gated {
                    tree,
                    then,
                    cause,
                    since: Instant::now(),
                });
                drop(state);
                self.notify();
            }
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
                .partition(|gated| shutting || state.gate.admits(gated.tree));
            state.gated = waiting;
            if !ready.is_empty() {
                drop(state);
                for gated in ready {
                    if !shutting {
                        crate::tree_actor::stages::gate_waited(gated.cause, gated.since);
                    }
                    (gated.then)(if shutting {
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

    /// The overlay limit of the lag gate (`agentfs.overlayLimit`): over it,
    /// a tree's new work waits for the builder.
    pub fn set_overlay_limit(&self, bytes: u64) {
        self.state().gate.set_overlay_limit(bytes);
        self.notify();
    }

    /// The actor's overlay of each tree, as it left them at a release of
    /// its lock: the gate's memory term. Wakes the gate thread when a tree's
    /// admission changed. Lock order: the actor, then History.
    pub(in crate::tree_actor) fn overlays(
        &self,
        overlays: impl IntoIterator<Item = (TreeId, u64)>,
    ) {
        let mut state = self.state();
        let mut changed = false;
        let mut reported = std::collections::BTreeSet::new();
        for (tree, bytes) in overlays {
            reported.insert(tree);
            changed |= state.gate.overlay(tree, bytes);
        }
        changed |= state.gate.retain_overlays(|tree| reported.contains(&tree));
        drop(state);
        if changed {
            self.notify();
        }
    }

    /// `tree`'s overlay as the actor last reported it to the gate, in
    /// estimated bytes: 0 before any report. For tests and metrics.
    pub fn overlay_bytes(&self, tree: TreeId) -> u64 {
        self.state().gate.overlay_of(tree)
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
            owners: state.owners.clone(),
            objects: state.objects.clone(),
        }
    }

    /// The builder's trees at the built point, with the point, once the
    /// built point is past `since`: shared, O(1).
    pub(in crate::tree_actor) fn built_trees(
        &self,
        since: RecordNo,
    ) -> Option<(RecordNo, imbl::OrdMap<TreeId, Tree>)> {
        let state = self.state();
        let trees = state.trees.as_ref().filter(|_| state.built > since)?;
        Some((state.built, trees.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::test_support::new_actor;

    /// A tree left out of a report (a fork the actor dropped) no longer
    /// holds the gate.
    #[test]
    fn a_tree_absent_from_a_report_is_dropped_from_the_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let actor = new_actor(&tmp.path().join("state"));
        let history = actor.history();
        history.set_overlay_limit(1_000);
        history.overlays([(TreeId::Root, 10), (TreeId::Fork(1), 5_000)]);
        assert_eq!(history.overlay_bytes(TreeId::Fork(1)), 5_000);
        assert_eq!(history.lag().health, Health::Stalled);
        history.overlays([(TreeId::Root, 10)]);
        assert_eq!(history.overlay_bytes(TreeId::Fork(1)), 0);
        assert_eq!(history.overlay_bytes(TreeId::Root), 10);
        assert_eq!(history.lag().health, Health::Current);
    }

    /// A built event moves both counts, a failed one only `ended`, and a
    /// wake neither: the repack task runs on built events alone.
    #[test]
    fn only_built_events_count_as_built() {
        let tmp = tempfile::tempdir().unwrap();
        let mut actor = new_actor(&tmp.path().join("state"));
        let w = crate::tree_actor::test_support::writer("w");
        let h = actor
            .write_open(
                TreeId::Root,
                "f",
                &w,
                Some(crate::tree_actor::test_support::ATTRS),
            )
            .unwrap();
        actor.write(h, 0, b"one\n", false).unwrap();
        actor.close(h).unwrap();
        actor.settle().unwrap();
        let history = actor.history().clone();
        let start = history.ends();
        assert!(start.built >= 1 && start.ended >= start.built, "{start:?}");
        history.wake_waiters();
        assert_eq!(history.ends(), start, "a wake is no batch end");
        history.failed_event(BuildError::NoSpace, 0);
        let failed = history.ends();
        assert_eq!(
            failed,
            Ends {
                built: start.built,
                ended: start.ended + 1
            }
        );
        assert_eq!(history.wait_batch_end(start.ended), Some(failed));
        let h = actor.write_open(TreeId::Root, "f", &w, None).unwrap();
        actor.write(h, 0, b"two\n", false).unwrap();
        actor.close(h).unwrap();
        actor.settle().unwrap();
        let built = history.ends();
        assert!(
            built.built > failed.built && built.ended > failed.ended,
            "{built:?}"
        );
    }
}
