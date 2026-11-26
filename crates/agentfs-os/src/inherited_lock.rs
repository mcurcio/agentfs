//! A file lock that its holder's child processes share:
//! [`InheritedLock`]. The tree engine's lease on a state area is one: the
//! process that serves a volume holds it with every git child it starts
//! (docs/library/core.md, "Locks and sockets"). A child gets the lock's open
//! file only through [`InheritedLock::spawn_with`], which registers it, so
//! [`InheritedLock::end_children`] ends every child that holds the lock:
//! once it has returned, the lock is free when its holder exits. Linux
//! system calls only (pidfds, `PR_SET_PDEATHSIG`); `clippy.toml` disallows
//! the calls this module makes everywhere else.

use std::io;
use std::os::fd::OwnedFd;
use std::process::{Child, Command};
use std::sync::{Mutex, RwLock};

use crate::file_lock::FileLock;

/// A lock held by this process and shared with each child started through
/// it. Dropping it ends every child that still holds the lock
/// ([`InheritedLock::end_children`]), then unlocks it ([`FileLock`]'s drop),
/// so the lock is never let go while a child still writes under it.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "the skeleton of page exit-3-reap: its bodies read every field"
)]
pub struct InheritedLock {
    lock: FileLock,
    /// `true` once [`InheritedLock::end_children`] has run: no child starts
    /// after it. A spawn holds the read side across its start, so the end,
    /// which takes the write side, never misses a child mid-spawn.
    ended: RwLock<bool>,
    /// A pidfd of each child started through the lock that may not have
    /// exited yet; those that have are dropped at the next spawn.
    live: Mutex<Vec<OwnedFd>>,
}

impl InheritedLock {
    /// Holds `lock`; no child shares it yet.
    #[expect(
        unused_variables,
        reason = "the skeleton of page exit-3-reap: no body yet"
    )]
    pub fn new(lock: FileLock) -> Self {
        todo!("page exit-3-reap")
    }

    /// Starts `command` through `spawn` (the caller's spawner, such as
    /// `agentfs_git::git_cli::counted` or `Command::spawn`) as a holder of
    /// the lock. Between its fork and its exec the child keeps the lock's
    /// open file across the exec (its close-on-exec flag cleared, in the
    /// child only), gets SIGKILL when the thread that started it ends
    /// (`PR_SET_PDEATHSIG`), and refuses to run (`ECHILD`) if that thread
    /// ended before. The child is registered by a pidfd until it has
    /// exited. The caller still owns and waits on the [`Child`].
    ///
    /// # Errors
    ///
    /// After [`InheritedLock::end_children`]: refused, `spawn` not called.
    /// `spawn`'s own error. A pidfd that cannot be opened: the child is
    /// killed and reaped first, so none runs unregistered.
    #[expect(
        unused_variables,
        reason = "the skeleton of page exit-3-reap: no body yet"
    )]
    pub fn spawn_with(
        &self,
        command: &mut Command,
        spawn: impl FnOnce(&mut Command) -> io::Result<Child>,
    ) -> io::Result<Child> {
        todo!("page exit-3-reap")
    }

    /// Ends every child that holds the lock, as a crash of this process
    /// ends them: SIGKILL through each one's pidfd, then a wait until each
    /// has exited, without reaping it (`waitid` with `WNOWAIT`: the thread
    /// that started it still waits on it). No child starts after it. When
    /// it returns, this process is the lock's one holder, so the lock is
    /// free once this process exits, or once it is dropped.
    ///
    /// # Errors
    ///
    /// A signal or a wait the kernel refused, other than for a child that
    /// has exited already.
    pub fn end_children(&self) -> io::Result<()> {
        todo!("page exit-3-reap")
    }
}
