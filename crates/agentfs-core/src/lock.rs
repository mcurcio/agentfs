//! Exclusive use of a state area by one process (design §2.3).
//!
//! Every command that opens a volume for writing (`mount`, `run`, `record`,
//! `flush`, `push`) takes [`StateLock`] first: an exclusive `flock` on
//! `<state>/meta/lock`. The kernel releases it when the holder exits, even
//! after `kill -9`, so a crash never leaves a stale lock. The file holds a
//! one-line description of the holder (`pid=… command=… since=…`) for
//! error messages and `agentfs status`; it is informational only, and a
//! description left behind by a dead holder is never mistaken for a held
//! lock.
//!
//! [`StateLock::holder`] probes with a shared lock that it drops at once,
//! so for a moment it can itself look like a holder. [`StateLock::acquire`]
//! therefore keeps trying for at least [`MIN_ACQUIRE_WAIT`] even when asked
//! not to wait.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};

use crate::push::utc_basic;
use crate::volume::StateDir;

/// How long [`StateLock::acquire`] keeps trying even with no wait, so a
/// concurrent [`StateLock::holder`] probe cannot make it fail.
pub const MIN_ACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// Polling interval while waiting for the lock.
const POLL: Duration = Duration::from_millis(50);

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
    // Held only for the lock it carries.
    _file: File,
    path: PathBuf,
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
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(io_err)?;
        let deadline = Instant::now() + wait.max(MIN_ACQUIRE_WAIT);
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(LockError::Busy {
                            state: state.root().to_path_buf(),
                            holder: describe(read_holder(&mut file)),
                        });
                    }
                    std::thread::sleep(POLL.min(deadline - now));
                }
                Err(e) => return Err(io_err(e.into())),
            }
        }
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!(
            "pid={} command={command} since={}\n",
            std::process::id(),
            utc_basic(since)
        );
        (|| {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(line.as_bytes())
        })()
        .map_err(io_err)?;
        Ok(Self { _file: file, path })
    }

    /// The holder's description if another process holds the lock on
    /// `state`, `None` if it is free (or the state area has no lock file).
    pub fn holder(state: &StateDir) -> io::Result<Option<String>> {
        let mut file = match File::open(state.lock_file()) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        loop {
            match flock(&file, FlockOperation::NonBlockingLockShared) {
                Ok(()) => {
                    flock(&file, FlockOperation::Unlock)?;
                    return Ok(None);
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    return Ok(Some(describe(read_holder(&mut file))));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_holder(file: &mut File) -> String {
    let mut text = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.take(512).read_to_string(&mut text);
    text.trim().to_owned()
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
        let probe = File::open(state.lock_file()).unwrap();
        flock(&probe, FlockOperation::LockShared).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(probe);
        });
        StateLock::acquire(&state, "mount", Duration::ZERO).unwrap();
        releaser.join().unwrap();
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
