//! A file lock that its holder's child processes share:
//! [`InheritedLock`]. The tree engine's lease on a state area is one: the
//! process that serves a volume holds it with every git child it starts
//! (docs/library/core.md, "Locks and sockets").
//!
//! **What it guarantees, and how.** The lock's file is private to the type:
//! nothing reads it out, so a child gets the lock's open file only through
//! [`InheritedLock::spawn_with`], which registers the child by a pidfd. A
//! holder cannot start a process of its own: the kernel refuses it every
//! fork (a seccomp filter installed before its exec lets `clone` make
//! threads only), so the lock never reaches a grandchild. Hence
//! [`InheritedLock::end_children`], which kills and waits for every
//! registered child, leaves no process but its holder on the lock. A child
//! started any other way (`Command::spawn` elsewhere in the process) still
//! shares the open file from its fork to its exec, as every file of the
//! process (docs/library/core.md, "Locks and sockets"): the lock is free at
//! its holder's exit once no such start is in flight.
//!
//! Linux system calls only (pidfds, `waitid`, `PR_SET_PDEATHSIG`,
//! `PR_SET_NO_NEW_PRIVS`, seccomp), for x86_64 and aarch64; `clippy.toml`
//! disallows the calls that share a descriptor everywhere else.

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
    reason = "the skeleton of page inherited-lock: its bodies read every field"
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
        reason = "the skeleton of page inherited-lock: no body yet"
    )]
    pub fn new(lock: FileLock) -> Self {
        todo!("page inherited-lock")
    }

    /// Starts `command` through `spawn` (the caller's spawner, such as
    /// `agentfs_git::git_cli::counted` or `Command::spawn`) as a holder of
    /// the lock. Between its fork and its exec the child: gets SIGKILL when
    /// the thread that started it ends (`PR_SET_PDEATHSIG`) and refuses to
    /// run (`ECHILD`) if that thread ended before; takes the seccomp filter
    /// under which every fork it tries fails with `EPERM` (`clone` without
    /// `CLONE_THREAD`, `fork`, `vfork`; `clone3` answers `ENOSYS`, so
    /// glibc's threads fall back to `clone`); keeps the lock's open file
    /// across its exec (close-on-exec cleared, in the child only). The child
    /// is registered by a pidfd until it has exited. The caller still owns
    /// and waits on the [`Child`].
    ///
    /// # Errors
    ///
    /// After [`InheritedLock::end_children`]: refused, `spawn` not called,
    /// with `io::Error::from_raw_os_error(libc::ENOTCONN)` (kind
    /// `NotConnected`): the holder has ended, as after a crash. `spawn`'s
    /// own error (one from the child's setup, such as `ECHILD`, among
    /// them). A pidfd that cannot be opened: the child is killed and reaped
    /// first, so none runs unregistered.
    #[expect(
        unused_variables,
        reason = "the skeleton of page inherited-lock: no body yet"
    )]
    pub fn spawn_with(
        &self,
        command: &mut Command,
        spawn: impl FnOnce(&mut Command) -> io::Result<Child>,
    ) -> io::Result<Child> {
        todo!("page inherited-lock")
    }

    /// Ends every child that holds the lock, as a crash of this process
    /// ends them: SIGKILL through each one's pidfd, then a wait until each
    /// has exited, without reaping it (`waitid` with `WNOWAIT`: the thread
    /// that started it still waits on it). No child starts after it. When
    /// it returns, no process this lock started holds it, and none can: a
    /// holder starts no process. Idempotent.
    ///
    /// # Errors
    ///
    /// A signal or a wait the kernel refused, other than for a child that
    /// has exited already. The children not yet waited for may then still
    /// hold the lock; none starts after it all the same.
    pub fn end_children(&self) -> io::Result<()> {
        todo!("page inherited-lock")
    }

    /// Whether [`InheritedLock::end_children`] has run: a child's failure
    /// from then on is the end's, not its own.
    pub fn ended(&self) -> bool {
        todo!("page inherited-lock")
    }
}

/// Returns once the child `pid` has exited, leaving it unreaped (`waitid`
/// on a pidfd with `WNOWAIT`): its process id stays its own until the
/// caller reaps it ([`Child::wait`]), so a signal sent through its
/// [`Child`] meanwhile cannot reach another process. The one wait without
/// a reap: [`InheritedLock::end_children`] makes it on each child, and the
/// tree engine's repack on its git before it reaps it. The caller
/// guarantees `pid` is a child of this process that nothing reaps before
/// this returns.
///
/// # Errors
///
/// `pid` reaped already or not a child (`ECHILD` or `ESRCH`), or a pidfd
/// or a wait the kernel refused; `EINTR` is retried.
#[expect(
    unused_variables,
    reason = "the skeleton of page inherited-lock: no body yet"
)]
pub fn exited(pid: u32) -> io::Result<()> {
    todo!("page inherited-lock")
}
