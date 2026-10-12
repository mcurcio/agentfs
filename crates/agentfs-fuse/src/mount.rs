//! Mounting and unmounting (design §1.1, §17.1).
//!
//! [`preflight`] runs before anything is mounted and refuses layouts that
//! would break invisibility or recurse into the mount:
//!
//! - the state area, the backing tree and the mount point must be three
//!   separate directories, none inside another. The state area under the
//!   backing tree would be visible through the mount; anything under the
//!   mount point would be reached through the mount by the daemon itself;
//! - no remote's `agentfsCredentials` file may be under the backing tree
//!   (it would be readable through the mount) or the mount point. This is a
//!   hard error, not a `RemoteMisconfigured` alarm: carrying on would hand
//!   the secret to the workload;
//! - a mount point left by a crashed daemon (`ENOTCONN`) is reported with
//!   the command that clears it.
//!
//! [`Mount::start`] mounts with fuser (multi-threaded, one cloned
//! `/dev/fuse` descriptor per thread) and these options: `fsname` agentfs
//! and the volume, subtype `agentfs`, `default_permissions` (the kernel
//! checks modes and ownership), `allow_other` unless turned off, `nodev`,
//! `nosuid`, `rw`. There is no `auto_unmount`: a crashed daemon leaves the
//! mount point failing with `ENOTCONN` rather than exposing the empty
//! directory beneath it to writes nothing would record. Mounting is done
//! with `mount(2)` as root and `fusermount3` otherwise.
//!
//! [`Mount::shutdown`] unmounts (lazily if the mount is busy), waits a grace
//! period for the session to end, stops the sweep and runs a final one.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentfs_core::config::Settings;
use agentfs_core::service::Stop;
use agentfs_core::volume::{Recorder, StateDir};
use fuser::{MountOption, SessionACL};

use crate::backing::Backing;
use crate::fuse::{FuseFs, Passthrough, PassthroughStatus};
use crate::ops::{Ops, OpsConfig};
use crate::sweep::Sweeper;

/// How to mount.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// The volume's name, shown as the mount's source (`agentfs:<volume>`).
    pub volume: String,
    pub backing: PathBuf,
    pub mountpoint: PathBuf,
    pub passthrough: Passthrough,
    /// Mount with `allow_other`, so every user can reach the mount (needs
    /// `user_allow_other` in `/etc/fuse.conf` when not root).
    pub allow_other: bool,
    /// FUSE threads; 0 picks the number of CPUs, between 2 and 16.
    pub threads: usize,
    pub sweep_interval: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("{what} {} does not exist or cannot be reached", path.display())]
    Missing {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("{what} {} is not a directory", path.display())]
    NotADirectory { what: &'static str, path: PathBuf },
    #[error(
        "{p} is a stale FUSE mount (transport endpoint is not connected), probably left by a \
         daemon that died; unmount it first: fusermount3 -u -z {p} (or, as root, umount -l {p})",
        p = .0.display()
    )]
    StaleMount(PathBuf),
    #[error("{a} and {b} are the same directory ({})", path.display())]
    Same {
        a: &'static str,
        b: &'static str,
        path: PathBuf,
    },
    #[error(
        "{inner} {} is inside {outer} {}: {why}",
        inner_path.display(),
        outer_path.display()
    )]
    Nested {
        inner: &'static str,
        inner_path: PathBuf,
        outer: &'static str,
        outer_path: PathBuf,
        why: &'static str,
    },
    #[error(
        "remote {remote:?}: its credentials file {} is inside {outer} {}, where the workload \
         could read it; keep credentials outside the backing tree and the mount point",
        path.display(),
        outer_path.display()
    )]
    CredentialsExposed {
        remote: String,
        path: PathBuf,
        outer: &'static str,
        outer_path: PathBuf,
    },
}

const STATE: &str = "the state area";
const BACKING: &str = "the backing tree";
const MOUNTPOINT: &str = "the mount point";

fn canonical_dir(what: &'static str, path: &Path) -> Result<PathBuf, PreflightError> {
    let canonical = path
        .canonicalize()
        .map_err(|source| PreflightError::Missing {
            what,
            path: path.to_path_buf(),
            source,
        })?;
    if !canonical.is_dir() {
        return Err(PreflightError::NotADirectory {
            what,
            path: path.to_path_buf(),
        });
    }
    Ok(canonical)
}

/// The canonical form of a path that may not exist yet: its existing
/// ancestor resolved, the rest appended.
fn canonical_any(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical_any(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

/// What a nesting of `inner` in `outer` would break.
fn why(inner: &'static str, outer: &'static str) -> &'static str {
    match (inner, outer) {
        (STATE, BACKING) => "history, journal and credentials would be visible through the mount",
        (_, MOUNTPOINT) => "the daemon would reach it through its own mount",
        (MOUNTPOINT, _) => "the mount would recurse into itself or hide its own files",
        _ => "the state area and the backing tree must be separate",
    }
}

/// Checks that mounting `backing` at `mountpoint` with state area `state`
/// keeps everything of the state area out of the mount (design §1.1,
/// §18 test 5). Returns the canonical backing tree and mount point.
pub fn preflight(
    state: &StateDir,
    settings: &Settings,
    backing: &Path,
    mountpoint: &Path,
) -> Result<(PathBuf, PathBuf), PreflightError> {
    if let Err(e) = rustix::fs::stat(mountpoint)
        && e == rustix::io::Errno::NOTCONN
    {
        return Err(PreflightError::StaleMount(mountpoint.to_path_buf()));
    }
    let state = canonical_dir(STATE, state.root())?;
    let backing = canonical_dir(BACKING, backing)?;
    let mountpoint = canonical_dir(MOUNTPOINT, mountpoint)?;
    let dirs = [
        (STATE, &state),
        (BACKING, &backing),
        (MOUNTPOINT, &mountpoint),
    ];
    for (i, (a, pa)) in dirs.iter().enumerate() {
        for (b, pb) in &dirs[i + 1..] {
            if pa == pb {
                return Err(PreflightError::Same {
                    a,
                    b,
                    path: (*pa).clone(),
                });
            }
            for ((inner, pi), (outer, po)) in [((a, pa), (b, pb)), ((b, pb), (a, pa))] {
                if pi.starts_with(po) {
                    return Err(PreflightError::Nested {
                        inner,
                        inner_path: (*pi).clone(),
                        outer,
                        outer_path: (*po).clone(),
                        why: why(inner, outer),
                    });
                }
            }
        }
    }
    for remote in &settings.remotes {
        let Some(creds) = &remote.credentials else {
            continue;
        };
        let creds = canonical_any(creds);
        for (outer, outer_path) in [(BACKING, &backing), (MOUNTPOINT, &mountpoint)] {
            if creds.starts_with(outer_path) {
                return Err(PreflightError::CredentialsExposed {
                    remote: remote.name.clone(),
                    path: creds,
                    outer,
                    outer_path: outer_path.clone(),
                });
            }
        }
    }
    Ok((backing, mountpoint))
}

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("opening the backing tree {}", path.display())]
    Backing { path: PathBuf, source: io::Error },
    #[error("mounting at {}", path.display())]
    Mount { path: PathBuf, source: io::Error },
    #[error(
        "/dev/fuse is missing: load the fuse module (modprobe fuse), or give the container \
         the device"
    )]
    NoFuse,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What [`Mount::shutdown`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UnmountReport {
    /// The mount was busy, so it was detached lazily.
    pub lazy: bool,
    /// Unmounting failed; the message.
    pub unmount_error: Option<String>,
    /// The session was still serving open files when the grace period
    /// ended; it is left to process exit.
    pub session_abandoned: bool,
    /// Open, changed files the final sweep recorded.
    pub swept: usize,
    /// Failed records the final sweep repaired, and those still failing.
    pub repaired: usize,
    pub unrepaired: usize,
}

/// FUSE threads for `requested` (0 = by CPU count).
pub fn thread_count(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    std::thread::available_parallelism()
        .map_or(2, std::num::NonZeroUsize::get)
        .clamp(2, 16)
}

/// fuser's configuration for `config`.
pub fn fuser_config(config: &MountConfig) -> fuser::Config {
    let mut c = fuser::Config::default();
    c.mount_options = vec![
        MountOption::FSName(format!("agentfs:{}", config.volume)),
        MountOption::Subtype("agentfs".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::RW,
    ];
    c.acl = if config.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    c.n_threads = Some(thread_count(config.threads));
    c.clone_fd = true;
    c
}

/// Raises the open-file soft limit to the hard limit: a directory rename
/// holds one descriptor per file of a record (design §3.1).
fn raise_nofile() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    if limit.current != limit.maximum {
        let _ = setrlimit(
            Resource::Nofile,
            rustix::process::Rlimit {
                current: limit.maximum,
                maximum: limit.maximum,
            },
        );
    }
}

/// A mounted volume.
pub struct Mount {
    ops: Arc<Ops>,
    mountpoint: PathBuf,
    status: Arc<Mutex<PassthroughStatus>>,
    ended: Arc<Stop>,
    sweeper: Option<Sweeper>,
    threads: usize,
}

impl Mount {
    /// Mounts `config.backing` at `config.mountpoint`, recording through
    /// `recorder`, and starts the sweep. Returns once the kernel has
    /// finished INIT. Run [`preflight`] first.
    pub fn start(recorder: Recorder, config: MountConfig) -> Result<Self, MountError> {
        if !Path::new("/dev/fuse").exists() {
            return Err(MountError::NoFuse);
        }
        raise_nofile();
        let backing = Backing::open(&config.backing).map_err(|source| MountError::Backing {
            path: config.backing.clone(),
            source,
        })?;
        let ops = Arc::new(Ops::new(backing, recorder, OpsConfig::default())?);
        let status = Arc::new(Mutex::new(PassthroughStatus::Off("not negotiated".into())));
        let fs = FuseFs::new(ops.clone(), config.passthrough, status.clone());
        let fuser_config = fuser_config(&config);
        let threads = fuser_config.n_threads.unwrap_or(1);
        // Session::new mounts and completes INIT before it returns.
        let session =
            fuser::spawn_mount(fs, &config.mountpoint, &fuser_config).map_err(|source| {
                MountError::Mount {
                    path: config.mountpoint.clone(),
                    source,
                }
            })?;
        let ended = Arc::new(Stop::default());
        {
            let ended = ended.clone();
            std::thread::Builder::new()
                .name("agentfs-session".into())
                .spawn(move || {
                    if let Err(e) = session.join() {
                        eprintln!("agentfs: the FUSE session ended with an error: {e}");
                    }
                    ended.stop();
                })?;
        }
        let sweeper = Sweeper::start(ops.clone(), config.sweep_interval)?;
        Ok(Self {
            ops,
            mountpoint: config.mountpoint,
            status,
            ended,
            sweeper: Some(sweeper),
            threads,
        })
    }

    /// Whether passthrough is in use.
    pub fn passthrough(&self) -> PassthroughStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set when the FUSE session ends: after [`Mount::shutdown`]'s unmount,
    /// or when something else unmounted it.
    pub fn ended(&self) -> Arc<Stop> {
        self.ended.clone()
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Unmounts (lazily if busy), waits up to `grace` for the session to
    /// end, stops the sweep and runs a final one over every file still
    /// open, plus the repair queue.
    pub fn shutdown(mut self, grace: Duration) -> UnmountReport {
        let mut report = UnmountReport::default();
        if !self.ended.is_stopped() {
            match unmount(&self.mountpoint) {
                Ok(lazy) => report.lazy = lazy,
                Err(e) => report.unmount_error = Some(e.to_string()),
            }
        }
        report.session_abandoned = !self.ended.wait(grace);
        if let Some(sweeper) = self.sweeper.take() {
            sweeper.stop();
        }
        let last = self.ops.sweep(true);
        report.swept = last.recorded;
        report.repaired = last.repaired;
        report.unrepaired = last.still_queued;
        report
    }
}

/// Unmounts `mountpoint`. Returns whether it had to be detached lazily
/// because it was busy.
fn unmount(mountpoint: &Path) -> io::Result<bool> {
    use rustix::mount::UnmountFlags;
    if rustix::process::geteuid().is_root() {
        return match rustix::mount::unmount(mountpoint, UnmountFlags::empty()) {
            Ok(()) => Ok(false),
            Err(rustix::io::Errno::BUSY) => {
                rustix::mount::unmount(mountpoint, UnmountFlags::DETACH)?;
                Ok(true)
            }
            // Already gone.
            Err(rustix::io::Errno::INVAL) => Ok(false),
            Err(e) => Err(e.into()),
        };
    }
    let fusermount = ["fusermount3", "fusermount"]
        .into_iter()
        .find(|bin| Command::new(bin).arg("-V").output().is_ok())
        .unwrap_or("fusermount3");
    let run = |args: &[&str]| Command::new(fusermount).args(args).arg(mountpoint).output();
    let out = run(&["-u", "-q", "--"])?;
    if out.status.success() {
        return Ok(false);
    }
    let lazy = run(&["-u", "-q", "-z", "--"])?;
    if lazy.status.success() {
        Ok(true)
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&lazy.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agentfs_core::config::RemoteSettings;

    use super::*;

    struct Layout {
        dir: tempfile::TempDir,
    }

    impl Layout {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            for d in ["state", "backing", "mnt"] {
                fs::create_dir_all(dir.path().join(d)).unwrap();
            }
            Self { dir }
        }

        fn p(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn check(&self, state: &str, backing: &str, mnt: &str) -> Result<(), PreflightError> {
            self.check_with(state, backing, mnt, &settings(vec![]))
        }

        fn check_with(
            &self,
            state: &str,
            backing: &str,
            mnt: &str,
            settings: &Settings,
        ) -> Result<(), PreflightError> {
            for d in [state, backing, mnt] {
                fs::create_dir_all(self.p(d)).unwrap();
            }
            preflight(
                &StateDir::new(self.p(state)),
                settings,
                &self.p(backing),
                &self.p(mnt),
            )
            .map(|_| ())
        }
    }

    fn settings(remotes: Vec<RemoteSettings>) -> Settings {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        agentfs_core::history::History::init(&git, "vol", None).unwrap();
        let mut s = Settings::load(&git).unwrap();
        s.remotes = remotes;
        s
    }

    fn remote(creds: PathBuf) -> RemoteSettings {
        RemoteSettings {
            name: "origin".into(),
            url: "file:///nowhere".into(),
            push_url: None,
            max_blob_size: None,
            credentials: Some(creds),
        }
    }

    #[test]
    fn siblings_are_fine() {
        let l = Layout::new();
        l.check("state", "backing", "mnt").unwrap();
    }

    #[test]
    fn nesting_is_refused() {
        let l = Layout::new();
        let err = l.check("backing/state", "backing", "mnt").unwrap_err();
        assert!(
            matches!(
                err,
                PreflightError::Nested {
                    inner: STATE,
                    outer: BACKING,
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            err.to_string().contains("visible through the mount"),
            "{err}"
        );
        let err = l.check("state/b", "state/b/x", "mnt").unwrap_err();
        assert!(matches!(err, PreflightError::Nested { .. }), "{err}");
        let err = l.check("state", "backing", "backing/mnt").unwrap_err();
        assert!(
            matches!(
                err,
                PreflightError::Nested {
                    inner: MOUNTPOINT,
                    outer: BACKING,
                    ..
                }
            ),
            "{err}"
        );
        let err = l.check("state", "backing", "state/mnt").unwrap_err();
        assert!(matches!(err, PreflightError::Nested { .. }), "{err}");
        let err = l.check("mnt/state", "backing", "mnt").unwrap_err();
        assert!(
            matches!(
                err,
                PreflightError::Nested {
                    inner: STATE,
                    outer: MOUNTPOINT,
                    ..
                }
            ),
            "{err}"
        );
        let err = l.check("state", "backing", "backing").unwrap_err();
        assert!(matches!(err, PreflightError::Same { .. }), "{err}");
        let err = l.check("state", "state", "mnt").unwrap_err();
        assert!(matches!(err, PreflightError::Same { .. }), "{err}");
    }

    #[test]
    fn nesting_through_symlinks_is_refused() {
        let l = Layout::new();
        fs::create_dir_all(l.p("backing/inner")).unwrap();
        std::os::unix::fs::symlink(l.p("backing/inner"), l.p("sneaky")).unwrap();
        let err = l.check("sneaky", "backing", "mnt").unwrap_err();
        assert!(matches!(err, PreflightError::Nested { .. }), "{err}");
    }

    #[test]
    fn exposed_credentials_are_refused() {
        let l = Layout::new();
        fs::write(l.p("backing/token"), "secret").unwrap();
        let s = settings(vec![remote(l.p("backing/token"))]);
        let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
        assert!(
            matches!(err, PreflightError::CredentialsExposed { .. }),
            "{err}"
        );
        // Through a symlinked path, and before the file exists.
        std::os::unix::fs::symlink(l.p("backing"), l.p("alias")).unwrap();
        let s = settings(vec![remote(l.p("alias/not-yet"))]);
        let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
        assert!(
            matches!(err, PreflightError::CredentialsExposed { .. }),
            "{err}"
        );
        // Outside is fine.
        fs::write(l.p("token"), "secret").unwrap();
        let s = settings(vec![remote(l.p("token"))]);
        l.check_with("state", "backing", "mnt", &s).unwrap();
    }

    #[test]
    fn missing_and_non_directories() {
        let l = Layout::new();
        fs::write(l.p("file"), "").unwrap();
        let err = preflight(
            &StateDir::new(l.p("state")),
            &settings(vec![]),
            &l.p("file"),
            &l.p("mnt"),
        )
        .unwrap_err();
        assert!(matches!(err, PreflightError::NotADirectory { .. }), "{err}");
        let err = preflight(
            &StateDir::new(l.p("state")),
            &settings(vec![]),
            &l.p("backing"),
            &l.p("nope"),
        )
        .unwrap_err();
        assert!(matches!(err, PreflightError::Missing { .. }), "{err}");
    }

    #[test]
    fn fuser_configuration() {
        let mut config = MountConfig {
            volume: "vol".into(),
            backing: "/b".into(),
            mountpoint: "/m".into(),
            passthrough: Passthrough::Auto,
            allow_other: true,
            threads: 0,
            sweep_interval: Duration::from_secs(30),
        };
        let c = fuser_config(&config);
        assert!(
            c.mount_options
                .contains(&MountOption::FSName("agentfs:vol".into()))
        );
        assert!(c.mount_options.contains(&MountOption::DefaultPermissions));
        assert!(!c.mount_options.contains(&MountOption::AutoUnmount));
        assert_eq!(c.acl, SessionACL::All);
        assert!(c.clone_fd);
        let n = c.n_threads.unwrap();
        assert!((2..=16).contains(&n), "{n}");
        config.allow_other = false;
        config.threads = 3;
        let c = fuser_config(&config);
        assert_eq!(c.acl, SessionACL::Owner);
        assert_eq!(c.n_threads, Some(3));
    }
}
