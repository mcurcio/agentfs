//! The LFS cache lock, `<state>/lfs/lock` (design §2.2, §7): a `flock`
//! ([`crate::file_lock`]) that keeps `agentfs lfs prune` from deleting an
//! object while anything that reads objects for writing them into the
//! tree holds it.
//!
//! Restores, reverts, patch applies and `agentfs lfs fetch` hold it
//! shared for as long as they use the cache ([`super::fetch::Fetcher`],
//! [`super::fetch::CacheOnly`]): from a restore's check, which makes every
//! object it writes available, through its writes. A prune takes it
//! exclusive for its deletions only, so it never removes an object
//! between a restore's check and its write. Commits and pushes do not
//! take it: a prune deletes only objects every remote's servers hold,
//! which a push never needs to upload and a commit may add again.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::file_lock::{Attempt, FileLock, Mode};

/// The lock file's name in `<state>/lfs`.
pub const LOCK_FILE: &str = "lock";

/// How often a waiting [`CacheLock::exclusive`] tries again.
const POLL: Duration = Duration::from_millis(100);

/// A held cache lock, released when dropped.
#[derive(Debug)]
pub struct CacheLock {
    _lock: FileLock,
}

fn open(dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    FileLock::open(&dir.join(LOCK_FILE))
}

impl CacheLock {
    /// The lock of the cache at `dir` (`<state>/lfs`), shared, waiting
    /// while a prune holds it.
    pub fn shared(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            _lock: FileLock::wait(open(dir)?, Mode::Shared)?,
        })
    }

    /// The lock exclusive, waiting up to `wait` while others hold it;
    /// `None` if they still do then.
    pub fn exclusive(dir: &Path, wait: Duration) -> io::Result<Option<Self>> {
        let mut file = open(dir)?;
        let deadline = Instant::now() + wait;
        loop {
            match FileLock::try_lock(file, Mode::Exclusive)? {
                Attempt::Taken(lock) => return Ok(Some(Self { _lock: lock })),
                Attempt::Busy(busy) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(POLL.min(deadline - now));
                    file = busy;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock let go is free at once, though another thread keeps spawning
    /// processes that share its file until they exec.
    #[test]
    fn a_lock_let_go_is_free_while_children_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let (busy, takes) = crate::file_lock::takes_while_children_spawn(|| {
            drop(CacheLock::shared(dir.path()).unwrap());
            CacheLock::exclusive(dir.path(), Duration::ZERO)
                .unwrap()
                .is_some()
        });
        assert_eq!(busy, 0, "busy with no holder, of {takes} takes");
    }

    #[test]
    fn shared_holders_keep_the_exclusive_lock_out() {
        let dir = tempfile::tempdir().unwrap();
        let a = CacheLock::shared(dir.path()).unwrap();
        let b = CacheLock::shared(dir.path()).unwrap();
        assert!(
            CacheLock::exclusive(dir.path(), Duration::ZERO)
                .unwrap()
                .is_none()
        );
        drop(a);
        let started = Instant::now();
        assert!(
            CacheLock::exclusive(dir.path(), Duration::from_millis(250))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(250));
        // Released while it waits: it gets the lock.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(b);
        });
        let held = CacheLock::exclusive(dir.path(), Duration::from_secs(5)).unwrap();
        assert!(held.is_some());
        releaser.join().unwrap();
        // A shared taker waits for the exclusive holder.
        let path = dir.path().to_owned();
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let _shared = CacheLock::shared(&path).unwrap();
            started.elapsed()
        });
        std::thread::sleep(Duration::from_millis(150));
        drop(held);
        assert!(waiter.join().unwrap() >= Duration::from_millis(100));
    }
}
