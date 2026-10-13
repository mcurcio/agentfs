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
//! Signals are a process concern: the binary decides when to call
//! [`Service::shutdown`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::events::{Event, Events};
use crate::push::{PushOutcome, RemotePusher, backoff};
use crate::repack::{self, RepackOutcome, RepackSchedule};
use crate::volume::{CommitReport, Committer, VolumeError};

/// The longest wait between commit attempts after failures.
pub const COMMIT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The shortest wait between loop iterations, whatever the settings say.
const MIN_DELAY: Duration = Duration::from_millis(10);

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

/// The running commit and push loops.
pub struct Service {
    stop: Arc<Stop>,
    /// Released once the final commit is done and the deadline is set: the
    /// push threads wait for it, then make their final attempt.
    committed: Arc<Stop>,
    /// When the final push must have started by; set before `committed`
    /// is released.
    deadline: Arc<Mutex<Option<Instant>>>,
    commit: JoinHandle<Result<CommitReport, VolumeError>>,
    pushes: Vec<(String, JoinHandle<()>)>,
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
        let committed = Arc::new(Stop::default());
        let deadline = Arc::new(Mutex::new(None));
        let commit = {
            let stop = stop.clone();
            let repacker = repack.map(|schedule| Repacker {
                schedule,
                push_locks: pushers.iter().map(|p| p.lock_path().to_owned()).collect(),
                events: events.clone(),
            });
            thread::Builder::new()
                .name("agentfs-commit".into())
                .spawn(move || commit_loop(&committer, &stop, repacker.as_ref()))?
        };
        let (done_tx, push_done) = mpsc::channel();
        let mut pushes = Vec::with_capacity(pushers.len());
        for mut pusher in pushers {
            let name = pusher.name().to_owned();
            let stop_flag = stop.clone();
            let committed_flag = committed.clone();
            let deadline = deadline.clone();
            let done = done_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("agentfs-push-{name}"))
                .spawn(move || {
                    loop {
                        pusher.push_once();
                        if stop_flag.wait(pusher.next_delay().max(MIN_DELAY)) {
                            break;
                        }
                    }
                    // The final commit may hold what this remote lacks.
                    while !committed_flag.wait(Duration::from_secs(3600)) {}
                    let deadline = *deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = final_push(&mut pusher, deadline);
                    let _ = done.send((pusher.name().to_owned(), result));
                });
            match spawned {
                Ok(handle) => pushes.push((name, handle)),
                Err(e) => {
                    // Wind down what already started before reporting.
                    stop.stop();
                    committed.stop();
                    return Err(e);
                }
            }
        }
        Ok(Self {
            stop,
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
        self.pushes.iter().map(|(name, _)| name.as_str())
    }

    /// Stops every loop: makes the final commit, then gives every remote
    /// that lacks commits one more push attempt (the final commit's among
    /// them), all within `push_grace` from the call. An attempt is only
    /// started before the deadline, and one still running at the deadline
    /// is abandoned; a grace of zero skips the final push.
    pub fn shutdown(self, push_grace: Duration) -> ShutdownReport {
        let deadline = Instant::now() + push_grace;
        self.stop.stop();
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

        let mut pending: Vec<(String, JoinHandle<()>)> = self.pushes;
        let mut final_pushes = Vec::new();
        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (done, result) = match self.push_done.recv_timeout(remaining) {
                Ok(done) => done,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                // Every sender gone: the rest panicked; join reports it.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(i) = pending.iter().position(|(name, _)| *name == done) {
                let (_, handle) = pending.swap_remove(i);
                let _ = handle.join();
            }
            final_pushes.push((done, result));
        }
        // Threads that finished without reporting (a panic) are not
        // abandoned; reap them.
        let mut abandoned = Vec::new();
        for (name, handle) in pending {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                abandoned.push(name);
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

/// The final push of one remote: one attempt if it lacks commits and the
/// deadline has not passed.
fn final_push(pusher: &mut RemotePusher, deadline: Option<Instant>) -> FinalPush {
    let unpushed = match pusher.unpushed() {
        Ok(None | Some(0)) => return FinalPush::NothingNew,
        Ok(Some(n)) => Some(n),
        // Unknown: an attempt finds out.
        Err(_) => None,
    };
    if deadline.is_some_and(|d| Instant::now() < d) {
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
    repacker: Option<&Repacker>,
) -> Result<CommitReport, VolumeError> {
    let window = committer.history().settings().commit_window.max(MIN_DELAY);
    let mut failures = 0u32;
    let mut next_repack = repacker.map(|r| Instant::now() + r.schedule.first);
    loop {
        let delay = backoff(window, failures, COMMIT_BACKOFF_CAP);
        if stop.wait(delay) {
            break;
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
        // The push thread may or may not report before the zero deadline;
        // either way nothing was pushed.
        match report.final_pushes.as_slice() {
            [] => assert_eq!(report.abandoned_pushes, ["origin"]),
            [(name, FinalPush::Skipped { unpushed })] => {
                assert_eq!((name.as_str(), *unpushed), ("origin", 1));
            }
            other => panic!("{other:?}"),
        }
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
}
