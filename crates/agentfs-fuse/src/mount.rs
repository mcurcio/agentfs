//! Mounting and unmounting (design §1.1, §17.1).
//!
//! [`preflight`] runs before anything is mounted and refuses layouts that
//! would break invisibility or recurse into the mount:
//!
//! - the state area, the backing tree and the mount point must be three
//!   separate directories, none inside another. The state area under the
//!   backing tree would be visible through the mount; anything under the
//!   mount point would be reached through the mount by the daemon itself;
//! - no file a remote names (`agentfsCredentials`, `agentfsSshKey`,
//!   `agentfsSshKeyPassphrase`, `agentfsSshAgent`, `agentfsKnownHosts`,
//!   `agentfsLfsCredentials`) may be under the backing tree (it would be
//!   readable through the mount) or the mount point. This is a
//!   hard error, not a `RemoteMisconfigured` alarm: carrying on would hand
//!   the secret to the workload;
//! - a mount point left by a crashed daemon (`ENOTCONN`) is reported with
//!   the command that clears it;
//! - no config fragment (`include.path`, design §4.3) may be under the
//!   backing tree or the mount point, for the same reason as credentials.
//!
//! [`mount_state`] tells a live agentfs mount from a stale one (its daemon
//! gone), from nothing and from another filesystem, using
//! `/proc/self/mountinfo` ([`agentfs_core::mountinfo`]) and a `statfs`; `agentfs
//! probe` and the hold mode of a restarted sidecar use it, and [`detach`]
//! clears a stale mount (design §17.1).
//!
//! [`Mount::start`] mounts with fuser (multi-threaded, one cloned
//! `/dev/fuse` descriptor per thread) and these options: `fsname` agentfs
//! and the volume, subtype `agentfs`, `default_permissions` (the kernel
//! checks modes and ownership), `allow_other` unless turned off, `nodev`,
//! `nosuid`, `rw`. There is no `auto_unmount`: a crashed daemon leaves the
//! mount point failing with `ENOTCONN` rather than exposing the empty
//! directory beneath it to writes nothing would record. Mounting is done
//! with `mount(2)` as root and `fusermount3` otherwise. Before it mounts,
//! it repairs the paths a previous mount could not record
//! ([`MountConfig::repair_store`], design §3.5), and refuses
//! `--passthrough on` without the privilege for it ([`crate::privileges`]).
//!
//! With [`MountConfig::stack_over`] (`--on-stale-mount recover`, design
//! §17.1) the mount point is a stale agentfs mount: FUSE is mounted at a
//! private directory of the state area, bind-mounted over the stale mount
//! (made shared, so propagation carries it into every copy of the stale
//! mount: workload containers, subPath binds), and the private directory is
//! detached. [`preflight_stacked`] checks the layout without touching the
//! stale mount point, and [`detach_stale_layers`] clears the layers crashes
//! leave under it.
//!
//! With [`MountConfig::overlay`] the daemon serves a second FUSE session,
//! the sandbox overlay ([`crate::overlay`]), at its own mount point over
//! the same [`Ops`]: one inode table, one name ordering, one set of open
//! files, one recorder, one sweep. Its options are the main mount's minus
//! `default_permissions` ([`fuser_config`], [`MountKind`]): the overlay
//! checks every access itself, as the view owner. [`preflight_overlay`]
//! refuses an overlay mount point that is the main one or nested with it;
//! [`Mount::start`] refuses the overlay with `--on-stale-mount recover`,
//! on a front, and on a volume whose rules read tree rule files.
//!
//! [`Mount::shutdown`] unmounts (lazily if the mount is busy; the overlay
//! first), waits a grace period for the sessions to end, stops the sweep
//! and runs a final one.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentfs_core::config::Settings;
use agentfs_core::mountinfo::{self, MountEntry};
use agentfs_core::service::Stop;
use agentfs_core::volume::{Recorder, StateDir};
use fuser::{MountOption, SessionACL};

use crate::backing::Backing;
use crate::fuse::{FuseFs, Passthrough, PassthroughStatus};
use crate::ops::{Cached, Ops, OpsConfig};
use crate::overlay::{Overlay, ViewOwner};
use crate::privileges::Privileges;
use crate::sweep::{Capturer, Sweeper};

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
    /// Where the repair queue is kept across mounts (design §3.5); `None`
    /// keeps it in memory only.
    pub repair_store: Option<PathBuf>,
    /// Mount over the stale agentfs mount at `mountpoint` instead of on it
    /// (`--on-stale-mount recover`, design §17.1); `None` mounts normally.
    pub stack_over: Option<StackOver>,
    /// Mount read-only (`ro`): the kernel refuses every change with
    /// `EROFS`, through any bind of the mount (a finished fork, design
    /// §11.5).
    pub read_only: bool,
    /// Also serve the sandbox overlay ([`crate::overlay`]) over the same
    /// operations, at its own mount point; `None` serves the main mount
    /// only.
    pub overlay: Option<OverlayConfig>,
}

/// Which of a daemon's FUSE sessions a mount is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// The volume's whole tree.
    Main,
    /// The sandbox overlay: the visibility gate and the view owner in front
    /// of the same operations ([`crate::overlay`]).
    SandboxOverlay,
}

/// What an agentfs mount's source names (sandbox overlay slice 4): its
/// volume and which of its daemon's sessions it is, so `agentfs probe`,
/// the hold and recover paths and the workload views never take one
/// session's mount point for the other's.
///
/// Rendered as `agentfs:<volume>` for the main mount (unchanged) and
/// `agentfs:<volume>:sandbox-overlay` for the sandbox overlay. A volume
/// name never holds `:` ([`agentfs_core::config::validate_name`]), so the
/// two never parse as each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSource {
    pub volume: String,
    pub kind: MountKind,
}

impl MountSource {
    pub fn new(volume: &str, kind: MountKind) -> Self {
        Self {
            volume: volume.to_owned(),
            kind,
        }
    }

    /// The source string a mount of this kind is made with (fuser's
    /// `fsname`).
    pub fn render(&self) -> String {
        todo!("sandbox overlay slice 4")
    }

    /// The source of an agentfs mount as mountinfo lists it; `None` for
    /// anything else.
    pub fn parse(source: &str) -> Option<Self> {
        let _ = source;
        todo!("sandbox overlay slice 4")
    }
}

/// The sandbox overlay's mount point and view owner
/// (`--sandbox-overlay`, `--sandbox-owner`).
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    pub mountpoint: PathBuf,
    pub owner: ViewOwner,
    /// Stack the overlay over the stale overlay mount a crashed daemon
    /// left at its mount point (`--on-stale-mount recover`, design §17.1),
    /// as [`MountConfig::stack_over`] does for the main mount; `None`
    /// mounts normally. Its source is not the main mount's.
    pub stack_over: Option<StackOver>,
}

/// How a mount is stacked over a stale one: FUSE is mounted at `source`, a
/// private empty directory, then bind-mounted over the mount point, where
/// mount propagation carries it into every copy of the stale mount
/// (workload containers, subPath binds), and `source` is detached.
#[derive(Debug, Clone)]
pub struct StackOver {
    pub source: PathBuf,
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
    /// `--on-stale-mount recover` found no stale agentfs mount to stack over.
    #[error("{} is not a stale agentfs mount: nothing to stack over", .0.display())]
    NotStale(PathBuf),
    /// `--on-stale-mount recover` found the stale mount of another volume:
    /// stacking over it would send that volume's workloads into this one's
    /// history.
    #[error(
        "{} is a stale mount of {found}, not of this volume ({expected}); refusing to stack \
         over it; detach it first if it is no longer wanted: umount -l {}",
        path.display(),
        path.display()
    )]
    OtherVolume {
        path: PathBuf,
        found: String,
        expected: String,
    },
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
    /// A remote's credentials, SSH key or known_hosts file is under the
    /// backing tree or the mount point: the workload could read the secret,
    /// or rewrite known_hosts to trust any host.
    #[error(
        "remote {remote:?}: its {key} file {} is inside {outer} {}, where the workload \
         could read or change it; keep credentials, keys and known_hosts outside the backing \
         tree and the mount point",
        path.display(),
        outer_path.display()
    )]
    CredentialsExposed {
        remote: String,
        /// The config key naming the file (`agentfsCredentials`,
        /// `agentfsSshKey`, `agentfsSshKeyPassphrase`, `agentfsSshAgent`,
        /// `agentfsKnownHosts` or `agentfsLfsCredentials`).
        key: &'static str,
        path: PathBuf,
        outer: &'static str,
        outer_path: PathBuf,
    },
    /// An operator config fragment (`include.path`, design §4.3, or an
    /// unlinked fork's `agentfs.fork.remotes`, §11.5) is under the backing
    /// tree or the mount point: the workload could rewrite the operator's
    /// settings and remotes.
    #[error(
        "{key} {} is inside {outer} {}, where the workload could change the \
         operator's settings; keep config fragments outside the backing tree and the mount \
         point",
        path.display(),
        outer_path.display()
    )]
    IncludeExposed {
        key: &'static str,
        path: PathBuf,
        outer: &'static str,
        outer_path: PathBuf,
    },
    /// The sandbox overlay's mount point is the main mount point, or one is
    /// inside the other: one mount would hide or reach into the other.
    #[error(
        "the sandbox overlay's mount point {} and the mount point {} are the same directory \
         or one is inside the other; give each its own directory",
        overlay.display(),
        main.display()
    )]
    OverlayNested { main: PathBuf, overlay: PathBuf },
}

const STATE: &str = "the state area";
const BACKING: &str = "the backing tree";
const MOUNTPOINT: &str = "the mount point";
const OVERLAY: &str = "the sandbox overlay's mount point";

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
/// §18 test 5), and every remote section's credentials, SSH key and
/// known_hosts file out of the backing tree and the mount point, invalid
/// sections included (design §17.1). Returns the canonical backing tree and
/// mount point.
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
    let mountpoint = canonical_dir(MOUNTPOINT, mountpoint)?;
    check_layout(state, settings, backing, mountpoint)
}

/// Checks the sandbox overlay's mount point `overlay` against the main
/// mount point `main` (canonical, as [`preflight`] returns it): neither may
/// be the other or inside it. Run [`preflight`] on `overlay` as well, for
/// the state area, the backing tree and the remotes' files. Returns the
/// canonical overlay mount point.
pub fn preflight_overlay(main: &Path, overlay: &Path) -> Result<PathBuf, PreflightError> {
    // A stale overlay mount (`--on-stale-mount recover`) answers `stat`
    // with ENOTCONN: it is resolved through its parent.
    let overlay = if rustix::fs::stat(overlay).err() == Some(rustix::io::Errno::NOTCONN) {
        canonical_any(overlay)
    } else {
        canonical_dir(OVERLAY, overlay)?
    };
    // The main mount point may be a stale mount (`--on-stale-mount`),
    // which answers `stat` with ENOTCONN.
    let main = canonical_any(main);
    if overlay.starts_with(&main) || main.starts_with(&overlay) {
        return Err(PreflightError::OverlayNested { main, overlay });
    }
    Ok(overlay)
}

/// [`preflight`] for a mount stacked over the stale agentfs mount at
/// `mountpoint` (`--on-stale-mount recover`, design §17.1): the same
/// checks, with the mount point resolved through its parent, since a
/// stale mount answers every `stat` with `ENOTCONN`. Refuses anything but
/// a stale agentfs mount of this volume (source `agentfs:<volume>`) there.
pub fn preflight_stacked(
    state: &StateDir,
    settings: &Settings,
    backing: &Path,
    mountpoint: &Path,
) -> Result<(PathBuf, PathBuf), PreflightError> {
    let (Some(parent), Some(name)) = (mountpoint.parent(), mountpoint.file_name()) else {
        return Err(PreflightError::NotStale(mountpoint.to_path_buf()));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let resolved = canonical_dir(MOUNTPOINT, parent)?.join(name);
    match mount_state(&resolved) {
        Ok(MountState::Stale { source }) => same_volume(&resolved, source, &settings.volume)?,
        _ => return Err(PreflightError::NotStale(resolved)),
    }
    check_layout(state, settings, backing, resolved)
}

/// Refuses a stale mount at `path` whose source is not `agentfs:<volume>`.
fn same_volume(path: &Path, source: String, volume: &str) -> Result<(), PreflightError> {
    let expected = format!("agentfs:{volume}");
    if source == expected {
        return Ok(());
    }
    Err(PreflightError::OtherVolume {
        path: path.to_path_buf(),
        found: source,
        expected,
    })
}

/// The checks of [`preflight`] past the mount point's own.
fn check_layout(
    state: &StateDir,
    settings: &Settings,
    backing: &Path,
    mountpoint: PathBuf,
) -> Result<(PathBuf, PathBuf), PreflightError> {
    let state = canonical_dir(STATE, state.root())?;
    let backing = canonical_dir(BACKING, backing)?;
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
    for (remote, key, path) in settings
        .remotes
        .pushed()
        .iter()
        .flat_map(|r| r.files().into_iter().map(move |(k, p)| (&r.name, k, p)))
        .chain(
            settings
                .invalid_remotes
                .iter()
                .flat_map(|r| r.files.iter().map(move |(k, p)| (&r.name, *k, p.as_path()))),
        )
    {
        let path = canonical_any(path);
        for (outer, outer_path) in [(BACKING, &backing), (MOUNTPOINT, &mountpoint)] {
            if path.starts_with(outer_path) {
                return Err(PreflightError::CredentialsExposed {
                    remote: remote.clone(),
                    key,
                    path,
                    outer,
                    outer_path: outer_path.clone(),
                });
            }
        }
    }
    // An unlinked fork's own remotes file is settings as an include is.
    let fragments = (settings.includes.iter().map(|p| ("include.path", p))).chain(
        settings
            .fork_remotes
            .iter()
            .map(|p| (agentfs_core::fork::identity::REMOTES_KEY, p)),
    );
    for (key, include) in fragments {
        let path = canonical_any(include);
        for (outer, outer_path) in [(BACKING, &backing), (MOUNTPOINT, &mountpoint)] {
            if path.starts_with(outer_path) {
                return Err(PreflightError::IncludeExposed {
                    key,
                    path,
                    outer,
                    outer_path: outer_path.clone(),
                });
            }
        }
    }
    Ok((backing, mountpoint))
}

/// What is mounted at a path, as `agentfs probe` and the hold mode of a
/// restarted sidecar see it (design §17.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    /// An agentfs mount whose daemon answers.
    Live { source: String },
    /// An agentfs mount whose daemon is gone (`ENOTCONN`).
    Stale { source: String },
    /// Nothing is mounted at the path itself.
    NotMounted,
    /// Something other than agentfs is mounted there.
    Foreign { fstype: String },
}

/// What is mounted at `path`: the last (topmost) mountinfo entry for it,
/// and for an agentfs mount, whether its daemon answers a `statfs`. The
/// path is resolved without touching its last component when that fails,
/// since a stale mount answers every `stat` with `ENOTCONN`.
pub fn mount_state(path: &Path) -> io::Result<MountState> {
    mount_state_in(&mountinfo::read()?, path)
}

/// `path` resolved as mountinfo names it, through its parent when the path
/// itself cannot be reached (a stale mount).
fn resolve_mountpoint(path: &Path) -> io::Result<PathBuf> {
    Ok(match path.canonicalize() {
        Ok(p) => p,
        Err(_) => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                parent.canonicalize()?.join(name)
            }
            _ => std::path::absolute(path)?,
        },
    })
}

fn mount_state_in(entries: &[MountEntry], path: &Path) -> io::Result<MountState> {
    // A path whose parent does not exist yet has nothing mounted on it.
    let resolved = match resolve_mountpoint(path) {
        Ok(resolved) => resolved,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MountState::NotMounted),
        Err(e) => return Err(e),
    };
    let Some(entry) = entries.iter().rev().find(|e| e.mountpoint == resolved) else {
        return Ok(MountState::NotMounted);
    };
    if !entry.is_agentfs() {
        return Ok(MountState::Foreign {
            fstype: entry.fstype.clone(),
        });
    }
    let source = entry.source.clone();
    match rustix::fs::statfs(&resolved) {
        Ok(_) => Ok(MountState::Live { source }),
        Err(rustix::io::Errno::NOTCONN) => Ok(MountState::Stale { source }),
        Err(e) => Err(e.into()),
    }
}

/// Detaches whatever is mounted at `path` lazily: `umount2(MNT_DETACH)`
/// with the privilege to unmount, `fusermount3 -u -z` otherwise. Used to
/// clear a stale agentfs mount (design §17.1).
pub fn detach(path: &Path) -> io::Result<()> {
    if Privileges::current().can_unmount() {
        return rustix::mount::unmount(path, rustix::mount::UnmountFlags::DETACH)
            .map_err(io::Error::from);
    }
    let out = Command::new(fusermount())
        .args(["-u", "-q", "-z", "--"])
        .arg(path)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

/// Detaches the stale agentfs mounts at `path`, topmost first, until the
/// topmost is not one, at most `max`; returns how many it detached. Each
/// crash of a recovered daemon leaves one stale layer under the mount
/// stacked over it (design §17.1).
pub fn detach_stale_layers(path: &Path, max: usize) -> io::Result<usize> {
    let mut n = 0;
    while n < max && matches!(mount_state(path)?, MountState::Stale { .. }) {
        detach(path)?;
        n += 1;
    }
    Ok(n)
}

/// Counts the agentfs mounts at `path` (stacked layers included).
pub fn agentfs_layers(path: &Path) -> io::Result<usize> {
    let resolved = resolve_mountpoint(path)?;
    Ok(mountinfo::read()?
        .iter()
        .filter(|e| e.mountpoint == resolved && e.is_agentfs())
        .count())
}

/// Prepares `source` for a stacked mount: clears stale mounts a crash
/// between mounting and detaching it left, and creates it (mode 0700) as
/// an empty directory.
fn prepare_stack_source(source: &Path) -> io::Result<()> {
    // The parent first: on a fresh state area it does not exist yet, and
    // looking for stale layers resolves the source through it.
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent)?;
    }
    detach_stale_layers(source, 16)?;
    match std::fs::DirBuilder::new().mode(0o700).create(source) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    if std::fs::read_dir(source)?.next().is_some() {
        return Err(io::Error::other(format!(
            "{} is not empty",
            source.display()
        )));
    }
    Ok(())
}

/// Bind-mounts the mount at `source` over `target`, makes the new layer
/// shared (best effort: under a Bidirectional emptyDir it already is) so it
/// propagates, then detaches `source`.
fn stack(source: &Path, target: &Path) -> io::Result<()> {
    use rustix::mount::{MountPropagationFlags, UnmountFlags};
    rustix::mount::mount_bind(source, target)?;
    if let Err(e) = rustix::mount::mount_change(target, MountPropagationFlags::SHARED) {
        eprintln!(
            "agentfs: making {} shared failed ({e}); copies of the stale mount may not see the \
             new one",
            target.display()
        );
    }
    if let Err(e) = rustix::mount::unmount(source, UnmountFlags::DETACH) {
        // Take the new layer off again, so the caller can unmount the
        // source and leave everything as it was.
        let _ = rustix::mount::unmount(target, UnmountFlags::DETACH);
        return Err(e.into());
    }
    Ok(())
}

/// `fusermount3`, or `fusermount` where only the old one is installed.
fn fusermount() -> &'static str {
    ["fusermount3", "fusermount"]
        .into_iter()
        .find(|bin| Command::new(bin).arg("-V").output().is_ok())
        .unwrap_or("fusermount3")
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
    /// `--passthrough on` cannot be honoured with these privileges.
    #[error("--passthrough on: {0}")]
    Passthrough(&'static str),
    /// Stacking over the stale mount failed (design §17.1).
    #[error("stacking over the stale mount at {}", path.display())]
    Stack { path: PathBuf, source: io::Error },
    /// The volume cannot have the sandbox overlay's visibility gate.
    #[error(transparent)]
    Gate(#[from] agentfs_core::visibility::GateError),
    /// The sandbox overlay is not served together with this.
    #[error("the sandbox overlay (--sandbox-overlay) cannot be served with {0} yet")]
    OverlayUnsupported(&'static str),
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
    /// fsync-class groups captured at unmount (design §8).
    pub captured: usize,
    /// Failed records the final sweep repaired, and those still failing.
    pub repaired: usize,
    pub unrepaired: usize,
    /// The paths still failing.
    pub unrepaired_paths: Vec<String>,
    /// Where they are saved for the next mount; `None` if they could not
    /// be saved (or no store is configured).
    pub repairs_saved: Option<PathBuf>,
}

/// What [`Mount::start`] repaired from the queue a previous mount left.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StartupRepairs {
    pub loaded: usize,
    pub repaired: usize,
    pub still_queued: usize,
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

/// fuser's configuration for `config`'s session of kind `kind`. The
/// sandbox overlay's has no `default_permissions`: the overlay checks
/// every access itself, as the view owner ([`crate::overlay`]).
pub fn fuser_config(config: &MountConfig, kind: MountKind) -> fuser::Config {
    let mut c = fuser::Config::default();
    c.mount_options = [
        Some(MountOption::FSName(format!("agentfs:{}", config.volume))),
        Some(MountOption::Subtype("agentfs".into())),
        (kind == MountKind::Main).then_some(MountOption::DefaultPermissions),
        Some(MountOption::NoDev),
        Some(MountOption::NoSuid),
        Some(if config.read_only {
            MountOption::RO
        } else {
            MountOption::RW
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
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

/// The sandbox overlay's visibility gate for a volume whose recorder is a
/// front (`is_front`) with path rules `rules`, when `overlay` is
/// configured: refused on a front, which does not serve it yet, and on a
/// volume whose rules read tree rule files ([`MountError::Gate`]).
fn overlay_gate(
    overlay: Option<&OverlayConfig>,
    is_front: bool,
    rules: &Arc<agentfs_core::rules::Rules>,
) -> Result<Option<agentfs_core::visibility::Gate>, MountError> {
    if overlay.is_none() {
        return Ok(None);
    }
    if is_front {
        return Err(MountError::OverlayUnsupported(
            "a front (--history-service)",
        ));
    }
    Ok(Some(agentfs_core::visibility::Gate::new(rules.clone())?))
}

/// The sandbox overlay's session, beside the main one.
struct OverlaySession {
    mountpoint: PathBuf,
    /// Set when its FUSE session ends.
    ended: Arc<Stop>,
}

/// Runs `session` until it ends, then sets `ended` and `either`.
fn watch_session(
    session: fuser::BackgroundSession,
    name: &str,
    ended: Arc<Stop>,
    either: Arc<Stop>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let Err(e) = session.join() {
                eprintln!("agentfs: the FUSE session ended with an error: {e}");
            }
            ended.stop();
            either.stop();
        })?;
    Ok(())
}

/// Mounts the sandbox overlay's session `fs` at `overlay`'s mount point,
/// stacked over its stale mount when [`OverlayConfig::stack_over`] says so
/// (as [`Mount::start`] stacks the main mount).
fn spawn_overlay<FS: fuser::Filesystem + Send + 'static>(
    fs: FS,
    overlay: &OverlayConfig,
    options: &fuser::Config,
) -> Result<fuser::BackgroundSession, MountError> {
    let stack_error = |source| MountError::Stack {
        path: overlay.mountpoint.clone(),
        source,
    };
    let at = match &overlay.stack_over {
        Some(stack) => {
            prepare_stack_source(&stack.source).map_err(stack_error)?;
            stack.source.clone()
        }
        None => overlay.mountpoint.clone(),
    };
    let session = fuser::spawn_mount(fs, &at, options).map_err(|source| MountError::Mount {
        path: at.clone(),
        source,
    })?;
    if overlay.stack_over.is_some()
        && let Err(source) = stack(&at, &overlay.mountpoint)
    {
        let _ = session.umount_and_join();
        return Err(stack_error(source));
    }
    Ok(session)
}

/// A mounted volume: the main mount, and the sandbox overlay when
/// configured, over one [`Ops`].
pub struct Mount {
    ops: Arc<Ops>,
    mountpoint: PathBuf,
    status: Arc<Mutex<PassthroughStatus>>,
    /// Set when either session ends.
    ended: Arc<Stop>,
    /// Set when the main session ends.
    main_ended: Arc<Stop>,
    overlay: Option<OverlaySession>,
    failed: Arc<Stop>,
    sweeper: Option<Sweeper>,
    capturer: Option<Capturer>,
    threads: usize,
    startup: StartupRepairs,
    /// Sends the kernel cache invalidations of [`MountHooks`].
    notifier: fuser::Notifier,
}

impl Mount {
    /// Mounts `config.backing` at `config.mountpoint`, recording through
    /// `recorder`, and starts the sweep. Returns once the kernel has
    /// finished INIT. Run [`preflight`] first.
    ///
    /// With [`MountConfig::overlay`], the sandbox overlay is mounted at its
    /// own mount point too, over the same [`Ops`]; if it cannot be, the
    /// main mount is unmounted again and the start fails. Run
    /// [`preflight_overlay`] first.
    pub fn start(recorder: Recorder, config: MountConfig) -> Result<Self, MountError> {
        // Everything the overlay needs is checked before anything mounts.
        let gate = overlay_gate(
            config.overlay.as_ref(),
            recorder.is_front(),
            recorder.rules(),
        )?;
        let overlay = config.overlay.clone().zip(gate);
        if !Path::new("/dev/fuse").exists() {
            return Err(MountError::NoFuse);
        }
        if config.passthrough == Passthrough::On
            && let Some(refusal) = Privileges::current().passthrough_refusal()
        {
            return Err(MountError::Passthrough(refusal));
        }
        raise_nofile();
        let backing = Backing::open(&config.backing).map_err(|source| MountError::Backing {
            path: config.backing.clone(),
            source,
        })?;
        let ops = Arc::new(Ops::new(backing, recorder, OpsConfig::default())?);
        // Paths a previous mount could not record are repaired from the
        // backing tree before any request is served.
        let mut startup = StartupRepairs::default();
        if let Some(store) = &config.repair_store {
            startup.loaded = ops.boundary().persist_repairs(store.clone())?;
            if startup.loaded > 0 {
                let report = ops.sweep(false);
                startup.repaired = report.repaired;
                startup.still_queued = report.still_queued;
            }
        }
        let status = Arc::new(Mutex::new(PassthroughStatus::Off("not negotiated".into())));
        let failed = Arc::new(Stop::default());
        let fs = FuseFs::new(
            ops.clone(),
            config.passthrough,
            status.clone(),
            failed.clone(),
        );
        let main_options = fuser_config(&config, MountKind::Main);
        let threads = main_options.n_threads.unwrap_or(1);
        // Stacked over a stale mount: FUSE goes to a private directory
        // first, since the stale mount point answers ENOTCONN.
        let at = match &config.stack_over {
            Some(stack) => {
                prepare_stack_source(&stack.source).map_err(|source| MountError::Stack {
                    path: config.mountpoint.clone(),
                    source,
                })?;
                stack.source.clone()
            }
            None => config.mountpoint.clone(),
        };
        // Session::new mounts and completes INIT before it returns.
        let session =
            fuser::spawn_mount(fs, &at, &main_options).map_err(|source| MountError::Mount {
                path: at.clone(),
                source,
            })?;
        if config.stack_over.is_some()
            && let Err(source) = stack(&at, &config.mountpoint)
        {
            // Nothing is left stacked; unmounting the source ends the
            // session.
            let _ = session.umount_and_join();
            return Err(MountError::Stack {
                path: config.mountpoint.clone(),
                source,
            });
        }
        // The overlay's session: its own passthrough status (each session
        // negotiates INIT), the shared stop for a failed passthrough. If it
        // cannot be mounted, the main mount is taken down again (its
        // stacked layer too), so the start leaves nothing mounted.
        let overlay = match overlay {
            Some((overlay, gate)) => {
                let fs = FuseFs::new(
                    Arc::new(Overlay::new(ops.clone(), gate, overlay.owner)),
                    config.passthrough,
                    Arc::new(Mutex::new(PassthroughStatus::Off("not negotiated".into()))),
                    failed.clone(),
                );
                let overlay_config = fuser_config(&config, MountKind::SandboxOverlay);
                match spawn_overlay(fs, &overlay, &overlay_config) {
                    Ok(overlay_session) => Some((overlay.mountpoint, overlay_session)),
                    Err(e) => {
                        if config.stack_over.is_some() {
                            let _ = rustix::mount::unmount(
                                &config.mountpoint,
                                rustix::mount::UnmountFlags::DETACH,
                            );
                        }
                        let _ = session.umount_and_join();
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        let notifier = session.notifier();
        let ended = Arc::new(Stop::default());
        let main_ended = Arc::new(Stop::default());
        watch_session(
            session,
            "agentfs-session",
            main_ended.clone(),
            ended.clone(),
        )?;
        let overlay = match overlay {
            Some((mountpoint, overlay_session)) => {
                let overlay_ended = Arc::new(Stop::default());
                watch_session(
                    overlay_session,
                    "agentfs-overlay",
                    overlay_ended.clone(),
                    ended.clone(),
                )?;
                Some(OverlaySession {
                    mountpoint,
                    ended: overlay_ended,
                })
            }
            None => None,
        };
        let sweeper = Sweeper::start(ops.clone(), config.sweep_interval)?;
        let capturer = if ops.boundary().recorder().has_fsync_class() {
            Some(Capturer::start(ops.clone())?)
        } else {
            None
        };
        Ok(Self {
            ops,
            mountpoint: config.mountpoint,
            status,
            ended,
            main_ended,
            overlay,
            failed,
            sweeper: Some(sweeper),
            capturer,
            threads,
            startup,
            notifier,
        })
    }

    /// Whether passthrough is in use.
    pub fn passthrough(&self) -> PassthroughStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set when a FUSE session ends (the main mount's or the sandbox
    /// overlay's): after [`Mount::shutdown`]'s unmount, or when something
    /// else unmounted it.
    pub fn ended(&self) -> Arc<Stop> {
        self.ended.clone()
    }

    /// Set when `--passthrough on` could not be honoured after INIT: the
    /// mount must stop ([`Mount::passthrough`] says why).
    pub fn failed(&self) -> Arc<Stop> {
        self.failed.clone()
    }

    /// What was repaired at start from a previous mount's queue.
    pub fn startup_repairs(&self) -> StartupRepairs {
        self.startup
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// The sandbox overlay's mount point, when it is served.
    pub fn overlay_mountpoint(&self) -> Option<&Path> {
        self.overlay.as_ref().map(|o| o.mountpoint.as_path())
    }

    /// What a restore through the mount needs of it (design §15).
    pub fn view(&self) -> MountView {
        MountView {
            mountpoint: self.mountpoint.clone(),
            ops: self.ops.clone(),
        }
    }

    /// What a front's listener answers the history service with (design
    /// §15): see [`MountHooks`].
    pub fn hooks(&self) -> MountHooks {
        MountHooks {
            ops: self.ops.clone(),
            notifier: self.notifier.clone(),
        }
    }

    /// Unmounts the sandbox overlay, then the main mount (each lazily if
    /// busy), waits up to `grace` for both sessions to end, stops the
    /// sweep and runs a final one over every file still open, plus the
    /// repair queue, then captures every changed fsync-class group.
    pub fn shutdown(mut self, grace: Duration) -> UnmountReport {
        let mut report = UnmountReport::default();
        let sessions = self
            .overlay
            .iter()
            .map(|o| (o.mountpoint.as_path(), &o.ended))
            .chain([(self.mountpoint.as_path(), &self.main_ended)]);
        let mut waits = Vec::new();
        for (mountpoint, ended) in sessions {
            if !ended.is_stopped() {
                match unmount(mountpoint) {
                    Ok(lazy) => report.lazy |= lazy,
                    Err(e) => {
                        report
                            .unmount_error
                            .get_or_insert_with(|| format!("{}: {e}", mountpoint.display()));
                    }
                }
            }
            waits.push(ended.clone());
        }
        let deadline = std::time::Instant::now() + grace;
        report.session_abandoned = !waits
            .iter()
            .all(|ended| ended.wait(deadline.saturating_duration_since(std::time::Instant::now())));
        if let Some(sweeper) = self.sweeper.take() {
            sweeper.stop();
        }
        if let Some(capturer) = self.capturer.take() {
            capturer.stop();
        }
        let last = self.ops.sweep(true);
        report.captured = self.ops.capture_all();
        report.swept = last.recorded;
        report.repaired = last.repaired;
        report.unrepaired = last.still_queued;
        if report.unrepaired > 0 {
            report.unrepaired_paths = self.ops.boundary().pending_paths();
            report.repairs_saved = self
                .ops
                .boundary()
                .repair_store()
                .and_then(|(store, error)| error.is_none().then_some(store));
        }
        report
    }
}

/// A running mount as a restore sees it: where to write, and which files
/// the workload holds open for writing (design §15). Cheap to clone.
#[derive(Clone)]
pub struct MountView {
    mountpoint: PathBuf,
    ops: Arc<Ops>,
}

impl MountView {
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Records every fsync-class group the running operator operation
    /// changed, each whole (design §8, §15). Call it after the operation's
    /// last write, before it ends: until then no other trigger records
    /// those groups. Returns how many captures were journaled.
    pub fn capture_operation(&self) -> Result<usize, agentfs_core::volume::VolumeError> {
        self.ops.capture_operation()
    }
}

impl agentfs_core::restore::Liveness for MountView {
    fn open_for_writing(&self, path: &agentfs_core::paths::RepoPath) -> bool {
        self.ops.open_for_writing(path.as_str().as_bytes())
    }

    /// The FUSE layer records every write through the mount.
    fn records_its_writes(&self) -> bool {
        true
    }
}

/// A front mount's answers to the history service's calls (design §15),
/// for a restore the service writes into the backing tree directly:
///
/// - settle: the sweep, as at every interval (the repair queue, then every
///   changed file open for writing), returning once it is journaled;
/// - open for writing: from the inode table, as for a restore through the
///   mount;
/// - invalidate: the kernel drops its cached entries along each path and
///   the attributes and pages of the node at it ([`Ops::cached_at`]).
///   Sent from the listener's thread, never a FUSE request's: the kernel
///   may wait on a request to finish an invalidation.
pub struct MountHooks {
    ops: Arc<Ops>,
    notifier: fuser::Notifier,
}

impl agentfs_core::history_service::FrontHooks for MountHooks {
    fn settle(&self) -> Result<(usize, usize), String> {
        let report = self.ops.sweep(false);
        Ok((report.recorded + report.repaired, report.still_queued))
    }

    fn open_for_writing(&self, paths: &[agentfs_core::paths::RepoPath]) -> Vec<bool> {
        paths
            .iter()
            .map(|p| self.ops.open_for_writing(p.as_str().as_bytes()))
            .collect()
    }

    fn invalidate(&self, paths: &[agentfs_core::paths::RepoPath]) -> Result<(), String> {
        use std::os::unix::ffi::OsStrExt;
        let mut first = None;
        for path in paths {
            for cached in self.ops.cached_at(path.as_str().as_bytes()) {
                // An inode or entry the kernel no longer holds is not an
                // error (fuser maps ENOENT to Ok).
                let sent = match &cached {
                    Cached::Node(id) => self.notifier.inval_inode(fuser::INodeNo(*id), 0, 0),
                    Cached::Entry(parent, name) => self
                        .notifier
                        .inval_entry(fuser::INodeNo(*parent), std::ffi::OsStr::from_bytes(name)),
                };
                if let Err(e) = sent {
                    first.get_or_insert_with(|| format!("invalidating {path}: {e}"));
                }
            }
        }
        first.map_or(Ok(()), Err)
    }
}

/// Unmounts `mountpoint`. Returns whether it had to be detached lazily
/// because it was busy.
fn unmount(mountpoint: &Path) -> io::Result<bool> {
    use rustix::mount::UnmountFlags;
    if Privileges::current().can_unmount() {
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
    let fusermount = fusermount();
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

    use agentfs_core::config::{InvalidRemote, RemoteSettings};

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
        s.remotes = match agentfs_core::non_empty::NonEmpty::new(remotes) {
            Some(remotes) => agentfs_core::config::Remotes::Pushed(remotes),
            None => agentfs_core::config::Remotes::LocalOnly,
        };
        s
    }

    fn remote(creds: PathBuf) -> RemoteSettings {
        RemoteSettings {
            name: "origin".into(),
            url: "file:///nowhere".into(),
            push_url: None,
            max_blob_size: None,
            max_push_size: None,
            max_lfs_object_size: None,
            proxy: None,
            credentials: Some(creds),
            ssh_key: None,
            known_hosts: None,
            lfs_url: None,
            lfs_push_url: None,
            lfs_credentials: None,
            ssh_agent: None,
            ssh_key_passphrase: None,
            fork_remote: false,
            push_source: agentfs_core::config::PushSource::Branch,
        }
    }

    fn ssh_remote(key: PathBuf, known_hosts: PathBuf) -> RemoteSettings {
        RemoteSettings {
            name: "ssh".into(),
            url: "ssh://git@git.example/r.git".into(),
            push_url: None,
            max_blob_size: None,
            max_push_size: None,
            max_lfs_object_size: None,
            proxy: None,
            lfs_url: None,
            lfs_push_url: None,
            lfs_credentials: None,
            ssh_agent: None,
            ssh_key_passphrase: None,
            credentials: None,
            ssh_key: Some(key),
            known_hosts: Some(known_hosts),
            fork_remote: false,
            push_source: agentfs_core::config::PushSource::Branch,
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
        // The LFS credential file is held to the same rule (design §7).
        let mut lfs = remote(l.p("token"));
        lfs.lfs_credentials = Some(l.p("mnt/lfs-token"));
        let err = l
            .check_with("state", "backing", "mnt", &settings(vec![lfs]))
            .unwrap_err();
        assert!(
            matches!(&err, PreflightError::CredentialsExposed { key, outer, .. }
                if *key == "agentfsLfsCredentials" && *outer == MOUNTPOINT),
            "{err}"
        );
    }

    #[test]
    fn exposed_ssh_keys_and_known_hosts_are_refused() {
        let l = Layout::new();
        let (key, kh) = (l.p("id"), l.p("known_hosts"));
        fs::write(&key, "key").unwrap();
        fs::write(&kh, "hosts").unwrap();
        l.check_with(
            "state",
            "backing",
            "mnt",
            &settings(vec![ssh_remote(key.clone(), kh.clone())]),
        )
        .unwrap();
        for (k, h, expected, outer) in [
            (l.p("backing/id"), kh.clone(), "agentfsSshKey", BACKING),
            (
                key.clone(),
                l.p("backing/.ssh/known_hosts"),
                "agentfsKnownHosts",
                BACKING,
            ),
            (l.p("mnt/id"), kh.clone(), "agentfsSshKey", MOUNTPOINT),
            (
                key.clone(),
                l.p("mnt/known_hosts"),
                "agentfsKnownHosts",
                MOUNTPOINT,
            ),
        ] {
            let s = settings(vec![ssh_remote(k, h)]);
            let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
            assert!(
                matches!(&err, PreflightError::CredentialsExposed { key, outer: o, remote, .. }
                    if *key == expected && *o == outer && remote == "ssh"),
                "{err}"
            );
            assert!(err.to_string().contains(expected), "{err}");
        }
        // A passphrase file, and an ssh-agent socket the workload could
        // replace with its own agent.
        for (expected, path) in [
            ("agentfsSshKeyPassphrase", l.p("backing/passphrase")),
            ("agentfsSshAgent", l.p("mnt/agent.sock")),
        ] {
            let mut remote = ssh_remote(key.clone(), kh.clone());
            if expected == "agentfsSshAgent" {
                remote.ssh_agent = Some(path);
            } else {
                remote.ssh_key_passphrase = Some(path);
            }
            let err = l
                .check_with("state", "backing", "mnt", &settings(vec![remote]))
                .unwrap_err();
            assert!(
                matches!(&err, PreflightError::CredentialsExposed { key, .. } if *key == expected),
                "{err}"
            );
        }
    }

    #[test]
    fn exposed_includes_are_refused() {
        let l = Layout::new();
        let mut s = settings(vec![]);
        s.includes = vec![l.p("operator.conf")];
        l.check_with("state", "backing", "mnt", &s).unwrap();
        for (path, outer) in [
            (l.p("backing/etc/agentfs.conf"), BACKING),
            (l.p("mnt/agentfs.conf"), MOUNTPOINT),
        ] {
            s.includes = vec![l.p("operator.conf"), path.clone()];
            let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
            assert!(
                matches!(&err, PreflightError::IncludeExposed { path: p, outer: o, .. }
                    if *p == path && *o == outer),
                "{err}"
            );
            assert!(err.to_string().contains("include.path"), "{err}");
        }
        // An unlinked fork's own remotes file likewise (design §11.5).
        s.includes = vec![l.p("operator.conf")];
        s.fork_remotes = Some(l.p("operator-remotes.conf"));
        l.check_with("state", "backing", "mnt", &s).unwrap();
        s.fork_remotes = Some(l.p("backing/remotes.conf"));
        let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
        assert!(
            matches!(&err, PreflightError::IncludeExposed { key, outer, .. }
                if *key == "agentfs.fork.remotes" && *outer == BACKING),
            "{err}"
        );
    }

    #[test]
    fn mount_state_reads_the_topmost_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mnt = root.join("mnt");
        fs::create_dir(&mnt).unwrap();
        let entry = |fstype: &str, source: &str| MountEntry {
            device: "0:1".into(),
            root: "/".into(),
            mountpoint: mnt.clone(),
            fstype: fstype.into(),
            source: source.into(),
            super_options: "rw".into(),
        };
        assert_eq!(mount_state_in(&[], &mnt).unwrap(), MountState::NotMounted);
        // Only the path itself counts, not a mount above it.
        let above = MountEntry {
            mountpoint: root.clone(),
            ..entry("fuse.agentfs", "agentfs:v")
        };
        assert_eq!(
            mount_state_in(std::slice::from_ref(&above), &mnt).unwrap(),
            MountState::NotMounted
        );
        // A foreign mount on top of an agentfs one wins, and the reverse.
        let stack = [entry("fuse.agentfs", "agentfs:v"), entry("tmpfs", "tmpfs")];
        assert_eq!(
            mount_state_in(&stack, &mnt).unwrap(),
            MountState::Foreign {
                fstype: "tmpfs".into()
            }
        );
        // The directory answers statfs here, so an agentfs entry is live;
        // the stale case (ENOTCONN) needs a real dead mount (tests/mount.rs).
        let stack = [entry("tmpfs", "tmpfs"), entry("fuse.agentfs", "agentfs:v")];
        assert_eq!(
            mount_state_in(&stack, &mnt).unwrap(),
            MountState::Live {
                source: "agentfs:v".into()
            }
        );
        // A path through a symlink resolves to the mount point.
        std::os::unix::fs::symlink(&mnt, root.join("alias")).unwrap();
        assert!(matches!(
            mount_state_in(&stack, &root.join("alias")).unwrap(),
            MountState::Live { .. }
        ));
        // A missing last component is resolved through its parent.
        assert_eq!(
            mount_state_in(&stack, &root.join("missing")).unwrap(),
            MountState::NotMounted
        );
        // So is a path whose parent is missing too (a fresh state area's
        // stack source).
        assert_eq!(
            mount_state_in(&stack, &root.join("no/such/dir")).unwrap(),
            MountState::NotMounted
        );
        // This process's real mountinfo parses and has the root.
        assert!(matches!(
            mount_state(Path::new("/")).unwrap(),
            MountState::Foreign { .. } | MountState::NotMounted
        ));
    }

    #[test]
    fn invalid_remote_sections_are_checked_too() {
        let l = Layout::new();
        let mut s = settings(vec![]);
        s.invalid_remotes = vec![InvalidRemote {
            name: "broken".into(),
            error: "whatever".into(),
            files: vec![
                ("agentfsCredentials", l.p("token")),
                ("agentfsSshKey", l.p("backing/keys/id")),
            ],
            fork_remote: false,
        }];
        let err = l.check_with("state", "backing", "mnt", &s).unwrap_err();
        assert!(
            matches!(&err, PreflightError::CredentialsExposed { key: "agentfsSshKey", remote, .. }
                if remote == "broken"),
            "{err}"
        );
        s.invalid_remotes[0].files.pop();
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
    fn stack_source_is_prepared_on_a_fresh_state_area() {
        // The parent (`<state>/meta`) does not exist yet: looking for stale
        // layers must not fail on it (it once did, so every first recover
        // failed with ENOENT).
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("state/meta/stack");
        prepare_stack_source(&source).unwrap();
        assert!(source.is_dir());
        // Again, over the empty directory it left.
        prepare_stack_source(&source).unwrap();
        fs::write(source.join("f"), "x").unwrap();
        assert!(prepare_stack_source(&source).is_err());
    }

    #[test]
    fn stacking_refuses_another_volume() {
        let p = Path::new("/mnt");
        same_volume(p, "agentfs:vol".into(), "vol").unwrap();
        let err = same_volume(p, "agentfs:other".into(), "vol").unwrap_err();
        assert!(matches!(err, PreflightError::OtherVolume { .. }), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("agentfs:other") && msg.contains("agentfs:vol"),
            "{msg}"
        );
    }

    #[test]
    fn stacking_needs_a_stale_mount() {
        let l = Layout::new();
        let err = preflight_stacked(
            &StateDir::new(l.p("state")),
            &settings(vec![]),
            &l.p("backing"),
            &l.p("mnt"),
        )
        .unwrap_err();
        assert!(matches!(err, PreflightError::NotStale(_)), "{err}");
        assert_eq!(detach_stale_layers(&l.p("mnt"), 4).unwrap(), 0);
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
            repair_store: None,
            stack_over: None,
            read_only: false,
            overlay: None,
        };
        let c = fuser_config(&config, MountKind::Main);
        assert!(c.mount_options.contains(&MountOption::RW));
        config.read_only = true;
        let ro = fuser_config(&config, MountKind::Main);
        assert!(ro.mount_options.contains(&MountOption::RO));
        assert!(!ro.mount_options.contains(&MountOption::RW));
        config.read_only = false;
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
        let c = fuser_config(&config, MountKind::Main);
        assert_eq!(c.acl, SessionACL::Owner);
        assert_eq!(c.n_threads, Some(3));
    }

    #[test]
    fn the_sandbox_overlay_mounts_without_default_permissions_only() {
        let config = MountConfig {
            volume: "vol".into(),
            backing: "/b".into(),
            mountpoint: "/m".into(),
            passthrough: Passthrough::Auto,
            allow_other: true,
            threads: 3,
            sweep_interval: Duration::from_secs(30),
            repair_store: None,
            stack_over: None,
            read_only: false,
            overlay: None,
        };
        let main = fuser_config(&config, MountKind::Main);
        let overlay = fuser_config(&config, MountKind::SandboxOverlay);
        assert!(
            main.mount_options
                .contains(&MountOption::DefaultPermissions)
        );
        assert!(
            !overlay
                .mount_options
                .contains(&MountOption::DefaultPermissions)
        );
        let without: Vec<_> = main
            .mount_options
            .iter()
            .filter(|o| **o != MountOption::DefaultPermissions)
            .cloned()
            .collect();
        assert_eq!(overlay.mount_options, without);
        assert_eq!(overlay.acl, main.acl);
        assert_eq!(overlay.n_threads, main.n_threads);
        assert_eq!(overlay.clone_fd, main.clone_fd);
    }

    #[test]
    fn the_overlay_mount_point_is_its_own_directory() {
        let l = Layout::new();
        fs::create_dir_all(l.p("mnt/inner")).unwrap();
        fs::create_dir_all(l.p("sandbox/overlay/inner")).unwrap();
        let main = l.p("mnt").canonicalize().unwrap();
        for overlay in ["mnt", "mnt/inner", "mnt/../mnt/inner"] {
            let err = preflight_overlay(&main, &l.p(overlay)).unwrap_err();
            assert!(
                matches!(err, PreflightError::OverlayNested { .. }),
                "{overlay}: {err}"
            );
        }
        // The main mount point inside the overlay's.
        let err =
            preflight_overlay(&l.p("sandbox/overlay/inner"), &l.p("sandbox/overlay")).unwrap_err();
        assert!(matches!(err, PreflightError::OverlayNested { .. }), "{err}");
        assert!(err.to_string().contains("one is inside the other"), "{err}");
        // Siblings pass, and the overlay's path comes back canonical.
        assert_eq!(
            preflight_overlay(&main, &l.p("sandbox/../sandbox/overlay")).unwrap(),
            l.p("sandbox/overlay").canonicalize().unwrap()
        );
        assert!(matches!(
            preflight_overlay(&main, &l.p("missing")).unwrap_err(),
            PreflightError::Missing { .. }
        ));
    }

    #[test]
    fn the_overlay_is_refused_on_a_front_and_with_tree_rules() {
        use agentfs_core::rules::{RuleOptions, Rules};
        use agentfs_core::visibility::GateError;

        use crate::overlay::OwnerIds;

        let l = Layout::new();
        let rules = |respect_tree_rules| {
            Arc::new(Rules::new(
                l.p("backing"),
                RuleOptions {
                    respect_tree_rules,
                    ..RuleOptions::default()
                },
                b"",
                b"",
            ))
        };
        let ids = OwnerIds {
            uid: 10000,
            gid: 10000,
        };
        let overlay = OverlayConfig {
            mountpoint: l.p("sandbox/overlay"),
            owner: ViewOwner::new(ids, &Privileges::default(), ids.uid).unwrap(),
            stack_over: None,
        };
        // Without the overlay there is nothing to refuse.
        assert!(overlay_gate(None, true, &rules(true)).unwrap().is_none());
        assert!(
            overlay_gate(Some(&overlay), false, &rules(false))
                .unwrap()
                .is_some()
        );
        let err = overlay_gate(Some(&overlay), true, &rules(false)).unwrap_err();
        assert!(matches!(err, MountError::OverlayUnsupported(_)), "{err}");
        assert!(err.to_string().contains("--history-service"), "{err}");
        let err = overlay_gate(Some(&overlay), false, &rules(true)).unwrap_err();
        assert!(
            matches!(err, MountError::Gate(GateError::TreeRules)),
            "{err}"
        );
    }
}
