//! Exclusive use of a state area by one process (design §2.3).
//!
//! Every command that opens a volume for writing (`mount`, `run`, `record`,
//! `flush`, `push`) takes [`StateLock`] first: an exclusive `flock` on
//! `<state>/meta/lock`. The kernel releases it when the holder exits, even
//! after `kill -9`, so a crash never leaves a stale lock. The file holds a
//! one-line description of the holder (`pid=… node=… command=… since=…`,
//! [`crate::lockfile`]) for
//! error messages and `agentfs status`; it is informational only, and a
//! description left behind by a dead holder is never mistaken for a held
//! lock.
//!
//! The lock is a [`HeldLock`], so it also holds where `flock` is emulated
//! with POSIX locks (NFS, design §14.2).
//!
//! [`StateLock::holder`] probes with a shared lock that it drops at once,
//! so for a moment it can itself look like a holder. [`StateLock::acquire`]
//! therefore keeps trying for at least [`MIN_ACQUIRE_WAIT`] even when asked
//! not to wait.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::lockfile::{self, HeldLock};
use crate::volume::StateDir;

/// How long [`StateLock::acquire`] keeps trying even with no wait, so a
/// concurrent [`StateLock::holder`] probe cannot make it fail.
pub const MIN_ACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// Polling interval while waiting for the lock, for the first
/// [`FAST_POLL_FOR`].
const POLL: Duration = Duration::from_millis(50);

/// How long [`StateLock::acquire`] polls at [`POLL`] before it backs off to
/// [`SLOW_POLL`]: a standby waiting without limit (design §14.2) must not
/// load a shared filesystem.
const FAST_POLL_FOR: Duration = Duration::from_secs(5);
const SLOW_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("state area {} is in use ({holder})", state.display())]
    Busy { state: PathBuf, holder: String },
    #[error("locking {}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// Held for as long as this process uses the state area. Dropping it (or
/// the process exiting) releases the lock.
#[derive(Debug)]
pub struct StateLock {
    held: HeldLock,
}

impl StateLock {
    /// Takes the exclusive lock on `state`, trying until `wait` (at least
    /// [`MIN_ACQUIRE_WAIT`]) has passed. `command` names the holder in the
    /// description other processes see. Creates `meta/` if an older state
    /// area lacks it.
    pub fn acquire(state: &StateDir, command: &str, wait: Duration) -> Result<Self, LockError> {
        let path = state.lock_file();
        let io_err = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        fs::create_dir_all(state.meta()).map_err(io_err)?;
        let start = Instant::now();
        let deadline = start.checked_add(wait.max(MIN_ACQUIRE_WAIT));
        let mut held = loop {
            if let Some(held) = HeldLock::try_exclusive(&path, 0o600).map_err(io_err)? {
                break held;
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Err(LockError::Busy {
                    state: state.root().to_path_buf(),
                    holder: describe(lockfile::probe(&path).ok().flatten().unwrap_or_default()),
                });
            }
            let poll = if now.duration_since(start) < FAST_POLL_FOR {
                POLL
            } else {
                SLOW_POLL
            };
            std::thread::sleep(deadline.map_or(poll, |d| poll.min(d - now)));
        };
        held.set_holder(&lockfile::holder_line(&format!("command={command}")))
            .map_err(io_err)?;
        Ok(Self { held })
    }

    /// The holder's description if a process holds the lock on `state`
    /// (this one included), `None` if it is free (or the state area has
    /// no lock file).
    pub fn holder(state: &StateDir) -> io::Result<Option<String>> {
        Ok(lockfile::probe(&state.lock_file())?.map(describe))
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        self.held.path()
    }
}

fn describe(holder: String) -> String {
    if holder.is_empty() {
        "holder unknown".into()
    } else {
        holder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        (dir, state)
    }

    #[test]
    fn second_acquire_is_busy_and_names_the_holder() {
        let (_dir, state) = state();
        let lock = StateLock::acquire(&state, "mount", Duration::ZERO).unwrap();
        assert!(lock.path().ends_with("meta/lock"));
        let err = StateLock::acquire(&state, "flush", Duration::ZERO).unwrap_err();
        let LockError::Busy { holder, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(
            holder.contains(&format!("pid={}", std::process::id())),
            "{holder}"
        );
        assert!(holder.contains("command=mount"), "{holder}");
        assert!(err.to_string().contains("is in use"), "{err}");
    }

    #[test]
    fn drop_releases() {
        let (_dir, state) = state();
        let lock = StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
        drop(lock);
        StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
    }

    #[test]
    fn waiting_succeeds_once_released() {
        let (_dir, state) = state();
        let lock = StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        let start = Instant::now();
        StateLock::acquire(&state, "flush", Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(250));
        releaser.join().unwrap();
    }

    #[test]
    fn a_short_wait_times_out() {
        let (_dir, state) = state();
        let _lock = StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
        let start = Instant::now();
        let err = StateLock::acquire(&state, "flush", Duration::from_millis(400)).unwrap_err();
        assert!(matches!(err, LockError::Busy { .. }), "{err:?}");
        assert!(start.elapsed() >= Duration::from_millis(400));
    }

    #[test]
    fn holder_probe() {
        let (_dir, state) = state();
        assert_eq!(StateLock::holder(&state).unwrap(), None, "no lock file");
        let lock = StateLock::acquire(&state, "mount", Duration::ZERO).unwrap();
        let holder = StateLock::holder(&state).unwrap().unwrap();
        assert!(holder.contains("command=mount"), "{holder}");
        drop(lock);
        assert_eq!(StateLock::holder(&state).unwrap(), None);
    }

    #[test]
    fn a_probe_does_not_make_acquire_fail() {
        let (_dir, state) = state();
        let lock = StateLock::acquire(&state, "x", Duration::ZERO).unwrap();
        drop(lock);
        // Hold a shared lock briefly, as a probe would.
        let probe = std::fs::File::open(state.lock_file()).unwrap();
        rustix::fs::flock(&probe, rustix::fs::FlockOperation::LockShared).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(probe);
        });
        StateLock::acquire(&state, "mount", Duration::ZERO).unwrap();
        releaser.join().unwrap();
    }

    #[test]
    fn posix_locks_behave_as_flock_within_a_process() {
        // NFS emulates flock with POSIX locks, which a process's own second
        // open neither conflicts with nor survives the close of.
        crate::lockfile::use_posix_locks_on_this_thread(true);
        let (_dir, state) = state();
        let lock = StateLock::acquire(&state, "serve-history", Duration::ZERO).unwrap();
        let err = StateLock::acquire(&state, "run", Duration::ZERO).unwrap_err();
        assert!(matches!(err, LockError::Busy { .. }), "{err:?}");
        for _ in 0..3 {
            let holder = StateLock::holder(&state).unwrap().unwrap();
            assert!(holder.contains("command=serve-history"), "{holder}");
            assert!(holder.contains(" node="), "{holder}");
        }
        assert!(crate::lockfile::held_here(lock.path()));
        drop(lock);
        assert_eq!(StateLock::holder(&state).unwrap(), None);
        StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
        crate::lockfile::use_posix_locks_on_this_thread(false);
    }

    #[test]
    fn creates_meta_for_old_state_areas() {
        let (_dir, state) = state();
        fs::create_dir_all(state.root()).unwrap();
        assert!(!state.meta().exists());
        StateLock::acquire(&state, "run", Duration::ZERO).unwrap();
        assert!(state.lock_file().exists());
    }
}
