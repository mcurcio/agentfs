//! Creating as the caller (design §6).
//!
//! A daemon running as root creates each new object with the caller's
//! filesystem identity, so the object is the caller's from the instant it
//! exists: another front (or process) opening the same name as the caller
//! in between never finds a root-owned file it may not open. For the
//! duration of one creating system call, [`AsCaller`] switches the
//! *current thread's* filesystem uid and gid (`setfsuid(2)`,
//! `setfsgid(2)`) and supplementary groups (the raw `setgroups(2)`
//! system call, which, unlike the C library's wrapper, changes only the
//! calling thread) to the caller's, and switches them back when dropped.
//! With a filesystem uid other than 0 the kernel also drops the thread's
//! filesystem capabilities (`CAP_DAC_OVERRIDE`, `CAP_FOWNER`,
//! `CAP_MKNOD`, ...) for that time, so the creation is checked exactly as
//! the caller's would be; they come back with the filesystem uid.
//!
//! Each thread that creates as a caller first unshares its filesystem
//! context (`unshare(CLONE_FS)`), and sets its umask to 0 while it acts as
//! the caller: the kernel has already applied the caller's umask to the
//! mode a request carries, so the object is created with exactly that
//! mode, never briefly narrower. Other work on the thread (records into
//! the state area) keeps the daemon's umask.
//!
//! Failing to switch back is not survivable (the thread would go on acting
//! as the caller): the process aborts.

use std::cell::Cell;
use std::fs;
use std::io;

use agentfs_core::record::Caller;
use rustix::fs::Mode;
use rustix::process::{Gid, Uid};

thread_local! {
    /// Whether this thread has its own filesystem context (umask).
    static OWN_FS: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Whose supplementary groups an identity has while it acts (sandbox
/// overlay slice 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supplementary {
    /// Those of the request's process ([`caller_groups`] of its pid): what
    /// the kernel would use.
    OfProcess,
    /// None: an identity the daemon substitutes for the request's (the
    /// sandbox overlay's view owner), whatever groups the requesting
    /// process has.
    None,
}

/// The current thread acting as a caller; switched back on drop.
#[must_use = "the thread acts as the caller only while this lives"]
pub struct AsCaller {
    fsuid: u32,
    fsgid: u32,
    groups: Vec<Gid>,
    /// The umask to put back, if this thread has its own.
    umask: Option<Mode>,
}

impl AsCaller {
    /// Switches the current thread's filesystem identity to `caller`'s,
    /// with the supplementary groups `groups` says ([`groups_of`]). Fails,
    /// with nothing changed, if the thread cannot.
    pub fn enter(caller: Caller, groups: Supplementary) -> io::Result<Self> {
        let theirs: Vec<Gid> = groups_of(caller, groups)
            .into_iter()
            .map(Gid::from_raw)
            .collect();
        let groups = rustix::process::getgroups()?;
        // Needs CAP_SETGID: without it, nothing has changed yet.
        rustix::thread::set_thread_groups(&theirs)?;
        // From here on, dropping `saved` switches everything back.
        let saved = Self {
            fsuid: current_fsuid(),
            fsgid: current_fsgid(),
            groups,
            umask: own_fs_context().then(|| rustix::process::umask(Mode::empty())),
        };
        set_fsgid(caller.gid);
        if current_fsgid() != caller.gid {
            drop(saved);
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        set_fsuid(caller.uid);
        if current_fsuid() != caller.uid {
            drop(saved);
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(saved)
    }
}

impl Drop for AsCaller {
    fn drop(&mut self) {
        set_fsuid(self.fsuid);
        set_fsgid(self.fsgid);
        let groups = rustix::thread::set_thread_groups(&self.groups);
        if let Some(umask) = self.umask {
            rustix::process::umask(umask);
        }
        if current_fsuid() != self.fsuid || current_fsgid() != self.fsgid || groups.is_err() {
            eprintln!(
                "agentfs: fatal: could not switch a thread back from a caller's identity \
                 (fsuid {}, fsgid {}): aborting",
                current_fsuid(),
                current_fsgid()
            );
            std::process::abort();
        }
    }
}

/// Runs `create` as `caller` (with the supplementary groups `groups`
/// says) when `enabled`, else (or if the thread cannot
/// switch, or the caller is refused with `EACCES` or `EPERM`) as the
/// daemon. Returns the result and whether it was made as the caller.
///
/// The fallback keeps what worked before: the kernel has already checked
/// the caller may create the name (`default_permissions`), so a refusal
/// as the caller means something only the daemon may do (a device node
/// needs `CAP_MKNOD`, which the caller lacks), or a group the caller's
/// process gained or lost meanwhile.
pub fn create_as<T>(
    enabled: bool,
    caller: Caller,
    groups: Supplementary,
    mut create: impl FnMut() -> rustix::io::Result<T>,
) -> (rustix::io::Result<T>, bool) {
    if enabled && let Ok(guard) = AsCaller::enter(caller, groups) {
        let made = create();
        drop(guard);
        match made {
            Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) => {}
            other => return (other, true),
        }
    }
    (create(), false)
}

/// Gives this thread its own filesystem context, once; whether it has
/// one. A thread that cannot keeps the shared umask while it acts as the
/// caller (the mode is then set right just after the creation, as when
/// creating as the daemon).
fn own_fs_context() -> bool {
    OWN_FS.with(|own| {
        if let Some(ok) = own.get() {
            return ok;
        }
        // SAFETY: CLONE_FS only unshares the cwd, root and umask, not the
        // descriptor table.
        let ok =
            unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::FS) }.is_ok();
        own.set(Some(ok));
        ok
    })
}

/// The supplementary groups `caller` acts with: its process's
/// ([`caller_groups`]) for [`Supplementary::OfProcess`], none for
/// [`Supplementary::None`].
pub fn groups_of(caller: Caller, groups: Supplementary) -> Vec<u32> {
    match groups {
        Supplementary::OfProcess => caller_groups(caller.pid),
        Supplementary::None => Vec::new(),
    }
}

/// The supplementary groups of process `pid` (`Groups:` in its status);
/// none if they cannot be read.
///
/// FUSE reports pid 0 for a caller outside the daemon's pid namespace (a
/// pod's process, seen from a CSI node plugin without `hostPID`): its
/// groups are unknown, said once on stderr. Creating as such a caller
/// then succeeds only through its primary group, else falls back to the
/// daemon (design §6, §14.1).
pub fn caller_groups(pid: u32) -> Vec<u32> {
    if pid == 0 {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "agentfs: a caller's process id is 0 (its pid namespace is not visible to this \
                 daemon), so its supplementary groups are unknown: a new object it may create \
                 only through one (a pod's fsGroup) is created as the daemon and then given to \
                 it, and another front's racing open of that name may fail with EACCES; run \
                 the CSI node plugin with hostPID: true (design §6)"
            );
        });
        return Vec::new();
    }
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| parse_groups(&s))
        .unwrap_or_default()
}

fn parse_groups(status: &str) -> Option<Vec<u32>> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Groups:"))
        .map(|g| {
            g.split_whitespace()
                .filter_map(|n| n.parse().ok())
                .collect()
        })
}

fn set_fsuid(uid: u32) {
    // SAFETY: setfsuid(2) has no memory effects; it changes only the
    // calling thread's credentials.
    unsafe { libc::setfsuid(uid) };
}

fn set_fsgid(gid: u32) {
    // SAFETY: as `set_fsuid`.
    unsafe { libc::setfsgid(gid) };
}

/// `setfsuid(-1)` changes nothing and returns the current value.
fn current_fsuid() -> u32 {
    // SAFETY: as `set_fsuid`.
    unsafe { libc::setfsuid(u32::MAX) as u32 }
}

fn current_fsgid() -> u32 {
    // SAFETY: as `set_fsuid`.
    unsafe { libc::setfsgid(u32::MAX) as u32 }
}

/// The current thread's filesystem uid (for tests).
pub fn fsuid() -> Uid {
    Uid::from_raw(current_fsuid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn groups_come_from_the_status_file() {
        let status =
            "Name:\tsh\nUid:\t10000\t10000\t10000\t10000\nGroups:\t4242 10000 \nNgid:\t0\n";
        assert_eq!(parse_groups(status), Some(vec![4242, 10000]));
        assert_eq!(parse_groups("Groups:\t\n"), Some(vec![]));
        assert_eq!(parse_groups("Name:\tsh\n"), None);
        let mine = caller_groups(std::process::id());
        let expected: Vec<u32> = rustix::process::getgroups()
            .unwrap()
            .into_iter()
            .map(Gid::as_raw)
            .collect();
        let mut mine_sorted = mine.clone();
        mine_sorted.sort_unstable();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        assert_eq!(mine_sorted, expected_sorted);
    }

    #[test]
    fn a_substituted_identity_has_no_supplementary_groups() {
        let me = Caller {
            uid: 1,
            gid: 1,
            pid: std::process::id(),
        };
        assert!(groups_of(me, Supplementary::None).is_empty());
        assert_eq!(
            groups_of(me, Supplementary::OfProcess),
            caller_groups(me.pid)
        );
    }

    /// Without the privilege, nothing changes and the daemon creates.
    #[test]
    fn without_the_privilege_the_daemon_creates() {
        let me = rustix::process::geteuid().as_raw();
        if me == 0 {
            return; // Covered by the root test below.
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let other = Caller {
            uid: me + 1,
            gid: me + 1,
            pid: std::process::id(),
        };
        let (made, as_caller) = std::thread::spawn(move || {
            create_as(true, other, Supplementary::OfProcess, || fs_create(&path))
        })
        .join()
        .unwrap();
        made.unwrap();
        assert!(!as_caller);
        assert_eq!(fs::metadata(dir.path().join("f")).unwrap().uid(), me);
        let (made, as_caller) = create_as(false, other, Supplementary::OfProcess, || {
            fs_create(&dir.path().join("g"))
        });
        made.unwrap();
        assert!(!as_caller);
    }

    /// As root: the object is the caller's, made with exactly the mode
    /// asked, and the thread is root again afterwards.
    #[test]
    fn as_root_the_caller_owns_what_it_creates() {
        if rustix::process::geteuid().as_raw() != 0 {
            eprintln!("skipped: needs root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        let caller = Caller {
            uid: 10000,
            gid: 10000,
            pid: std::process::id(),
        };
        let r = root.clone();
        std::thread::spawn(move || {
            let path = r.join("f");
            let (made, as_caller) = create_as(true, caller, Supplementary::OfProcess, || {
                rustix::fs::open(
                    &path,
                    rustix::fs::OFlags::CREATE
                        | rustix::fs::OFlags::EXCL
                        | rustix::fs::OFlags::WRONLY,
                    rustix::fs::Mode::from_raw_mode(0o664),
                )
            });
            made.unwrap();
            assert!(as_caller);
            let m = fs::metadata(&path).unwrap();
            assert_eq!((m.uid(), m.gid()), (10000, 10000));
            assert_eq!(m.mode() & 0o7777, 0o664, "umask 0 on this thread");
            assert_eq!(fsuid().as_raw(), 0, "root again");
            // A root-only directory is refused as the caller: the
            // daemon creates there instead.
            let private = r.join("private");
            fs::create_dir(&private).unwrap();
            fs::set_permissions(
                &private,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .unwrap();
            let (made, as_caller) = create_as(true, caller, Supplementary::OfProcess, || {
                fs_create(&private.join("g"))
            });
            made.unwrap();
            assert!(!as_caller, "fell back after EACCES");
            assert_eq!(fs::metadata(private.join("g")).unwrap().uid(), 0);
        })
        .join()
        .unwrap();
    }

    fn fs_create(path: &std::path::Path) -> rustix::io::Result<rustix::fd::OwnedFd> {
        rustix::fs::open(
            path,
            rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL | rustix::fs::OFlags::WRONLY,
            rustix::fs::Mode::from_raw_mode(0o644),
        )
    }
}
