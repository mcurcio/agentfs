//! A remote's push lock: an `flock` on `<state>/meta/push/<remote>.lock`, held
//! by a push while it runs and by a repack ([`crate::repack`]) so it never
//! deletes a pack a push reads (design §8, §11.2).
//!
//! **Released explicitly.** An `flock` belongs to the open file, and a
//! child process spawned by any thread of this process shares that open
//! file from its fork until its exec closes it (`O_CLOEXEC`). Released
//! only by closing, a lock would stay held, for as long as such a child
//! takes to reach its exec, after its holder let go: the next push or
//! repack, in this process or another, would find it busy though no one
//! holds it. A spawn is no rare event in the daemon (the repack runs
//! `git`) nor in a test binary. [`PushLock`] therefore unlocks before it
//! closes: the unlock releases the lock for every sharer at once.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

/// A push lock held; dropping it releases it, whatever shares its file.
#[derive(Debug)]
pub struct PushLock(File);

impl PushLock {
    /// Takes the push lock at `path` without waiting; `None` while another
    /// holder has it.
    pub fn try_take(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Some(Self(file))),
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for PushLock {
    fn drop(&mut self) {
        // Closing alone would leave it held by a child not yet at its exec.
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A lock let go is free at once, though another thread keeps spawning
    /// processes that share its file until they exec.
    #[test]
    fn a_lock_let_go_is_free_while_children_spawn() {
        const SPAWNS: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("origin.lock");
        let spawned = Arc::new(AtomicUsize::new(0));
        let spawner = {
            let spawned = spawned.clone();
            std::thread::spawn(move || {
                for _ in 0..SPAWNS {
                    std::process::Command::new("true").status().unwrap();
                    spawned.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        let (mut taken, mut busy) = (0, 0);
        while spawned.load(Ordering::SeqCst) < SPAWNS {
            match PushLock::try_take(&path).unwrap() {
                Some(lock) => {
                    taken += 1;
                    drop(lock);
                }
                None => busy += 1,
            }
        }
        spawner.join().unwrap();
        assert_eq!(busy, 0, "busy with no holder, of {} takes", taken + busy);
    }

    #[test]
    fn a_held_lock_is_busy_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("origin.lock");
        let held = PushLock::try_take(&path).unwrap().unwrap();
        assert!(PushLock::try_take(&path).unwrap().is_none());
        drop(held);
        assert!(PushLock::try_take(&path).unwrap().is_some());
    }
}
