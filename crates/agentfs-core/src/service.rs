//! The background service: the commit loop and one push loop per remote
//! (design §3.4, §11.2).
//!
//! - **Commit loop** (thread `agentfs-commit`): every `commitWindow` it
//!   commits the groups that can no longer grow
//!   ([`Committer::commit_settled`]). After a failure it backs off, doubling
//!   up to [`COMMIT_BACKOFF_CAP`]; the records stay journaled. On shutdown it
//!   commits everything left, including the open group.
//! - **Push loops** (threads `agentfs-push-<remote>`): each pushes at once,
//!   then again after [`RemotePusher::next_delay`]: `pushInterval` after a
//!   success, the backoff after a failure. A thread per remote means a slow
//!   or hung remote never delays another.
//!
//! - **Repack** (on the commit thread): every `repackInterval` (design §8)
//!   the commit loop runs [`crate::repack::repack`] between commits, then
//!   refreshes the committer's object database. A failure emits
//!   [`Event::RepackFailed`] and is tried again an interval later; a round
//!   a push put off is tried again `retry` (a second) later. A stop
//!   kills a repack in progress, so shutdown never waits for one.
//!
//! [`Service::shutdown`] stops every loop, makes the final commit, then
//! gives each remote that lacks commits one more push attempt, all within
//! one grace period (design §11.2), and a grace of zero skips the final
//! push. At the grace's end each push is cancelled ([`PushStop`]): one
//! that has not finished its negotiation sends nothing, and one still
//! sending over the network is left on its detached thread until the
//! process exits. A push to a local remote past its negotiation is
//! updating that remote's pack and refs, which libgit2 cannot stop and an
//! exit would leave locked, so shutdown waits for it, at most
//! [`LOCAL_UPDATE_CAP`] more. Every long-running mode
//! (`mount`, `run`, `serve-history`) stops this way, so the last commit
//! before a stop reaches the remotes when they answer in time, and
//! otherwise waits for the next start.
//!
//! **Push now** ([`ServiceHandle::push_now`], design §2.3): `agentfs push`
//! against a running daemon asks the commit loop to commit everything
//! recorded (but an operation still running, so a restore is never
//! published half done), then wakes each chosen push loop past its
//! interval or backoff, and waits up to a timeout for the attempts. The
//! loops do the work themselves, so a request never overlaps their own
//! pushes; requests are counted in generations, and an attempt serves
//! every request made before it started.
//!
//! **Checkpoint** ([`ServiceHandle::checkpoint`], design §19.2 item 11):
//! the commit loop serves it between commits, as it repacks, so the
//! branch keeps one writer: it commits everything recorded (as push now
//! does), then cuts the checkpoint ([`crate::checkpoint::cut`]).
//!
//! Signals are a process concern: the binary decides when to call
//! [`Service::shutdown`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::checkpoint::{self, CheckpointOutcome, CheckpointRequest};
use crate::events::{Event, Events};
use crate::push::{PushOutcome, PushPhase, PushStop, QueuePush, RemotePusher, backoff};
use crate::repack::{self, RepackOutcome, RepackSchedule};
use crate::volume::{CommitReport, Committer, VolumeError};

/// The longest wait between commit attempts after failures.
pub const COMMIT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The shortest wait between loop iterations, whatever the settings say.
const MIN_DELAY: Duration = Duration::from_millis(10);

/// How long past the push grace [`Service::shutdown`] still waits for a
/// remote that is not inside a push attempt to report its final push
/// decision (a local read: whether the remote lacks commits).
const DECISION_SETTLE: Duration = Duration::from_secs(2);

/// How often [`Service::shutdown`] looks at its clock while it waits.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// How long past the push grace [`Service::shutdown`] still waits for a
/// push to a local remote that is updating the remote's pack and refs
/// (design §11.2). That is disk work only (the transfer over a network
/// never waits), so it is short unless the disk is stalled; the cap keeps
/// a hung filesystem from holding the stop forever. It is also the
/// shutdown's ceiling: past the grace plus this, nothing at all is waited
/// for (a decision's [`DECISION_SETTLE`] included). 10 s: the default
/// grace (5 s) plus this stays within Kubernetes' default 30 s
/// termination grace, and the CSI node plugin's 30 s unstage grace, with
/// room for the final commit.
pub const LOCAL_UPDATE_CAP: Duration = Duration::from_secs(10);

/// How many push-now commit results are kept for the waiters still to
/// read them.
const COMMIT_RESULTS_KEPT: usize = 64;

/// The time the service's deadlines run on: [`Service::shutdown`]'s grace
/// period (design §11.2) and [`ServiceHandle::push_now`]'s timeout. The
/// system's monotonic clock, or a test's that stands still.
trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Instant;
}

/// [`Instant::now`].
#[derive(Debug)]
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A stop flag loops can sleep on.
#[derive(Debug, Default)]
pub struct Stop {
    stopped: Mutex<bool>,
    cond: Condvar,
}

impl Stop {
    pub fn stop(&self) {
        *self.lock() = true;
        self.cond.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *self.lock()
    }

    /// Sleeps up to `timeout`; true if stopped (now or already).
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.lock();
        let (guard, _) = self
            .cond
            .wait_timeout_while(guard, timeout, |stopped| !*stopped)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the final push at shutdown did for one remote (design §11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalPush {
    /// One more attempt was made: its outcome.
    Pushed(PushOutcome),
    /// The remote already had every commit, or there is no branch yet.
    NothingNew,
    /// The grace period was spent before an attempt could start; this many
    /// commits wait for the next start.
    Skipped { unpushed: usize },
}

/// What [`Service::shutdown`] found.
#[derive(Debug)]
pub struct ShutdownReport {
    /// The last commit pass, which includes the open group.
    pub final_commit: Result<CommitReport, VolumeError>,
    /// The final push of each remote whose push thread finished within the
    /// grace period, in name order.
    pub final_pushes: Vec<(String, FinalPush)>,
    /// Remotes whose push was still running when the grace period ended,
    /// before it could update the remote.
    pub abandoned_pushes: Vec<String>,
    /// Local remotes whose push was still updating the remote's pack and
    /// refs [`LOCAL_UPDATE_CAP`] after the grace period: a ref lock may
    /// remain in the remote.
    pub abandoned_updates: Vec<String>,
}

/// A remote's result in a [`PushNowReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteResult {
    pub name: String,
    pub outcome: PushOutcome,
    /// What the attempt's patch queue push did, if there was one.
    pub queue: Option<QueuePush>,
    /// Commits the remote still lacks after the attempt; `None` if unknown
    /// or there is no branch yet.
    pub unpushed: Option<usize>,
    /// Earlier attempts of the same request that each sent only part of
    /// the backlog (design §11.3), oldest first; filled by callers that
    /// follow the steps, empty from the service.
    pub steps: Vec<PushOutcome>,
}

/// What [`ServiceHandle::push_now`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNowReport {
    /// The commit made first: what it committed, or why it failed (the
    /// pushes still ran, with what was already committed).
    pub committed: Result<CommitReport, String>,
    /// The remotes whose attempt finished in time, in name order.
    pub remotes: Vec<RemoteResult>,
    /// Remotes still pushing at the timeout (the commit included, when it
    /// did not finish in time either); the attempt carries on in the
    /// background.
    pub still_running: Vec<String>,
}

/// Why [`ServiceHandle::push_now`] could not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PushNowError {
    #[error("the daemon is stopping; its final push sends what is committed")]
    Stopping,
    #[error("no remote named {0:?} is configured")]
    UnknownRemote(String),
}

/// Why [`ServiceHandle::checkpoint`] cut nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointNowError {
    #[error("the daemon is stopping; checkpoint again after it restarts")]
    Stopping,
    #[error(
        "the checkpoint did not finish in time; it is still cut when the commit loop gets to it"
    )]
    TimedOut,
    #[error("{0}")]
    Failed(String),
}

/// A remote's last finished attempt: the request generation it served.
#[derive(Debug, Clone)]
struct PushDone {
    generation: u64,
    result: RemoteResult,
}

/// What the loops and [`ServiceHandle`]s share, under one lock.
#[derive(Debug, Default)]
struct SignalState {
    /// The loops stop.
    stop: bool,
    /// No new push-now requests; waiting ones return
    /// [`PushNowError::Stopping`]. The loops keep running.
    closed: bool,
    commit_requested: u64,
    commit_done: u64,
    /// The results of the last push-now commits, oldest first, each with
    /// the request generation it served: a waiter reads the first that
    /// served its own, so a later request cannot overwrite it.
    commit_results: VecDeque<(u64, Result<CommitReport, String>)>,
    push_requested: HashMap<String, u64>,
    push_done: HashMap<String, PushDone>,
    /// The last checkpoint request's id.
    checkpoint_next: u64,
    /// Checkpoints the commit loop has still to cut, oldest first.
    checkpoints: VecDeque<(u64, CheckpointRequest)>,
    /// Cut checkpoints whose waiters have not read them yet, by request
    /// id.
    checkpoint_results: VecDeque<(u64, Result<CheckpointOutcome, String>)>,
}

/// What a loop woke for.
enum Woke {
    Stop,
    /// A request; its generation.
    Request(u64),
    Timeout,
}

#[derive(Debug, Default)]
struct Signal {
    state: Mutex<SignalState>,
    cond: Condvar,
}

impl Signal {
    fn lock(&self) -> std::sync::MutexGuard<'_, SignalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut SignalState)) {
        f(&mut self.lock());
        self.cond.notify_all();
    }

    /// Sleeps up to `timeout`, or until a stop or a request `pending`
    /// finds.
    fn wait(&self, timeout: Duration, pending: impl Fn(&SignalState) -> Option<u64>) -> Woke {
        let guard = self.lock();
        let (guard, _) = self
            .cond
            .wait_timeout_while(guard, timeout, |s| !s.stop && pending(s).is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.stop {
            Woke::Stop
        } else if let Some(generation) = pending(&guard) {
            Woke::Request(generation)
        } else {
            Woke::Timeout
        }
    }

    /// Waits until `done` holds, the handle is closed or `deadline` passes;
    /// returns the state at that point and whether `done` held.
    fn wait_until(
        &self,
        clock: &dyn Clock,
        deadline: Instant,
        done: impl Fn(&SignalState) -> bool,
    ) -> (std::sync::MutexGuard<'_, SignalState>, bool) {
        let mut guard = self.lock();
        loop {
            if done(&guard) {
                return (guard, true);
            }
            let now = clock.now();
            if guard.closed || guard.stop || now >= deadline {
                return (guard, false);
            }
            guard = self
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// Asks a running [`Service`] to commit and push now (design §2.3, §11.2):
/// `agentfs push` against a running daemon. Cheap to clone; usable from
/// any thread.
#[derive(Debug, Clone)]
pub struct ServiceHandle {
    signal: Arc<Signal>,
    /// Remotes with a push loop, in name order.
    remotes: Arc<Vec<String>>,
    /// What the timeout is measured on: the service's clock.
    clock: Arc<dyn Clock>,
}

impl ServiceHandle {
    /// Remotes with a push loop, in name order.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// Commits everything recorded (but an operation still running,
    /// [`Committer::commit_now`]), then pushes each of `remotes` (every
    /// remote when empty) at once, past any backoff, and waits for the
    /// results up to `timeout`. The work is done by the service's own
    /// threads, so it never overlaps their pushes; an attempt still running
    /// at the timeout carries on.
    pub fn push_now(
        &self,
        remotes: &[String],
        timeout: Duration,
    ) -> Result<PushNowReport, PushNowError> {
        let deadline = self.clock.now() + timeout;
        if let Some(unknown) = remotes.iter().find(|r| !self.remotes.contains(r)) {
            return Err(PushNowError::UnknownRemote(unknown.clone()));
        }
        let mut chosen: Vec<String> = if remotes.is_empty() {
            self.remotes.to_vec()
        } else {
            remotes.to_vec()
        };
        chosen.sort();
        chosen.dedup();

        let commit_generation = {
            let mut s = self.signal.lock();
            if s.closed || s.stop {
                return Err(PushNowError::Stopping);
            }
            s.commit_requested += 1;
            s.commit_requested
        };
        self.signal.cond.notify_all();
        let (state, done) = self.signal.wait_until(&*self.clock, deadline, |s| {
            s.commit_done >= commit_generation
        });
        if !done {
            if state.closed || state.stop {
                return Err(PushNowError::Stopping);
            }
            return Ok(PushNowReport {
                committed: Err("the commit did not finish in time".into()),
                remotes: Vec::new(),
                still_running: chosen,
            });
        }
        let committed = state
            .commit_results
            .iter()
            .find(|(served, _)| *served >= commit_generation)
            .map_or_else(|| Ok(CommitReport::default()), |(_, r)| r.clone());
        drop(state);

        let mut generations = Vec::with_capacity(chosen.len());
        {
            let mut s = self.signal.lock();
            for name in &chosen {
                let generation = s.push_requested.entry(name.clone()).or_default();
                *generation += 1;
                generations.push((name.clone(), *generation));
            }
        }
        self.signal.cond.notify_all();
        let finished = |s: &SignalState, name: &str, generation: u64| {
            s.push_done
                .get(name)
                .is_some_and(|d| d.generation >= generation)
        };
        let (state, _) = self.signal.wait_until(&*self.clock, deadline, |s| {
            generations
                .iter()
                .all(|(name, generation)| finished(s, name, *generation))
        });
        if (state.closed || state.stop)
            && !generations
                .iter()
                .all(|(name, generation)| finished(&state, name, *generation))
        {
            return Err(PushNowError::Stopping);
        }
        let mut report = PushNowReport {
            committed,
            remotes: Vec::new(),
            still_running: Vec::new(),
        };
        for (name, generation) in generations {
            if finished(&state, &name, generation) {
                report.remotes.push(state.push_done[&name].result.clone());
            } else {
                report.still_running.push(name);
            }
        }
        Ok(report)
    }

    /// Cuts a checkpoint (design §19.2 item 11): the commit loop commits
    /// everything recorded (but an operation still running) after its
    /// current commit, then runs [`checkpoint::cut`], so the branch keeps
    /// one writer. Waits for it up to `timeout`; a checkpoint not cut by
    /// then is still cut when the loop gets to it.
    pub fn checkpoint(
        &self,
        request: CheckpointRequest,
        timeout: Duration,
    ) -> Result<CheckpointOutcome, CheckpointNowError> {
        let deadline = self.clock.now() + timeout;
        let id = {
            let mut s = self.signal.lock();
            if s.closed || s.stop {
                return Err(CheckpointNowError::Stopping);
            }
            s.checkpoint_next += 1;
            let id = s.checkpoint_next;
            s.checkpoints.push_back((id, request));
            id
        };
        self.signal.cond.notify_all();
        let (mut state, done) = self.signal.wait_until(&*self.clock, deadline, |s| {
            s.checkpoint_results.iter().any(|(served, _)| *served == id)
        });
        if !done {
            return Err(if state.closed || state.stop {
                CheckpointNowError::Stopping
            } else {
                CheckpointNowError::TimedOut
            });
        }
        let at = state
            .checkpoint_results
            .iter()
            .position(|(served, _)| *served == id);
        match at.and_then(|at| state.checkpoint_results.remove(at)) {
            Some((_, result)) => result.map_err(CheckpointNowError::Failed),
            None => Err(CheckpointNowError::Failed(
                "the checkpoint's result was lost".into(),
            )),
        }
    }

    /// Refuses new requests and wakes those waiting, which return
    /// [`PushNowError::Stopping`]; the loops keep running. Call it before
    /// the control socket stops, so no request outlives the service.
    pub fn close(&self) {
        self.signal.update(|s| s.closed = true);
    }
}

/// One remote's push thread.
struct PushThread {
    name: String,
    handle: JoinHandle<()>,
    /// Where the thread is ([`PushPhase`]: idle, while it sleeps or
    /// decides its final push from local state; in an attempt; or
    /// updating a local remote), and cancels its attempts at the grace's
    /// end.
    stop: Arc<PushStop>,
}

/// The running commit and push loops.
pub struct Service {
    /// Kills a repack in progress at shutdown.
    stop: Arc<Stop>,
    signal: Arc<Signal>,
    remotes: Arc<Vec<String>>,
    /// Released once the final commit is done and the deadline is set: the
    /// push threads wait for it, then make their final attempt.
    committed: Arc<Stop>,
    /// When the final push must have started by; set before `committed`
    /// is released.
    deadline: Arc<Mutex<Option<Instant>>>,
    /// What the deadline is measured on.
    clock: Arc<dyn Clock>,
    /// [`LOCAL_UPDATE_CAP`]; a test shortens it.
    update_cap: Duration,
    /// [`DECISION_SETTLE`]; a test shortens it.
    decision_settle: Duration,
    /// How many repack rounds the commit loop ran and scheduled the next
    /// of ([`Repacker::rounds`]).
    #[cfg(test)]
    repack_rounds: Arc<std::sync::atomic::AtomicUsize>,
    commit: JoinHandle<Result<CommitReport, VolumeError>>,
    pushes: Vec<PushThread>,
    push_done: mpsc::Receiver<(String, FinalPush)>,
    events: Events,
}

impl Service {
    /// Starts the loops, repacking every `repackInterval` from the
    /// committer's settings. The committer and pushers report their own
    /// events; `events` receives the service's own (a commit thread that
    /// panicked, a failed repack).
    pub fn start(
        committer: Committer,
        pushers: Vec<RemotePusher>,
        events: Events,
    ) -> io::Result<Self> {
        let schedule = committer
            .history()
            .settings()
            .repack_interval
            .map(RepackSchedule::every);
        Self::start_with_repack(committer, pushers, events, schedule)
    }

    /// [`Service::start`] with an explicit repack schedule (`None`: never).
    pub fn start_with_repack(
        committer: Committer,
        pushers: Vec<RemotePusher>,
        events: Events,
        repack: Option<RepackSchedule>,
    ) -> io::Result<Self> {
        Self::spawn(committer, pushers, events, repack, Arc::new(SystemClock))
    }

    /// [`Service::start_with_repack`] with the shutdown grace measured on
    /// `clock`.
    fn spawn(
        committer: Committer,
        pushers: Vec<RemotePusher>,
        events: Events,
        repack: Option<RepackSchedule>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        let stop = Arc::new(Stop::default());
        let signal = Arc::new(Signal::default());
        let committed = Arc::new(Stop::default());
        let deadline = Arc::new(Mutex::new(None));
        let mut names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
        names.sort();
        #[cfg(test)]
        let repack_rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let commit = {
            let stop = stop.clone();
            let signal = signal.clone();
            let clock = clock.clone();
            let repacker = repack.map(|schedule| Repacker {
                schedule,
                push_locks: pushers.iter().map(|p| p.lock_path().to_owned()).collect(),
                events: events.clone(),
                #[cfg(test)]
                rounds: repack_rounds.clone(),
            });
            thread::Builder::new()
                .name("agentfs-commit".into())
                .spawn(move || {
                    commit_loop(&committer, &stop, &signal, repacker.as_ref(), &*clock)
                })?
        };
        let (done_tx, push_done) = mpsc::channel();
        let mut pushes = Vec::with_capacity(pushers.len());
        for mut pusher in pushers {
            let name = pusher.name().to_owned();
            let signal_flag = signal.clone();
            let committed_flag = committed.clone();
            let deadline = deadline.clone();
            let clock = clock.clone();
            let done = done_tx.clone();
            let push_stop = pusher.stop_handle();
            let phase = push_stop.clone();
            let spawned = thread::Builder::new()
                .name(format!("agentfs-push-{name}"))
                .spawn(move || {
                    push_loop(&mut pusher, &signal_flag, &phase);
                    // The final commit may hold what this remote lacks.
                    while !committed_flag.wait(Duration::from_secs(3600)) {}
                    let deadline = *deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = final_push(&mut pusher, deadline, &*clock, &phase);
                    #[cfg(test)]
                    pusher.before_report();
                    let _ = done.send((pusher.name().to_owned(), result));
                    // Only now: the shutdown waits for a local push up to
                    // its report.
                    phase.end_attempt();
                });
            match spawned {
                Ok(handle) => pushes.push(PushThread {
                    name,
                    handle,
                    stop: push_stop,
                }),
                Err(e) => {
                    // Wind down what already started before reporting.
                    stop.stop();
                    signal.update(|s| s.stop = true);
                    committed.stop();
                    return Err(e);
                }
            }
        }
        Ok(Self {
            stop,
            signal,
            remotes: Arc::new(names),
            committed,
            deadline,
            clock,
            update_cap: LOCAL_UPDATE_CAP,
            decision_settle: DECISION_SETTLE,
            #[cfg(test)]
            repack_rounds,
            commit,
            pushes,
            push_done,
            events,
        })
    }

    /// Remotes with a push loop.
    pub fn remotes(&self) -> impl Iterator<Item = &str> {
        self.pushes.iter().map(|p| p.name.as_str())
    }

    /// A handle that asks the loops to commit and push now.
    pub fn handle(&self) -> ServiceHandle {
        ServiceHandle {
            signal: self.signal.clone(),
            remotes: self.remotes.clone(),
            clock: self.clock.clone(),
        }
    }

    /// Stops every loop: makes the final commit, then gives every remote
    /// that lacks commits one more push attempt (the final commit's among
    /// them), all within `push_grace` from the call. An attempt is only
    /// started before the deadline; at the deadline every attempt is
    /// cancelled, and one still running is abandoned, unless it is
    /// updating a local remote: that one is waited for, up to
    /// [`LOCAL_UPDATE_CAP`] more. A grace of zero skips the final push.
    /// Push-now requests still waiting return [`PushNowError::Stopping`].
    pub fn shutdown(self, push_grace: Duration) -> ShutdownReport {
        let deadline = self.clock.now() + push_grace;
        self.stop.stop();
        self.signal.update(|s| {
            s.stop = true;
            s.closed = true;
        });
        let final_commit = self.commit.join().unwrap_or_else(|_| {
            let error = "the commit thread panicked";
            self.events.emit(Event::CommitFailed {
                error: error.into(),
                backlog: 0,
            });
            Err(VolumeError::Io(io::Error::other(error)))
        });
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(deadline);
        self.committed.stop();

        let mut pending: Vec<PushThread> = self.pushes;
        let mut final_pushes = Vec::new();
        let mut take = |pending: &mut Vec<PushThread>, (done, result): (String, FinalPush)| {
            if let Some(i) = pending.iter().position(|p| p.name == done) {
                let thread = pending.swap_remove(i);
                let _ = thread.handle.join();
            }
            final_pushes.push((done, result));
        };
        // Past the deadline, a remote that is not inside an attempt is only
        // deciding (it finds the deadline passed and skips): wait a little
        // for that decision rather than call it abandoned. A local update
        // let finish past the deadline gets as long again after it. Nothing
        // is waited for past the ceiling, whatever hangs.
        let ceiling = deadline + self.update_cap.max(self.decision_settle);
        let mut settle = deadline + self.decision_settle;
        while !pending.is_empty() {
            let now = self.clock.now();
            let until = if now < deadline {
                deadline
            } else {
                // Nothing is sent from now on; a local update in progress
                // is let finish, up to its report. Every push is cancelled,
                // each read once.
                let phases: Vec<PushPhase> = pending.iter().map(|p| p.stop.cancel()).collect();
                let updating = phases.iter().any(|p| p.must_wait());
                if updating {
                    settle = settle.max(now + self.decision_settle);
                }
                let deciding = pending
                    .iter()
                    .zip(&phases)
                    .any(|(p, &phase)| phase == PushPhase::Idle && !p.handle.is_finished());
                let mut until = now;
                if deciding {
                    until = until.max(settle);
                }
                if updating {
                    until = ceiling;
                }
                until.min(ceiling)
            };
            let remaining = until.saturating_duration_since(now);
            if remaining.is_zero() {
                // Whatever was reported already, from threads that have
                // since finished too.
                while let Ok(done) = self.push_done.try_recv() {
                    take(&mut pending, done);
                }
                break;
            }
            // Looks again at least every `SHUTDOWN_POLL`: the deadline is on
            // `clock`, which need not be the time `recv_timeout` measures.
            match self.push_done.recv_timeout(remaining.min(SHUTDOWN_POLL)) {
                Ok(done) => take(&mut pending, done),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every sender gone: the rest panicked; join reports it.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Threads that finished without reporting (a panic) are not
        // abandoned; reap them.
        let mut abandoned = Vec::new();
        let mut abandoned_updates = Vec::new();
        for thread in pending {
            if thread.handle.is_finished() {
                let _ = thread.handle.join();
            } else if thread.stop.cancel().must_wait() {
                abandoned_updates.push(thread.name);
            } else {
                abandoned.push(thread.name);
            }
        }
        abandoned.sort();
        abandoned_updates.sort();
        final_pushes.sort_by(|a, b| a.0.cmp(&b.0));
        ShutdownReport {
            final_commit,
            final_pushes,
            abandoned_pushes: abandoned,
            abandoned_updates,
        }
    }
}

/// One remote's push loop until the stop: an attempt, then a sleep of
/// [`RemotePusher::next_delay`] that a push-now request cuts short.
/// `phase` is in an attempt for the length of each.
fn push_loop(pusher: &mut RemotePusher, signal: &Signal, phase: &PushStop) {
    let name = pusher.name().to_owned();
    let mut served = 0u64;
    loop {
        // The request this attempt serves: every one made before it starts.
        // No attempt starts once the stop is set: the final push follows.
        let generation = {
            let s = signal.lock();
            if s.stop {
                return;
            }
            s.push_requested.get(&name).copied().unwrap_or(0)
        };
        phase.begin_attempt();
        let outcome = pusher.push_once();
        phase.end_attempt();
        let result = RemoteResult {
            name: name.clone(),
            outcome,
            queue: pusher.queue().cloned(),
            unpushed: pusher.unpushed().ok().flatten(),
            steps: Vec::new(),
        };
        served = served.max(generation);
        signal.update(|s| {
            s.push_done.insert(
                name.clone(),
                PushDone {
                    generation: served,
                    result,
                },
            );
        });
        let delay = pusher.next_delay().max(MIN_DELAY);
        let woke = signal.wait(delay, |s| {
            s.push_requested
                .get(&name)
                .copied()
                .filter(|&requested| requested > served)
        });
        if let Woke::Stop = woke {
            return;
        }
    }
}

/// The final push of one remote: one attempt if it lacks commits and the
/// deadline has not passed. `phase` is in an attempt from its start, and
/// the caller ends it once the result is reported, so
/// [`Service::shutdown`] tells an attempt still running (abandoned, or
/// waited for while it updates a local remote) from a decision not yet
/// reported (skipped).
fn final_push(
    pusher: &mut RemotePusher,
    deadline: Option<Instant>,
    clock: &dyn Clock,
    phase: &PushStop,
) -> FinalPush {
    let unpushed = match pusher.unpushed() {
        Ok(None | Some(0)) => return FinalPush::NothingNew,
        Ok(Some(n)) => Some(n),
        // Unknown: an attempt finds out.
        Err(_) => None,
    };
    if deadline.is_some_and(|d| clock.now() < d) {
        phase.begin_attempt();
        FinalPush::Pushed(pusher.push_once())
    } else {
        FinalPush::Skipped {
            unpushed: unpushed.unwrap_or(0),
        }
    }
}

/// The repack half of the commit loop.
struct Repacker {
    schedule: RepackSchedule,
    push_locks: Vec<PathBuf>,
    events: Events,
    /// Counts the rounds the commit loop ran and scheduled the next of:
    /// a test that sees the count knows when the next round is due.
    #[cfg(test)]
    rounds: Arc<std::sync::atomic::AtomicUsize>,
}

impl Repacker {
    /// Repacks the committer's repository; failures become events. A stop
    /// kills a repack in progress. Returns how long until the next round:
    /// the schedule's `retry` when a push put this one off, else its
    /// `interval`.
    fn run(&self, committer: &Committer, stop: &Stop) -> Duration {
        let repo = committer.history().repo();
        let result = repack::repack(Path::new("git"), repo.path(), &self.push_locks, stop);
        match result {
            Ok(RepackOutcome::PushInProgress) => return self.schedule.retry,
            Ok(RepackOutcome::NothingToDo) => return self.schedule.interval,
            _ => {}
        }
        // A repack that failed late may still have moved objects into new
        // packs: see them either way.
        let refreshed = repo.odb().and_then(|odb| odb.refresh());
        let error = match (result, refreshed) {
            (Err(e), _) => e.to_string(),
            (Ok(_), Err(e)) => format!("refreshing the object database: {e}"),
            (Ok(_), Ok(())) => return self.schedule.interval,
        };
        self.events.emit(Event::RepackFailed { error });
        self.schedule.interval
    }
}

fn commit_loop(
    committer: &Committer,
    stop: &Stop,
    signal: &Signal,
    repacker: Option<&Repacker>,
    clock: &dyn Clock,
) -> Result<CommitReport, VolumeError> {
    let window = committer.history().settings().commit_window.max(MIN_DELAY);
    let mut failures = 0u32;
    // The repack schedule runs on `clock`.
    let mut next_repack = repacker.map(|r| clock.now() + r.schedule.first);
    loop {
        let delay = backoff(window, failures, COMMIT_BACKOFF_CAP);
        // A commit request's generation; 0 for checkpoints alone.
        let woke = signal.wait(delay, |s| {
            if s.commit_requested > s.commit_done {
                Some(s.commit_requested)
            } else {
                (!s.checkpoints.is_empty()).then_some(0)
            }
        });
        let generation = match woke {
            Woke::Stop => break,
            Woke::Request(generation) => Some(generation),
            Woke::Timeout => None,
        };
        if generation == Some(0) {
            serve_checkpoints(committer, signal);
            continue;
        }
        if let Some(generation) = generation {
            // Push now (design §2.3): everything recorded, except an
            // operation still running.
            let result = committer.commit_now();
            match &result {
                Ok(_) => failures = 0,
                Err(_) => failures = failures.saturating_add(1),
            }
            signal.update(|s| {
                s.commit_done = s.commit_done.max(generation);
                if s.commit_results.len() >= COMMIT_RESULTS_KEPT {
                    s.commit_results.pop_front();
                }
                s.commit_results
                    .push_back((generation, result.map_err(|e| e.to_string())));
            });
            serve_checkpoints(committer, signal);
            continue;
        }
        if let (Some(r), Some(at)) = (repacker, next_repack)
            && clock.now() >= at
        {
            let wait = r.run(committer, stop);
            next_repack = Some(clock.now() + wait.max(MIN_DELAY));
            // Counted only once the next round is scheduled: a test that
            // moves the clock after seeing the count moves it past a due
            // time already read from the clock, never before it is read.
            #[cfg(test)]
            r.rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if committer.backlog() == 0 {
            continue;
        }
        // Failures are reported as events by the committer.
        match committer.commit_settled(now_ms()) {
            Ok(_) => failures = 0,
            Err(_) => failures = failures.saturating_add(1),
        }
    }
    committer.commit_pending()
}

/// Cuts every queued checkpoint on the commit thread, each after a commit
/// of everything recorded (but an operation still running), and posts the
/// results for their waiters.
fn serve_checkpoints(committer: &Committer, signal: &Signal) {
    loop {
        let Some((id, request)) = signal.lock().checkpoints.pop_front() else {
            return;
        };
        let result = match committer.commit_now() {
            Ok(_) => checkpoint::cut(committer, &request).map_err(|e| e.to_string()),
            Err(e) => Err(format!("committing what is recorded: {e}")),
        };
        signal.update(|s| {
            // A waiter that timed out never reads its result.
            if s.checkpoint_results.len() >= COMMIT_RESULTS_KEPT {
                s.checkpoint_results.pop_front();
            }
            s.checkpoint_results.push_back((id, result));
        });
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::events::{Event, MemorySink, NullSink, PushFailureKind};
    use crate::file_lock::{FileLock, Mode};
    use crate::identity::Context;
    use crate::paths::RepoPath;
    use crate::push;
    use crate::record::{Caller, Trigger};
    use crate::telemetry::Telemetry;
    use crate::volume::{self, Input, Recorder, StateDir};

    const CALLER: Caller = Caller {
        uid: 1,
        gid: 1,
        pid: 1,
    };

    fn setup(config: &[(&str, &str)]) -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        let mut c = git2::Config::open(&state.git().join("config")).unwrap();
        for (k, v) in config {
            c.set_str(k, v).unwrap();
        }
        fs::create_dir_all(dir.path().join("tree")).unwrap();
        (dir, state)
    }

    fn open(
        dir: &Path,
        state: &StateDir,
        events: Events,
    ) -> (Recorder, Committer, Vec<RemotePusher>) {
        open_with(dir, state, events, &Telemetry::noop())
    }

    fn open_with(
        dir: &Path,
        state: &StateDir,
        events: Events,
        telemetry: &Telemetry,
    ) -> (Recorder, Committer, Vec<RemotePusher>) {
        let context = Context {
            volume: "vol".into(),
            replica: "r".into(),
            pod_uid: "u".into(),
            ..Context::default()
        };
        let (rec, com, _) =
            volume::open(state, &dir.join("tree"), context, events.clone(), telemetry).unwrap();
        let settings = com.history().settings().clone();
        let pushers = push::open_all(state, &settings, events, telemetry).unwrap();
        (rec, com, pushers)
    }

    fn record(rec: &Recorder, dir: &Path, name: &str) {
        let backing = dir.join("tree").join(name);
        fs::write(&backing, name).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![Input::Write {
                    path: RepoPath::new(name).unwrap(),
                    backing,
                    executable: false,
                }],
            )
            .unwrap();
        assert!(recorded.seq.is_some());
    }

    fn local_head(state: &StateDir) -> Option<git2::Oid> {
        git2::Repository::open_bare(state.git())
            .unwrap()
            .refname_to_id("refs/heads/agentfs/vol")
            .ok()
    }

    fn packs(state: &StateDir) -> usize {
        fs::read_dir(state.git().join("objects/pack"))
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// The commit loop repacks on schedule, and keeps committing after.
    #[test]
    fn the_commit_loop_repacks() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("git is not installed: skipped");
            return;
        }
        let (dir, state) = setup(&[("agentfs.commitWindow", "10ms")]);
        let sink = Arc::new(MemorySink::default());
        let (rec, com, pushers) = open(dir.path(), &state, sink.clone());
        record(&rec, dir.path(), "a");
        let schedule = RepackSchedule {
            first: Duration::ZERO,
            interval: Duration::from_millis(20),
            retry: Duration::from_millis(20),
        };
        let service =
            Service::start_with_repack(com, pushers, sink.clone(), Some(schedule)).unwrap();
        wait_for("committed and repacked", || {
            local_head(&state).is_some() && packs(&state) > 0
        });
        // The committer still reads and writes the history after a repack.
        record(&rec, dir.path(), "b");
        let report = service.shutdown(Duration::ZERO);
        report.final_commit.unwrap();
        let repo = git2::Repository::open_bare(state.git()).unwrap();
        let tree = repo
            .find_commit(local_head(&state).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_name("a").is_some() && tree.get_name("b").is_some());
        let events = sink.take();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::RepackFailed { .. })),
            "{events:?}"
        );
    }

    /// A round a push put off is due again `retry` later, not an interval
    /// later, so pushes that keep coming cannot starve the repack; the
    /// commit loop schedules its next round by what [`Repacker::run`]
    /// returns. Each round here is called directly, so the test knows a
    /// round met the held lock rather than hoping one ran in time.
    #[test]
    fn a_repack_a_push_put_off_is_tried_again_soon() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("git is not installed: skipped");
            return;
        }
        let (dir, state) = setup(&[]);
        // Something loose to pack.
        git2::Repository::open_bare(state.git())
            .unwrap()
            .blob(b"loose")
            .unwrap();
        // A push in progress, as far as the repack can tell.
        let lock_path = state.push_lock("good");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = FileLock::try_take(&lock_path, Mode::Exclusive)
            .unwrap()
            .unwrap();
        let sink = Arc::new(MemorySink::default());
        let (_rec, com, _pushers) = open(dir.path(), &state, sink.clone());
        let schedule = RepackSchedule {
            first: Duration::ZERO,
            interval: Duration::from_secs(3600),
            retry: Duration::from_millis(50),
        };
        let repacker = Repacker {
            schedule,
            push_locks: vec![lock_path],
            events: sink.clone(),
            rounds: Arc::default(),
        };
        let stop = Stop::default();
        assert_eq!(
            repacker.run(&com, &stop),
            schedule.retry,
            "a round a push put off is due again after retry"
        );
        assert_eq!(packs(&state), 0, "a repack ran while a push held its lock");
        drop(lock);
        assert_eq!(
            repacker.run(&com, &stop),
            schedule.interval,
            "a round that ran is due again after the interval"
        );
        assert!(packs(&state) > 0, "the round after the push did not repack");
        let events = sink.take();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::RepackFailed { .. })),
            "{events:?}"
        );
    }

    /// The commit loop schedules its next round by what the round
    /// returned: one a push put off is due again `retry` later on the
    /// clock, not an interval later.
    #[test]
    fn the_commit_loop_tries_a_put_off_repack_again_after_retry() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("git is not installed: skipped");
            return;
        }
        let nowhere = tempfile::tempdir().unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "10ms"),
            ("agentfs.pushInterval", "1h"),
            (
                "remote.origin.url",
                &nowhere.path().join("missing").display().to_string(),
            ),
        ]);
        // Something loose to pack.
        git2::Repository::open_bare(state.git())
            .unwrap()
            .blob(b"loose")
            .unwrap();
        // A push in progress, as far as the repack can tell.
        let lock_path = state.push_lock("origin");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = FileLock::try_take(&lock_path, Mode::Exclusive)
            .unwrap()
            .unwrap();
        let (_rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let retry = Duration::from_secs(1);
        let schedule = RepackSchedule {
            first: Duration::ZERO,
            interval: Duration::from_secs(3600),
            retry,
        };
        let clock = PausedClock::new();
        let service = Service::spawn(
            com,
            pushers,
            Arc::new(NullSink),
            Some(schedule),
            clock.clone(),
        )
        .unwrap();
        let rounds = service.repack_rounds.clone();
        // Counted once the next round is scheduled, so the clock moves
        // only after the loop has read it.
        wait_for("a round met the push's lock", || {
            rounds.load(Ordering::SeqCst) >= 1
        });
        assert_eq!(packs(&state), 0, "a repack ran while a push held its lock");
        drop(lock);
        clock.advance(retry);
        wait_for("the round due after retry repacked", || packs(&state) > 0);
        service.shutdown(Duration::ZERO).final_commit.unwrap();
    }

    #[test]
    fn stop_wakes_waiters() {
        let stop = Arc::new(Stop::default());
        assert!(!stop.wait(Duration::from_millis(1)));
        // A waiter for an hour: only the stop can end its wait in time.
        let (tx, rx) = mpsc::channel();
        {
            let stop = stop.clone();
            thread::spawn(move || tx.send(stop.wait(Duration::from_secs(3600))).unwrap());
        }
        stop.stop();
        assert!(
            rx.recv_timeout(Duration::from_secs(60))
                .expect("the stop woke the waiter")
        );
        assert!(stop.is_stopped());
        assert!(stop.wait(Duration::from_secs(60)), "already stopped");
    }

    #[test]
    fn commits_and_pushes_in_the_background() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let good = remote.path().display().to_string();
        let bad = remote.path().join("missing").display().to_string();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "10ms"),
            ("agentfs.pushInterval", "50ms"),
            ("remote.good.url", &good),
            ("remote.bad.url", &bad),
        ]);
        let sink = Arc::new(MemorySink::new());
        let (rec, com, pushers) = open(dir.path(), &state, sink.clone());
        let service = Service::spawn(com, pushers, sink.clone(), None, PausedClock::new()).unwrap();
        let mut remotes: Vec<&str> = service.remotes().collect();
        remotes.sort();
        assert_eq!(remotes, ["bad", "good"]);
        record(&rec, dir.path(), "a");
        record(&rec, dir.path(), "b");

        let remote_repo = git2::Repository::open_bare(remote.path()).unwrap();
        wait_for("the good remote caught up", || {
            let local = local_head(&state);
            let pushed = remote_repo.refname_to_id("refs/heads/agentfs/vol").ok();
            local.is_some()
                && local == pushed
                && remote_repo
                    .find_commit(pushed.unwrap())
                    .unwrap()
                    .tree()
                    .unwrap()
                    .len()
                    == 2
        });

        // The clock stands still, so every final push, the failing one's
        // too, finishes within the grace, and the shutdown returns with
        // them, never at the deadline.
        let report = shutdown_watched(service, Duration::from_secs(1));
        report.final_commit.as_ref().unwrap();
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");

        let events = sink.take();
        let failed: Vec<(&str, PushFailureKind)> = events
            .iter()
            .filter_map(|e| match e {
                Event::PushFailed { remote, kind, .. } => Some((remote.as_str(), *kind)),
                _ => None,
            })
            .collect();
        assert!(!failed.is_empty());
        assert!(
            failed
                .iter()
                .all(|f| *f == ("bad", PushFailureKind::Unreachable)),
            "{failed:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::PushSucceeded { remote, .. } if remote == "good"))
        );
    }

    /// The service's loops emit telemetry from their own threads.
    #[test]
    fn background_loops_report_telemetry() {
        use crate::telemetry::testing::{Harness, span_attr};
        use crate::telemetry::{names, spans};

        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.path().display().to_string()),
        ]);
        let h = Harness::new();
        let events = h.telemetry.sink(Arc::new(NullSink));
        let (rec, com, pushers) = open_with(dir.path(), &state, events.clone(), &h.telemetry);
        let settings = com.history().settings().clone();
        h.telemetry.observe_volume(&state, &settings);
        let service = Service::spawn(com, pushers, events, None, PausedClock::new()).unwrap();
        record(&rec, dir.path(), "a");
        assert_eq!(h.gauge_u64(names::JOURNAL_BACKLOG, &[]), Some(1));
        let report = shutdown_watched(service, Duration::from_secs(5));
        report.final_commit.unwrap();

        assert_eq!(h.sum(names::COMMIT_COUNT, &[]), Some(1));
        assert_eq!(h.gauge_u64(names::JOURNAL_BACKLOG, &[]), Some(0));
        let head = local_head(&state).unwrap();
        let commits = h.spans(spans::COMMIT);
        assert_eq!(
            span_attr(commits.last().unwrap(), "agentfs.commit.id"),
            Some(head.to_string())
        );
        // The push loop's first attempt ran before anything was committed.
        let pushes = h.spans(spans::PUSH);
        assert!(!pushes.is_empty());
        assert!(
            pushes
                .iter()
                .all(|s| span_attr(s, "agentfs.remote").as_deref() == Some("origin"))
        );
    }

    fn remote_head(remote: &Path) -> Option<git2::Oid> {
        git2::Repository::open_bare(remote)
            .unwrap()
            .refname_to_id("refs/heads/agentfs/vol")
            .ok()
    }

    /// A remote, intervals of an hour, and the first push (of nothing)
    /// done, so only the final push or a request can send what is
    /// recorded next. The service runs on the returned paused clock: its
    /// deadlines pass only when the test moves it.
    fn idle_service(
        remote: &Path,
    ) -> (
        tempfile::TempDir,
        StateDir,
        Recorder,
        Service,
        Arc<PausedClock>,
    ) {
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.display().to_string()),
        ]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let clock = PausedClock::new();
        let service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        // The first push (of nothing) is done; the loop now waits an hour.
        let first = push_now(&service.handle(), &[]);
        assert!(first.still_running.is_empty(), "{first:?}");
        (dir, state, rec, service, clock)
    }

    /// [`ServiceHandle::push_now`] of `remotes` on a service whose clock
    /// stands still: it returns once every attempt is done, failing the
    /// test after a minute.
    fn push_now(handle: &ServiceHandle, remotes: &[&str]) -> PushNowReport {
        let handle = handle.clone();
        let remotes: Vec<String> = remotes.iter().map(|&r| r.to_owned()).collect();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            tx.send(handle.push_now(&remotes, Duration::from_secs(60)))
                .unwrap();
        });
        rx.recv_timeout(Duration::from_secs(60))
            .expect("the push-now request returned")
            .unwrap()
    }

    /// Shuts `service` down with `grace`, failing the test if that takes
    /// a minute. On a clock that stands still the deadline never passes,
    /// so it returns once every final push is reported.
    fn shutdown_watched(service: Service, grace: Duration) -> ShutdownReport {
        let (rx, _) = shutdown_in_background(service, grace);
        rx.recv_timeout(Duration::from_secs(60))
            .expect("the shutdown returned")
    }

    /// A clock that stands still until a test moves it, so a shutdown's
    /// deadline passes exactly when the test says, however slow the
    /// machine.
    #[derive(Debug)]
    struct PausedClock(Mutex<Instant>);

    impl PausedClock {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Instant::now())))
        }

        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }
    }

    impl Clock for PausedClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    /// Makes `remote`'s config a FIFO: libgit2 blocks opening it, which
    /// stands in for a push hung on the network. Returns the FIFO.
    fn hang(remote: &Path) -> std::path::PathBuf {
        let fifo = remote.join("config");
        fs::remove_file(&fifo).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        fifo
    }

    /// Lets the thread blocked opening `fifo` go.
    fn release(fifo: &Path) {
        use std::io::Write as _;
        let mut writer = fs::OpenOptions::new().write(true).open(fifo).unwrap();
        writer.write_all(b"").unwrap();
    }

    /// Waits until `what` holds, failing after a minute.
    fn wait_for(what: &str, holds: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !holds() {
            assert!(Instant::now() < deadline, "never: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Shuts `service` down with `grace` on `clock` and moves the clock to
    /// the deadline once `ready` holds: the shutdown has set its deadline,
    /// and `ready` says the remotes are where the test wants them. The
    /// shutdown must still be waiting then; it must return once the clock
    /// reaches the deadline.
    fn shutdown_at_deadline(
        service: Service,
        clock: &PausedClock,
        grace: Duration,
        ready: impl Fn() -> bool,
    ) -> ShutdownReport {
        let deadline = service.deadline.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(service.shutdown(grace)).unwrap());
        wait_for("the deadline is set and the remotes are ready", || {
            deadline.lock().unwrap().is_some() && ready()
        });
        assert!(rx.try_recv().is_err(), "returned before the deadline");
        clock.advance(grace);
        rx.recv_timeout(Duration::from_secs(60))
            .expect("returned at the deadline")
    }

    /// Every shutdown pushes its final commit within the grace period.
    #[test]
    fn a_final_push_sends_the_final_commit() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let report = shutdown_watched(service, Duration::from_secs(10));
        report.final_commit.unwrap();
        assert!(report.abandoned_pushes.is_empty());
        let local = local_head(&state).expect("the final commit");
        assert_eq!(remote_head(remote.path()), Some(local));
        assert_eq!(report.final_pushes.len(), 1);
        let (name, result) = &report.final_pushes[0];
        assert_eq!(name, "origin");
        assert!(
            matches!(result, FinalPush::Pushed(PushOutcome::Pushed { to, .. }) if *to == local),
            "{result:?}"
        );
    }

    /// A remote that already has everything gets no final attempt.
    #[test]
    fn nothing_new_makes_no_final_push() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (_dir, _state, _rec, service, _clock) = idle_service(remote.path());
        let report = shutdown_watched(service, Duration::from_secs(10));
        assert_eq!(
            report.final_pushes,
            [("origin".to_owned(), FinalPush::NothingNew)]
        );
    }

    /// A grace of zero makes the final commit and skips the push.
    #[test]
    fn zero_grace_skips_the_final_push() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let report = shutdown_watched(service, Duration::ZERO);
        let commit = report.final_commit.unwrap();
        assert_eq!(commit.commits, 1);
        assert!(local_head(&state).is_some());
        assert_eq!(remote_head(remote.path()), None);
        // No attempt started, so nothing is abandoned: the remote reports
        // its skip, though the deadline passed before it could.
        assert!(
            report.abandoned_pushes.is_empty(),
            "{:?}",
            report.abandoned_pushes
        );
        assert_eq!(
            report.final_pushes,
            [("origin".to_owned(), FinalPush::Skipped { unpushed: 1 })]
        );
    }

    /// A grace of zero with a remote that has everything reports nothing
    /// new, not an abandoned push.
    #[test]
    fn zero_grace_with_nothing_new_is_not_abandoned() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (_dir, _state, _rec, service, _clock) = idle_service(remote.path());
        let report = shutdown_watched(service, Duration::ZERO);
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert_eq!(
            report.final_pushes,
            [("origin".to_owned(), FinalPush::NothingNew)]
        );
    }

    /// A checkpoint is cut on the commit thread, after a commit of what
    /// is recorded: the marker's first parent holds the last record,
    /// though the commit window would not have committed it yet, and the
    /// loop commits on top of the marker afterwards.
    #[test]
    fn a_checkpoint_is_cut_on_the_commit_thread_after_a_commit() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        let handle = service.handle();
        record(&rec, dir.path(), "a");
        push_now(&handle, &[]);
        let committed = local_head(&state).unwrap();
        record(&rec, dir.path(), "b");
        let request = CheckpointRequest {
            message: Some("intent".into()),
            caller: CALLER,
        };
        let outcome = handle
            .checkpoint(request.clone(), Duration::from_secs(60))
            .unwrap();
        let CheckpointOutcome::Cut { change, marker, .. } = outcome else {
            panic!("nothing cut: {outcome:?}");
        };
        let repo = git2::Repository::open_bare(state.git()).unwrap();
        assert_eq!(local_head(&state), Some(marker));
        let m = repo.find_commit(marker).unwrap();
        assert_eq!(m.parent_id(1).unwrap(), change);
        let parent = m.parent(0).unwrap();
        assert_eq!(
            parent.parent_ids().collect::<Vec<_>>(),
            [committed],
            "the marker's first parent is not the commit of the last record"
        );
        assert!(parent.tree().unwrap().get_name("b").is_some());
        assert_eq!(
            handle.checkpoint(request, Duration::from_secs(60)),
            Ok(CheckpointOutcome::NothingNew)
        );
        record(&rec, dir.path(), "c");
        push_now(&handle, &[]);
        let next = repo.find_commit(local_head(&state).unwrap()).unwrap();
        assert_eq!(next.parent_ids().collect::<Vec<_>>(), [marker]);
        let report = shutdown_watched(service, Duration::from_secs(1));
        report.final_commit.unwrap();
        assert_eq!(
            handle.checkpoint(
                CheckpointRequest {
                    message: None,
                    caller: CALLER,
                },
                Duration::from_secs(1)
            ),
            Err(CheckpointNowError::Stopping)
        );
    }

    /// Two push-now requests served one after the other each read their
    /// own commit's result, though both are served before either waiter
    /// runs.
    #[test]
    fn concurrent_push_now_requests_read_their_own_commit() {
        let signal = Arc::new(Signal::default());
        let handle = ServiceHandle {
            signal: signal.clone(),
            remotes: Arc::new(Vec::new()),
            clock: Arc::new(SystemClock),
        };
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let handle = handle.clone();
                thread::spawn(move || handle.push_now(&[], Duration::from_secs(30)).unwrap())
            })
            .collect();
        // A stand-in commit loop: once both requests are in, it serves
        // generation 1 (5 records) and then 2 (none) under one lock, so no
        // waiter can read between them.
        let (state, both) = signal.wait_until(
            &SystemClock,
            Instant::now() + Duration::from_secs(30),
            |s| s.commit_requested == 2,
        );
        assert!(both);
        drop(state);
        signal.update(|s| {
            for (generation, records) in [(1, 5), (2, 0)] {
                s.commit_done = generation;
                s.commit_results.push_back((
                    generation,
                    Ok(CommitReport {
                        records,
                        commits: usize::from(records > 0),
                        ..CommitReport::default()
                    }),
                ));
            }
        });
        let mut records: Vec<usize> = waiters
            .into_iter()
            .map(|w| w.join().unwrap().committed.unwrap().records)
            .collect();
        records.sort_unstable();
        assert_eq!(records, [0, 5]);
    }

    /// The final push is abandoned at the deadline when the remote hangs.
    #[test]
    fn final_push_respects_the_deadline() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.path().display().to_string()),
        ]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let clock = PausedClock::new();
        let service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        let pushing = service.pushes[0].stop.clone();
        // The first push (of nothing) is done; the next waits an hour.
        let first = service
            .handle()
            .push_now(&[], Duration::from_secs(60))
            .unwrap();
        assert!(first.still_running.is_empty(), "{first:?}");
        record(&rec, dir.path(), "a");
        // Now the remote hangs.
        let fifo = hang(remote.path());
        // The final push starts, since the clock stands before the
        // deadline, and hangs.
        let report = shutdown_at_deadline(service, &clock, Duration::from_millis(300), || {
            pushing.phase() != PushPhase::Idle
        });
        report.final_commit.unwrap();
        assert!(local_head(&state).is_some());
        assert_eq!(report.abandoned_pushes, ["origin"]);
        assert!(report.final_pushes.is_empty(), "{:?}", report.final_pushes);
        release(&fifo);
    }

    #[test]
    fn shutdown_flushes_the_open_group() {
        let (dir, state) = setup(&[("agentfs.commitWindow", "1h")]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        assert!(pushers.is_empty());
        let service = Service::start(com, pushers, Arc::new(NullSink)).unwrap();
        record(&rec, dir.path(), "a");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(local_head(&state), None, "the group is still open");
        let report = service.shutdown(Duration::from_secs(1));
        let commit = report.final_commit.unwrap();
        assert_eq!((commit.records, commit.commits), (1, 1));
        assert!(local_head(&state).is_some());
    }

    #[test]
    fn no_remotes_runs_only_the_commit_loop() {
        let (dir, state) = setup(&[("agentfs.commitWindow", "10ms")]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let service = Service::start(com, pushers, Arc::new(NullSink)).unwrap();
        assert_eq!(service.remotes().count(), 0);
        record(&rec, dir.path(), "a");
        wait_for("committed", || local_head(&state).is_some());
        let report = service.shutdown(Duration::ZERO);
        assert_eq!(report.final_commit.unwrap(), CommitReport::default());
        assert!(report.abandoned_pushes.is_empty());
    }

    /// A push still running at the deadline is abandoned; a remote that
    /// finished its final push within the grace is not.
    #[test]
    fn a_push_still_running_is_abandoned_after_the_grace_period() {
        let hung = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(hung.path()).unwrap();
        let fifo = hang(hung.path());
        let idle = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(idle.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "10ms"),
            ("agentfs.pushInterval", "1h"),
            ("remote.hung.url", &hung.path().display().to_string()),
            ("remote.idle.url", &idle.path().display().to_string()),
        ]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        record(&rec, dir.path(), "a");
        com.commit_pending().unwrap();
        let clock = PausedClock::new();
        let service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        let hung_pushing = service
            .pushes
            .iter()
            .find(|p| p.name == "hung")
            .unwrap()
            .stop
            .clone();
        // The idle remote has everything and waits an hour: its final push
        // finds nothing new.
        let idle_push = service
            .handle()
            .push_now(&["idle".to_owned()], Duration::from_secs(60))
            .unwrap();
        assert_eq!(idle_push.remotes[0].unpushed, Some(0), "{idle_push:?}");
        // The hung remote's first push is stuck.
        wait_for("the hung push started", || {
            hung_pushing.phase() != PushPhase::Idle
        });

        let report = shutdown_at_deadline(service, &clock, Duration::from_millis(200), || true);
        report.final_commit.unwrap();
        assert_eq!(report.abandoned_pushes, ["hung"]);
        assert_eq!(
            report.final_pushes,
            [("idle".to_owned(), FinalPush::NothingNew)]
        );
        release(&fifo);
    }

    /// Holds a push at the end of its negotiation, once armed, until
    /// released: a push to a local remote is then inside its update.
    #[derive(Default)]
    struct Gate {
        /// `(armed, entered, released)`.
        state: Mutex<(bool, bool, bool)>,
        cond: Condvar,
    }

    impl Gate {
        fn arm(&self) {
            self.state.lock().unwrap().0 = true;
        }

        fn pass(&self) {
            let mut state = self.state.lock().unwrap();
            if !state.0 {
                return;
            }
            state.1 = true;
            self.cond.notify_all();
            while !state.2 {
                state = self.cond.wait(state).unwrap();
            }
        }

        fn entered(&self) -> bool {
            self.state.lock().unwrap().1
        }

        fn release(&self) {
            self.state.lock().unwrap().2 = true;
            self.cond.notify_all();
        }
    }

    /// What [`held_final_push`] sets up.
    struct Held {
        remote: tempfile::TempDir,
        _dir: tempfile::TempDir,
        state: StateDir,
        service: Service,
        clock: Arc<PausedClock>,
        gate: Arc<Gate>,
        stop: Arc<PushStop>,
    }

    /// Where [`held_final_push`] holds the final push.
    enum Hold {
        /// Inside its update of the local remote.
        InUpdate,
        /// Its update ended, its bookkeeping not done.
        AfterUpdate,
        /// Its final push done, its report not yet sent.
        BeforeReport,
    }

    /// A service on a paused clock whose one local remote has had its
    /// first push, with `a` recorded, and its final push held by the gate
    /// where `hold` says.
    fn held_final_push(hold: Hold) -> Held {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.path().display().to_string()),
        ]);
        let (rec, com, mut pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let gate = Arc::new(Gate::default());
        let held = gate.clone();
        match hold {
            Hold::InUpdate => pushers[0].set_after_negotiation(move || held.pass()),
            Hold::AfterUpdate => pushers[0].set_after_update(move || held.pass()),
            Hold::BeforeReport => pushers[0].set_before_report(move || held.pass()),
        }
        let stop = pushers[0].stop_handle();
        let clock = PausedClock::new();
        let service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        let first = service
            .handle()
            .push_now(&[], Duration::from_secs(60))
            .unwrap();
        assert!(first.still_running.is_empty(), "{first:?}");
        record(&rec, dir.path(), "a");
        gate.arm();
        Held {
            remote,
            _dir: dir,
            state,
            service,
            clock,
            gate,
            stop,
        }
    }

    /// Ref locks left in `remote`.
    fn ref_locks(remote: &Path) -> Vec<std::path::PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|e| e == "lock") {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&remote.join("refs"), &mut found);
        found
    }

    /// Shuts `service` down on another thread: the report arrives on the
    /// receiver, and the flag is set once it was sent.
    fn shutdown_in_background(
        service: Service,
        grace: Duration,
    ) -> (mpsc::Receiver<ShutdownReport>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let returned = Arc::new(AtomicBool::new(false));
        let flag = returned.clone();
        thread::spawn(move || {
            tx.send(service.shutdown(grace)).unwrap();
            flag.store(true, Ordering::SeqCst);
        });
        (rx, returned)
    }

    /// A push to a local remote that is updating the remote when the
    /// grace ends is waited for, not left to die with the process inside
    /// the remote's ref update, which left `<remote>/refs/...lock` and
    /// every later push refused (design §11.2).
    #[test]
    fn a_local_update_running_at_the_grace_end_is_waited_for() {
        let held = held_final_push(Hold::InUpdate);
        let grace = Duration::from_millis(300);
        let (rx, returned) = shutdown_in_background(held.service, grace);
        wait_for("the final push is updating the remote", || {
            held.gate.entered()
        });
        held.clock.advance(grace);
        wait_for("the shutdown cancelled the push, or returned", || {
            held.stop.cancelled() || returned.load(Ordering::SeqCst)
        });
        assert!(
            !returned.load(Ordering::SeqCst),
            "the shutdown returned while the push was updating the local remote"
        );
        held.gate.release();
        let report = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        report.final_commit.as_ref().unwrap();
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert!(report.abandoned_updates.is_empty(), "{report:?}");
        let local = local_head(&held.state).expect("the final commit");
        assert!(
            matches!(
                &report.final_pushes[..],
                [(name, FinalPush::Pushed(PushOutcome::Pushed { to, .. }))]
                    if name == "origin" && *to == local
            ),
            "{:?}",
            report.final_pushes
        );
        assert_eq!(remote_head(held.remote.path()), Some(local));
        assert!(ref_locks(held.remote.path()).is_empty());
    }

    /// A local update still running at the cap is abandoned, named apart
    /// from pushes abandoned before they touched the remote: the stop
    /// cannot wait for a hung filesystem forever.
    #[test]
    fn a_local_update_still_running_at_the_cap_is_abandoned() {
        let mut held = held_final_push(Hold::InUpdate);
        let cap = Duration::from_millis(200);
        held.service.update_cap = cap;
        held.service.decision_settle = Duration::from_millis(100);
        let grace = Duration::from_millis(300);
        let (rx, returned) = shutdown_in_background(held.service, grace);
        wait_for("the final push is updating the remote", || {
            held.gate.entered()
        });
        held.clock.advance(grace);
        wait_for("the shutdown cancelled the push", || held.stop.cancelled());
        assert!(!returned.load(Ordering::SeqCst), "returned before the cap");
        held.clock.advance(cap);
        let report = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        report.final_commit.as_ref().unwrap();
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert_eq!(report.abandoned_updates, ["origin"]);
        assert!(report.final_pushes.is_empty(), "{report:?}");
        held.gate.release();
    }

    /// A local push whose update has ended but which has not reported
    /// yet is still waited for, past the window a decision gets: a
    /// shutdown that looked in between once listed it abandoned. `hold`
    /// is where it is held: in its bookkeeping (its push state, its
    /// alarms), or with its report not yet sent.
    fn a_local_push_is_waited_for_until_it_reports(hold: Hold) {
        let mut held = held_final_push(hold);
        // A decision gets 100 ms; the update up to 5 s.
        held.service.decision_settle = Duration::from_millis(100);
        held.service.update_cap = Duration::from_secs(5);
        let grace = Duration::from_millis(300);
        let (rx, returned) = shutdown_in_background(held.service, grace);
        wait_for("the final push updated the remote", || held.gate.entered());
        // Past the decision's window, before the cap.
        held_past(
            &held.clock,
            &held.stop,
            &returned,
            grace + Duration::from_secs(1),
        );
        held.gate.release();
        let report = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        report.final_commit.as_ref().unwrap();
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert!(report.abandoned_updates.is_empty(), "{report:?}");
        let local = local_head(&held.state).expect("the final commit");
        assert!(
            matches!(
                &report.final_pushes[..],
                [(_, FinalPush::Pushed(PushOutcome::Pushed { to, .. }))] if *to == local
            ),
            "{:?}",
            report.final_pushes
        );
    }

    #[test]
    fn a_local_push_finishing_after_its_update_is_waited_for() {
        a_local_push_is_waited_for_until_it_reports(Hold::AfterUpdate);
    }

    #[test]
    fn a_local_push_about_to_report_is_waited_for() {
        a_local_push_is_waited_for_until_it_reports(Hold::BeforeReport);
    }

    /// Moves `clock` on by `by` and waits until the shutdown has looked
    /// twice since, asserting it has not returned.
    fn held_past(clock: &PausedClock, stop: &PushStop, returned: &AtomicBool, by: Duration) {
        let before = stop.cancels();
        clock.advance(by);
        wait_for("the shutdown looked twice, or returned", || {
            stop.cancels() >= before + 2 || returned.load(Ordering::SeqCst)
        });
        assert!(
            !returned.load(Ordering::SeqCst),
            "the shutdown returned while a push was still to be waited for"
        );
    }

    /// One remote's thread held deciding its final push, another's held
    /// in its local update: the shutdown still returns once the clock
    /// passes the deadline plus the cap, naming both (design §11.2: the
    /// stop is bounded however its pushes hang).
    #[test]
    fn a_stop_is_bounded_with_a_hung_update_and_a_hung_decision() {
        let local = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(local.path()).unwrap();
        let slow = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(slow.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.local.url", &local.path().display().to_string()),
            ("remote.slow.url", &slow.path().display().to_string()),
        ]);
        let (rec, com, mut pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let updating = Arc::new(Gate::default());
        let deciding = Arc::new(Gate::default());
        let mut stop = None;
        for pusher in &mut pushers {
            if pusher.name() == "local" {
                let gate = updating.clone();
                pusher.set_after_negotiation(move || gate.pass());
                stop = Some(pusher.stop_handle());
            } else {
                let gate = deciding.clone();
                pusher.set_before_unpushed(move || gate.pass());
            }
        }
        let stop = stop.unwrap();
        let clock = PausedClock::new();
        let mut service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        let first = push_now(&service.handle(), &[]);
        assert!(first.still_running.is_empty(), "{first:?}");
        record(&rec, dir.path(), "a");
        updating.arm();
        deciding.arm();
        service.update_cap = Duration::from_millis(200);
        service.decision_settle = Duration::from_millis(100);
        let grace = Duration::from_millis(300);
        let (rx, returned) = shutdown_in_background(service, grace);
        wait_for("both final pushes are held", || {
            updating.entered() && deciding.entered()
        });
        held_past(&clock, &stop, &returned, grace);
        // Far past the cap: whatever still hangs is given up on.
        clock.advance(Duration::from_secs(60));
        let report = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the shutdown returned past the deadline plus the cap");
        report.final_commit.as_ref().unwrap();
        assert_eq!(report.abandoned_updates, ["local"]);
        assert_eq!(report.abandoned_pushes, ["slow"]);
        updating.release();
        deciding.release();
    }

    /// What [`late_update`] leaves running.
    struct LateUpdate {
        _remote: tempfile::TempDir,
        _dir: tempfile::TempDir,
        clock: Arc<PausedClock>,
        stop: Arc<PushStop>,
        deciding: Arc<Gate>,
        returned: Arc<AtomicBool>,
        rx: mpsc::Receiver<ShutdownReport>,
    }

    /// A push-loop attempt updates a local remote across the deadline,
    /// until the clock stands `ends` past it, and the thread then decides
    /// its final push, held there by `deciding`. The shutdown gives a
    /// decision `settle` and waits for nothing past `ceiling` after the
    /// deadline.
    fn late_update(settle: Duration, ceiling: Duration, ends: Duration) -> LateUpdate {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.path().display().to_string()),
        ]);
        let (rec, com, mut pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let updating = Arc::new(Gate::default());
        let deciding = Arc::new(Gate::default());
        let gate = updating.clone();
        pushers[0].set_after_negotiation(move || gate.pass());
        let gate = deciding.clone();
        pushers[0].set_before_unpushed(move || gate.pass());
        let stop = pushers[0].stop_handle();
        let clock = PausedClock::new();
        let mut service =
            Service::spawn(com, pushers, Arc::new(NullSink), None, clock.clone()).unwrap();
        let first = push_now(&service.handle(), &[]);
        assert!(first.still_running.is_empty(), "{first:?}");
        record(&rec, dir.path(), "a");
        updating.arm();
        // A push-now attempt, held in its update; the request returns
        // `Stopping` at the shutdown.
        let handle = service.handle();
        thread::spawn(move || handle.push_now(&[], Duration::from_secs(60)));
        wait_for("the push-now attempt is updating", || updating.entered());
        service.decision_settle = settle;
        service.update_cap = ceiling;
        let deadline = service.deadline.clone();
        let grace = Duration::from_millis(300);
        let (rx, returned) = shutdown_in_background(service, grace);
        wait_for("the deadline is set", || deadline.lock().unwrap().is_some());
        held_past(&clock, &stop, &returned, grace + ends);
        deciding.arm();
        updating.release();
        wait_for("the thread decides after its update", || deciding.entered());
        LateUpdate {
            _remote: remote,
            _dir: dir,
            clock,
            stop,
            deciding,
            returned,
            rx,
        }
    }

    /// A decision right after a local update that ended past the window a
    /// decision gets from the deadline gets its window again, and the
    /// remote is reported, not abandoned.
    #[test]
    fn a_decision_after_a_late_local_update_gets_its_window() {
        let late = late_update(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_millis(500),
        );
        let before = late.stop.cancels();
        wait_for("the shutdown looked twice, or returned", || {
            late.stop.cancels() >= before + 2 || late.returned.load(Ordering::SeqCst)
        });
        assert!(
            !late.returned.load(Ordering::SeqCst),
            "the shutdown gave up on a decision right after a late update"
        );
        late.deciding.release();
        let report = late.rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert!(report.abandoned_updates.is_empty(), "{report:?}");
        assert_eq!(report.final_pushes.len(), 1, "{report:?}");
    }

    /// The window a decision gets after an update never reaches past the
    /// ceiling: an update ending just before it and a decision that hangs
    /// still end the stop there.
    #[test]
    fn a_decision_after_an_update_never_outlasts_the_ceiling() {
        let late = late_update(
            Duration::from_millis(300),
            Duration::from_secs(1),
            Duration::from_millis(900),
        );
        // Past the ceiling, inside the window the update's end opened.
        late.clock.advance(Duration::from_millis(200));
        let report = late
            .rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the shutdown returned at the ceiling");
        assert_eq!(report.abandoned_pushes, ["origin"]);
        assert!(report.abandoned_updates.is_empty(), "{report:?}");
        late.deciding.release();
    }

    fn record_as(rec: &Recorder, dir: &Path, name: &str, caller: Caller) {
        let backing = dir.join("tree").join(name);
        fs::write(&backing, name).unwrap();
        rec.record(
            Trigger::Close,
            caller,
            vec![Input::Write {
                path: RepoPath::new(name).unwrap(),
                backing,
                executable: false,
            }],
        )
        .unwrap();
    }

    /// Push now commits what is recorded and pushes it at once, though
    /// both intervals are an hour.
    #[test]
    fn push_now_commits_and_pushes() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        let handle = service.handle();
        assert_eq!(handle.remotes(), ["origin"]);
        record(&rec, dir.path(), "a");
        let report = push_now(&handle, &[]);
        let committed = report.committed.clone().unwrap();
        assert_eq!((committed.records, committed.commits), (1, 1));
        assert!(report.still_running.is_empty());
        let local = local_head(&state).unwrap();
        assert_eq!(remote_head(remote.path()), Some(local));
        assert_eq!(report.remotes.len(), 1);
        let result = &report.remotes[0];
        assert_eq!(result.name, "origin");
        assert!(
            matches!(result.outcome, PushOutcome::Pushed { to, .. } if to == local),
            "{result:?}"
        );
        assert_eq!(result.unpushed, Some(0));
        // A second request with nothing new is up to date.
        let again = push_now(&handle, &["origin"]);
        assert_eq!(again.committed.as_ref().unwrap().records, 0);
        assert!(
            matches!(again.remotes[0].outcome, PushOutcome::UpToDate { .. }),
            "{again:?}"
        );
        let report = shutdown_watched(service, Duration::from_secs(5));
        assert_eq!(
            report.final_pushes,
            [("origin".to_owned(), FinalPush::NothingNew)]
        );
    }

    /// A request skips the backoff of a remote that failed.
    #[test]
    fn push_now_bypasses_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("later.git");
        let (dir2, state, rec, service, _clock) = idle_service(&remote);
        // The first push failed (the remote does not exist), so the loop
        // backs off for an hour; now the remote appears.
        git2::Repository::init_bare(&remote).unwrap();
        record(&rec, dir2.path(), "a");
        // Without the bypass, the request would wait out the hour.
        let report = push_now(&service.handle(), &[]);
        assert!(report.remotes[0].outcome.is_success(), "{report:?}");
        assert_eq!(remote_head(&remote), local_head(&state));
        shutdown_watched(service, Duration::ZERO);
    }

    #[test]
    fn push_now_unknown_remote() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (_dir, _state, _rec, service, _clock) = idle_service(remote.path());
        assert_eq!(
            service
                .handle()
                .push_now(&["nope".to_owned()], Duration::from_secs(1)),
            Err(PushNowError::UnknownRemote("nope".into()))
        );
        shutdown_watched(service, Duration::ZERO);
    }

    #[test]
    fn push_now_after_close_is_stopping() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        let handle = service.handle();
        handle.close();
        assert_eq!(
            handle.push_now(&[], Duration::from_secs(1)),
            Err(PushNowError::Stopping)
        );
        // Closing refuses requests only: the loops still run, and the
        // shutdown still commits and pushes.
        record(&rec, dir.path(), "a");
        let report = shutdown_watched(service, Duration::from_secs(10));
        assert_eq!(report.final_commit.unwrap().commits, 1);
        assert_eq!(remote_head(remote.path()), local_head(&state));
        assert_eq!(
            handle.push_now(&[], Duration::from_secs(1)),
            Err(PushNowError::Stopping)
        );
    }

    /// A request that waits on a hung remote returns at its timeout and
    /// names the remote; the attempt carries on.
    #[test]
    fn push_now_times_out_and_reports_still_running() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, clock) = idle_service(remote.path());
        let pushing = service.pushes[0].stop.clone();
        record(&rec, dir.path(), "a");
        let fifo = hang(remote.path());
        let handle = service.handle();
        let timeout = Duration::from_millis(200);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(handle.push_now(&[], timeout)).unwrap());
        // The loop waits an hour, so the attempt that starts serves the
        // request, after its commit; it hangs, and the request waits for
        // it until the clock reaches the timeout.
        wait_for("the request's push started", || {
            pushing.phase() != PushPhase::Idle
        });
        assert!(rx.try_recv().is_err(), "returned before its timeout");
        clock.advance(timeout);
        let report = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("returned at its timeout")
            .unwrap();
        assert_eq!(report.committed.unwrap().commits, 1);
        assert!(local_head(&state).is_some());
        assert!(report.remotes.is_empty());
        assert_eq!(report.still_running, ["origin"]);
        let report = shutdown_at_deadline(service, &clock, Duration::from_millis(100), || true);
        assert_eq!(report.abandoned_pushes, ["origin"]);
        release(&fifo);
    }

    /// Push now never commits an operation still running (a restore):
    /// it would publish it half done.
    #[test]
    fn push_now_leaves_a_running_restore_group_uncommitted() {
        use crate::record::OperationKind;

        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service, _clock) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let guard = rec
            .begin_operation(
                OperationKind::Restore,
                "0123456789abcdef0123456789abcdef01234567".into(),
                "restore b".into(),
                CALLER,
                &[77],
            )
            .unwrap();
        record_as(&rec, dir.path(), "b", Caller { pid: 77, ..CALLER });
        let report = push_now(&service.handle(), &[]);
        assert_eq!(report.committed.unwrap().records, 1);
        let repo = git2::Repository::open_bare(state.git()).unwrap();
        let tree = repo
            .find_commit(local_head(&state).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_name("a").is_some());
        assert!(tree.get_name("b").is_none(), "the restore is still running");
        assert_eq!(remote_head(remote.path()), local_head(&state));
        guard.finish();
        let report = shutdown_watched(service, Duration::from_secs(10));
        assert_eq!(report.final_commit.unwrap().records, 1);
        assert_eq!(remote_head(remote.path()), local_head(&state));
    }
}
