//! Failpoints: named places on the durable path where a test build can
//! kill the process, fail with an I/O error, panic or stall (design §18
//! test 3, the crash model of §3.3 and §3.4).
//!
//! Every step of record → commit → ref update → push that leaves
//! something on disk calls [`hit`] with its name, listed in [`POINTS`].
//! In a normal build (`failpoints` feature off, as in every release and
//! image) [`hit`] is an inlined `Ok(())` and nothing else of this module
//! exists beyond the names. With the feature on, a point does what the
//! [`ENV_VAR`] variable (read once, on the first hit) or
//! `configure` (in-process tests) says:
//!
//! ```text
//! AGENTFS_FAILPOINTS='journal.after_fsync=kill@3;push.after_remote_update=error'
//! ```
//!
//! `name=action[@n]`, joined with `;`. The action is `kill` (SIGKILL to
//! this process, as a crash or power cut of the daemon looks to the
//! state area), `error` (the step returns an `io::Error`), `panic`, or
//! `sleep(<ms>)`; `@n` fires on the `n`th hit of the point only (default
//! 1). Before it acts, a point writes
//! `agentfs failpoint <name> triggered (<action>)` to stderr, which the
//! harnesses look for.
//!
//! An unknown name or bad syntax in the variable aborts the process with
//! a message: a test that misspells a point must not pass by never firing.

#[cfg(not(feature = "failpoints"))]
use std::io;

/// The environment variable a `failpoints` build reads its spec from.
/// Release builds have no failpoints and ignore it.
pub const ENV_VAR: &str = "AGENTFS_FAILPOINTS";

/// Every failpoint, in the order a change passes them. A test
/// (`every_hit_is_listed`) keeps this list equal to the `hit` calls in
/// the source.
pub const POINTS: &[&str] = &[
    // The synchronous step (design §3.2).
    "pending.after_freeze",
    "journal.before_write",
    "journal.before_fsync",
    "journal.after_fsync",
    "record.after_journal",
    "service.after_record",
    "front.after_reply",
    "fuse.after_record",
    "repairs.before_rename",
    // Commits (design §3.3, §3.4).
    "lfs.after_cache_insert",
    "commit.after_tree",
    "commit.sync_objects",
    "commit.after_object",
    "commit.after_ref",
    "commit.after_sync",
    "journal.truncate.after_tmp",
    "journal.truncate.after_rename",
    "commit.before_pending_remove",
    "pending.remove.between",
    // Pushes (design §11).
    "push.after_lfs_upload",
    "push.after_remote_update",
    "push.diverged.after_pending",
    "push.diverged.after_ref",
    "state.json.before_rename",
    // The patch queue (design §12) and repack (§3.4).
    "patch.after_tree_write",
    "patch.after_record",
    "patch.after_commit",
    "queue.after_status",
    "repack.running",
    // The history service's state lock across nodes (design §14.2).
    "lock.heartbeat",
];

/// Passes failpoint `name`: `Ok(())` unless a test build was told to act
/// here. A no-op in builds without the `failpoints` feature.
#[cfg(not(feature = "failpoints"))]
#[inline(always)]
pub fn hit(_name: &'static str) -> io::Result<()> {
    Ok(())
}

#[cfg(feature = "failpoints")]
pub use imp::{FailpointError, Guard, configure, hit, hits, trace};

#[cfg(feature = "failpoints")]
mod imp {
    use std::collections::HashMap;
    use std::io::{self, Write as _};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, Once, PoisonError};
    use std::time::Duration;

    use super::{ENV_VAR, POINTS};

    /// A spec that cannot be used.
    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum FailpointError {
        #[error("unknown failpoint {0:?} (see agentfs_core::failpoints::POINTS)")]
        UnknownPoint(String),
        #[error("bad failpoint spec {0:?}: expected name=kill|error|panic|sleep(ms)[@n]")]
        BadSpec(String),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Action {
        Kill,
        Error,
        Panic,
        Sleep(u64),
    }

    impl Action {
        fn label(self) -> String {
            match self {
                Self::Kill => "kill".into(),
                Self::Error => "error".into(),
                Self::Panic => "panic".into(),
                Self::Sleep(ms) => format!("sleep({ms})"),
            }
        }
    }

    #[derive(Default)]
    struct State {
        /// Only this thread's hits count and act, for a spec set by
        /// [`configure`]: in-process tests run in parallel. `None` (the
        /// environment's spec): every thread.
        thread: Option<std::thread::ThreadId>,
        /// Point → (action, the hit it fires on).
        actions: HashMap<&'static str, (Action, u64)>,
        hits: HashMap<&'static str, u64>,
        trace: Vec<&'static str>,
    }

    /// Whether any spec is in force: the fast path of [`hit`] is this
    /// one relaxed load.
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static STATE: Mutex<Option<State>> = Mutex::new(None);
    static ENV: Once = Once::new();
    /// Serialises in-process tests that configure failpoints.
    static TESTS: Mutex<()> = Mutex::new(());

    fn state() -> MutexGuard<'static, Option<State>> {
        STATE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn parse(spec: &str) -> Result<HashMap<&'static str, (Action, u64)>, FailpointError> {
        let mut actions = HashMap::new();
        for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let bad = || FailpointError::BadSpec(part.to_owned());
            let (name, rest) = part.split_once('=').ok_or_else(bad)?;
            let name = name.trim();
            let point = POINTS
                .iter()
                .copied()
                .find(|p| *p == name)
                .ok_or_else(|| FailpointError::UnknownPoint(name.to_owned()))?;
            let (action, nth) = match rest.trim().rsplit_once('@') {
                Some((a, n)) => (a, n.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(bad)?),
                None => (rest.trim(), 1),
            };
            let action = match action {
                "kill" => Action::Kill,
                "error" => Action::Error,
                "panic" => Action::Panic,
                other => {
                    let ms = other
                        .strip_prefix("sleep(")
                        .and_then(|s| s.strip_suffix(')'))
                        .and_then(|ms| ms.parse().ok())
                        .ok_or_else(bad)?;
                    Action::Sleep(ms)
                }
            };
            actions.insert(point, (action, nth));
        }
        Ok(actions)
    }

    fn load_env() {
        ENV.call_once(|| {
            let Some(spec) = std::env::var_os(ENV_VAR) else {
                return;
            };
            let spec = spec.to_string_lossy();
            match parse(&spec) {
                Ok(actions) => {
                    *state() = Some(State {
                        actions,
                        ..State::default()
                    });
                    ACTIVE.store(true, Ordering::Release);
                }
                Err(e) => {
                    eprintln!("agentfs: {ENV_VAR}: {e}");
                    std::process::abort();
                }
            }
        });
    }

    /// Passes failpoint `name`, doing what the spec in force says when
    /// this is the hit it names.
    pub fn hit(name: &'static str) -> io::Result<()> {
        load_env();
        if !ACTIVE.load(Ordering::Relaxed) {
            return Ok(());
        }
        let action = {
            let mut guard = state();
            let Some(st) = guard.as_mut() else {
                return Ok(());
            };
            if st.thread.is_some_and(|t| t != std::thread::current().id()) {
                return Ok(());
            }
            let count = st.hits.entry(name).or_default();
            *count += 1;
            let count = *count;
            st.trace.push(name);
            match st.actions.get(name) {
                Some((action, nth)) if *nth == count => *action,
                _ => return Ok(()),
            }
        };
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "agentfs failpoint {name} triggered ({})",
            action.label()
        );
        let _ = stderr.flush();
        drop(stderr);
        match action {
            Action::Kill => {
                let me = rustix::process::getpid();
                let _ = rustix::process::kill_process(me, rustix::process::Signal::KILL);
                // SIGKILL cannot be caught; in case delivery lags.
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            Action::Error => Err(io::Error::other(format!("failpoint {name}"))),
            Action::Panic => panic!("failpoint {name}"),
            Action::Sleep(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(())
            }
        }
    }

    /// How many times `name` was hit since the spec in force was set.
    /// Hits are counted only while a spec is in force (from [`ENV_VAR`] or
    /// [`configure`], even an empty one).
    pub fn hits(name: &str) -> u64 {
        load_env();
        state()
            .as_ref()
            .and_then(|st| st.hits.iter().find(|(k, _)| **k == name).map(|(_, v)| *v))
            .unwrap_or(0)
    }

    /// Every point hit since the spec in force was set, in order.
    pub fn trace() -> Vec<&'static str> {
        load_env();
        state()
            .as_ref()
            .map(|st| st.trace.clone())
            .unwrap_or_default()
    }

    /// Sets `spec` (the [`ENV_VAR`] syntax; empty counts hits only) for
    /// the calling thread until the guard drops: hits on other threads
    /// neither count nor act, so in-process tests can run in parallel.
    /// Guards serialise: a second `configure` waits for the first guard
    /// to drop. A spec from [`ENV_VAR`] applies to every thread.
    pub fn configure(spec: &str) -> Result<Guard, FailpointError> {
        let actions = parse(spec)?;
        let lock = TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        load_env();
        *state() = Some(State {
            actions,
            thread: Some(std::thread::current().id()),
            ..State::default()
        });
        ACTIVE.store(true, Ordering::Release);
        Ok(Guard { _lock: lock })
    }

    /// Keeps a [`configure`]d spec in force; dropping it turns every
    /// failpoint off.
    pub struct Guard {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.store(false, Ordering::Release);
            *state() = None;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn specs_parse() {
            let a = parse("journal.after_fsync=kill@3; push.after_remote_update=error").unwrap();
            assert_eq!(a["journal.after_fsync"], (Action::Kill, 3));
            assert_eq!(a["push.after_remote_update"], (Action::Error, 1));
            let a = parse("commit.after_ref=sleep(25)@2;").unwrap();
            assert_eq!(a["commit.after_ref"], (Action::Sleep(25), 2));
            assert!(parse("").unwrap().is_empty());
        }

        #[test]
        fn bad_specs_are_refused() {
            assert_eq!(
                parse("no.such.point=kill"),
                Err(FailpointError::UnknownPoint("no.such.point".into()))
            );
            for bad in [
                "journal.after_fsync",
                "journal.after_fsync=explode",
                "journal.after_fsync=kill@0",
                "journal.after_fsync=kill@x",
                "journal.after_fsync=sleep(x)",
            ] {
                assert!(
                    matches!(parse(bad), Err(FailpointError::BadSpec(_))),
                    "{bad}"
                );
            }
        }

        #[test]
        fn the_nth_hit_fires_once_and_errors() {
            let _guard = configure("journal.before_write=error@2").unwrap();
            assert!(hit("journal.before_write").is_ok());
            let e = hit("journal.before_write").unwrap_err();
            assert_eq!(e.to_string(), "failpoint journal.before_write");
            assert!(hit("journal.before_write").is_ok());
            assert_eq!(hits("journal.before_write"), 3);
            assert!(hit("commit.after_ref").is_ok());
            assert_eq!(
                trace(),
                [
                    "journal.before_write",
                    "journal.before_write",
                    "journal.before_write",
                    "commit.after_ref"
                ]
            );
        }

        #[test]
        fn a_dropped_guard_turns_everything_off() {
            {
                let _guard = configure("commit.after_tree=error").unwrap();
                assert!(hit("commit.after_tree").is_err());
            }
            assert!(hit("commit.after_tree").is_ok());
            assert_eq!(hits("commit.after_tree"), 0);
        }

        #[test]
        fn sleep_and_panic() {
            let _guard = configure("commit.after_tree=sleep(1);commit.after_ref=panic").unwrap();
            assert!(hit("commit.after_tree").is_ok());
            let panicked = std::panic::catch_unwind(|| hit("commit.after_ref"));
            assert!(panicked.is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    fn scan(dir: &Path, found: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let needle = "failpoints::hit(\"";
                let mut rest = text.as_str();
                while let Some(i) = rest.find(needle) {
                    rest = &rest[i + needle.len()..];
                    let end = rest.find('"').unwrap();
                    found.insert(rest[..end].to_owned());
                }
            }
        }
    }

    /// [`POINTS`] is exactly the set of failpoint calls (`hit` with a name) in
    /// the workspace's sources, so a renamed or removed point fails here.
    #[test]
    fn every_hit_is_listed() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut found = BTreeSet::new();
        for krate in ["agentfs-core", "agentfs-fuse", "agentfs"] {
            scan(&crates.join(krate).join("src"), &mut found);
        }
        let listed: BTreeSet<String> = POINTS.iter().map(|p| (*p).to_owned()).collect();
        assert_eq!(found, listed);
        assert_eq!(listed.len(), POINTS.len(), "a point is listed twice");
    }

    #[test]
    fn hit_is_a_no_op_without_a_spec() {
        assert!(hit("commit.after_ref").is_ok());
    }
}
