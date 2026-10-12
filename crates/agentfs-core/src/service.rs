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
//! [`Service::shutdown`] stops every loop, waits for the final commit, and
//! waits for pushes in progress only up to a grace period: stopping never
//! waits for a push (design §11.2). libgit2 cannot cancel a push, so a push
//! still running after the grace period is left on its detached thread
//! until the process exits.
//!
//! Signals are a process concern: the binary decides when to call
//! [`Service::shutdown`].

use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::events::{Event, Events};
use crate::push::{RemotePusher, backoff};
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

/// What [`Service::shutdown`] found.
#[derive(Debug)]
pub struct ShutdownReport {
    /// The last commit pass, which includes the open group.
    pub final_commit: Result<CommitReport, VolumeError>,
    /// Remotes whose push was still running when the grace period ended.
    pub abandoned_pushes: Vec<String>,
}

/// The running commit and push loops.
pub struct Service {
    stop: Arc<Stop>,
    commit: JoinHandle<Result<CommitReport, VolumeError>>,
    pushes: Vec<(String, JoinHandle<()>)>,
    push_done: mpsc::Receiver<String>,
    events: Events,
}

impl Service {
    /// Starts the loops. The committer and pushers report their own events;
    /// `events` receives the service's own (a commit thread that panicked).
    pub fn start(
        committer: Committer,
        pushers: Vec<RemotePusher>,
        events: Events,
    ) -> io::Result<Self> {
        let stop = Arc::new(Stop::default());
        let commit = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("agentfs-commit".into())
                .spawn(move || commit_loop(&committer, &stop))?
        };
        let (done_tx, push_done) = mpsc::channel();
        let mut pushes = Vec::with_capacity(pushers.len());
        for mut pusher in pushers {
            let name = pusher.name().to_owned();
            let stop_flag = stop.clone();
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
                    let _ = done.send(pusher.name().to_owned());
                });
            match spawned {
                Ok(handle) => pushes.push((name, handle)),
                Err(e) => {
                    // Wind down what already started before reporting.
                    stop.stop();
                    return Err(e);
                }
            }
        }
        Ok(Self {
            stop,
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

    /// Stops every loop: makes the final commit, then waits up to
    /// `push_grace` (from the call) for pushes in progress.
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

        let mut pending: Vec<(String, JoinHandle<()>)> = self.pushes;
        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let done = match self.push_done.recv_timeout(remaining) {
                Ok(name) => name,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                // Every sender gone: the rest panicked; join reports it.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(i) = pending.iter().position(|(name, _)| *name == done) {
                let (_, handle) = pending.swap_remove(i);
                let _ = handle.join();
            }
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
        ShutdownReport {
            final_commit,
            abandoned_pushes: abandoned,
        }
    }
}

fn commit_loop(committer: &Committer, stop: &Stop) -> Result<CommitReport, VolumeError> {
    let window = committer.history().settings().commit_window.max(MIN_DELAY);
    let mut failures = 0u32;
    loop {
        let delay = backoff(window, failures, COMMIT_BACKOFF_CAP);
        if stop.wait(delay) {
            break;
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
