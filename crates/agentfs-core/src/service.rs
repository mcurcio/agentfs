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
//!   [`Event::RepackFailed`] and is tried again an interval later. A stop
//!   kills a repack in progress, so shutdown never waits for one.
//!
//! [`Service::shutdown`] stops every loop, makes the final commit, then
//! gives each remote that lacks commits one more push attempt, all within
//! one grace period (design §11.2): stopping never waits longer than that
//! for a push, and a grace of zero skips the final push. libgit2 cannot
//! cancel a push, so a push still running after the grace period is left
//! on its detached thread until the process exits. Every long-running mode
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
//! Signals are a process concern: the binary decides when to call
//! [`Service::shutdown`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::events::{Event, Events};
use crate::push::{PushOutcome, QueuePush, RemotePusher, backoff};
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

/// How many push-now commit results are kept for the waiters still to
/// read them.
const COMMIT_RESULTS_KEPT: usize = 64;

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
    /// Remotes whose push was still running when the grace period ended.
    pub abandoned_pushes: Vec<String>,
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
        deadline: Instant,
        done: impl Fn(&SignalState) -> bool,
    ) -> (std::sync::MutexGuard<'_, SignalState>, bool) {
        let mut guard = self.lock();
        loop {
            if done(&guard) {
                return (guard, true);
            }
            let now = Instant::now();
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
        let deadline = Instant::now() + timeout;
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
        let (state, done) = self
            .signal
            .wait_until(deadline, |s| s.commit_done >= commit_generation);
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
        let (state, _) = self.signal.wait_until(deadline, |s| {
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
    /// Set while the thread is inside a push attempt (network I/O); clear
    /// while it sleeps or decides its final push, which reads only local
    /// state.
    pushing: Arc<AtomicBool>,
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
        let stop = Arc::new(Stop::default());
        let signal = Arc::new(Signal::default());
        let committed = Arc::new(Stop::default());
        let deadline = Arc::new(Mutex::new(None));
        let mut names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
        names.sort();
        let commit = {
            let stop = stop.clone();
            let signal = signal.clone();
            let repacker = repack.map(|schedule| Repacker {
                schedule,
                push_locks: pushers.iter().map(|p| p.lock_path().to_owned()).collect(),
                events: events.clone(),
            });
            thread::Builder::new()
                .name("agentfs-commit".into())
                .spawn(move || commit_loop(&committer, &stop, &signal, repacker.as_ref()))?
        };
        let (done_tx, push_done) = mpsc::channel();
        let mut pushes = Vec::with_capacity(pushers.len());
        for mut pusher in pushers {
            let name = pusher.name().to_owned();
            let signal_flag = signal.clone();
            let committed_flag = committed.clone();
            let deadline = deadline.clone();
            let done = done_tx.clone();
            let pushing = Arc::new(AtomicBool::new(false));
            let pushing_flag = pushing.clone();
            let spawned = thread::Builder::new()
                .name(format!("agentfs-push-{name}"))
                .spawn(move || {
                    push_loop(&mut pusher, &signal_flag, &pushing_flag);
                    // The final commit may hold what this remote lacks.
                    while !committed_flag.wait(Duration::from_secs(3600)) {}
                    let deadline = *deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = final_push(&mut pusher, deadline, &pushing_flag);
                    let _ = done.send((pusher.name().to_owned(), result));
                });
            match spawned {
                Ok(handle) => pushes.push(PushThread {
                    name,
                    handle,
                    pushing,
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
        }
    }

    /// Stops every loop: makes the final commit, then gives every remote
    /// that lacks commits one more push attempt (the final commit's among
    /// them), all within `push_grace` from the call. An attempt is only
    /// started before the deadline, and one still running at the deadline
    /// is abandoned; a grace of zero skips the final push. Push-now
    /// requests still waiting return [`PushNowError::Stopping`].
    pub fn shutdown(self, push_grace: Duration) -> ShutdownReport {
        let deadline = Instant::now() + push_grace;
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
        // Past the deadline, a remote that is not inside an attempt is only
        // deciding (it finds the deadline passed and skips): wait a little
        // for that decision rather than call it abandoned.
        let settle = deadline + DECISION_SETTLE;
        while !pending.is_empty() {
            let now = Instant::now();
            let deciding = pending
                .iter()
                .any(|p| !p.pushing.load(Ordering::SeqCst) && !p.handle.is_finished());
            let until = if now < deadline {
                deadline
            } else if deciding {
                settle
            } else {
                break;
            };
            let remaining = until.saturating_duration_since(now);
            if remaining.is_zero() {
                break;
            }
            let (done, result) = match self.push_done.recv_timeout(remaining) {
                Ok(done) => done,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                // Every sender gone: the rest panicked; join reports it.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(i) = pending.iter().position(|p| p.name == done) {
                let thread = pending.swap_remove(i);
                let _ = thread.handle.join();
            }
            final_pushes.push((done, result));
        }
        // Threads that finished without reporting (a panic) are not
        // abandoned; reap them.
        let mut abandoned = Vec::new();
        for thread in pending {
            if thread.handle.is_finished() {
                let _ = thread.handle.join();
            } else {
                abandoned.push(thread.name);
            }
        }
        abandoned.sort();
        final_pushes.sort_by(|a, b| a.0.cmp(&b.0));
        ShutdownReport {
            final_commit,
            final_pushes,
            abandoned_pushes: abandoned,
        }
    }
}

/// One remote's push loop until the stop: an attempt, then a sleep of
/// [`RemotePusher::next_delay`] that a push-now request cuts short.
/// `pushing` is set for the length of each attempt.
fn push_loop(pusher: &mut RemotePusher, signal: &Signal, pushing: &AtomicBool) {
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
        pushing.store(true, Ordering::SeqCst);
        let outcome = pusher.push_once();
        pushing.store(false, Ordering::SeqCst);
        let result = RemoteResult {
            name: name.clone(),
            outcome,
            queue: pusher.queue().cloned(),
            unpushed: pusher.unpushed().ok().flatten(),
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
/// deadline has not passed. `pushing` is set for the length of the attempt,
/// so [`Service::shutdown`] tells an attempt still running (abandoned) from
/// a decision not yet reported (skipped).
fn final_push(
    pusher: &mut RemotePusher,
    deadline: Option<Instant>,
    pushing: &AtomicBool,
) -> FinalPush {
    let unpushed = match pusher.unpushed() {
        Ok(None | Some(0)) => return FinalPush::NothingNew,
        Ok(Some(n)) => Some(n),
        // Unknown: an attempt finds out.
        Err(_) => None,
    };
    if deadline.is_some_and(|d| Instant::now() < d) {
        pushing.store(true, Ordering::SeqCst);
        let outcome = pusher.push_once();
        pushing.store(false, Ordering::SeqCst);
        FinalPush::Pushed(outcome)
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
}

impl Repacker {
    /// Repacks the committer's repository; failures become events. A stop
    /// kills a repack in progress.
    fn run(&self, committer: &Committer, stop: &Stop) {
        let repo = committer.history().repo();
        let result = repack::repack(Path::new("git"), repo.path(), &self.push_locks, stop);
        if matches!(
            result,
            Ok(RepackOutcome::PushInProgress | RepackOutcome::NothingToDo)
        ) {
            return;
        }
        // A repack that failed late may still have moved objects into new
        // packs: see them either way.
        let refreshed = repo.odb().and_then(|odb| odb.refresh());
        let error = match (result, refreshed) {
            (Err(e), _) => e.to_string(),
            (Ok(_), Err(e)) => format!("refreshing the object database: {e}"),
            (Ok(_), Ok(())) => return,
        };
        self.events.emit(Event::RepackFailed { error });
    }
}

fn commit_loop(
    committer: &Committer,
    stop: &Stop,
    signal: &Signal,
    repacker: Option<&Repacker>,
) -> Result<CommitReport, VolumeError> {
    let window = committer.history().settings().commit_window.max(MIN_DELAY);
    let mut failures = 0u32;
    let mut next_repack = repacker.map(|r| Instant::now() + r.schedule.first);
    loop {
        let delay = backoff(window, failures, COMMIT_BACKOFF_CAP);
        let woke = signal.wait(delay, |s| {
            (s.commit_requested > s.commit_done).then_some(s.commit_requested)
        });
        let generation = match woke {
            Woke::Stop => break,
            Woke::Request(generation) => Some(generation),
            Woke::Timeout => None,
        };
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
            continue;
        }
        if let (Some(r), Some(at)) = (repacker, next_repack)
            && Instant::now() >= at
        {
            r.run(committer, stop);
            next_repack = Some(Instant::now() + r.schedule.interval.max(MIN_DELAY));
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::events::{Event, MemorySink, NullSink, PushFailureKind};
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
        };
        let service =
            Service::start_with_repack(com, pushers, sink.clone(), Some(schedule)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while local_head(&state).is_none() || packs(&state) == 0 {
            assert!(Instant::now() < deadline, "never committed and repacked");
            thread::sleep(Duration::from_millis(10));
        }
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
    #[test]
    fn stop_wakes_waiters() {
        let stop = Arc::new(Stop::default());
        assert!(!stop.wait(Duration::from_millis(1)));
        let waiter = {
            let stop = stop.clone();
            thread::spawn(move || stop.wait(Duration::from_secs(60)))
        };
        let start = Instant::now();
        stop.stop();
        assert!(waiter.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(10));
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
        let service = Service::start(com, pushers, sink.clone()).unwrap();
        let mut remotes: Vec<&str> = service.remotes().collect();
        remotes.sort();
        assert_eq!(remotes, ["bad", "good"]);
        record(&rec, dir.path(), "a");
        record(&rec, dir.path(), "b");

        let remote_repo = git2::Repository::open_bare(remote.path()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let local = local_head(&state);
            let pushed = remote_repo.refname_to_id("refs/heads/agentfs/vol").ok();
            if local.is_some() && local == pushed {
                let head = remote_repo.find_commit(pushed.unwrap()).unwrap();
                if head.tree().unwrap().len() == 2 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "the good remote never caught up");
            thread::sleep(Duration::from_millis(20));
        }

        let start = Instant::now();
        let report = service.shutdown(Duration::from_secs(1));
        assert!(start.elapsed() < Duration::from_secs(3));
        report.final_commit.unwrap();
        assert!(report.abandoned_pushes.is_empty());

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
        let service = Service::start(com, pushers, events).unwrap();
        record(&rec, dir.path(), "a");
        assert_eq!(h.gauge_u64(names::JOURNAL_BACKLOG, &[]), Some(1));
        let report = service.shutdown(Duration::from_secs(5));
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
    /// done, so only the final push can send what is recorded next.
    fn idle_service(remote: &Path) -> (tempfile::TempDir, StateDir, Recorder, Service) {
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "1h"),
            ("agentfs.pushInterval", "1h"),
            ("remote.origin.url", &remote.display().to_string()),
        ]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        let service = Service::start(com, pushers, Arc::new(NullSink)).unwrap();
        // Let the first push (of nothing) happen.
        thread::sleep(Duration::from_millis(100));
        (dir, state, rec, service)
    }

    /// Every shutdown pushes its final commit within the grace period.
    #[test]
    fn a_final_push_sends_the_final_commit() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let report = service.shutdown(Duration::from_secs(10));
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
        let (_dir, _state, _rec, service) = idle_service(remote.path());
        let report = service.shutdown(Duration::from_secs(10));
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
        let (dir, state, rec, service) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let report = service.shutdown(Duration::ZERO);
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
        let (_dir, _state, _rec, service) = idle_service(remote.path());
        let report = service.shutdown(Duration::ZERO);
        assert!(report.abandoned_pushes.is_empty(), "{report:?}");
        assert_eq!(
            report.final_pushes,
            [("origin".to_owned(), FinalPush::NothingNew)]
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
        let (state, both) = signal.wait_until(Instant::now() + Duration::from_secs(30), |s| {
            s.commit_requested == 2
        });
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
        use std::io::Write as _;

        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        // Now the remote hangs: libgit2 blocks opening a FIFO config.
        let fifo = remote.path().join("config");
        fs::remove_file(&fifo).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let start = Instant::now();
        let report = service.shutdown(Duration::from_millis(300));
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(250), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        report.final_commit.unwrap();
        assert!(local_head(&state).is_some());
        assert_eq!(report.abandoned_pushes, ["origin"]);
        assert!(report.final_pushes.is_empty());
        // Release the detached thread.
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writer.write_all(b"").unwrap();
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while local_head(&state).is_none() {
            assert!(Instant::now() < deadline, "never committed");
            thread::sleep(Duration::from_millis(10));
        }
        let report = service.shutdown(Duration::ZERO);
        assert_eq!(report.final_commit.unwrap(), CommitReport::default());
        assert!(report.abandoned_pushes.is_empty());
    }

    #[test]
    fn a_push_still_running_is_abandoned_after_the_grace_period() {
        use std::io::Write as _;

        // A remote whose config is a FIFO: libgit2 blocks opening it, which
        // stands in for a push hung on the network.
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let fifo = remote.path().join("config");
        fs::remove_file(&fifo).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let (dir, state) = setup(&[
            ("agentfs.commitWindow", "10ms"),
            ("remote.hung.url", &remote.path().display().to_string()),
            (
                "remote.idle.url",
                &remote.path().join("missing").display().to_string(),
            ),
        ]);
        let (rec, com, pushers) = open(dir.path(), &state, Arc::new(NullSink));
        record(&rec, dir.path(), "a");
        com.commit_pending().unwrap();
        let service = Service::start(com, pushers, Arc::new(NullSink)).unwrap();
        thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        let report = service.shutdown(Duration::from_millis(200));
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(150), "{took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
        report.final_commit.unwrap();
        assert_eq!(report.abandoned_pushes, ["hung"]);

        // Release the detached thread.
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writer.write_all(b"").unwrap();
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
        let (dir, state, rec, service) = idle_service(remote.path());
        let handle = service.handle();
        assert_eq!(handle.remotes(), ["origin"]);
        record(&rec, dir.path(), "a");
        let report = handle.push_now(&[], Duration::from_secs(30)).unwrap();
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
        let again = handle
            .push_now(&["origin".to_owned()], Duration::from_secs(30))
            .unwrap();
        assert_eq!(again.committed.unwrap().records, 0);
        assert!(matches!(
            again.remotes[0].outcome,
            PushOutcome::UpToDate { .. }
        ));
        let report = service.shutdown(Duration::from_secs(5));
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
        let (dir2, state, rec, service) = idle_service(&remote);
        // The first push failed (the remote does not exist), so the loop
        // backs off for an hour; now the remote appears.
        git2::Repository::init_bare(&remote).unwrap();
        record(&rec, dir2.path(), "a");
        let started = Instant::now();
        let report = service
            .handle()
            .push_now(&[], Duration::from_secs(30))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(report.remotes[0].outcome.is_success(), "{report:?}");
        assert_eq!(remote_head(&remote), local_head(&state));
        service.shutdown(Duration::ZERO);
    }

    #[test]
    fn push_now_unknown_remote() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (_dir, _state, _rec, service) = idle_service(remote.path());
        assert_eq!(
            service
                .handle()
                .push_now(&["nope".to_owned()], Duration::from_secs(1)),
            Err(PushNowError::UnknownRemote("nope".into()))
        );
        service.shutdown(Duration::ZERO);
    }

    #[test]
    fn push_now_after_close_is_stopping() {
        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service) = idle_service(remote.path());
        let handle = service.handle();
        handle.close();
        assert_eq!(
            handle.push_now(&[], Duration::from_secs(1)),
            Err(PushNowError::Stopping)
        );
        // Closing refuses requests only: the loops still run, and the
        // shutdown still commits and pushes.
        record(&rec, dir.path(), "a");
        let report = service.shutdown(Duration::from_secs(10));
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
        use std::io::Write as _;

        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service) = idle_service(remote.path());
        record(&rec, dir.path(), "a");
        let fifo = remote.path().join("config");
        fs::remove_file(&fifo).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let started = Instant::now();
        let report = service
            .handle()
            .push_now(&[], Duration::from_millis(500))
            .unwrap();
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(400), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(report.committed.unwrap().commits, 1);
        assert!(local_head(&state).is_some());
        assert!(report.remotes.is_empty());
        assert_eq!(report.still_running, ["origin"]);
        let report = service.shutdown(Duration::from_millis(100));
        assert_eq!(report.abandoned_pushes, ["origin"]);
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writer.write_all(b"").unwrap();
    }

    /// Push now never commits an operation still running (a restore):
    /// it would publish it half done.
    #[test]
    fn push_now_leaves_a_running_restore_group_uncommitted() {
        use crate::record::OperationKind;

        let remote = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(remote.path()).unwrap();
        let (dir, state, rec, service) = idle_service(remote.path());
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
        let report = service
            .handle()
            .push_now(&[], Duration::from_secs(30))
            .unwrap();
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
        let report = service.shutdown(Duration::from_secs(10));
        assert_eq!(report.final_commit.unwrap().records, 1);
        assert_eq!(remote_head(remote.path()), local_head(&state));
    }
}
