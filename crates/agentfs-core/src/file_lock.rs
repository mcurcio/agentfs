//! Every advisory file lock agentfs takes (design §2.2): the state-area
//! lock (§2.3), the push locks (§11.2), the LFS cache lock and each partial
//! download's lock (§7), and the fronts' slot leases (§14). They are all
//! `flock`s, through std's [`File::lock`] family, and all go through
//! [`FileLock`].
//!
//! **Released explicitly.** An `flock` belongs to the open file, and a
//! child process spawned by any thread of this process shares that open
//! file from its fork until its exec closes it. `O_CLOEXEC`, which std sets
//! on every file it opens, closes it only at that exec, so it does not
//! shorten the window. Released only by closing, a lock would stay held,
//! for as long as such a child takes to reach its exec, after its holder
//! let go: the next taker, in this process or another, would find it busy
//! though no one holds it. A spawn is no rare event in the daemon (the
//! repack runs `git`) nor in a test binary. [`FileLock`] therefore unlocks
//! before it closes: the unlock releases the lock for every sharer at once.
//! A lock taken any other way would bring the failure back, so nothing
//! else in agentfs calls `flock`: `clippy.toml` disallows the calls this
//! module makes everywhere else.
#![allow(
    clippy::disallowed_methods,
    reason = "the one home of agentfs's file locks"
)]

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Which lock to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Any number of holders, none of them exclusive.
    Shared,
    /// One holder alone.
    Exclusive,
}

/// What [`FileLock::try_lock`] found.
#[derive(Debug)]
pub enum Attempt {
    /// The lock, now held.
    Taken(FileLock),
    /// Another holder has a conflicting lock; the file, unlocked, for a
    /// later try or to read the holder's description.
    Busy(File),
}

impl Attempt {
    /// The lock if it was taken.
    pub fn taken(self) -> Option<FileLock> {
        match self {
            Self::Taken(lock) => Some(lock),
            Self::Busy(_) => None,
        }
    }
}

/// A lock held on a file; dropping it unlocks it, whatever shares the
/// file, then closes the file.
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Opens the lock file at `path` for reading and writing (its holder
    /// may describe itself in it), creating it with mode 0600.
    pub fn open(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
    }

    /// Takes `mode` on `file`, waiting while another holder has a
    /// conflicting lock.
    pub fn wait(file: File, mode: Mode) -> io::Result<Self> {
        loop {
            let result = match mode {
                Mode::Shared => file.lock_shared(),
                Mode::Exclusive => file.lock(),
            };
            match result {
                Ok(()) => return Ok(Self { file }),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Takes `mode` on `file` without waiting.
    pub fn try_lock(file: File, mode: Mode) -> io::Result<Attempt> {
        loop {
            let result = match mode {
                Mode::Shared => file.try_lock_shared(),
                Mode::Exclusive => file.try_lock(),
            };
            match result {
                Ok(()) => return Ok(Attempt::Taken(Self { file })),
                Err(TryLockError::WouldBlock) => return Ok(Attempt::Busy(file)),
                Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(TryLockError::Error(e)) => return Err(e),
            }
        }
    }

    /// Opens ([`FileLock::open`]) the lock file at `path` and takes `mode`
    /// without waiting; `None` while another holder has a conflicting lock.
    pub fn try_take(path: &Path, mode: Mode) -> io::Result<Option<Self>> {
        Ok(Self::try_lock(Self::open(path)?, mode)?.taken())
    }

    /// The locked file.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// The locked file, to write or seek.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing alone would leave it held by a child not yet at its exec.
        let _ = self.file.unlock();
    }
}

/// Runs `take` while another thread keeps spawning child processes, each
/// of which shares this process's open files until its exec: at least 200
/// takes, and until at least 100 children have been spawned. `take` takes a
/// lock nobody else holds and lets it go, returning whether it was free.
/// Returns how many takes found it busy, and how many takes there were.
#[cfg(test)]
pub(crate) fn takes_while_children_spawn(mut take: impl FnMut() -> bool) -> (usize, usize) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const TAKES: usize = 200;
    const SPAWNS: usize = 100;
    let spawned = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let spawner = {
        let (spawned, done) = (spawned.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                std::process::Command::new("true").status().unwrap();
                spawned.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    let (mut busy, mut takes) = (0, 0);
    while takes < TAKES || spawned.load(Ordering::SeqCst) < SPAWNS {
        takes += 1;
        if !take() {
            busy += 1;
        }
    }
    done.store(true, Ordering::SeqCst);
    spawner.join().unwrap();
    (busy, takes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock let go is free at once, though another thread keeps spawning
    /// processes that share its file until they exec.
    #[test]
    fn a_lock_let_go_is_free_while_children_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.lock");
        let (busy, takes) = takes_while_children_spawn(|| {
            drop(FileLock::wait(FileLock::open(&path).unwrap(), Mode::Shared).unwrap());
            FileLock::try_take(&path, Mode::Exclusive)
                .unwrap()
                .is_some()
        });
        assert_eq!(busy, 0, "busy with no holder, of {takes} takes");
    }

    #[test]
    fn shared_holders_share_and_keep_an_exclusive_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.lock");
        let a = FileLock::try_take(&path, Mode::Shared).unwrap().unwrap();
        let b = FileLock::try_take(&path, Mode::Shared).unwrap().unwrap();
        assert!(
            FileLock::try_take(&path, Mode::Exclusive)
                .unwrap()
                .is_none()
        );
        drop(a);
        assert!(
            FileLock::try_take(&path, Mode::Exclusive)
                .unwrap()
                .is_none()
        );
        drop(b);
        let held = FileLock::try_take(&path, Mode::Exclusive).unwrap().unwrap();
        let Attempt::Busy(file) =
            FileLock::try_lock(File::open(&path).unwrap(), Mode::Shared).unwrap()
        else {
            panic!("a shared lock beside an exclusive one");
        };
        drop(held);
        assert!(
            FileLock::try_lock(file, Mode::Shared)
                .unwrap()
                .taken()
                .is_some()
        );
    }

    #[test]
    fn a_waiter_gets_the_lock_once_it_is_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.lock");
        let held = FileLock::try_take(&path, Mode::Exclusive).unwrap().unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || FileLock::wait(FileLock::open(&path).unwrap(), Mode::Shared))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!waiter.is_finished());
        drop(held);
        waiter.join().unwrap().unwrap();
    }
}
