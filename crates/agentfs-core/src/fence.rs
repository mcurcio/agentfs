//! Fencing a history service that may have lost its state lock (design
//! §2.3, §14.2).
//!
//! On a shared filesystem the state lock is a lease the NFS server grants
//! the node: a node cut off from the server loses it after the server's
//! lease time, and a standby elsewhere then takes it. The cut-off service
//! must stop writing the journal and the branch before that happens, or
//! two writers would interleave.
//!
//! The holder therefore renews the lock in a heartbeat
//! ([`crate::lock::StateLock::start_heartbeat`]): every `fence / 5` it
//! rewrites its holder line in the lock file and syncs it, a write the NFS
//! server refuses once the lock is lost. Each success renews the process's
//! [`Fence`]; the journal's write and every commit check it
//! ([`check`]) and refuse to write once the last renewal is older than
//! the fence (default 10 s). The server's lease must be at least twice the
//! fence, so a fenced service stops before its lock can be granted
//! elsewhere. A heartbeat failure is a lost lock: the service raises
//! `StateLockLost` and exits 75 at once, without a final commit.
//!
//! Without a heartbeat (a local filesystem, `--lock-heartbeat off`) no
//! fence is installed and [`check`] always passes.

use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// The default fence: how old the last renewal may be.
pub const DEFAULT_FENCE: Duration = Duration::from_secs(10);

/// The process's fence, once a heartbeat installs it.
static FENCE: OnceLock<Fence> = OnceLock::new();

/// When the lock was last renewed, and how old that may be.
#[derive(Debug)]
pub struct Fence {
    max_age: Duration,
    renewed: Mutex<Option<Instant>>,
}

impl Fence {
    /// A fence renewed now.
    pub fn new(max_age: Duration) -> Self {
        Self {
            max_age,
            renewed: Mutex::new(Some(Instant::now())),
        }
    }

    /// The lock was renewed at `at`.
    pub fn renew(&self, at: Instant) {
        *self.lock() = Some(at);
    }

    /// The lock is lost: nothing may be written any more.
    pub fn revoke(&self) {
        *self.lock() = None;
    }

    /// `Ok` if the last renewal is no older than the fence at `now`.
    pub fn check_at(&self, now: Instant) -> io::Result<()> {
        match *self.lock() {
            Some(at) if now.saturating_duration_since(at) <= self.max_age => Ok(()),
            Some(at) => Err(io::Error::other(format!(
                "the state lock was last renewed {:?} ago, over the {:?} fence: refusing to \
                 write in case another node holds it now (design §14.2)",
                now.saturating_duration_since(at),
                self.max_age
            ))),
            None => Err(io::Error::other(
                "the state lock is lost: refusing to write (design §14.2)",
            )),
        }
    }

    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.renewed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Installs the process's fence; the first call wins. Returns it.
pub fn install(max_age: Duration) -> &'static Fence {
    FENCE.get_or_init(|| Fence::new(max_age))
}

/// The process's fence, if a heartbeat installed one.
pub fn installed() -> Option<&'static Fence> {
    FENCE.get()
}

/// `Ok` unless this process holds a fenced lock that is not fresh: called
/// before every journal write and commit.
pub fn check() -> io::Result<()> {
    match FENCE.get() {
        Some(fence) => fence.check_at(Instant::now()),
        None => Ok(()),
    }
}

/// Whether to run the lock heartbeat (`--lock-heartbeat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHeartbeat {
    /// On when the state area is on a network filesystem.
    Auto,
    On,
    Off,
}

impl std::str::FromStr for LockHeartbeat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(format!("{s:?}: use auto, on or off")),
        }
    }
}

impl LockHeartbeat {
    /// Whether it runs for a state area at `path`.
    pub fn enabled(self, path: &Path) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => is_network_fs(path),
        }
    }
}

/// Whether `path` is on a network or cluster filesystem, by `statfs`
/// magic: NFS, SMB/CIFS, Ceph, 9p, AFS, Lustre, GFS2, OCFS2, or FUSE
/// (SeaweedFS, JuiceFS and other shared filesystems mount through it).
pub fn is_network_fs(path: &Path) -> bool {
    const NETWORK: &[i64] = &[
        0x6969,                 // NFS
        0x517B,                 // SMB
        0xFE53_4D42_u32 as i64, // SMB2
        0xFF53_4D42_u32 as i64, // CIFS
        0x00C3_6400,            // Ceph
        0x0102_1997,            // 9p
        0x5346_414F,            // AFS
        0x0BD0_0BD0,            // Lustre
        0x0116_1970,            // GFS2
        0x7461_636F,            // OCFS2
        0x6573_5546,            // FUSE
    ];
    #[allow(clippy::unnecessary_cast)] // f_type is not i64 on every target.
    rustix::fs::statfs(path).is_ok_and(|s| NETWORK.contains(&(s.f_type as i64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_passes_while_fresh_and_refuses_after() {
        let fence = Fence::new(Duration::from_secs(10));
        let t0 = Instant::now();
        fence.renew(t0);
        fence.check_at(t0).unwrap();
        fence.check_at(t0 + Duration::from_secs(10)).unwrap();
        let err = fence
            .check_at(t0 + Duration::from_secs(11))
            .unwrap_err()
            .to_string();
        assert!(err.contains("over the 10s fence"), "{err}");
        // A renewal makes it fresh again.
        fence.renew(t0 + Duration::from_secs(11));
        fence.check_at(t0 + Duration::from_secs(12)).unwrap();
        // A clock that reads earlier than the renewal is still fresh.
        fence.check_at(t0).unwrap();
        fence.revoke();
        let err = fence.check_at(t0).unwrap_err().to_string();
        assert!(err.contains("lost"), "{err}");
    }

    #[test]
    fn modes_parse_and_local_filesystems_are_not_network() {
        assert_eq!("auto".parse(), Ok(LockHeartbeat::Auto));
        assert_eq!("on".parse(), Ok(LockHeartbeat::On));
        assert_eq!("off".parse(), Ok(LockHeartbeat::Off));
        assert!("yes".parse::<LockHeartbeat>().is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(LockHeartbeat::On.enabled(dir.path()));
        assert!(!LockHeartbeat::Off.enabled(dir.path()));
        // /proc is no network filesystem.
        assert!(!is_network_fs(Path::new("/proc")));
    }
}
