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

#![allow(
    clippy::disallowed_methods,
    reason = "the one home of a descriptor shared with a child: its close-on-exec is cleared here"
)]

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::{Mutex, PoisonError, RwLock};

use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};

use crate::file_lock::FileLock;

/// The signal a holder gets when the thread that started it ends. Tests
/// stop it instead, so that a child outlives its holder and its share of
/// the lock can be seen.
#[cfg(not(test))]
const DEATH_SIGNAL: libc::c_int = libc::SIGKILL;
#[cfg(test)]
const DEATH_SIGNAL: libc::c_int = libc::SIGSTOP;

/// A lock held by this process and shared with each child started through
/// it. Dropping it ends every child that still holds the lock
/// ([`InheritedLock::end_children`]), then unlocks it ([`FileLock`]'s drop),
/// so the lock is never let go while a child still writes under it.
#[derive(Debug)]
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
    pub fn new(lock: FileLock) -> Self {
        Self {
            lock,
            ended: RwLock::new(false),
            live: Mutex::new(Vec::new()),
        }
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
    pub fn spawn_with(
        &self,
        command: &mut Command,
        spawn: impl FnOnce(&mut Command) -> io::Result<Child>,
    ) -> io::Result<Child> {
        let ended = self.ended.read().unwrap_or_else(PoisonError::into_inner);
        if *ended {
            return Err(io::Error::from_raw_os_error(libc::ENOTCONN));
        }
        let fd = self.lock.file().as_raw_fd();
        // SAFETY: getpid has no preconditions.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the closure runs in the child between its fork and its
        // exec, so it makes system calls only and allocates nothing (the
        // filter is a static, its program a stack value). `fd` stays open
        // across the start: `self` owns the lock's file.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, DEATH_SIGNAL as libc::c_ulong) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // The starting thread ended before the death signal was
                // armed: this child would outlive it, unregistered.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let program = libc::sock_fprog {
                    len: NO_FORK.len() as libc::c_ushort,
                    filter: NO_FORK.as_ptr().cast_mut(),
                };
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                    &raw const program,
                ) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                let lock = BorrowedFd::borrow_raw(fd);
                let mut flags = rustix::io::fcntl_getfd(lock)?;
                flags.remove(rustix::io::FdFlags::CLOEXEC);
                rustix::io::fcntl_setfd(lock, flags)?;
                Ok(())
            });
        }
        let mut child = spawn(command)?;
        // Race-free: nothing has waited on the child yet, so its pid is
        // still its own.
        let pidfd = match rustix::process::pidfd_open(Pid::from_child(&child), PidfdFlags::empty())
        {
            Ok(pidfd) => pidfd,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.into());
            }
        };
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.retain(|pidfd| !has_exited(pidfd.as_fd()));
        live.push(pidfd);
        drop(live);
        drop(ended);
        Ok(child)
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
        let mut ended = self.ended.write().unwrap_or_else(PoisonError::into_inner);
        *ended = true;
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let mut first_error = None;
        // Every child is signalled before any is waited for, so they die
        // together. One whose signal or wait failed stays registered.
        let mut signalled = Vec::with_capacity(live.len());
        for pidfd in std::mem::take(&mut *live) {
            match rustix::process::pidfd_send_signal(&pidfd, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => signalled.push(pidfd),
                Err(e) => {
                    first_error.get_or_insert(io::Error::from(e));
                    live.push(pidfd);
                }
            }
        }
        for pidfd in signalled {
            match wait_exit(pidfd.as_fd()) {
                // Reaped already by the thread that started it.
                Ok(()) | Err(rustix::io::Errno::CHILD) => {}
                Err(e) => {
                    first_error.get_or_insert(io::Error::from(e));
                    live.push(pidfd);
                }
            }
        }
        drop(live);
        drop(ended);
        first_error.map_or(Ok(()), Err)
    }

    /// Whether [`InheritedLock::end_children`] has run: a child's failure
    /// from then on is the end's, not its own.
    pub fn ended(&self) -> bool {
        *self.ended.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many children are registered: those started that had not
    /// exited at the last spawn, and the last one.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Drop for InheritedLock {
    fn drop(&mut self) {
        // The FileLock field unlocks after this, once no child holds it.
        let _ = self.end_children();
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
pub fn exited(pid: u32) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
    let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())?;
    Ok(wait_exit(pidfd.as_fd())?)
}

/// Waits until the process of `pidfd`, a child of this one, has exited,
/// without reaping it. `ECHILD`: it was reaped already, or is no child.
fn wait_exit(pidfd: BorrowedFd<'_>) -> rustix::io::Result<()> {
    loop {
        match rustix::process::waitid(
            WaitId::PidFd(pidfd),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Whether the child of `pidfd` has exited (or been reaped): its pidfd
/// can go.
fn has_exited(pidfd: BorrowedFd<'_>) -> bool {
    match rustix::process::waitid(
        WaitId::PidFd(pidfd),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
    ) {
        Ok(status) => status.is_some(),
        Err(rustix::io::Errno::CHILD) => true,
        Err(_) => false,
    }
}

/// `BPF_STMT`.
const fn statement(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

/// `BPF_JUMP`: `jt` and `jf` count the instructions skipped.
const fn jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    }
}

const LOAD: u32 = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
const IF_EQUAL: u32 = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
const IF_AT_LEAST: u32 = libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K;
const IF_ANY_SET: u32 = libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K;
const RETURN: u32 = libc::BPF_RET | libc::BPF_K;
const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const NOT_PERMITTED: u32 = libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA);
const NO_SUCH_CALL: u32 = libc::SECCOMP_RET_ERRNO | (libc::ENOSYS as u32 & libc::SECCOMP_RET_DATA);
/// Offsets in `struct seccomp_data`: the call's number, the arch, and the
/// low word of its first argument (both arches are little-endian).
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0_LOW: u32 = 16;
const CLONE_THREAD: u32 = libc::CLONE_THREAD as u32;

/// The no-fork filter, after `samples/seccomp/bpf-direct.c` in the kernel
/// tree: another arch's calls (and x32's) answer `ENOSYS`; `clone3` answers
/// `ENOSYS`, so glibc falls back to `clone` (Docker's default profile does
/// the same); `clone` is allowed only with `CLONE_THREAD` (Firecracker's
/// rule), `fork` and `vfork` never. Everything else is allowed.
#[cfg(target_arch = "x86_64")]
static NO_FORK: [libc::sock_filter; 16] = {
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    [
        /* 0 */ statement(LOAD, ARCH),
        /* 1 */ jump(IF_EQUAL, AUDIT_ARCH_X86_64, 1, 0),
        /* 2 */ statement(RETURN, NO_SUCH_CALL),
        /* 3 */ statement(LOAD, NR),
        /* 4 */ jump(IF_AT_LEAST, 0x4000_0000, 0, 1),
        /* 5 */ statement(RETURN, NO_SUCH_CALL),
        /* 6 */ jump(IF_EQUAL, libc::SYS_clone3 as u32, 8, 0),
        /* 7 */ jump(IF_EQUAL, libc::SYS_clone as u32, 3, 0),
        /* 8 */ jump(IF_EQUAL, libc::SYS_fork as u32, 5, 0),
        /* 9 */ jump(IF_EQUAL, libc::SYS_vfork as u32, 4, 0),
        /* 10 */ statement(RETURN, ALLOW),
        /* 11 */ statement(LOAD, ARG0_LOW),
        /* 12 */ jump(IF_ANY_SET, CLONE_THREAD, 0, 1),
        /* 13 */ statement(RETURN, ALLOW),
        /* 14 */ statement(RETURN, NOT_PERMITTED),
        /* 15 */ statement(RETURN, NO_SUCH_CALL),
    ]
};

/// The no-fork filter: see the x86_64 one. aarch64 has no `fork`,
/// `vfork` or second ABI.
#[cfg(target_arch = "aarch64")]
static NO_FORK: [libc::sock_filter; 12] = {
    const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
    [
        /* 0 */ statement(LOAD, ARCH),
        /* 1 */ jump(IF_EQUAL, AUDIT_ARCH_AARCH64, 1, 0),
        /* 2 */ statement(RETURN, NO_SUCH_CALL),
        /* 3 */ statement(LOAD, NR),
        /* 4 */ jump(IF_EQUAL, libc::SYS_clone3 as u32, 6, 0),
        /* 5 */ jump(IF_EQUAL, libc::SYS_clone as u32, 1, 0),
        /* 6 */ statement(RETURN, ALLOW),
        /* 7 */ statement(LOAD, ARG0_LOW),
        /* 8 */ jump(IF_ANY_SET, CLONE_THREAD, 0, 1),
        /* 9 */ statement(RETURN, ALLOW),
        /* 10 */ statement(RETURN, NOT_PERMITTED),
        /* 11 */ statement(RETURN, NO_SUCH_CALL),
    ]
};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("agentfs_os::inherited_lock's no-fork filter is written for x86_64 and aarch64");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_lock::Mode;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ChildStdin, Stdio};

    const MODE: &str = "AGENTFS_INHERITED_LOCK_HOLDER";
    const DIR: &str = "AGENTFS_INHERITED_LOCK_DIR";

    fn take(path: &Path) -> FileLock {
        FileLock::try_take(path, Mode::Exclusive).unwrap().unwrap()
    }

    /// A `cat` of the pipe on its stdin: it runs until that pipe's last
    /// writer closes it.
    fn cat(stdin: Stdio) -> Command {
        let mut command = Command::new("cat");
        command
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// This test binary re-run as a holder (`MODE` set): "end" and "hold"
    /// take the lock in `DIR`, start two `cat`s of the driver's pipe
    /// through it and print their pids; "end" then ends them and exits
    /// without a drop, "hold" waits to be killed. "fork" is itself a
    /// holder: it makes a thread, then tries to start a process.
    #[test]
    fn holder_process() {
        let Some(mode) = std::env::var_os(MODE) else {
            return;
        };
        if mode == "fork" {
            std::thread::spawn(|| ()).join().unwrap();
            let refused = Command::new("true").spawn().map(|_| ()).unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::EPERM), "{refused}");
            std::process::exit(0);
        }
        let dir = PathBuf::from(std::env::var_os(DIR).unwrap());
        let lock = InheritedLock::new(take(&dir.join("lock")));
        let children: Vec<Child> = (0..2)
            .map(|_| {
                lock.spawn_with(&mut cat(Stdio::inherit()), Command::spawn)
                    .unwrap()
            })
            .collect();
        let pids: Vec<String> = children.iter().map(|c| c.id().to_string()).collect();
        println!("pids {}", pids.join(" "));
        if mode == "end" {
            lock.end_children().unwrap();
            std::process::exit(0);
        }
        loop {
            std::thread::park();
        }
    }

    /// The driver's side of a holder: the holder, the pipe its children
    /// read, and a pidfd of each child still running.
    struct Holder {
        process: Child,
        _pipe: ChildStdin,
        children: Vec<OwnedFd>,
    }

    impl Holder {
        fn start(mode: &str, dir: &Path) -> Self {
            let mut process = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "inherited_lock::tests::holder_process",
                    "--nocapture",
                ])
                .env(MODE, mode)
                .env(DIR, dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let pipe = process.stdin.take().unwrap();
            let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
            let pids = loop {
                let line = lines.next().expect("the holder printed no pids").unwrap();
                if let Some(pids) = line.strip_prefix("pids ") {
                    break pids.to_owned();
                }
            };
            let children = pids
                .split(' ')
                .filter_map(|pid| {
                    let pid = Pid::from_raw(pid.parse().unwrap()).unwrap();
                    // An ended child may be gone already.
                    rustix::process::pidfd_open(pid, PidfdFlags::empty()).ok()
                })
                .collect();
            Self {
                process,
                _pipe: pipe,
                children,
            }
        }

        /// Kills the holder's children that still run.
        fn kill_children(&self) {
            for pidfd in &self.children {
                match rustix::process::pidfd_send_signal(pidfd, Signal::KILL) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }

    /// (a) The end leaves the lock free at the holder's exit, though the
    /// holder exits without a drop and its children would outlive it
    /// (their death signal stops them in tests).
    #[test]
    fn a_holder_that_ends_its_children_leaves_the_lock_free_at_its_exit() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = Holder::start("end", dir.path());
        let status = holder.process.wait().unwrap();
        let free = FileLock::try_take(&dir.path().join("lock"), Mode::Exclusive)
            .unwrap()
            .is_some();
        holder.kill_children();
        assert!(status.success(), "the holder failed: {status}");
        assert!(free, "the lock was held at the holder's exit");
    }

    /// (b) A killed holder's children keep the lock until they exit: the
    /// lock's open file reaches them across their exec.
    #[test]
    fn a_killed_holders_children_keep_the_lock_until_they_exit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let mut holder = Holder::start("hold", dir.path());
        assert_eq!(holder.children.len(), 2);
        holder.process.kill().unwrap();
        holder.process.wait().unwrap();
        let held = FileLock::try_take(&path, Mode::Exclusive)
            .unwrap()
            .is_none();
        holder.kill_children();
        assert!(held, "the lock was free while the children ran");
        FileLock::wait(FileLock::open(&path).unwrap(), Mode::Exclusive).unwrap();
    }

    /// (c) After the end no child starts, and the spawner is not called.
    #[test]
    fn a_spawn_after_the_end_is_refused_unstarted() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InheritedLock::new(take(&dir.path().join("lock")));
        assert!(!lock.ended());
        lock.end_children().unwrap();
        assert!(lock.ended());
        let mut called = false;
        let refused = lock
            .spawn_with(&mut Command::new("true"), |command| {
                called = true;
                command.spawn()
            })
            .unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::ENOTCONN));
        assert_eq!(refused.kind(), io::ErrorKind::NotConnected);
        assert!(!called, "the spawner ran after the end");
        lock.end_children().unwrap();
    }

    /// (d) The end leaves each child to its starter, which reaps it killed.
    #[test]
    fn the_starter_reaps_a_child_the_end_killed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InheritedLock::new(take(&dir.path().join("lock")));
        let mut child = lock
            .spawn_with(&mut cat(Stdio::piped()), Command::spawn)
            .unwrap();
        lock.end_children().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status}");
    }

    /// (e) A drop ends the children, then unlocks.
    #[test]
    fn a_drop_ends_the_children_then_unlocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let lock = InheritedLock::new(take(&path));
        let mut child = lock
            .spawn_with(&mut cat(Stdio::piped()), Command::spawn)
            .unwrap();
        drop(lock);
        let status = child.try_wait().unwrap();
        assert_eq!(
            status.and_then(|s| s.signal()),
            Some(libc::SIGKILL),
            "the child ran on after the drop"
        );
        assert!(
            FileLock::try_take(&path, Mode::Exclusive)
                .unwrap()
                .is_some()
        );
    }

    /// (f) Children that have exited, reaped or not, leave the registry at
    /// the next spawn.
    #[test]
    fn exited_children_leave_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InheritedLock::new(take(&dir.path().join("lock")));
        let mut unreaped = lock
            .spawn_with(&mut Command::new("true"), Command::spawn)
            .unwrap();
        exited(unreaped.id()).unwrap();
        let mut reaped = lock
            .spawn_with(&mut Command::new("true"), Command::spawn)
            .unwrap();
        reaped.wait().unwrap();
        let mut running = lock
            .spawn_with(&mut cat(Stdio::piped()), Command::spawn)
            .unwrap();
        assert_eq!(lock.live(), 1);
        lock.end_children().unwrap();
        assert_eq!(lock.live(), 0);
        assert!(unreaped.wait().unwrap().success());
        assert_eq!(running.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// (g) A holder makes threads but starts no process.
    #[test]
    fn a_holder_makes_threads_but_cannot_fork() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InheritedLock::new(take(&dir.path().join("lock")));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "inherited_lock::tests::holder_process",
                "--nocapture",
            ])
            .env(MODE, "fork")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let output = lock
            .spawn_with(&mut command, Command::spawn)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// (h) `exited` returns once the child has exited, and leaves it to be
    /// reaped; a reaped child is an error.
    #[test]
    fn exited_waits_without_reaping() {
        let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let pid = child.id();
        exited(pid).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        assert!(exited(pid).is_err());
    }
}
